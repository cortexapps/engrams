//! The orchestrator's SSE event tail: `GET /api/v1/sessions/:id/events`.
//!
//! Frames are blank-line separated groups of `id:`, `event:` and `data:`
//! lines. The orchestrator sends an `event: ping` every 15 s as a keepalive
//! and omits `id:` on chunk and lagged frames. The parser is the one the Bun
//! CLI carried: a frame is emitted on the blank line that ends it, so a
//! partial frame at EOF is dropped.

use bytes::Bytes;
use futures::StreamExt;

use crate::connect::Transport;
use crate::error::{Code, Error};

/// One SSE frame as the orchestrator emits it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: Vec<String>,
}

/// Line-level SSE parser. Feed it bytes; take frames.
#[derive(Debug, Default)]
pub struct Parser {
    buf: String,
    frame: Frame,
    out: std::collections::VecDeque<Frame>,
}

impl Parser {
    /// Append a chunk. Invalid UTF-8 is replaced, never fatal: the stream is
    /// JSON and the orchestrator writes UTF-8.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        while let Some(nl) = self.buf.find('\n') {
            let mut line = self.buf[..nl].to_string();
            self.buf.drain(..=nl);
            if line.ends_with('\r') {
                line.pop();
            }
            if line.is_empty() {
                let frame = std::mem::take(&mut self.frame);
                self.out.push_back(frame);
            } else if let Some(v) = line.strip_prefix("id:") {
                self.frame.id = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("event:") {
                self.frame.event = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("data:") {
                self.frame.data.push(v.trim_start().to_string());
            }
        }
    }

    /// The next complete frame, if one has been parsed.
    pub fn next_frame(&mut self) -> Option<Frame> {
        self.out.pop_front()
    }
}

type BodyStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// An open event tail.
pub struct EventStream {
    body: BodyStream,
    parser: Parser,
}

impl EventStream {
    /// Open the tail. `since` replays strictly-after that index first;
    /// `last_event_id` is the reconnect cursor (the server uses the larger
    /// of the two).
    pub async fn open(
        transport: &Transport,
        session_id: &str,
        since: Option<i64>,
        last_event_id: Option<i64>,
    ) -> Result<EventStream, Error> {
        let mut url = format!(
            "{}/api/v1/sessions/{}/events",
            transport.host(),
            percent_encode_path(session_id)
        );
        if let Some(s) = since {
            url.push_str(&format!("?since={s}"));
        }
        let mut req = transport
            .authorize(transport.http().get(url))
            .header("accept", "text/event-stream");
        if let Some(id) = last_event_id {
            req = req.header("last-event-id", id.to_string());
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(Error::new(
                Code::from_http_status(status),
                format!("events stream failed: HTTP {status}"),
            ));
        }
        Ok(EventStream {
            body: Box::pin(resp.bytes_stream()),
            parser: Parser::default(),
        })
    }

    /// The next frame, `None` when the server closes the stream.
    pub async fn next(&mut self) -> Result<Option<Frame>, Error> {
        loop {
            if let Some(f) = self.parser.next_frame() {
                return Ok(Some(f));
            }
            match self.body.next().await {
                Some(Ok(chunk)) => self.parser.feed(&chunk),
                Some(Err(e)) => return Err(Error::transport(e)),
                None => return Ok(None),
            }
        }
    }
}

/// `encodeURIComponent` for a path segment: everything but the unreserved
/// set is percent-encoded.
pub fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames_across_chunk_boundaries_and_drops_the_tail() {
        let mut p = Parser::default();
        p.feed(b"event: ping\ndata: \n\nid: 0\nevent: run_st");
        assert_eq!(
            p.next_frame(),
            Some(Frame {
                id: None,
                event: Some("ping".into()),
                data: vec![String::new()]
            })
        );
        assert!(p.next_frame().is_none());
        p.feed(b"arted\r\ndata: {\"a\":1}\ndata: more\n\nid: 1\nevent: partial\n");
        assert_eq!(
            p.next_frame(),
            Some(Frame {
                id: Some("0".into()),
                event: Some("run_started".into()),
                data: vec!["{\"a\":1}".into(), "more".into()],
            })
        );
        assert!(
            p.next_frame().is_none(),
            "a frame without its blank line is not emitted"
        );
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(percent_encode_path("a b/c"), "a%20b%2Fc");
        assert_eq!(percent_encode_path("sess-1_x.y"), "sess-1_x.y");
    }
}
