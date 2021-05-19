use crate::cli::Args;
use anyhow::Error;
use futures::TryFutureExt;
use hyper::header::CONTENT_LENGTH;
use hyper::http::uri::Authority;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Method, Request, Response, Server};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::convert::Infallible;
use std::path::PathBuf;

type HttpClient = Client<hyper::client::HttpConnector>;

const TAURI_OPTIONS: &str = "tauri:options";

#[derive(Debug, Deserialize)]
struct TauriOptions {
  application: PathBuf,
}

impl TauriOptions {
  #[cfg(target_os = "linux")]
  fn into_native_object(self) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
      "webkitgtk:browserOptions".into(),
      json!({"binary": self.application}),
    );
    map
  }

  #[cfg(target_os = "windows")]
  fn into_native_object(self) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("ms:edgeChromium".into(), json!(true));
    map.insert("browserName".into(), json!("webview2"));
    map.insert("ms:edgeOptions".into(), json!({"binary": self.application}));
    map
  }
}

async fn handle(
  client: HttpClient,
  mut req: Request<Body>,
  args: Args,
) -> Result<Response<Body>, Error> {
  // manipulate a new session to convert options to the native driver format
  if let (&Method::POST, "/session") = (req.method(), req.uri().path()) {
    let (mut parts, body) = req.into_parts();

    // get the body from the future stream and parse it as json
    let body = hyper::body::to_bytes(body).await?;
    let json: Value = serde_json::from_slice(&body)?;

    // manipulate the json to convert from tauri option to native driver options
    let json = map_capabilities(json);

    // serialize json and update the content-length header to be accurate
    let bytes = serde_json::to_vec(&json)?;
    parts.headers.insert(CONTENT_LENGTH, bytes.len().into());

    req = Request::from_parts(parts, bytes.into());
  }

  client
    .request(forward_to_native_driver(req, args)?)
    .err_into()
    .await
}

/// Transform the request to a request for the native webdriver server.
fn forward_to_native_driver(mut req: Request<Body>, args: Args) -> Result<Request<Body>, Error> {
  let host: Authority = {
    let headers = req.headers_mut();
    headers.remove("host").expect("hyper request has host")
  }
  .to_str()?
  .parse()?;

  let path = req
    .uri()
    .path_and_query()
    .expect("hyper request has uri")
    .clone();

  let uri = format!(
    "http://{}:{}{}",
    host.host(),
    args.native_port,
    path.as_str()
  );

  let (mut parts, body) = req.into_parts();
  parts.uri = uri.parse()?;
  Ok(Request::from_parts(parts, body))
}

/// only happy path for now, no errors
fn map_capabilities(mut json: Value) -> Value {
  let mut native = None;
  if let Some(capabilities) = json.get_mut("capabilities") {
    if let Some(always_match) = capabilities.get_mut("alwaysMatch") {
      if let Some(always_match) = always_match.as_object_mut() {
        if let Some(tauri_options) = always_match.remove(TAURI_OPTIONS) {
          if let Ok(options) = serde_json::from_value::<TauriOptions>(tauri_options) {
            native = Some(options.into_native_object());
          }
        }

        if let Some(native) = native.clone() {
          always_match.extend(native);
        }
      }
    }
  }

  if let Some(native) = native {
    if let Some(desired) = json.get_mut("desiredCapabilities") {
      if let Some(desired) = desired.as_object_mut() {
        desired.remove(TAURI_OPTIONS);
        desired.extend(native);
      }
    }
  }

  json
}

#[tokio::main(flavor = "current_thread")]
pub async fn run(args: Args) {
  let address = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));

  // the client we use to proxy requests to the native webdriver
  let client = Client::builder()
    .http1_preserve_header_case(true)
    .http1_title_case_headers(true)
    .retry_canceled_requests(false)
    //.set_host(false)
    .build_http();

  // pass a copy of the client to the http request handler
  let service = make_service_fn(move |_| {
    let client = client.clone();
    async move {
      Ok::<_, Infallible>(service_fn(move |request| {
        handle(client.clone(), request, args)
      }))
    }
  });

  // set up a http1 server that uses the service we just created
  let server = Server::bind(&address)
    .http1_title_case_headers(true)
    .http1_preserve_header_case(true)
    .http1_only(true)
    .serve(service);

  if let Err(e) = server.await {
    eprintln!("tauri-driver http server error: {}", e);
  }
}
