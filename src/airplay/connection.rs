//! RTSP client connection to a speaker, optionally encrypted after pairing.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::trace;

use crate::crypto::channel::ChannelCipher;
use crate::proto::rtsp::{Message, Request};

/// The speaker closed the connection. Usually another device took it over,
/// which is normal and not an error worth retrying.
#[derive(Debug, thiserror::Error)]
#[error("speaker closed the connection")]
pub struct ClosedBySpeaker;

fn is_closed_by_peer(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset};
    matches!(e.kind(), ConnectionReset | ConnectionAborted | BrokenPipe)
}

/// User agent of a recent macOS Music app. Some receivers gate features on it.
pub const USER_AGENT: &str = "AirPlay/409.16";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Connection {
    stream: TcpStream,
    cipher: Option<ChannelCipher>,
    /// Raw bytes read from the socket and not yet decrypted.
    raw: Vec<u8>,
    /// Decrypted bytes not yet parsed into a message.
    plain: Vec<u8>,
    cseq: u32,
    /// Headers added to every request (DACP-ID, Active-Remote, ...).
    common_headers: Vec<(String, String)>,
}

impl Connection {
    pub async fn connect(addr: SocketAddr, common_headers: Vec<(String, String)>) -> Result<Self> {
        let stream = tokio::time::timeout(REQUEST_TIMEOUT, TcpStream::connect(addr))
            .await
            .context("timed out connecting")?
            .with_context(|| format!("could not connect to {addr}"))?;
        stream.set_nodelay(true)?;
        Ok(Self { stream, cipher: None, raw: Vec::new(), plain: Vec::new(), cseq: 0, common_headers })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream.local_addr()?)
    }

    /// Switches the connection to encrypted framing. Must be called right
    /// after pairing completes, before any other request.
    pub fn enable_encryption(&mut self, cipher: ChannelCipher) {
        self.cipher = Some(cipher);
    }

    /// Sends a request and waits for its response. Non-2xx responses are
    /// returned, not treated as errors; use [`Connection::request_ok`] for that.
    pub async fn request(&mut self, req: Request) -> Result<Message> {
        self.cseq += 1;
        let mut req = req;
        req.headers.splice(0..0, self.common_headers.iter().cloned());
        let bytes = req.encode(self.cseq);
        trace!(target: "airpipe::rtsp", ">>> {}", String::from_utf8_lossy(&bytes[..bytes.len() - req.body.len()]));
        let wire = match &mut self.cipher {
            Some(c) => c.encrypt(&bytes),
            None => bytes,
        };
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            match self.stream.write_all(&wire).await {
                Err(e) if is_closed_by_peer(&e) => return Err(ClosedBySpeaker.into()),
                other => other?,
            }
            self.read_message().await
        })
        .await
        .with_context(|| format!("timed out waiting for response to {} {}", req.method, req.uri))?
    }

    /// Like [`Connection::request`] but fails on non-2xx status.
    pub async fn request_ok(&mut self, req: Request) -> Result<Message> {
        let what = format!("{} {}", req.method, req.uri);
        let resp = self.request(req).await?;
        match resp.status() {
            Some(200..=299) => Ok(resp),
            Some(code) => bail!("{what} failed: {code} {}", resp.reason()),
            None => bail!("{what}: malformed response"),
        }
    }

    async fn read_message(&mut self) -> Result<Message> {
        loop {
            if let Some((msg, used)) = Message::parse(&self.plain)? {
                self.plain.drain(..used);
                trace!(target: "airpipe::rtsp", "<<< {} {:?} ({} byte body)", msg.start_line, msg.headers, msg.body.len());
                return Ok(msg);
            }
            let mut buf = [0u8; 8192];
            let n = match self.stream.read(&mut buf).await {
                Ok(0) => return Err(ClosedBySpeaker.into()),
                Ok(n) => n,
                // Some receivers reset the connection instead of closing it.
                Err(e) if is_closed_by_peer(&e) => return Err(ClosedBySpeaker.into()),
                Err(e) => return Err(e.into()),
            };
            match &mut self.cipher {
                Some(c) => {
                    self.raw.extend_from_slice(&buf[..n]);
                    let plain = c.decrypt(&mut self.raw)?;
                    self.plain.extend_from_slice(&plain);
                }
                None => self.plain.extend_from_slice(&buf[..n]),
            }
        }
    }
}
