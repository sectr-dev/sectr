//! Middleware tests: request-id echo/chaining, and that handler events
//! carry the `http.request` span with the request id + session id (the
//! whole point — flat events become children of their request).

use super::*;
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use tower::ServiceExt;

async fn handler() -> &'static str {
  tracing::info!("handler event inside the request span");
  "ok"
}

fn test_app() -> Router {
  // Minimal one-route router — the middleware is stateless; auth and the
  // registry are not involved on this route.
  Router::new()
    .route("/sessions/{session_id}", axum::routing::get(handler))
    .layer(axum::middleware::from_fn(request_span))
}

/// Run `f` under a JSON fmt subscriber whose output lands in `out`.
fn capture_logs<R>(out: std::sync::Arc<std::sync::Mutex<Vec<u8>>>, f: impl FnOnce() -> R) -> R {
  use tracing_subscriber::fmt::MakeWriter;
  struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
  impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
      CaptureWriter(self.0.clone())
    }
  }
  struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
  impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      self.0.lock().unwrap().extend_from_slice(buf);
      Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  let subscriber = tracing_subscriber::fmt()
    .json()
    .with_writer(Capture(out))
    .finish();
  tracing::subscriber::with_default(subscriber, f)
}

/// Current-thread runtime factory — one per call, never nested inside a
/// capturing subscriber scope (see the NOTE above).
fn rt() -> tokio::runtime::Runtime {
  tokio::runtime::Builder::new_current_thread()
    .build()
    .unwrap()
}

/// Throwaway capture — installs a live dispatcher for the whole request so
/// the shared handler's callsite Interest is first-registered against a
/// REAL dispatcher. libtest runs tests in parallel threads in one process
/// and the Interest cache is process-global: the first touch must be under
/// a live subscriber, or later capturing tests lose the span context.
#[test]
fn echoes_request_id_and_records_status() {
  let rt = rt();
  let throwaway = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
  let response = capture_logs(throwaway, || {
    rt.block_on(async {
      let req = axum::http::Request::builder()
        .uri("/sessions/0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
        .body(Body::empty())
        .unwrap();
      test_app().oneshot(req).await.unwrap()
    })
  });
  assert_eq!(response.status(), StatusCode::OK);
  let echoed = response
    .headers()
    .get(&REQUEST_ID_HEADER)
    .and_then(|v| v.to_str().ok())
    .expect("X-Request-Id echoed");
  Uuid::parse_str(echoed).expect("echoed request id is a UUID");
}

#[test]
fn honors_inbound_request_id() {
  let rt = rt();
  let throwaway = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
  let response = capture_logs(throwaway, || {
    rt.block_on(async {
      let req = axum::http::Request::builder()
        .uri("/sessions/0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c")
        .header(&REQUEST_ID_HEADER, "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c")
        .body(Body::empty())
        .unwrap();
      test_app().oneshot(req).await.unwrap()
    })
  });
  assert_eq!(
    response.headers().get(&REQUEST_ID_HEADER).unwrap(),
    "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c"
  );
}

#[test]
fn session_id_path_param_lands_on_span() {
  let rt = rt(); // runtime FIRST, subscriber second
  let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
  capture_logs(out.clone(), || {
    let response = rt.block_on(async {
      let req = axum::http::Request::builder()
        .uri("/sessions/0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5d")
        .body(Body::empty())
        .unwrap();
      test_app().oneshot(req).await.unwrap()
    });
    assert_eq!(response.status(), StatusCode::OK);
  });

  // The handler's event must carry the http.request span with the path
  // param stamped (JSON fmt embeds `span` on child events).
  let logs = String::from_utf8_lossy(&out.lock().unwrap()).to_string();
  let event = logs
    .lines()
    .find(|l| l.contains("handler event inside the request span"))
    .expect("handler event captured");
  let parsed: serde_json::Value = serde_json::from_str(event).unwrap();
  let span = parsed.get("span").expect("event is inside a span");
  assert_eq!(span.get("name").unwrap(), "http.request");
  assert_eq!(
    span.get("session_id").unwrap(),
    "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5d"
  );
  assert!(span.get("request_id").is_some());
  assert!(span.get("status").is_none(), "status records on close only");
}
