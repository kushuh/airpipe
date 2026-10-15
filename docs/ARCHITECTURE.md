# Architecture

This document explains how airpipe turns desktop audio into AirPlay 2
streams: the module layout, the threads, and the protocol as implemented.
It is written for reviewers and contributors.

## Overview

```
 apps ──▶ PipeWire graph ──▶ [sink.rs] Audio/Sink stream
                                   │ S16LE 44.1 kHz stereo
                                   ▼
                          lock-free ring (rtrb)
                                   │
                                   ▼
             [stream/] engine thread: jitter buffer → ALAC frame →
             RTP header → ChaCha20-Poly1305 → UDP to every speaker
                 ▲                 │                   ▲
     sync packets│                 │ audio             │ retransmit requests
                 │                 ▼                   │ (UDP 6112)
            ┌──────────── speaker (AirPlay 2 receiver) ─────────────┐
            │  RTSP control (TCP 7000, encrypted) ◀── [airplay/]    │
            │  event channel (TCP, encrypted)   ──▶ remote control  │
            │  timing requests (UDP → 6111)     ──▶ [stream/timing] │
            └────────────────────────────────────────────────────────┘

 [group/] orchestrates: connects speakers when audio flows, releases them
 when idle, maps volume, forwards metadata from [mpris.rs].
```

## Module map

| Module                | Responsibility                                                         | I/O? |
|-----------------------|------------------------------------------------------------------------|------|
| `proto/tlv8.rs`       | TLV8 encoding of pairing messages                                     | no   |
| `proto/alac.rs`       | Uncompressed ALAC frames                                               | no   |
| `proto/rtp.rs`        | Audio, sync, retransmit and timing packet layouts                      | no   |
| `proto/ntp.rs`        | Monotonic sender clock in NTP format                                   | no   |
| `proto/rtsp.rs`       | RTSP framing (both directions)                                         | no   |
| `proto/dmap.rs`       | DMAP encoding of track metadata                                        | no   |
| `proto/plist.rs`      | Vetted parsing of plists from the network (size, depth, expansion)     | no   |
| `proto/mod.rs`        | `printable`: strips terminal control characters from network text      | no   |
| `crypto/srp.rs`       | SRP-6a client (3072-bit, SHA-512, HomeKit variant)                     | no   |
| `crypto/pairing.rs`   | pair-setup (transient, PIN/password) and pair-verify state machines   | no   |
| `crypto/channel.rs`   | HKDF key derivation and encrypted framing for TCP channels            | no   |
| `crypto/audio.rs`     | Audio packet encryption                                                | no   |
| `airplay/connection.rs` | RTSP client over TCP, switching to encryption after pairing        | yes  |
| `airplay/session.rs`  | One speaker: setup sequence, then an actor serving commands            | yes  |
| `airplay/pair.rs`     | Sends the pairing messages (`/pair-setup`, `/pair-verify`)             | yes  |
| `airplay/events.rs`   | Event channel from the speaker: remote-control commands                | yes  |
| `stream/jitter.rs`    | Cushion and clock-drift correction between PipeWire and the engine     | no   |
| `stream/mod.rs`       | Real-time packet engine and retransmission                             | yes  |
| `stream/timing.rs`    | NTP timing responder                                                   | yes  |
| `sink.rs`             | PipeWire output device, volume and latency reporting                   | yes  |
| `mpris.rs`            | Now-playing metadata and remote control via MPRIS                      | yes  |
| `discovery.rs`        | Finding speakers through Avahi (system D-Bus)                          | yes  |
| `group/mod.rs`        | Group lifecycle, retries, metadata fan-out                             | yes  |
| `group/volume.rs`     | Volume rules: slider → AirPlay dB, ceilings, start level               | no   |
| `random.rs`           | The single source of randomness (keys, nonces, IDs), from the OS        | yes  |
| `config.rs`           | Global settings (`config.toml`), defaults, validation                  | no*  |
| `store.rs`            | Device memory (`devices.toml`): identity, per-speaker settings, keys   | yes  |
| `control.rs`          | Control socket (`status`, `reload`, `gain`, `stop`), single instance   | yes  |
| `default_output.rs`   | Gives the previous default output back when airpipe stops              | yes  |
| `main.rs`             | CLI: standalone commands, and the client side of the control socket    | yes  |

Everything marked "no I/O" is pure and has byte-level unit tests.
(*`config.rs` parsing is pure; loading reads the file, `run` creates the
commented template on first use, and `use` edits the speaker list.)

## Background service and control socket

`airpipe run` is the long-running process, normally started by the systemd
user unit `airpipe.service`. The other commands either work alone (`list`,
`info`, `pair`, `unpair`, `tone`, `config`) or talk to it through
`$XDG_RUNTIME_DIR/airpipe.sock` (`control.rs`): one tab-separated request
line, answered by `ok`/`error` and text. `use`, `add` and `remove` write
`output.speakers` in `config.toml`, then send `reload`; so the file is the
single source of truth for the speakers, and works the same whether airpipe
is running or not.

`run` first takes an exclusive lock on `$XDG_RUNTIME_DIR/airpipe.lock`. A
second airpipe exits with status 3, which the unit lists in
`RestartPreventExitStatus`, so two never run at once (they would fight over
the output and the UDP ports). Holding the lock also makes it safe to remove
a socket file left by a crash.

On `reload`, the group scans the network in a background task, then
`Group::on_resolved` releases speakers no longer listed, adds new ones,
re-reads `devices.toml`, and renames the output.

When airpipe stops (`airpipe stop`, SIGTERM, Ctrl-C), `default_output.rs`
checks PipeWire's `default` metadata: if our output is the configured
default, it sets the previous one again (or clears the key so WirePlumber
picks the best output), waiting for PipeWire to confirm before the output
disappears.

## Threads

| Thread            | What runs there                                    | Why separate |
|-------------------|----------------------------------------------------|--------------|
| `airpipe-pipewire`| PipeWire main loop; `process` runs on PipeWire's RT data thread | PipeWire objects are not `Send` |
| `airpipe-audio`   | Packet engine, paced by absolute deadlines         | Audio timing must not depend on async scheduling |
| `airpipe-timing`  | Blocking UDP responder                             | Low-latency replies improve clock estimates |
| tokio (1 thread)  | RTSP sessions, event channels, discovery, MPRIS, control socket, group controller | Light concurrent I/O, none of it timing-critical; one thread is plenty |

Data crosses threads in these ways only:

* Samples go through a single-producer single-consumer `rtrb` ring. The
  PipeWire RT callback never locks and never allocates.
* The list of speakers to send to (`Engine::add_target` / `remove_target`) is
  behind a mutex the engine takes a few times per packet, each time for
  microseconds; the group takes it only to add or remove a speaker.
* The engine publishes two counters as atomics: the next RTP timestamp (for
  metadata) and the number of audible packets (for idle detection).
* The timing thread reads the allow-list of speakers (`AllowedPeers`, a
  mutex-protected set) before answering.
* Control events go through tokio channels; a session's volume goes through
  a `watch` channel, so only the latest value is kept and it is never lost.

## Protocol

### Discovery

Speakers advertise `_airplay._tcp`. Their TXT records carry `deviceid` (our
stable key for a speaker), `features` (a 64-bit capability mask split into
two hex words) and `flags` (status flags: PIN or password required).
airpipe asks the Avahi daemon over D-Bus: its warm cache answers instantly,
whereas a fresh mDNS client must wait for speakers to reply. airpipe only
browses. It never registers a service.

### Session setup (`airplay/session.rs`)

```
GET  /info                       status flags
POST /pair-setup  M1..M4         transient pairing (X-Apple-HKP: 4)
  or /pair-verify M1..M4         with stored keys (X-Apple-HKP: 3)
-- control channel switches to encryption --
SETUP rtsp://<us>/<id>           {timingProtocol: NTP, timingPort, groupUUID, ...}
                                 ← {eventPort}
connect event channel            encrypted with the Events keys
RECORD
SETUP rtsp://<us>/<id>           {streams: [{type: 96, ct: 2 (ALAC), shk, controlPort, ...}]}
                                 ← {dataPort, controlPort}
SET_PARAMETER volume: -20.000000
```

After setup:

* `POST /feedback` every 2 s keeps the session alive.
* `SET_PARAMETER` carries volume, DMAP text (`application/x-dmap-tagged`),
  artwork (`image/jpeg|png`) and `progress: start/current/end` as RTP
  timestamps.
* `TEARDOWN` with an empty plist ends the session. A session is owned by
  its `SessionHandle`: dropping the handle makes the session actor send
  `TEARDOWN` and stop, so no code path can leave a session running.
* While connecting or streaming, a speaker holds a `PeerPermit` that lets
  it use our timing port; the permit is revoked when it is dropped.

### Pairing (`crypto/pairing.rs`, `crypto/srp.rs`)

* **Transient:** SRP-6a with user `Pair-Setup` and password `3939`. The
  64-byte SRP session key `K` is the shared secret.
* **PIN/password:** the same SRP exchange with the real secret, then M5/M6
  exchange Ed25519 long-term keys, each signed over
  `HKDF(K, "Pair-Setup-*-Sign-Salt", ...) | identifier | public key` and
  encrypted with ChaCha20-Poly1305 under nonce labels `PS-Msg05`/`PS-Msg06`.
* **Verify:** X25519 ephemeral exchange. Each side signs
  `own ephemeral | identifier | peer ephemeral` with its long-term key, using
  nonces `PV-Msg02`/`PV-Msg03`. The X25519 shared secret becomes the shared
  secret.

HomeKit SRP quirks: `k = H(N | PAD(g))` and `u = H(PAD(A) | PAD(B))` use
padded values, while `M1` hashes unpadded ones.

### Encrypted channels (`crypto/channel.rs`)

Frames are `u16 LE length | ciphertext | 16-byte tag`, with at most 1024
plaintext bytes per frame. The AAD is the length field. The nonce is
`[0;4] || u64 LE counter`, with one counter per direction. Keys are
HKDF-SHA512 of the shared secret:

| Channel | Salt           | Our write key info              | Our read key info               |
|---------|----------------|---------------------------------|---------------------------------|
| Control | `Control-Salt` | `Control-Write-Encryption-Key`  | `Control-Read-Encryption-Key`   |
| Events  | `Events-Salt`  | `Events-Read-Encryption-Key`    | `Events-Write-Encryption-Key`   |

The event channel's labels are swapped because the speaker opens it as a
reverse connection.

### Audio (`stream/`)

* **Format:** ALAC 44.1 kHz/16-bit/stereo, 352 frames per packet, using
  ALAC's uncompressed escape (1416 bytes). No encoder library is needed and
  CPU use is negligible.
* **Packet:** a 12-byte RTP header (payload type 0x60, marker bit on a
  speaker's first packet), then the ChaCha20-Poly1305 payload, the tag and an
  8-byte nonce tail. The AAD is header bytes 4..12 (timestamp and SSRC).
  The key (`shk`) is ours and is shared by the whole group, so each packet is
  encrypted once and sent to every speaker.
* **Timeline:** packet `k` has `rtptime = rtp0 + 352k` and is due at
  `t0 + 352k / 44100` (computed in 128-bit arithmetic). Sync packets
  (`0x90 d4` first, `0x80 d4` about every second) state "rtptime
  `next − latency` is playing when packet `next` is due", using the packet's
  *scheduled* time, so scheduling jitter never moves the timeline. Speakers
  learn our clock from timing requests, so all speakers in a group play each
  sample at the same instant.
* **Retransmits:** about 8 s of encrypted packets are kept. `80 d5` requests
  from current speakers are answered on the data socket, within a budget of
  32 packets per tick overall and a token bucket of 250 packets/s (burst 128)
  per speaker; requests for packets outside the history are ignored.
* **Idle:** with no speaker connected the engine skips encoding and
  encryption and wakes only every 12 packets (~96 ms). Only packets with
  non-zero samples count as audio, so an app holding the output open with
  digital silence does not keep speakers busy.

### Clock drift (`stream/jitter.rs`)

PipeWire produces audio at its graph clock's rate, and we consume it at our
monotonic clock's rate. A cushion (2048, 3072 or 4096 frames depending on
the latency profile) absorbs burstiness. When the
smoothed fill drifts outside ±50% of the cushion, one frame per packet is
dropped or repeated, which is inaudible and corrects up to about 125
frames/s, far more than real clock drift. If the fill exceeds 4× the
cushion, the buffer skips ahead so latency stays bounded.

### Volume (`sink.rs`, `group/volume.rs`)

* On Linux the system volume of the selected output lives in PipeWire, and
  every desktop, mixer and volume key acts on it. airpipe's output is a
  PipeWire node, so it receives volume changes through its `Props` param,
  with no desktop-specific code.
* The sink forces PipeWire's software volume to 1.0, so samples are never
  scaled, and reports the slider position (`cbrt(linear)`) and mute state.
  With `follow_system = false` it leaves software volume alone, and the
  slider attenuates digitally like on any device.
* The slider covers every speaker's full range, linearly in dB: 0% is
  -30 dB (AirPlay's quietest step), 100% is 0 dB (full power). Then the
  live `airpipe gain` trim applies, and the result is clamped to the
  speaker's `max_volume` and the optional `--max-volume` cap
  (`group::volume::VolumeInputs`).
* At start-up, and when speakers are added, the slider is moved to the
  speakers' `default_volume` (the lowest across the group). WirePlumber may
  restore a remembered volume onto the new node right after start-up, so
  airpipe re-asserts its level for the first 3 seconds.

### Latency reporting

The sink announces `ProcessLatency = latency + jitter cushion` to PipeWire.
Players that do A/V sync (mpv, browsers, GStreamer) read it through the
graph's latency reporting and delay video accordingly.

## Persistent state

Everything lives in `$XDG_CONFIG_HOME/airpipe/` (usually `~/.config/airpipe/`):

* `config.toml` holds global settings. It is written once from
  `config::TEMPLATE` and then belongs to the user; airpipe only changes
  `output.speakers` (with `toml_edit`, keeping comments). Unknown keys are
  errors, so typos are caught.
* `devices.toml` (mode 0600) holds the device memory. `[controller]` has our
  Ed25519 key, pairing identifier and device ID. `[devices."<deviceid>"]`
  has one entry per discovered AirPlay target, with its settings and, after
  `airpipe pair`, a `pairing` table with the speaker's long-term public key.
  It is edited in place with `toml_edit`, so user comments and edits survive
  airpipe's updates. Transient pairing stores no keys.

## Testing strategy

* **Unit tests** cover every wire format with byte-level expectations, plus
  SRP (against a reference server in the test), pair-verify (against a
  simulated accessory, including signature rejection), channel encryption
  (including split reads and tampering), audio encryption (decrypted the way a
  receiver does), the jitter buffer (start-up, underrun, drift both ways,
  runaway source), volume mapping and storage. The HKDF test vector comes
  from an independent implementation (Python's `hmac`), not our own code.
* **Hardware tests** use `airpipe tone` (protocol only) and `airpipe run`
  plus `pw-play` (full path), always with a low volume cap. See CONTRIBUTING.md.
