//! airpipe: stream Linux system audio from PipeWire to AirPlay 2 speakers.
//!
//! # Module map
//!
//! | Module      | What it does                                            | Pure? |
//! |-------------|---------------------------------------------------------|-------|
//! | [`proto`]   | Wire formats (RTSP, RTP, TLV8, ALAC, DMAP, NTP time) and guards for untrusted input (plist limits, `printable`) | yes   |
//! | [`crypto`]  | SRP, pairing, channel and audio encryption              | yes   |
//! | [`airplay`] | Talking to one speaker: connection, pairing, session    | no    |
//! | [`stream`]  | Real-time audio engine: pacing, packets, retransmits    | no    |
//! | [`sink`]    | The PipeWire output device                              | no    |
//! | [`default_output`] | Hands the default output back when airpipe quits | no    |
//! | [`group`]   | Runs a group of speakers; [`group::volume`] rules       | no    |
//! | [`discovery`] | Finding speakers through Avahi                        | no    |
//! | [`mpris`]   | Now-playing info and remote control via MPRIS           | no    |
//! | [`control`] | The control socket for `airpipe status`, `use`, `stop`… | no    |
//! | [`config`]  | `config.toml` (global settings)                         | yes*  |
//! | [`store`]   | `devices.toml` (device memory, keys)                    | no    |
//! | [`random`]  | The single source of randomness                         | no    |
//!
//! "Pure" modules do no I/O and are unit-tested byte for byte.
//! (*`config` parsing is pure; loading reads the file, `run` creates the
//! commented template on first use, and `use` edits the speaker list.)
//!
//! # Where to start reading
//!
//! 1. `group/mod.rs`: what happens when you run `airpipe run`.
//! 2. `airplay/session.rs`: the conversation with a speaker, step by step.
//! 3. `stream/mod.rs`: how audio becomes timed, encrypted packets.
//!
//! `docs/ARCHITECTURE.md` explains the protocol and threading in prose.

pub mod airplay;
pub mod config;
pub mod control;
pub mod crypto;
pub mod default_output;
pub mod discovery;
pub mod group;
pub mod mpris;
pub mod proto;
pub mod random;
pub mod sink;
pub mod store;
pub mod stream;
