//! The event channel: a reverse connection on which the speaker sends us
//! requests, mostly remote-control commands from its buttons or app.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::session::{SessionEvent, SessionId};
use crate::crypto::channel::ChannelCipher;
use crate::proto::rtsp::{self, Message};

/// Playback commands a speaker can send back (its buttons or companion app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCommand {
    Play,
    Pause,
    Next,
    Previous,
}

/// Reads requests the speaker sends on the event channel, acknowledges them,
/// and forwards remote control commands.
pub(super) async fn run(
    mut stream: TcpStream,
    mut cipher: ChannelCipher,
    session: SessionId,
    events: mpsc::Sender<SessionEvent>,
) {
    let mut raw = Vec::new();
    let mut plain = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        raw.extend_from_slice(&buf[..n]);
        match cipher.decrypt(&mut raw) {
            Ok(p) => plain.extend_from_slice(&p),
            Err(e) => {
                warn!("event channel: {e}");
                return;
            }
        }
        loop {
            let (msg, used) = match Message::parse(&plain) {
                Ok(Some(m)) => m,
                Ok(None) => break, // wait for more data
                Err(e) => {
                    warn!("event channel: {e}; closing it");
                    return;
                }
            };
            plain.drain(..used);
            let reply = cipher.encrypt(&rtsp::ok_response(msg.header("CSeq")));
            if stream.write_all(&reply).await.is_err() {
                return;
            }
            match parse_remote_command(&msg) {
                Some(command) => {
                    let _ = events.send(SessionEvent::Remote { session, command }).await;
                }
                None => debug!("event from speaker: {} ({} byte body)", msg.start_line, msg.body.len()),
            }
        }
    }
}

/// Extracts a `sendMediaRemoteCommand` from an event-channel request.
fn parse_remote_command(msg: &Message) -> Option<RemoteCommand> {
    let value = crate::proto::plist::parse(&msg.body).ok()?;
    let dict = value.as_dictionary()?;
    if dict.get("type")?.as_string()? != "sendMediaRemoteCommand" {
        return None;
    }
    match dict.get("value")?.as_string()? {
        "play" => Some(RemoteCommand::Play),
        "paus" | "pause" => Some(RemoteCommand::Pause),
        "nitm" => Some(RemoteCommand::Next),
        "pitm" => Some(RemoteCommand::Previous),
        other => {
            debug!("unhandled remote command {other:?}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::session::plist_bytes;
    use plist::Dictionary;

    #[test]
    fn remote_commands_are_parsed() {
        let mut d = Dictionary::new();
        d.insert("type".into(), "sendMediaRemoteCommand".into());
        d.insert("value".into(), "nitm".into());
        let msg = Message { body: plist_bytes(d), ..Default::default() };
        assert_eq!(parse_remote_command(&msg), Some(RemoteCommand::Next));

        let mut d = Dictionary::new();
        d.insert("type".into(), "updateInfo".into());
        d.insert("value".into(), "x".into());
        let msg = Message { body: plist_bytes(d), ..Default::default() };
        assert_eq!(parse_remote_command(&msg), None);
    }
}
