# Security

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: **Security →
Report a vulnerability** on this repository. Do not open a public issue.
You should get an answer within a week.

Only the latest release is supported.

## What airpipe protects

airpipe is a *sender*: it plays your computer's audio on AirPlay speakers.
It is designed so that installing it does not make your computer reachable
in new ways.

| Property | How it is enforced |
|----------|--------------------|
| Nothing can stream **to** your computer | airpipe never accepts audio or video and never advertises itself over mDNS. |
| Minimal listening surface | Two UDP ports (6111 timing, 6112 control). Packets from hosts that are not speakers airpipe is currently using are dropped without a reply or parsing, so the ports cannot be used for reflection. Per 8 ms tick at most 16 control packets are read and 32 packets retransmitted in total, and each speaker has a retransmit budget of 250 packets/s, so a flood of spoofed requests can neither stall audio nor be amplified much. The installer opens them to your local network only. |
| No internet access | airpipe only talks to speakers on the LAN and to local services (PipeWire, Avahi, D-Bus). |
| Encrypted control and audio | After pairing, the control channel, event channel and audio are encrypted with ChaCha20-Poly1305 using keys derived from the pairing secret. Audio nonces come from a packet counter, with a disjoint range for the marker-bit copies, so a nonce is never reused under one key. |
| Bounded input handling | Every RTSP message from a speaker is size-limited (1 MiB) and parsed by small tested parsers; malformed framing closes the connection. Plists (parsed by the third-party `plist` crate) are first walked iteratively and rejected beyond 64 KiB, 32 levels of nesting, 20 000 values or 1 MiB of text after expanding shared references, so crafted input can neither overflow the stack nor exhaust memory. Text shown in the terminal is stripped of control characters. Connection attempts and requests time out. |
| Local control for you only | `airpipe status`, `use`, `stop`... talk to the running airpipe through a Unix socket in `$XDG_RUNTIME_DIR` (private to your user), with mode 0600 and a check of the connecting process's user id. It is not reachable from the network. Requests are one short line, with a size limit and a timeout. |
| Hearing safety | Each start, and each time speakers are added, plays at the speakers' `default_volume` (with the default `start = "device-default"`) (25% for known high-output brands). Volume never exceeds each speaker's `max_volume`; the last function before the network clamps to the speaker's maximum. |
| Private keys stay private | Our pairing key is in `~/.config/airpipe/devices.toml`, written atomically with mode 0600. Secrets are never logged. |
| Supply chain | Dependencies are audited by `cargo-deny` (advisories, licenses, sources) on every change. CI actions are pinned to commit hashes and updated through reviewed Dependabot PRs. Builds use `--locked`. |
| Release integrity | Each release has `SHA256SUMS` (checked by the installer) and a signed build provenance attestation (see below). |

## Known limitations

These come from the AirPlay protocol or from trade-offs, and are documented
so you can judge them:

* **Transient pairing does not authenticate the speaker.** Most speakers
  use "transient" pairing (no PIN), where both sides know a fixed public
  password. It encrypts the traffic, but a device impersonating your
  speaker's network address could receive the audio. Speakers that use PIN
  or password pairing (`airpipe pair`) are authenticated with long-term
  keys on every connection.
* **Now-playing artwork is read from local files.** Media players announce
  artwork as a `file://` path over the session D-Bus. airpipe reads it only
  if it is a regular file (opened non-blocking, so FIFOs and devices cannot
  hang it), a JPEG or PNG, and under 1 MiB, and sends it only to speakers
  you stream to. A malicious local program could make airpipe send a local
  image to your speaker; it cannot make it send other kinds of files.
  Disable with `[metadata] enabled = false`.
* **Release packages are not GPG-signed.** The installer verifies SHA-256
  checksums from the same release. For a cryptographic link to this
  repository's CI, verify the attestation:

  ```sh
  gh attestation verify airpipe-0.1.0-1.x86_64.rpm --repo kushuh/airpipe
  ```
