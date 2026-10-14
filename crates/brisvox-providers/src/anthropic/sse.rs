//! Incremental Server-Sent Events parser with a bounded buffer.
//!
//! Follows the WHATWG SSE line grammar: lines end in `\n`, `\r\n` or `\r`; a blank line
//! dispatches the event; `data` lines are joined with `\n`; lines starting with `:` are comments
//! (keep-alives) and the `event`, `id` and `retry` fields are ignored (the adapter dispatches on
//! the JSON `type` field: Anthropic repeats it in the `event:` line, but the JSON is
//! authoritative). An event that is still incomplete when the stream ends is dropped, as the
//! spec says.
//!
//! This is the same parser as the OpenAI adapter's (copied rather than shared so the adapters
//! stay independent features).
//!
//! The parser is fed with arbitrary chunks ([`SseParser::push`]) and lent the dispatched data
//! one event at a time ([`SseParser::next_event`]) without a per-event allocation: the data
//! lives in a buffer that is reused.

use std::fmt;

/// Why the stream cannot be parsed further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseError {
    /// An unfinished event holds more than the configured number of bytes.
    TooLarge {
        /// The configured cap.
        limit: usize,
    },
    /// The event data is not UTF-8.
    InvalidUtf8,
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { limit } => write!(f, "SSE event larger than {limit} bytes"),
            Self::InvalidUtf8 => f.write_str("SSE event data is not UTF-8"),
        }
    }
}

/// Buffers bytes and yields dispatched events.
#[derive(Debug)]
pub(crate) struct SseParser {
    /// Received bytes; `buf[start..]` is not consumed yet.
    buf: Vec<u8>,
    start: usize,
    /// Of `buf[start..]`, the prefix known to contain no line terminator (never rescanned).
    scanned: usize,
    /// Data of the event being assembled (or, after `next_event`, of the event just lent).
    data: Vec<u8>,
    has_data: bool,
    /// `data` holds an event that was lent out: clear it before assembling the next one.
    lent: bool,
    max_bytes: usize,
}

impl SseParser {
    /// A parser that holds at most `max_bytes` for one unfinished event.
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
            scanned: 0,
            data: Vec::new(),
            has_data: false,
            lent: false,
            max_bytes,
        }
    }

    /// Appends received bytes.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// The data of the next complete event, `Ok(None)` when more bytes are needed.
    ///
    /// The returned text is valid until the next call. Call it until it returns `Ok(None)`
    /// after every [`push`](Self::push).
    ///
    /// # Errors
    /// [`SseError::TooLarge`] when the unfinished remainder exceeds the cap,
    /// [`SseError::InvalidUtf8`] for a non-UTF-8 event.
    pub(crate) fn next_event(&mut self) -> Result<Option<&str>, SseError> {
        if self.lent {
            self.data.clear();
            self.lent = false;
        }
        let mut dispatch = false;
        loop {
            let pending = self.buf.get(self.start..).unwrap_or_default();
            let scan = pending.get(self.scanned..).unwrap_or_default();
            let Some(offset) = scan.iter().position(|b| matches!(b, b'\n' | b'\r')) else {
                self.scanned = pending.len();
                break;
            };
            let end = self.scanned + offset;
            let terminator = match (pending.get(end), pending.get(end + 1)) {
                (Some(b'\r'), Some(b'\n')) => 2,
                // A trailing `\r` may be the first half of `\r\n`: wait for the next byte.
                (Some(b'\r'), None) => {
                    self.scanned = end;
                    break;
                }
                _ => 1,
            };
            let line = pending.get(..end).unwrap_or_default();
            if line.is_empty() {
                self.advance(end + terminator);
                if self.has_data {
                    self.has_data = false;
                    dispatch = true;
                    break;
                }
                continue;
            }
            if line.first() != Some(&b':') {
                let (field, value) = match line.iter().position(|b| *b == b':') {
                    Some(colon) => {
                        let value = line.get(colon + 1..).unwrap_or_default();
                        (
                            line.get(..colon).unwrap_or_default(),
                            value.strip_prefix(b" ").unwrap_or(value),
                        )
                    }
                    None => (line, &[][..]),
                };
                if field == b"data" {
                    if self.has_data {
                        self.data.push(b'\n');
                    }
                    self.data.extend_from_slice(value);
                    self.has_data = true;
                    if self.data.len() > self.max_bytes {
                        return Err(SseError::TooLarge {
                            limit: self.max_bytes,
                        });
                    }
                }
            }
            self.advance(end + terminator);
        }
        self.compact();
        if dispatch {
            self.lent = true;
            return std::str::from_utf8(&self.data)
                .map(Some)
                .map_err(|_| SseError::InvalidUtf8);
        }
        let held = self.buf.len().saturating_sub(self.start) + self.data.len();
        if held > self.max_bytes {
            return Err(SseError::TooLarge {
                limit: self.max_bytes,
            });
        }
        Ok(None)
    }

    /// Marks `n` pending bytes as consumed.
    fn advance(&mut self, n: usize) {
        self.start += n;
        self.scanned = 0;
    }

    /// Reclaims consumed bytes.
    fn compact(&mut self) {
        if self.start >= self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start >= 16 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `chunks` and collects every dispatched event.
    fn events(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = SseParser::new(1 << 20);
        let mut out = Vec::new();
        for chunk in chunks {
            parser.push(chunk);
            while let Some(event) = parser.next_event().unwrap() {
                out.push(event.to_owned());
            }
        }
        out
    }

    const STREAM: &[u8] = b"event: a\ndata: {\"n\":1}\n\n: keep-alive\n\ndata: two\r\ndata: lines\r\n\r\ndata:nospace\r\rid: 7\nretry: 5\n\ndata: [DONE]\n\n";

    fn expected() -> Vec<String> {
        vec![
            "{\"n\":1}".to_owned(),
            "two\nlines".to_owned(),
            "nospace".to_owned(),
            "[DONE]".to_owned(),
        ]
    }

    #[test]
    fn a_whole_stream_in_one_chunk_yields_every_event() {
        assert_eq!(events(&[STREAM]), expected());
    }

    #[test]
    fn splitting_the_stream_at_every_byte_boundary_changes_nothing() {
        for cut in 0..=STREAM.len() {
            let (a, b) = STREAM.split_at(cut);
            assert_eq!(events(&[a, b]), expected(), "split at {cut}");
        }
        let singles: Vec<&[u8]> = STREAM.chunks(1).collect();
        assert_eq!(events(&singles), expected());
    }

    #[test]
    fn comments_and_ignored_fields_dispatch_nothing() {
        assert!(events(&[b": ping\n\n: ping\n\nevent: x\nid: 1\n\n"]).is_empty());
    }

    #[test]
    fn an_incomplete_event_at_the_end_is_dropped() {
        assert_eq!(events(&[b"data: one\n\ndata: tw"]), vec!["one".to_owned()]);
        assert!(events(&[b"data: one\n"]).is_empty());
    }

    #[test]
    fn a_data_line_without_a_colon_is_an_empty_data_field() {
        assert_eq!(events(&[b"data\n\n"]), vec![String::new()]);
        assert_eq!(events(&[b"data\ndata: x\n\n"]), vec!["\nx".to_owned()]);
    }

    #[test]
    fn a_crlf_split_between_chunks_is_one_terminator() {
        assert_eq!(
            events(&[b"data: a\r", b"\n\r", b"\ndata: b\r\n\r\n"]),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn an_event_over_the_cap_is_an_error_not_unbounded_growth() {
        let mut parser = SseParser::new(64);
        parser.push(&[b'x'; 200]);
        assert_eq!(parser.next_event(), Err(SseError::TooLarge { limit: 64 }));
        let mut parser = SseParser::new(64);
        parser.push(b"data: ");
        parser.push(&[b'x'; 100]);
        parser.push(b"\n\n");
        assert_eq!(parser.next_event(), Err(SseError::TooLarge { limit: 64 }));
    }

    #[test]
    fn many_small_events_in_one_chunk_are_not_capped_by_their_total() {
        let mut parser = SseParser::new(64);
        parser.push(&b"data: 0123456789\n\n".repeat(50));
        let mut count = 0;
        while parser.next_event().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 50);
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        let mut parser = SseParser::new(64);
        parser.push(b"data: \xff\xfe\n\n");
        assert_eq!(parser.next_event(), Err(SseError::InvalidUtf8));
    }

    #[test]
    fn the_buffer_is_reclaimed_between_events() {
        let mut parser = SseParser::new(1 << 20);
        for _ in 0..1000 {
            parser.push(b"data: 0123456789abcdef\n\n");
            assert!(parser.next_event().unwrap().is_some());
            assert!(parser.next_event().unwrap().is_none());
        }
        assert!(parser.buf.is_empty());
    }
}
