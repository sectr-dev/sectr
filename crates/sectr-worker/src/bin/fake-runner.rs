//! A minimal fake invoke-contract runner for container E2E tests:
//! binds `127.0.0.1:$SECTR_PORT`, answers every `POST /invocations` with an
//! SSE body replaying the runner-event frames from the `SECTR_FRAMES_JSON`
//! env (a JSON array of runner-event JSON strings — the same script format
//! as `sectr_worker::testing`'s python fake runner, but env-borne so it can
//! ride the assignment's runner env into a container with no python).
//!
//! Not a production artifact — dev/test tooling only.

#[tokio::main]
async fn main() {
  let port: u16 = std::env::var("SECTR_PORT")
    .expect("SECTR_PORT is required")
    .parse()
    .expect("SECTR_PORT must be a port number");
  let frames_json = std::env::var("SECTR_FRAMES_JSON").expect("SECTR_FRAMES_JSON is required");
  let frames: Vec<String> =
    serde_json::from_str(&frames_json).expect("frames must be a JSON array of strings");
  // Optional: hold each response stream open (no EOF) for N seconds after
  // the frames — a runner that goes silent WITHOUT ending the stream. The
  // restart test uses it to exercise lease-expiry reclaim (a stream
  // END would let the ENGINE synthesize the turn end instead).
  let hang_secs: u64 = std::env::var("SECTR_HANG_SECS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0);

  let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
    .await
    .expect("bind SECTR_PORT");
  eprintln!("fake-runner listening on 127.0.0.1:{port}");
  loop {
    let Ok((mut socket, _)) = listener.accept().await else {
      continue;
    };
    let frames = frames.clone();
    tokio::spawn(async move {
      // One request per connection is enough for the invoke contract
      // (the invoke client opens a fresh POST per invocation).
      let mut buf = Vec::with_capacity(4096);
      let mut chunk = [0u8; 4096];
      // Read until end of headers (the invoke request has a JSON body
      // we don't need to parse — Content-Length bounds it, but we can
      // just read a bit past the header terminator for the fake).
      loop {
        match socket.try_read(&mut chunk) {
          Ok(0) => return,
          Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
              break;
            }
          }
          Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
          }
          Err(_) => return,
        }
      }
      let body = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
        frames
          .iter()
          .map(|f| format!("data: {f}\n\n"))
          .collect::<String>()
      );
      use tokio::io::AsyncWriteExt;
      if socket.write_all(body.as_bytes()).await.is_ok() {
        if hang_secs > 0 {
          tokio::time::sleep(std::time::Duration::from_secs(hang_secs)).await;
        }
        let _ = socket.shutdown().await;
      }
    });
  }
}
