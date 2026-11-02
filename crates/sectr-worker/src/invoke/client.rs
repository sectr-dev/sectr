//! Invoke client: POST the invocation to the runner's
//! `http://127.0.0.1:$SECTR_PORT/invocations` and turn the SSE response into
//! [`StreamItem`]s. Transport only — finality rules live in the validator.

use futures::StreamExt;
use sectr_common::events::InvocationRequest;

use super::sse::SseDecoder;
use super::validate::{Failure, FrameError, StreamItem};

/// POST the invocation, return the decoded turn stream.
pub struct InvokeClient {
  http: reqwest::Client,
}

/// Request-level failure (connection refused, non-200, timeout). The turn
/// lifecycle maps these onto `StreamItem::Failed`-equivalent handling —
/// they occur before any frame, so there is nothing to invalidate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvokeError {
  #[error("runner unreachable: {0}")]
  Transport(String),
  #[error("runner returned {status}: {body}")]
  HttpStatus {
    status: reqwest::StatusCode,
    body: String,
  },
}

/// Decoded (not yet validated) turn stream: frames, a clean end, or a
/// mid-stream transport failure. Feed to a [`StreamValidator`](super::StreamValidator).
pub type TurnStream = std::pin::Pin<Box<dyn futures::Stream<Item = StreamItem> + Send>>;

impl Default for InvokeClient {
  fn default() -> Self {
    Self::new()
  }
}

impl InvokeClient {
  pub fn new() -> Self {
    InvokeClient {
      http: reqwest::Client::new(),
    }
  }

  /// POST the invocation to `{runner_base}/invocations` (the process reads
  /// its port from `$SECTR_PORT`; it binds localhost) and return the
  /// SSE-decoded response stream. Headers/body per
  /// `schema/invocation.schema.json`: the transcript already includes this
  /// invocation's INVOCATION_STARTED.
  pub async fn invoke(
    &self,
    runner_base: &str,
    request: &InvocationRequest,
  ) -> Result<TurnStream, InvokeError> {
    let url = format!("{}/invocations", runner_base.trim_end_matches('/'));
    let response = self
      .http
      .post(&url)
      .json(request)
      .send()
      .await
      .map_err(|e| InvokeError::Transport(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
      let body = response.text().await.unwrap_or_default();
      return Err(InvokeError::HttpStatus {
        status,
        body: body.chars().take(500).collect(),
      });
    }

    let body = response.bytes_stream();
    // The stream is FINITE: exactly one terminal item (`Ended` or
    // `Failed`), then `None` forever. A previous version kept yielding
    // `Ended` on every poll after the body was exhausted — consumers
    // that collect until `None` (all of them) would loop forever.
    Ok(Box::pin(futures::stream::unfold(
      (body, SseDecoder::default(), false),
      |(mut body, mut decoder, done)| async move {
        if done {
          return None;
        }
        loop {
          if let Some(data) = decoder.next_frame() {
            let frame = serde_json::from_str(&data)
              .map_err(|e| FrameError(format!("invalid runner frame: {e}")));
            return Some((StreamItem::Frame(frame), (body, decoder, false)));
          }
          match body.next().await {
            Some(Ok(chunk)) => decoder.push(&chunk),
            Some(Err(e)) => {
              return Some((
                StreamItem::Failed(Failure::Transport {
                  detail: e.to_string(),
                }),
                (body, decoder, true),
              ));
            }
            None => {
              return Some((StreamItem::Ended, (body, decoder, true)));
            }
          }
        }
      },
    )))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::Body;
  use axum::http::StatusCode;
  use axum::routing::post;
  use futures::StreamExt;
  use sectr_common::events::{EventPayload, InvocationReason};
  use std::time::Duration;
  use tokio::time::timeout;
  use uuid::Uuid;

  fn request() -> InvocationRequest {
    InvocationRequest {
      session_id: Uuid::now_v7(),
      invocation_id: Uuid::now_v7(),
      route: "/agent".into(),
      transcript: Vec::new(),
      reason: InvocationReason::UserMessage {
        input: "hi".to_string().try_into().unwrap(),
        extra: Default::default(),
      },
      extra: Default::default(),
    }
  }

  async fn spawn_server(routes: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
      axum::serve(listener, routes).await.unwrap();
    });
    (format!("http://{addr}"), handle)
  }

  fn sse_body(frames: &[String]) -> Body {
    let payload: Vec<String> = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    Body::from_stream(futures::stream::iter(
      payload.into_iter().map(Ok::<_, std::io::Error>),
    ))
  }

  fn delta_frame(text: &str) -> String {
    sectr_common::events::RunnerEvent::new(EventPayload::MessageDelta(
      sectr_common::events::MessageDelta {
        message_id: "m1".into(),
        delta: sectr_common::events::TypedDelta::Text {
          text: text.into(),
          extra: Default::default(),
        },
        extra: Default::default(),
      },
    ))
    .unwrap()
    .to_json_string()
  }

  async fn collect(mut stream: TurnStream) -> Vec<StreamItem> {
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
      items.push(item);
    }
    items
  }

  #[tokio::test]
  async fn two_frames_in_one_chunk_are_both_drained() {
    // Regression test: ONE body chunk containing TWO complete frames.
    // The stream must yield Frame1, Frame2, Ended — the second frame
    // stays buffered in the decoder across the first yield (unfold
    // state) and is extracted on the next poll BEFORE the exhausted
    // body is consulted. If the loop-top decoder check were skipped
    // after a yield, Frame2 would be silently lost.
    let routes = axum::Router::new().route(
      "/invocations",
      post(|| async {
        let body = format!(
          "data: {}\n\ndata: {}\n\n",
          delta_frame("one"),
          delta_frame("two")
        );
        (
          [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
          Body::from(body), // single chunk
        )
      }),
    );
    let (base, server) = spawn_server(routes).await;
    let stream = InvokeClient::new().invoke(&base, &request()).await.unwrap();
    let items = collect(stream).await;
    server.abort();

    assert_eq!(items.len(), 3, "both frames + Ended, exactly");
    let StreamItem::Frame(Ok(e1)) = &items[0] else {
      panic!("expected frame 1, got {:?}", items[0]);
    };
    let EventPayload::MessageDelta(d1) = &e1.payload else {
      panic!("expected delta");
    };
    let sectr_common::events::TypedDelta::Text { text, .. } = &d1.delta else {
      panic!("expected text delta");
    };
    assert_eq!(text, "one");
    assert!(matches!(items[1], StreamItem::Frame(Ok(_))));
    assert!(matches!(items[2], StreamItem::Ended));
  }

  #[tokio::test]
  async fn decodes_sse_response_into_frames() {
    let routes = axum::Router::new().route(
      "/invocations",
      post(|| async {
        (
          [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
          sse_body(&[delta_frame("he"), delta_frame("llo")]),
        )
      }),
    );
    let (base, server) = spawn_server(routes).await;
    let client = InvokeClient::new();
    let stream = client.invoke(&base, &request()).await.unwrap();
    let items = timeout(Duration::from_secs(5), collect(stream))
      .await
      .expect("stream must finish");
    server.abort();

    assert_eq!(items.len(), 3);
    for item in &items[..2] {
      let StreamItem::Frame(Ok(event)) = item else {
        panic!("expected frames, got {item:?}");
      };
      assert_eq!(
        event.event_type,
        sectr_common::events::RunnerEventType::MessageDelta
      );
    }
    assert!(matches!(items[2], StreamItem::Ended));
  }

  #[tokio::test]
  async fn invalid_frame_becomes_frame_error() {
    let routes = axum::Router::new().route(
      "/invocations",
      post(|| async {
        (
          [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
          sse_body(&["{not json".to_string()]),
        )
      }),
    );
    let (base, server) = spawn_server(routes).await;
    let stream = InvokeClient::new().invoke(&base, &request()).await.unwrap();
    let items = collect(stream).await;
    server.abort();

    assert_eq!(items.len(), 2);
    assert!(matches!(&items[0], StreamItem::Frame(Err(FrameError(_)))));
  }

  #[tokio::test]
  async fn non_200_is_http_status_error() {
    let routes = axum::Router::new().route(
      "/invocations",
      post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let (base, server) = spawn_server(routes).await;
    let result = InvokeClient::new().invoke(&base, &request()).await;
    server.abort();
    let InvokeError::HttpStatus { status, body } = result.err().expect("expected error") else {
      panic!("expected HttpStatus");
    };
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("boom"));
  }

  #[tokio::test]
  async fn unreachable_runner_is_transport_error() {
    let result = InvokeClient::new()
      .invoke("http://127.0.0.1:1", &request())
      .await;
    assert!(matches!(
      result.err().expect("expected error"),
      InvokeError::Transport(_)
    ));
  }
}
