//! The smallest HTTP/1.1 surface the desktop shell needs: a bounded request parser and a response
//! writer. No async runtime, no framework, no extra dependency.
//!
//! The limits below are part of the security posture, not tuning knobs. A browser talking to
//! `127.0.0.1` cannot legitimately exceed them, and every one of them turns into a specific status
//! instead of an unbounded read:
//!
//! - request line 8 KiB -> `414`
//! - header block 32 KiB -> `431`
//! - body 1 MiB -> `413`
//! - a read that stalls for 10 s is treated as a dead client and the connection is dropped
//!
//! Not implemented on purpose: TLS (loopback only), HTTP/2, chunked transfer encoding, and request
//! pipelining. Anything unparseable is answered with a status and a closed connection rather than
//! guessed at, because a lenient parser in front of `POST /api/prompt` (which spawns the agent) is
//! exactly the kind of surface that produces request-smuggling bugs.

use std::io::{BufRead, ErrorKind, Write};
use std::time::Duration;

/// A request line longer than this is rejected with `414 URI Too Long`.
pub const MAX_REQUEST_LINE: usize = 8 * 1024;
/// The whole header block longer than this is rejected with `431`.
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
/// A body larger than this is rejected with `413` before a single body byte is read.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Header lines are capped as well, so a flood of tiny headers cannot cost more than the byte cap.
pub const MAX_HEADERS: usize = 128;
/// How long a single read may stall before the connection is considered dead.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Applied to every response: the shell never wants the browser to re-sniff a type or leak a URL.
pub const SECURITY_HEADERS: [(&str, &str); 2] = [("x-content-type-options", "nosniff"), ("referrer-policy", "no-referrer")];

/// The document policy. The UI ships its own script and stylesheet, no inline code, no CDN, and it
/// only talks back to this server, so `'self'` everywhere and `'none'` as the default.
pub const DOCUMENT_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'";

/// One parsed request. Only what the shell routes on is kept.
pub struct Request {
    pub method: String,
    pub target: String,
    pub version: String,
    pub body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Request {
    /// Header lookup, case-insensitive. Names are stored lowercased by the parser.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// The path portion of the request target (no query string).
    pub fn path(&self) -> &str {
        match self.target.find('?') {
            Some(index) => &self.target[..index],
            None => &self.target,
        }
    }

    /// The raw query string, empty when the target has none.
    pub fn query(&self) -> &str {
        match self.target.find('?') {
            Some(index) => &self.target[index + 1..],
            None => "",
        }
    }

    /// Whether the client is willing to reuse this connection for another request.
    pub fn wants_keep_alive(&self) -> bool {
        let close = self.header("connection").map(|value| value.split(',').any(|token| token.trim().eq_ignore_ascii_case("close"))).unwrap_or(false);
        if close {
            return false;
        }
        let keep = self.header("connection").map(|value| value.split(',').any(|token| token.trim().eq_ignore_ascii_case("keep-alive"))).unwrap_or(false);
        keep || self.version == "HTTP/1.1"
    }
}

/// The result of trying to read one request off a connection.
pub enum Incoming {
    /// The peer closed the connection before sending anything (or went idle).
    Empty,
    Request(Box<Request>),
    /// The request broke a limit or was unparseable: answer with this status, then close.
    Rejected { status: u16, message: &'static str },
}

enum LineError {
    Eof,
    TooLong,
    Io(std::io::Error),
}

/// Reads one CRLF-terminated line, refusing to buffer more than `limit` bytes for it.
fn read_line_limited<R: BufRead>(reader: &mut R, limit: usize, out: &mut Vec<u8>) -> Result<(), LineError> {
    out.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(LineError::Io(error)),
        };
        if available.is_empty() {
            return Err(LineError::Eof);
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                if out.len() + index + 1 > limit {
                    return Err(LineError::TooLong);
                }
                out.extend_from_slice(&available[..=index]);
                reader.consume(index + 1);
                return Ok(());
            }
            None => {
                let length = available.len();
                if out.len() + length > limit {
                    return Err(LineError::TooLong);
                }
                out.extend_from_slice(available);
                reader.consume(length);
            }
        }
    }
}

/// Reads one request. Any body is consumed fully, so a kept-alive connection stays in sync.
pub fn read_request<R: BufRead>(reader: &mut R) -> std::io::Result<Incoming> {
    let mut line: Vec<u8> = Vec::new();
    let mut head: Option<Vec<u8>> = None;
    // A client may send stray CRLFs between requests; skip a couple, then give up.
    for _ in 0..3 {
        match read_line_limited(reader, MAX_REQUEST_LINE, &mut line) {
            Ok(()) => {}
            Err(LineError::Eof) => return Ok(Incoming::Empty),
            Err(LineError::TooLong) => return Ok(Incoming::Rejected { status: 414, message: "request line too long" }),
            Err(LineError::Io(error)) => return Err(error),
        }
        if !line.iter().all(|byte| matches!(byte, b'\r' | b'\n')) {
            head = Some(line.clone());
            break;
        }
    }
    let Some(head) = head else { return Ok(Incoming::Empty) };

    let (method, target, version) = {
        let text = String::from_utf8_lossy(&head).to_string();
        let mut parts = text.split_whitespace();
        let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
            return Ok(Incoming::Rejected { status: 400, message: "malformed request line" });
        };
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || !method.bytes().all(|byte| byte.is_ascii_alphabetic()) || !target.starts_with('/') {
            return Ok(Incoming::Rejected { status: 400, message: "malformed request line" });
        }
        (method.to_string(), target.to_string(), version.to_string())
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        match read_line_limited(reader, MAX_HEADER_BYTES, &mut line) {
            Ok(()) => {}
            Err(LineError::Eof) => return Ok(Incoming::Rejected { status: 400, message: "truncated request headers" }),
            Err(LineError::TooLong) => return Ok(Incoming::Rejected { status: 431, message: "request headers too large" }),
            Err(LineError::Io(error)) => return Err(error),
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES || headers.len() >= MAX_HEADERS {
            return Ok(Incoming::Rejected { status: 431, message: "request headers too large" });
        }
        let parsed = {
            let text = String::from_utf8_lossy(&line).to_string();
            let trimmed = text.trim_end_matches(['\r', '\n']).to_string();
            if trimmed.is_empty() {
                None
            } else {
                match trimmed.split_once(':') {
                    Some((name, value)) => Some((name.trim().to_ascii_lowercase(), value.trim().to_string())),
                    None => return Ok(Incoming::Rejected { status: 400, message: "malformed header line" }),
                }
            }
        };
        match parsed {
            Some(header) => headers.push(header),
            None => break,
        }
    }

    let mut content_length: Option<usize> = None;
    for (name, value) in &headers {
        match name.as_str() {
            "content-length" => {
                if content_length.is_some() {
                    return Ok(Incoming::Rejected { status: 400, message: "duplicate content-length" });
                }
                match value.parse::<usize>() {
                    Ok(length) => content_length = Some(length),
                    Err(_) => return Ok(Incoming::Rejected { status: 400, message: "invalid content-length" }),
                }
            }
            // Refusing the framing we do not implement is safer than guessing where a body ends.
            "transfer-encoding" => return Ok(Incoming::Rejected { status: 400, message: "transfer-encoding is not supported" }),
            _ => {}
        }
    }
    let length = content_length.unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Ok(Incoming::Rejected { status: 413, message: "request body too large" });
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Incoming::Request(Box::new(Request { method, target, version, body, headers })))
}

/// The reason phrase for every status the shell can emit.
pub fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

/// Writes a status line and headers. `content_length` is omitted for bodyless statuses (`304`).
///
/// Every header value here comes from a constant or from a hex digest; no request data ever reaches
/// this function, so header injection has no path in.
pub fn write_head<W: Write>(out: &mut W, status: u16, content_type: Option<&str>, extra: &[(&str, &str)], content_length: Option<usize>, keep_alive: bool) -> std::io::Result<()> {
    let mut head = String::with_capacity(320);
    head.push_str("HTTP/1.1 ");
    head.push_str(&status.to_string());
    head.push(' ');
    head.push_str(status_reason(status));
    head.push_str("\r\n");
    if let Some(content_type) = content_type {
        head.push_str("content-type: ");
        head.push_str(content_type);
        head.push_str("\r\n");
    }
    for (name, value) in SECURITY_HEADERS {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(length) = content_length {
        head.push_str("content-length: ");
        head.push_str(&length.to_string());
        head.push_str("\r\n");
    }
    head.push_str(if keep_alive { "connection: keep-alive\r\n" } else { "connection: close\r\n" });
    head.push_str("\r\n");
    out.write_all(head.as_bytes())
}

/// Writes a complete response (head, then body) and flushes it to the socket.
pub fn write_response<W: Write>(out: &mut W, status: u16, content_type: &str, body: &[u8], extra: &[(&str, &str)], keep_alive: bool) -> std::io::Result<()> {
    write_head(out, status, Some(content_type), extra, Some(body.len()), keep_alive)?;
    if !body.is_empty() {
        out.write_all(body)?;
    }
    out.flush()
}

/// Writes a small plain-text response, used for rejections and 404s.
pub fn write_text<W: Write>(out: &mut W, status: u16, body: &str, keep_alive: bool) -> std::io::Result<()> {
    write_response(out, status, "text/plain; charset=utf-8", body.as_bytes(), &[("cache-control", "no-store")], keep_alive)
}
