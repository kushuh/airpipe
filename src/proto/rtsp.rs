//! RTSP/1.0 message framing.
//!
//! AirPlay's control channel is RTSP-shaped HTTP: a start line, headers and an
//! optional body sized by `Content-Length`. The same framing is used in both
//! directions, and on the event channel the *speaker* sends requests to us.

use thiserror::Error;

/// Upper bound for a single received message; anything bigger is a protocol
/// error. Speakers send small replies (a few KiB at most), so this mainly
/// bounds memory if a peer misbehaves.
const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed start line: {0:?}")]
    StartLine(String),
    #[error("malformed header line: {0:?}")]
    Header(String),
    #[error("message exceeds {MAX_MESSAGE} bytes")]
    TooLarge,
}

/// A parsed RTSP message (request or response).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    pub start_line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Status code if this is a response (`RTSP/1.0 200 OK`).
    pub fn status(&self) -> Option<u16> {
        let mut parts = self.start_line.splitn(3, ' ');
        let proto = parts.next()?;
        if !proto.starts_with("RTSP/") && !proto.starts_with("HTTP/") {
            return None;
        }
        parts.next()?.parse().ok()
    }

    /// Reason phrase of a response, for error messages. It comes from the
    /// network, so control characters are replaced.
    pub fn reason(&self) -> String {
        super::printable(self.start_line.splitn(3, ' ').nth(2).unwrap_or(""))
    }

    /// Tries to parse one complete message from the front of `buf`.
    ///
    /// Returns `Ok(None)` when more data is needed, otherwise the message and
    /// the number of bytes it occupied.
    pub fn parse(buf: &[u8]) -> Result<Option<(Message, usize)>, ParseError> {
        let Some(head_end) = find(buf, b"\r\n\r\n") else {
            if buf.len() > MAX_MESSAGE {
                return Err(ParseError::TooLarge);
            }
            return Ok(None);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]);
        let mut lines = head.split("\r\n");
        let start_line = lines.next().unwrap_or_default().to_string();
        if start_line.split(' ').count() < 2 {
            return Err(ParseError::StartLine(start_line));
        }
        let mut headers = Vec::new();
        for line in lines {
            let (k, v) = line.split_once(':').ok_or_else(|| ParseError::Header(line.to_string()))?;
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        let mut msg = Message { start_line, headers, body: Vec::new() };
        let len: usize = msg.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
        if len > MAX_MESSAGE {
            return Err(ParseError::TooLarge);
        }
        let body_start = head_end + 4;
        if buf.len() < body_start + len {
            return Ok(None);
        }
        msg.body = buf[body_start..body_start + len].to_vec();
        Ok(Some((msg, body_start + len)))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Builder for an outgoing request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: &str, uri: &str) -> Self {
        Self { method: method.into(), uri: uri.into(), headers: Vec::new(), body: Vec::new() }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers.push(("Content-Type".into(), content_type.into()));
        self.body = body;
        self
    }

    pub fn encode(&self, cseq: u32) -> Vec<u8> {
        let mut out = format!("{} {} RTSP/1.0\r\nCSeq: {}\r\n", self.method, self.uri, cseq);
        for (k, v) in &self.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Encodes a minimal `200 OK` response, used to acknowledge event-channel requests.
pub fn ok_response(cseq: Option<&str>) -> Vec<u8> {
    let mut out = String::from("RTSP/1.0 200 OK\r\nServer: airpipe\r\n");
    if let Some(cseq) = cseq {
        out.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    out.push_str("Content-Length: 0\r\n\r\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_encoding() {
        let bytes = Request::new("SET_PARAMETER", "rtsp://10.0.0.2/1")
            .header("User-Agent", "airpipe")
            .body("text/parameters", b"volume: -10.0\r\n".to_vec())
            .encode(7);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("SET_PARAMETER rtsp://10.0.0.2/1 RTSP/1.0\r\nCSeq: 7\r\n"));
        assert!(text.contains("Content-Type: text/parameters\r\n"));
        assert!(text.ends_with("Content-Length: 15\r\n\r\nvolume: -10.0\r\n"));
    }

    #[test]
    fn response_parsing_waits_for_body() {
        let full = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 4\r\n\r\nbodyEXTRA";
        assert!(Message::parse(&full[..20]).unwrap().is_none());
        assert!(Message::parse(&full[..48]).unwrap().is_none());
        let (msg, used) = Message::parse(full).unwrap().unwrap();
        assert_eq!(used, full.len() - 5);
        assert_eq!(msg.status(), Some(200));
        assert_eq!(msg.reason(), "OK");
        assert_eq!(msg.header("cseq"), Some("1"));
        assert_eq!(msg.body, b"body");
    }

    #[test]
    fn request_parsing() {
        let (msg, _) = Message::parse(b"POST /command RTSP/1.0\r\nCSeq: 3\r\n\r\n").unwrap().unwrap();
        assert_eq!(msg.start_line, "POST /command RTSP/1.0");
        assert_eq!(msg.status(), None);
    }

    #[test]
    fn rejects_garbage_headers() {
        assert!(Message::parse(b"RTSP/1.0 200 OK\r\nnot a header\r\n\r\n").is_err());
    }
}
