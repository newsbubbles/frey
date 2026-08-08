//! Server-sent events, parsed defensively.
//!
//! This module exists because of one verified, expensive failure mode: **a bare `.json()` on an
//! HTTP 200 intermittently throws**, because keepalive comment frames arrive before the body. A
//! client that assumes the first bytes of a 200 are JSON works in development, works in CI, and
//! fails in production under load, when the upstream is slow enough to need keepalives.
//!
//! So: never parse a stream body eagerly. Feed it here, and take frames.

/// One decoded SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A `data:` payload with its optional `event:` name.
    Event {
        /// The event name, if the stream supplied one.
        name: Option<String>,
        /// The payload, with `data:` prefixes stripped and continuation lines joined.
        data: String,
    },
    /// A `: comment` line. Almost always a keepalive. Callers ignore these; the parser surfaces
    /// them so that "the connection is alive but idle" is distinguishable from "nothing arrived".
    Comment(String),
}

/// Incremental SSE decoder.
///
/// Feed it bytes as they arrive; take whole frames as they complete. Partial frames are retained
/// across calls, because a chunk boundary can fall anywhere — including inside a UTF-8 sequence.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
    pending: Vec<u8>,
}

impl SseDecoder {
    /// A decoder with an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk and take whatever frames it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.pending.extend_from_slice(chunk);
        // Decode as much valid UTF-8 as possible, keeping any trailing partial sequence.
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                self.buffer.push_str(text);
                self.pending.clear();
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    // Safety is not needed: `from_utf8` on the validated prefix cannot fail.
                    if let Ok(text) = std::str::from_utf8(&self.pending[..valid]) {
                        self.buffer.push_str(text);
                    }
                    self.pending.drain(..valid);
                }
            }
        }
        self.take_frames()
    }

    /// Flush anything left after the stream ends.
    pub fn finish(&mut self) -> Vec<Frame> {
        if !self.buffer.trim().is_empty() && !self.buffer.ends_with("\n\n") {
            self.buffer.push_str("\n\n");
        }
        self.take_frames()
    }

    fn take_frames(&mut self) -> Vec<Frame> {
        let mut frames = Vec::new();
        // Frames are separated by a blank line. Accept both LF and CRLF endings.
        while let Some(end) = find_frame_end(&self.buffer) {
            let raw: String = self.buffer.drain(..end.0).collect();
            self.buffer.drain(..end.1);
            if let Some(frame) = parse_frame(&raw) {
                frames.push(frame);
            }
        }
        frames
    }
}

/// Returns `(frame_len, separator_len)` for the first complete frame.
fn find_frame_end(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|i| (i, 2));
    let crlf = buffer.find("\r\n\r\n").map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_frame(raw: &str) -> Option<Frame> {
    let mut name = None;
    let mut data: Vec<&str> = Vec::new();
    let mut comment: Option<String> = None;

    for line in raw.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(':') {
            comment = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }

    if !data.is_empty() {
        Some(Frame::Event { name, data: data.join("\n") })
    } else {
        comment.map(Frame::Comment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(frames: Vec<Frame>) -> Vec<String> {
        frames
            .into_iter()
            .filter_map(|f| match f {
                Frame::Event { data, .. } => Some(data),
                Frame::Comment(_) => None,
            })
            .collect()
    }

    #[test]
    fn keepalive_comments_before_the_body_do_not_break_parsing() {
        // The verified production failure: an upstream sends keepalives while it thinks, and a
        // client that calls `.json()` on the response body throws on an HTTP 200.
        let mut d = SseDecoder::new();
        let frames = d.push(b": keepalive\n\n: keepalive\n\ndata: {\"ok\":true}\n\n");

        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], Frame::Comment("keepalive".into()));
        assert_eq!(events(frames), vec![r#"{"ok":true}"#]);
    }

    #[test]
    fn frames_split_across_chunk_boundaries_are_reassembled() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"par").is_empty(), "an incomplete frame yields nothing");
        assert!(d.push(b"tial\":1}").is_empty());
        assert_eq!(events(d.push(b"\n\n")), vec![r#"{"partial":1}"#]);
    }

    #[test]
    fn a_boundary_inside_a_utf8_sequence_is_survived() {
        // é is two bytes. Splitting between them must not corrupt the stream or panic.
        let mut d = SseDecoder::new();
        let payload = "data: {\"t\":\"café\"}\n\n".as_bytes();
        let split = payload.len() - 8;
        d.push(&payload[..split]);
        let frames = d.push(&payload[split..]);
        assert_eq!(events(frames), vec![r#"{"t":"café"}"#]);
    }

    #[test]
    fn named_events_and_multi_line_data_are_both_handled() {
        let mut d = SseDecoder::new();
        let frames = d.push(b"event: content_block_delta\ndata: line one\ndata: line two\n\n");
        assert_eq!(
            frames[0],
            Frame::Event {
                name: Some("content_block_delta".into()),
                data: "line one\nline two".into(),
            }
        );
    }

    #[test]
    fn crlf_line_endings_are_accepted() {
        let mut d = SseDecoder::new();
        assert_eq!(events(d.push(b"data: {\"ok\":1}\r\n\r\n")), vec![r#"{"ok":1}"#]);
    }

    #[test]
    fn a_final_frame_without_a_trailing_blank_line_is_not_lost() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: last").is_empty());
        assert_eq!(events(d.finish()), vec!["last"]);
    }

    #[test]
    fn an_empty_stream_finishes_cleanly() {
        assert!(SseDecoder::new().finish().is_empty());
    }
}
