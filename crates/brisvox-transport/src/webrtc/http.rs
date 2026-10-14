//! A minimal HTTP/1.1 server side for the signaling endpoint.
//!
//! WHIP needs four verbs on two paths and bodies of a few kilobytes of SDP, so the transport
//! parses requests itself instead of adding a web framework: a request line, a header block of
//! at most [`MAX_HEADER_BYTES`] and [`MAX_HEADERS`] fields, and a `Content-Length` body (chunked
//! requests are refused with 411). One request per connection: every response says
//! `Connection: close`. All reads and writes run under a deadline on the injected clock.

use std::future::Future;
use std::time::Duration;

use brisvox_core::Clock;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest request head (request line and headers) accepted.
pub(crate) const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Most header fields accepted.
pub(crate) const MAX_HEADERS: usize = 64;

/// A deadline of [`timeout`] passed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Elapsed;

/// Runs `future`, giving up after `after` on `clock` (the injected clock, not `tokio::time`, so a
/// simulated clock drives every timeout). `future` is dropped when the deadline wins.
pub(crate) async fn timeout<F: Future>(
    clock: &dyn Clock,
    after: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    tokio::select! {
        biased;
        output = future => Ok(output),
        () = clock.sleep(after) => Err(Elapsed),
    }
}

/// Writes all of `data` to `writer` and flushes it, giving up `limit` after the write first
/// blocks (on the injected clock). Unlike [`timeout`] around `write_all`, a write the socket
/// takes at once (the usual case for a media packet) never creates a timer: the deadline is
/// armed on the first `Pending` and kept until the write completes.
pub(crate) async fn write_bounded<W>(
    clock: &dyn Clock,
    limit: Duration,
    writer: &mut W,
    data: &[u8],
) -> Result<std::io::Result<()>, Elapsed>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    use std::task::Poll;

    let mut sent = 0usize;
    let mut flushed = false;
    let mut deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    std::future::poll_fn(|cx| {
        loop {
            let rest = data.get(sent..).unwrap_or_default();
            if !rest.is_empty() {
                match std::pin::Pin::new(&mut *writer).poll_write(cx, rest) {
                    Poll::Ready(Ok(0)) => {
                        return Poll::Ready(Ok(Err(std::io::ErrorKind::WriteZero.into())));
                    }
                    Poll::Ready(Ok(n)) => {
                        sent = sent.saturating_add(n);
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Ok(Err(e))),
                    Poll::Pending => {}
                }
            } else if !flushed {
                match std::pin::Pin::new(&mut *writer).poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        flushed = true;
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Ok(Err(e))),
                    Poll::Pending => {}
                }
            } else {
                return Poll::Ready(Ok(Ok(())));
            }
            // The socket would block: from here on the write has a deadline.
            let timer = deadline.get_or_insert_with(|| Box::pin(clock.sleep(limit)));
            return match timer.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
                Poll::Pending => Poll::Pending,
            };
        }
    })
    .await
}

/// A parsed request.
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    /// The request target without its query string.
    pub(crate) path: String,
    /// The query string after `?`, without it (`None` when the target has none). Only the
    /// WebSocket upgrade reads it (`?token=`).
    pub(crate) query: Option<String>,
    headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    /// A request with the given headers, for tests.
    #[cfg(test)]
    pub(crate) fn for_test(method: &str, path: &str, headers: &[(&str, &str)]) -> Self {
        Self {
            method: method.to_owned(),
            path: path.to_owned(),
            query: None,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: Vec::new(),
        }
    }

    /// The value of header `name` (case-insensitive), if present.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Every value of header `name` in request order, joined with `, ` (a list header may be
    /// repeated: RFC 9110 5.3). `None` when it is absent.
    pub(crate) fn header_joined(&self, name: &str) -> Option<String> {
        let values: Vec<&str> = self
            .headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect();
        (!values.is_empty()).then(|| values.join(", "))
    }
}

/// Why no request could be read.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// The peer closed or the socket failed before a full request arrived: nobody to answer.
    Gone,
    /// The request did not complete in time: nobody worth answering.
    Slow,
    /// The request is unacceptable; answer with this.
    Refuse(Response),
}

fn refuse(status: u16, message: &str) -> ReadError {
    ReadError::Refuse(Response::text(status, message))
}

/// Reads one request from `stream`, all within `limit` on `clock`.
pub(crate) async fn read_request<S>(
    stream: &mut S,
    clock: &dyn Clock,
    limit: Duration,
    max_body: usize,
) -> Result<Request, ReadError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match timeout(clock, limit, read_request_inner(stream, max_body)).await {
        Ok(result) => result,
        Err(Elapsed) => Err(ReadError::Slow),
    }
}

async fn read_request_inner<S>(stream: &mut S, max_body: usize) -> Result<Request, ReadError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(position) = find_head_end(&buf) {
            break position;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(refuse(431, "request head too large"));
        }
        let n = stream.read(&mut chunk).await.map_err(|_| ReadError::Gone)?;
        if n == 0 {
            return Err(ReadError::Gone);
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    };
    if head_end > MAX_HEADER_BYTES {
        return Err(refuse(431, "request head too large"));
    }
    let head = std::str::from_utf8(buf.get(..head_end).unwrap_or_default())
        .map_err(|_| refuse(400, "request head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(refuse(400, "malformed request line"));
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(refuse(400, "malformed method"));
    }
    if !version.starts_with("HTTP/1.") {
        return Err(refuse(505, "HTTP/1.x only"));
    }
    if !target.starts_with('/') {
        return Err(refuse(400, "request target must be an absolute path"));
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(refuse(431, "too many header fields"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(refuse(400, "malformed header field"));
        };
        if name.is_empty() || name.starts_with([' ', '\t']) || name.ends_with([' ', '\t']) {
            return Err(refuse(400, "malformed header name"));
        }
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
    let mut request = Request {
        method: method.to_owned(),
        path: target.split('?').next().unwrap_or(target).to_owned(),
        query: target.split_once('?').map(|(_, query)| query.to_owned()),
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding").is_some() {
        return Err(refuse(411, "chunked request bodies are not supported"));
    }
    let length = content_length(&request)?;
    if length > max_body {
        return Err(refuse(413, "request body too large"));
    }
    if length > 0
        && request
            .header("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    {
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .map_err(|_| ReadError::Gone)?;
    }
    let body_start = head_end + 4;
    let mut body = buf.get(body_start..).unwrap_or_default().to_vec();
    if body.len() > length {
        return Err(refuse(400, "bytes after the request body"));
    }
    body.reserve(length - body.len());
    while body.len() < length {
        let want = (length - body.len()).min(chunk.len());
        let n = stream
            .read(chunk.get_mut(..want).unwrap_or_default())
            .await
            .map_err(|_| ReadError::Gone)?;
        if n == 0 {
            return Err(ReadError::Gone);
        }
        body.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
    request.body = body;
    Ok(request)
}

/// Offset of the `\r\n\r\n` that ends the head.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The request's `Content-Length`: 0 when absent, an error when malformed or repeated with
/// different values.
fn content_length(request: &Request) -> Result<usize, ReadError> {
    let mut found: Option<usize> = None;
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("content-length") {
            let parsed: usize = value
                .parse()
                .map_err(|_| refuse(400, "malformed Content-Length"))?;
            if found.is_some_and(|previous| previous != parsed) {
                return Err(refuse(400, "conflicting Content-Length"));
            }
            found = Some(parsed);
        }
    }
    Ok(found.unwrap_or(0))
}

/// A response to write.
#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    /// A response with no body.
    pub(crate) fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A plain-text response.
    pub(crate) fn text(status: u16, message: &str) -> Self {
        Self::new(status).body("text/plain; charset=utf-8", message.as_bytes().to_vec())
    }

    /// Adds a header. `value` must not contain CR or LF (callers pass fixed text, validated
    /// configuration and ids made by this crate).
    pub(crate) fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Sets the body and its content type.
    pub(crate) fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers.push(("Content-Type", content_type.to_owned()));
        self.body = body;
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

/// Writes `response` and flushes, within `limit` on `clock`. Errors are the peer's business
/// (it may have gone), so they are only reported as a bool.
pub(crate) async fn write_response<S>(
    stream: &mut S,
    clock: &dyn Clock,
    limit: Duration,
    response: &Response,
) -> bool
where
    S: AsyncWrite + Unpin,
{
    let mut out = Vec::with_capacity(256 + response.body.len());
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\n",
            response.status,
            reason(response.status)
        )
        .as_bytes(),
    );
    for (name, value) in &response.headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            response.body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&response.body);
    let write = async {
        stream.write_all(&out).await?;
        stream.flush().await
    };
    matches!(timeout(clock, limit, write).await, Ok(Ok(())))
}

/// Writes the `101 Switching Protocols` answer of a WebSocket upgrade (RFC 6455 4.2.2) within
/// `limit` on `clock`. `accept_key` is the derived `Sec-WebSocket-Accept` (base64) and
/// `subprotocol` the selected `Sec-WebSocket-Protocol`, if any (printable ASCII; the caller
/// checked). Returns whether the answer was written.
pub(crate) async fn write_switching_protocols<S>(
    stream: &mut S,
    clock: &dyn Clock,
    limit: Duration,
    accept_key: &str,
    subprotocol: Option<&str>,
) -> bool
where
    S: AsyncWrite + Unpin,
{
    let mut out = String::with_capacity(192);
    out.push_str(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n",
    );
    out.push_str("Sec-WebSocket-Accept: ");
    out.push_str(accept_key);
    out.push_str("\r\nCache-Control: no-store\r\n");
    if let Some(subprotocol) = subprotocol {
        out.push_str("Sec-WebSocket-Protocol: ");
        out.push_str(subprotocol);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    matches!(
        write_bounded(clock, limit, stream, out.as_bytes()).await,
        Ok(Ok(()))
    )
}

/// Longest the server lingers to read what a refused client is still sending.
const LINGER: Duration = Duration::from_millis(500);

/// Most bytes read and discarded while lingering.
const LINGER_BYTES: usize = 256 * 1024;

/// Closes a connection politely: sends FIN, then reads and discards what the client is still
/// sending (bounded in time and bytes). Closing with unread data pending would reset the
/// connection, and the client would see a reset instead of the response (a 413 for a big body).
pub(crate) async fn close_politely<S>(stream: &mut S, clock: &dyn Clock)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let drain = async {
        let _ = stream.shutdown().await;
        let mut sink = [0u8; 4096];
        let mut total = 0usize;
        while total < LINGER_BYTES {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
        }
    };
    let _ = timeout(clock, LINGER, drain).await;
}

#[cfg(test)]
mod tests {
    use brisvox_core::SystemClock;
    use tokio::io::duplex;

    use super::*;

    const LIMIT: Duration = Duration::from_secs(2);

    async fn parse(raw: &[u8], max_body: usize) -> Result<Request, ReadError> {
        let clock = SystemClock;
        let (mut client, mut server) = duplex(64 * 1024);
        client.write_all(raw).await.unwrap();
        drop(client);
        read_request(&mut server, &clock, LIMIT, max_body).await
    }

    fn refused(result: Result<Request, ReadError>) -> u16 {
        match result {
            Err(ReadError::Refuse(response)) => response.status,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_post_with_a_body_parses() {
        let request = parse(
            b"POST /whip?x=1 HTTP/1.1\r\nHost: h\r\nContent-Type: application/sdp\r\nContent-Length: 5\r\n\r\nv=0\r\n",
            100,
        )
        .await
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/whip");
        assert_eq!(request.header("content-type"), Some("application/sdp"));
        assert_eq!(request.header("CONTENT-TYPE"), Some("application/sdp"));
        assert_eq!(request.body, b"v=0\r\n");
    }

    #[tokio::test]
    async fn a_body_arriving_in_pieces_is_assembled() {
        let clock = SystemClock;
        let (mut client, mut server) = duplex(64);
        let writer = tokio::spawn(async move {
            client
                .write_all(b"POST /w HTTP/1.1\r\nContent-Length: 10\r\n\r\n01234")
                .await
                .unwrap();
            SystemClock.sleep(Duration::from_millis(20)).await;
            client.write_all(b"56789").await.unwrap();
            client
        });
        let request = read_request(&mut server, &clock, LIMIT, 100).await.unwrap();
        assert_eq!(request.body, b"0123456789");
        drop(writer.await.unwrap());
    }

    #[tokio::test]
    async fn malformed_and_oversized_requests_are_refused() {
        assert_eq!(refused(parse(b"BOGUS\r\n\r\n", 10).await), 400);
        assert_eq!(refused(parse(b"get / HTTP/1.1\r\n\r\n", 10).await), 400);
        assert_eq!(refused(parse(b"GET / HTTP/2.0\r\n\r\n", 10).await), 505);
        assert_eq!(refused(parse(b"GET x HTTP/1.1\r\n\r\n", 10).await), 400);
        assert_eq!(
            refused(parse(b"GET / HTTP/1.1\r\nno colon here\r\n\r\n", 10).await),
            400
        );
        assert_eq!(
            refused(parse(b"POST / HTTP/1.1\r\nContent-Length: 11\r\n\r\n", 10).await),
            413
        );
        assert_eq!(
            refused(parse(b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n", 10).await),
            400
        );
        assert_eq!(
            refused(
                parse(
                    b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab",
                    10
                )
                .await
            ),
            400
        );
        assert_eq!(
            refused(parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n", 10).await),
            411
        );
        let long = format!(
            "GET / HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES)
        );
        assert_eq!(refused(parse(long.as_bytes(), 10).await), 431);
        let mut many = String::from("GET / HTTP/1.1\r\n");
        for n in 0..=MAX_HEADERS {
            many.push_str(&format!("X-{n}: v\r\n"));
        }
        many.push_str("\r\n");
        assert_eq!(refused(parse(many.as_bytes(), 10).await), 431);
    }

    #[tokio::test]
    async fn a_truncated_request_is_gone_and_a_stalled_one_is_slow() {
        assert!(matches!(
            parse(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nab", 100).await,
            Err(ReadError::Gone)
        ));
        let clock = SystemClock;
        let (client, mut server) = duplex(64);
        let result = read_request(&mut server, &clock, Duration::from_millis(50), 10).await;
        assert!(matches!(result, Err(ReadError::Slow)));
        drop(client);
    }

    #[tokio::test]
    async fn expect_continue_is_answered_before_the_body() {
        let clock = SystemClock;
        let (mut client, mut server) = duplex(1024);
        client
            .write_all(b"POST /w HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\n")
            .await
            .unwrap();
        let reader =
            tokio::spawn(
                async move { read_request(&mut server, &clock, LIMIT, 10).await.unwrap() },
            );
        let mut interim = [0u8; 25];
        client.read_exact(&mut interim).await.unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        client.write_all(b"abc").await.unwrap();
        assert_eq!(reader.await.unwrap().body, b"abc");
    }

    #[tokio::test]
    async fn responses_carry_length_close_and_headers() {
        let clock = SystemClock;
        let (mut client, mut server) = duplex(4096);
        let response = Response::new(201)
            .header("Location", "/whip/abc")
            .body("application/sdp", b"v=0\r\n".to_vec());
        assert!(write_response(&mut server, &clock, LIMIT, &response).await);
        drop(server);
        let mut text = String::new();
        client.read_to_string(&mut text).await.unwrap();
        assert!(text.starts_with("HTTP/1.1 201 Created\r\n"), "{text}");
        assert!(text.contains("Location: /whip/abc\r\n"));
        assert!(text.contains("Content-Type: application/sdp\r\n"));
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\nv=0\r\n"));
    }
}
