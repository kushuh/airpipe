//! The control socket: how `airpipe status`, `use`, `reload`... talk to the
//! running `airpipe run` (usually the systemd user service).
//!
//! * `$XDG_RUNTIME_DIR/airpipe.lock`: held by the running airpipe, so only
//!   one runs at a time (a second one exits with [`ALREADY_RUNNING`]).
//! * `$XDG_RUNTIME_DIR/airpipe.sock`: a Unix socket for the current user only
//!   (the directory is private, the socket is mode 0600, and the peer's user
//!   id is checked). It is local: nothing on the network can reach it.
//!
//! One request per connection, plain text:
//!
//! ```text
//! client:  gain<TAB>Living Room<TAB>-6<LF>
//! airpipe: ok<LF>                  (or "error")
//!          Living Room: gain -6.0 dB …
//!          (connection closed)
//! ```

use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// Exit status of `airpipe run` when another one is already running. The
/// systemd unit lists it in `RestartPreventExitStatus`, so it does not retry.
pub const ALREADY_RUNNING: i32 = 3;

/// Requests are one short line; replies a few lines per speaker.
const MAX_REQUEST: u64 = 4096;
const MAX_REPLY: u64 = 64 * 1024;
/// A client that does not finish its request in time is dropped.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client waits for an answer, and the daemon for a client to
/// take it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// Clients served at once; more are refused (they are the same user's
/// scripts, but a stuck one must not pile up tasks).
const MAX_CLIENTS: usize = 16;

/// How to start airpipe, for messages.
pub const HOW_TO_START: &str = "Start it in the background (and at every login) with:\n\
     \n    systemctl --user enable --now airpipe.service\n\
     \nor run `airpipe run` in a terminal.";

/// A command needs the running airpipe and there is none.
#[derive(Debug, Error)]
#[error("airpipe is not running. {HOW_TO_START}")]
pub struct NotRunning;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Status,
    /// Re-read config.toml (speakers) and devices.toml (volumes) and apply them.
    Reload,
    /// Live per-speaker trim in dB, until restart.
    Gain {
        speaker: String,
        db: f32,
    },
    Stop,
}

/// What airpipe answers: success or failure, plus text for the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub text: String,
}

impl Reply {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { ok: true, text: text.into() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { ok: false, text: text.into() }
    }
}

impl Request {
    fn encode(&self) -> String {
        let fields = match self {
            Request::Status => vec!["status".to_string()],
            Request::Reload => vec!["reload".to_string()],
            Request::Gain { speaker, db } => vec!["gain".into(), speaker.clone(), db.to_string()],
            Request::Stop => vec!["stop".to_string()],
        };
        fields.join("\t") + "\n"
    }

    fn decode(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.trim_end_matches('\n').split('\t').collect();
        match fields.as_slice() {
            ["status"] => Some(Request::Status),
            ["reload"] => Some(Request::Reload),
            ["stop"] => Some(Request::Stop),
            ["gain", speaker, db] => Some(Request::Gain {
                speaker: speaker.to_string(),
                db: db.parse().ok().filter(|v: &f32| v.is_finite())?,
            }),
            _ => None,
        }
    }
}

fn encode_reply(reply: &Reply) -> String {
    format!("{}\n{}", if reply.ok { "ok" } else { "error" }, reply.text)
}

fn decode_reply(text: &str) -> Option<Reply> {
    let (status, rest) = text.split_once('\n').unwrap_or((text, ""));
    let ok = match status {
        "ok" => true,
        "error" => false,
        _ => return None,
    };
    Some(Reply { ok, text: crate::proto::printable(rest).trim_end().to_string() })
}

/// `$XDG_RUNTIME_DIR`: private to the user, emptied at logout. Refused if
/// other users can access it (e.g. pointed at /tmp), since the socket and
/// lock must not be reachable or replaceable by them.
fn runtime_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .context("XDG_RUNTIME_DIR is not set: airpipe runs in a desktop user session")?;
    check_private(&dir)?;
    Ok(dir)
}

fn check_private(dir: &Path) -> Result<()> {
    let mode = fs::metadata(dir).with_context(|| format!("reading {}", dir.display()))?.permissions().mode();
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "{} is accessible to other users (mode {:o}); XDG_RUNTIME_DIR must be private",
            dir.display(),
            mode & 0o777
        );
    }
    Ok(())
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("airpipe.sock"))
}

#[derive(Debug, Error)]
#[error("airpipe is already running (`airpipe status` shows it). Stop it first with `airpipe stop`.")]
pub struct AlreadyRunning;

/// A request from a client, and where to send the answer.
pub type Incoming = (Request, oneshot::Sender<Reply>);

/// The listening side, owned by `airpipe run`.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    /// Our user id (the socket's owner); only clients of this user are served.
    uid: u32,
    /// Held while running; the OS releases it if the process dies.
    _lock: File,
}

impl Server {
    /// Takes the single-instance lock and opens the socket. Fails with
    /// [`AlreadyRunning`] if another airpipe holds the lock.
    pub fn bind() -> Result<Self> {
        Self::bind_in(&runtime_dir()?)
    }

    fn bind_in(dir: &Path) -> Result<Self> {
        let lock_path = dir.join("airpipe.lock");
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(AlreadyRunning.into()),
            Err(fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", lock_path.display()));
            }
        }
        // We hold the lock, so a socket file left here is from an airpipe
        // that died: replace it.
        let path = dir.join("airpipe.sock");
        match fs::remove_file(&path) {
            Ok(()) => debug!("removed a stale {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
        let listener = UnixListener::bind(&path).with_context(|| format!("creating {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let uid = fs::metadata(&path)?.uid();
        Ok(Self { listener, path, uid, _lock: lock })
    }

    /// Serves clients in the background; their requests arrive on the
    /// returned channel, one at a time. When the receiver is dropped, the
    /// task finishes sending replies already given (e.g. to `airpipe stop`),
    /// removes the socket and ends: await the returned handle before exiting.
    pub fn spawn(self) -> (mpsc::Receiver<Incoming>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = self.listener.accept() => match accepted {
                        Ok((stream, _)) if clients.len() < MAX_CLIENTS => {
                            clients.spawn(serve(stream, self.uid, tx.clone()));
                        }
                        Ok(_) => debug!("control socket: too many clients, refusing one"),
                        Err(e) => warn!("control socket: {e}"),
                    },
                    Some(_) = clients.join_next() => {} // a client is done
                    _ = tx.closed() => break, // airpipe is shutting down
                }
            }
            let finish = async { while clients.join_next().await.is_some() {} };
            let _ = tokio::time::timeout(Duration::from_secs(1), finish).await;
        });
        (rx, task)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Handles one client connection.
async fn serve(stream: UnixStream, uid: u32, requests: mpsc::Sender<Incoming>) {
    // The socket is private already; this guards against a permissive umask
    // or a runtime directory shared by mistake.
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == uid => {}
        other => return debug!("control socket: refusing a client of another user ({other:?})"),
    }
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(MAX_REQUEST));
    let reply = match tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(_)) => match Request::decode(&line) {
            Some(request) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                if requests.send((request, reply_tx)).await.is_err() {
                    return; // shutting down
                }
                reply_rx.await.unwrap_or_else(|_| Reply::error("airpipe is shutting down"))
            }
            None => Reply::error(format!("unknown request {:?}", crate::proto::printable_line(&line))),
        },
        _ => return,
    };
    let _ = tokio::time::timeout(REPLY_TIMEOUT, write.write_all(encode_reply(&reply).as_bytes())).await;
}

/// Sends a request to the running airpipe. Fails with [`NotRunning`] if
/// there is none.
pub async fn send(request: &Request) -> Result<Reply> {
    let path = socket_path()?;
    send_to(&path, request).await
}

async fn send_to(path: &Path, request: &Request) -> Result<Reply> {
    let mut stream = match UnixStream::connect(path).await {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
            return Err(NotRunning.into());
        }
        Err(e) => return Err(e).with_context(|| format!("connecting to {}", path.display())),
    };
    let exchange = async {
        stream.write_all(request.encode().as_bytes()).await?;
        let mut text = String::new();
        stream.take(MAX_REPLY).read_to_string(&mut text).await?;
        anyhow::Ok(text)
    };
    let text = tokio::time::timeout(REPLY_TIMEOUT, exchange).await.context("airpipe did not answer in time")??;
    decode_reply(&text).context("airpipe sent an unexpected reply (is it the same version?)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for r in
            [Request::Status, Request::Reload, Request::Stop, Request::Gain { speaker: "Living Room".into(), db: -6.5 }]
        {
            assert_eq!(Request::decode(&r.encode()), Some(r));
        }
        assert_eq!(Request::decode("gain\tKitchen\tloud\n"), None);
        assert_eq!(Request::decode("gain\tKitchen\tNaN\n"), None);
        assert_eq!(Request::decode("rm -rf\n"), None);
    }

    #[test]
    fn replies_round_trip_and_are_printable() {
        let r = Reply::ok("Bureau: streaming\n  second line");
        assert_eq!(decode_reply(&encode_reply(&r)), Some(r));
        assert_eq!(decode_reply("error\nnope"), Some(Reply::error("nope")));
        assert_eq!(decode_reply("ok\n\x1b]0;evil\x07hi").unwrap().text, "\u{FFFD}]0;evil\u{FFFD}hi");
        assert_eq!(decode_reply("hello"), None);
    }

    #[tokio::test]
    async fn one_instance_and_requests_over_the_socket() {
        let dir = std::env::temp_dir().join(format!("airpipe-control-{}", hex::encode(crate::random::bytes::<6>())));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_private(&dir).is_err(), "a directory others can read is refused");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        check_private(&dir).unwrap();
        let sock = dir.join("airpipe.sock");

        let missing = send_to(&sock, &Request::Status).await.unwrap_err();
        assert!(missing.is::<NotRunning>(), "{missing:?}");
        assert!(missing.to_string().contains("systemctl --user enable --now airpipe.service"), "{missing}");

        fs::write(&sock, "").unwrap(); // stale file from a crash
        let server = Server::bind_in(&dir).unwrap();
        assert!(Server::bind_in(&dir).err().unwrap().is::<AlreadyRunning>());
        let mode = fs::metadata(&sock).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let (mut incoming, _task) = server.spawn();
        tokio::spawn(async move {
            while let Some((request, reply)) = incoming.recv().await {
                let _ = reply.send(Reply::ok(format!("got {request:?}")));
            }
        });
        let reply = send_to(&sock, &Request::Gain { speaker: "Kitchen".into(), db: -3.0 }).await.unwrap();
        assert_eq!(reply, Reply::ok("got Gain { speaker: \"Kitchen\", db: -3.0 }"));
        fs::remove_dir_all(dir).unwrap();
    }
}
