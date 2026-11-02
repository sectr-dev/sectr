//! Minimal SSE frame decoder for the invoke response stream: byte chunks in
//! (any chunking), `data:` payloads out. Per the invoke contract each
//! frame's `data:` is one JSON document (`runnerEvent`); the runner uses no
//! other fields.
//!
//! What is supported (everything the contract needs, nothing more):
//! - LF and CRLF line terminators (lone-CR is not — the SSE spec allows it,
//!   but no HTTP sender produces it and the contract never will).
//! - Multi-line `data:` frames (lines joined with `\n` per the SSE spec).
//! - `:` comment lines and `event:`/`id:`/`retry:` fields, ignored.
//! - Comment-only / field-only frames are skipped entirely.
//!
//! An incomplete trailing frame at end of stream is discarded (per spec).

/// Byte chunks in, complete SSE `data:` payloads out.
#[derive(Default)]
pub struct SseDecoder {
  buf: Vec<u8>,
}

impl SseDecoder {
  /// Feed raw bytes. Any chunking is fine — frames are only emitted once
  /// their terminating blank line has arrived.
  pub fn push(&mut self, chunk: &[u8]) {
    // Strip CR: after this, lines end at LF (CRLF collapses to LF).
    self
      .buf
      .extend(chunk.iter().copied().filter(|&b| b != b'\r'));
  }

  /// Pop the next completed frame's data payload, skipping frames with no
  /// data lines. `None` means "need more bytes" (or stream end).
  pub fn next_frame(&mut self) -> Option<String> {
    loop {
      let end = find_blank_line(&self.buf)?;
      let frame: Vec<u8> = self.buf.drain(..end + 2).collect();
      let frame = std::str::from_utf8(&frame[..end]).ok()?;
      if let Some(data) = frame_data(frame) {
        return Some(data);
      }
      // No data lines (comments / other fields only) — skip to the
      // next frame.
    }
  }
}

/// Index of the first blank line (`\n\n`) in `buf`, if complete.
fn find_blank_line(buf: &[u8]) -> Option<usize> {
  buf.windows(2).position(|w| w == b"\n\n")
}

fn frame_data(frame: &str) -> Option<String> {
  let mut lines = Vec::new();
  for line in frame.split('\n') {
    if line.starts_with(':') {
      continue; // comment
    }
    if let Some(value) = line.strip_prefix("data:") {
      // Strip exactly one optional leading space (SSE spec).
      lines.push(value.strip_prefix(' ').unwrap_or(value));
    }
    // event:/id:/retry: and anything else: ignored.
  }
  if lines.is_empty() {
    None
  } else {
    Some(lines.join("\n"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn frames(input: &[u8]) -> Vec<String> {
    let mut dec = SseDecoder::default();
    dec.push(input);
    let mut out = Vec::new();
    while let Some(f) = dec.next_frame() {
      out.push(f);
    }
    out
  }

  #[test]
  fn single_frame_lf() {
    assert_eq!(frames(b"data: {\"a\":1}\n\n"), vec!["{\"a\":1}"]);
  }

  #[test]
  fn crlf_terminators() {
    assert_eq!(
      frames(b"data: one\r\n\r\ndata: two\r\n\r\n"),
      vec!["one", "two"]
    );
  }

  #[test]
  fn chunk_boundaries_are_invisible() {
    let mut dec = SseDecoder::default();
    let bytes = b"data: hel";
    dec.push(bytes); // "data: hel" — mid-frame
    assert!(dec.next_frame().is_none());
    dec.push(b"lo\n");
    assert!(dec.next_frame().is_none()); // blank line not yet seen
    dec.push(b"\n\ndata: {\"x\":");
    assert_eq!(dec.next_frame(), Some("hello".to_string()));
    dec.push(b"2}\n\n");
    assert_eq!(dec.next_frame(), Some("{\"x\":2}".to_string()));
    assert!(dec.next_frame().is_none());
  }

  #[test]
  fn blank_line_split_across_chunks() {
    let mut dec = SseDecoder::default();
    dec.push(b"data: a\n");
    dec.push(b"\ndata: b\n\n");
    assert_eq!(dec.next_frame(), Some("a".to_string()));
    assert_eq!(dec.next_frame(), Some("b".to_string()));
  }

  #[test]
  fn multiline_data_joins_with_newline() {
    assert_eq!(frames(b"data: l1\ndata: l2\n\n"), vec!["l1\nl2"]);
  }

  #[test]
  fn comments_and_other_fields_ignored() {
    assert_eq!(
      frames(b": keepalive\nevent: message\ndata: payload\nid: 7\nretry: 100\n\n"),
      vec!["payload"]
    );
  }

  #[test]
  fn frame_without_data_is_skipped() {
    assert_eq!(frames(b": ping\n\n:data: fake\n"), Vec::<String>::new());
    assert_eq!(frames(b": ping\n\ndata: real\n\n"), vec!["real"]);
  }

  #[test]
  fn incomplete_trailing_frame_is_held() {
    let mut dec = SseDecoder::default();
    dec.push(b"data: {\"partial\":");
    assert!(dec.next_frame().is_none());
    // Stream ends mid-frame → the partial frame is discarded by the
    // caller (nothing calls next_frame again with more data).
    assert_eq!(dec.next_frame(), None);
  }
}
