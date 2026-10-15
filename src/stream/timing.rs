//! NTP-style timing responder (UDP, `network.timing_port` in config.toml,
//! default 6111).
//!
//! Each speaker periodically sends a timing request; the reply lets it
//! estimate the offset between its clock and ours, which is what keeps every
//! speaker in a group playing the same sample at the same time.
//!
//! Only speakers airpipe is currently connecting or streaming to get a reply
//! ([`AllowedPeers`]). Everything else is dropped unanswered, so the port
//! cannot be used to reflect traffic at other hosts. Access is granted as a
//! [`PeerPermit`] that revokes itself when dropped, so no code path can
//! forget to revoke it.

use std::collections::HashMap;
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::proto::ntp::Clock;
use crate::proto::rtp;

/// Addresses allowed to use the timing service, with a count of the permits
/// held for each. Cheap to clone.
#[derive(Clone, Default)]
pub struct AllowedPeers(Arc<Mutex<HashMap<IpAddr, usize>>>);

/// Permission for one address to use the timing service, revoked on drop.
pub struct PeerPermit {
    peers: AllowedPeers,
    ip: IpAddr,
}

impl AllowedPeers {
    pub fn allow(&self, ip: IpAddr) -> PeerPermit {
        *self.lock().entry(ip).or_insert(0) += 1;
        PeerPermit { peers: self.clone(), ip }
    }

    fn contains(&self, ip: &IpAddr) -> bool {
        self.lock().contains_key(ip)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, usize>> {
        // Each update is a single counter change; ignore poisoning.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for PeerPermit {
    fn drop(&mut self) {
        let mut peers = self.peers.lock();
        if let Some(n) = peers.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                peers.remove(&self.ip);
            }
        }
    }
}

pub struct TimingServer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TimingServer {
    pub fn start(port: u16, clock: Clock, peers: AllowedPeers) -> Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .with_context(|| format!("binding timing port {port}/udp (is another airpipe already running?)"))?;
        // Wake up regularly to notice `stop`.
        socket.set_read_timeout(Some(Duration::from_millis(250)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let thread = std::thread::Builder::new().name("airpipe-timing".into()).spawn(move || {
            let mut buf = [0u8; 64];
            while !stop_flag.load(Ordering::Relaxed) {
                let (n, peer) = match socket.recv_from(&mut buf) {
                    Ok(r) => r,
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                        continue;
                    }
                    Err(e) => {
                        warn!("timing socket error: {e}");
                        continue;
                    }
                };
                let received = clock.now();
                if !peers.contains(&peer.ip()) {
                    continue;
                }
                match rtp::timing_reply(&buf[..n], received, clock.now()) {
                    Some(reply) => {
                        if let Err(e) = socket.send_to(&reply, peer) {
                            warn!("could not answer timing request from {peer}: {e}");
                        }
                    }
                    None => debug!("ignoring {n}-byte non-timing packet from {peer}"),
                }
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for TimingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowed_peers_get_replies() {
        let peers = AllowedPeers::default();
        let server_port = {
            // Find a free port by binding and releasing it.
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.local_addr().unwrap().port()
        };
        let _server = TimingServer::start(server_port, Clock::new(), peers.clone()).unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xd2;
        let mut reply = [0u8; 64];

        client.send_to(&request, ("127.0.0.1", server_port)).unwrap();
        assert!(client.recv_from(&mut reply).is_err(), "unknown peer must be ignored");

        let first = peers.allow("127.0.0.1".parse().unwrap());
        let second = peers.allow("127.0.0.1".parse().unwrap());
        client.send_to(&request, ("127.0.0.1", server_port)).unwrap();
        let (n, _) = client.recv_from(&mut reply).expect("allowed peer gets a reply");
        assert_eq!(n, 32);
        assert_eq!(&reply[..2], &[0x80, 0xd3]);

        drop(first);
        client.send_to(&request, ("127.0.0.1", server_port)).unwrap();
        assert!(client.recv_from(&mut reply).is_ok(), "still allowed while a permit remains");

        drop(second);
        client.send_to(&request, ("127.0.0.1", server_port)).unwrap();
        assert!(client.recv_from(&mut reply).is_err(), "revoked once the last permit is dropped");
    }
}
