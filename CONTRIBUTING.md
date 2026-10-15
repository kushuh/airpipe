# Contributing to airpipe

Thanks for helping! This guide is for people and AI agents alike. It is
written to be read in pieces: every section stands on its own.

* [Ways to contribute](#ways-to-contribute)
* [Ground rules](#ground-rules)
* [Development setup](#development-setup)
* [Where things are](#where-things-are)
* [Testing](#testing)
* [Continuous integration](#continuous-integration)
* [Packaging and releases](#packaging-and-releases)
* [Pull request checklist](#pull-request-checklist)

## Ways to contribute

* **Report a speaker.** Run `airpipe info "<speaker>"` and open an issue
  saying whether it works, with that output. This is the most valuable
  contribution: AirPlay is undocumented, so every device tested matters.
* **Report a bug.** Include your distribution, `airpipe --version`, and a
  log made with `RUST_LOG=airpipe=debug`. Run with `RUST_LOG=airpipe=trace`
  to include every message exchanged with the speaker.
* **Send code.** Read the ground rules below first.

## Ground rules

These are hard requirements; reviews check them.

1. **Sender only.** airpipe must never accept audio or video from the
   network, and never advertise itself over mDNS. Do not add network
   listeners beyond the timing and control UDP ports, and keep both
   answering only speakers airpipe is using (`stream::timing::AllowedPeers`,
   engine targets). The only other listener is the local control socket
   (`src/control.rs`), for the current user only. See
   [SECURITY.md](SECURITY.md).
2. **Volume safety.** No code path may send a speaker more than its
   `max_volume`, and with `start = "device-default"` a fresh start or added
   speakers play at the speakers' `default_volume`. Every volume is computed by
   `src/group/volume.rs`, and
   `session::volume_body` clamps to 0 dB as a last resort. Changes to either
   need tests.
3. **No surprise network access.** airpipe talks only to speakers on the
   LAN. It does not fetch artwork or anything else from the internet.
4. **Few, standard dependencies.** Prefer the standard library. A new crate
   must be widely used and actively maintained, and the pull request must
   say why std or an existing dependency is not enough. `cargo deny check`
   must pass.
5. **Pure where possible.** Wire formats (`src/proto/`) and cryptography
   (`src/crypto/`) do no I/O and have byte-level unit tests. Keep it that
   way: new formats go there, with tests.
6. **`unsafe` only at the PipeWire boundary** (`src/sink.rs`), with a
   `// SAFETY:` comment.
7. **Do not copy GPL code.** OwnTone and shairport-sync are GPL. Reading them
   as protocol documentation is fine; copying code is not. airpipe is
   MIT OR Apache-2.0.
8. **Cite protocol facts.** AirPlay knowledge comes from reverse
   engineering. When code relies on a non-obvious fact, say where it comes
   from in a comment (OwnTone, shairport-sync, PipeWire, or "observed on
   <device>").
9. **Pin CI actions.** Third-party GitHub Actions are referenced by full
   commit hash with the version in a comment; Dependabot keeps them current.
   Never replace a hash with a tag.
10. **Readable over clever.** Code is read by people with little time and
   little context. Prefer small functions, plain names, and a doc comment on
   every module saying what it does and why.

## Development setup

### Packages

| Need                     | openSUSE                     | Debian/Ubuntu                          | Fedora                   |
|--------------------------|------------------------------|----------------------------------------|--------------------------|
| Rust ≥ 1.89              | `rust cargo`                 | `rustup` (distro Rust is often older)  | `rust cargo`             |
| C toolchain + libclang   | `gcc clang clang-devel`      | `build-essential clang libclang-dev`   | `gcc clang clang-devel`  |
| PipeWire headers         | `pipewire-devel`             | `libpipewire-0.3-dev`                  | `pipewire-devel`         |
| pkg-config               | `pkgconf`                    | `pkg-config`                           | `pkgconf`                |
| mDNS tools (optional)    | `avahi-utils`                | `avahi-utils`                          | `avahi-tools`            |

openSUSE in one line:

```sh
sudo zypper in rust cargo clang clang-devel gcc pipewire-devel avahi-utils
```

`libclang` is needed because the `pipewire` crate generates its bindings at
build time.

### Commands

```sh
cargo build                                   # debug build
cargo run -- list                             # run without installing
cargo test                                    # all unit tests (no network or audio needed)
cargo clippy --all-targets -- -D warnings     # lints; CI fails on any warning
cargo fmt                                     # formatting (see rustfmt.toml)
cargo deny check                              # dependency audit (cargo install cargo-deny)
RUSTDOCFLAGS=-D warnings cargo doc --no-deps  # documentation builds without warnings
```

### Runtime

To run airpipe you also need PipeWire running, Avahi running, and the
firewall open as described in the README's
[Firewall](README.md#firewall) section.

## Where things are

`src/lib.rs` has a module map and a suggested reading order.
`docs/ARCHITECTURE.md` explains the protocol, threads and data flow.

| If you want to change...                  | Look at                                          |
|-------------------------------------------|--------------------------------------------------|
| How a speaker is set up, step by step     | `src/airplay/session.rs`                         |
| Pairing (PIN, password, transient)        | `src/airplay/pair.rs`, `src/crypto/pairing.rs`   |
| Speaker buttons / remote control          | `src/airplay/events.rs`, `src/mpris.rs`          |
| Volume behaviour                          | `src/group/volume.rs`                            |
| When speakers connect and disconnect      | `src/group/mod.rs`                               |
| Timing, latency, packet sending           | `src/stream/mod.rs`, `src/stream/jitter.rs`      |
| The PipeWire output device                | `src/sink.rs`                                    |
| Speaker discovery                         | `src/discovery.rs`                               |
| Settings files                            | `src/config.rs`, `src/store.rs`                  |
| Command-line interface                    | `src/main.rs`                                    |
| `airpipe status`/`use`/`stop`, one instance | `src/control.rs`, `Group::on_request`          |
| Giving the default output back on stop    | `src/default_output.rs`                          |
| Installer, packages, systemd, firewall    | `packaging/`                                     |

## Testing

### Unit tests

`cargo test` needs no network, speakers or PipeWire. Every wire format has a
byte-level test, and crypto is tested against simulated peers and
independently computed vectors. New code in `src/proto/`, `src/crypto/`,
`src/group/volume.rs`, `src/config.rs` or `src/store.rs` needs tests.

### On real speakers

> **Volume first.** 100% is a speaker's full power, and some speakers are
> very loud. Test with a low cap and quiet sounds: `airpipe tone` plays a
> soft tone at 5% by default; for `run`, pass `--max-volume 7` (about
> -28 dB).

```sh
RUST_LOG=airpipe=debug cargo run -- list
RUST_LOG=airpipe=debug cargo run -- info "<speaker>"
RUST_LOG=airpipe=debug cargo run -- tone "<speaker>"     # protocol only, no PipeWire

cargo run -- stop                       # if the installed airpipe is running
cargo run -- use "<speaker>"
RUST_LOG=airpipe=debug cargo run -- run --max-volume 7
# in another terminal:
cargo run -- status
pactl set-sink-volume airpipe 50%
pw-play --target airpipe some-quiet-file.wav
```

`RUST_LOG=airpipe=trace` logs every RTSP request and response (decrypted),
which is the main tool for protocol work.

To test without touching your own settings, use a throwaway configuration:
`export XDG_CONFIG_HOME=$(mktemp -d)` in both terminals.

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `main` and every pull
request:

| Job                 | Checks                                                                      |
|---------------------|-----------------------------------------------------------------------------|
| rustfmt             | `cargo fmt --check`                                                         |
| clippy              | lints with warnings as errors, and rustdoc without warnings                 |
| test                | unit and doc tests                                                          |
| MSRV                | builds with Rust 1.89 (`rust-version` in Cargo.toml)                        |
| cargo-deny          | security advisories, licenses (`deny.toml`), crate sources                  |
| package             | release build, `.deb` and `.rpm` contents and dependencies                  |
| installer (lint)    | `shellcheck` and `dash -n` on the shell scripts                             |
| installer (distros) | real install and uninstall on Ubuntu (apt), Fedora (dnf), openSUSE (zypper) |

Dependabot (`.github/dependabot.yml`) opens weekly PRs for action pins and
Rust dependencies. Releases additionally get a signed build provenance
attestation (`actions/attest-build-provenance`).

If a dependency raises the minimum Rust version, update `rust-version` in
`Cargo.toml`, the MSRV job, and the table above. To find the highest
requirement among dependencies:

```sh
cargo metadata --format-version 1 --filter-platform x86_64-unknown-linux-gnu \
  | python3 -c 'import json,sys; m=json.load(sys.stdin); print(max((p.get("rust_version") or "0", p["name"]) for p in m["packages"]))'
```

## Packaging and releases

### Files

| File                                  | Purpose / installed to                          |
|---------------------------------------|-------------------------------------------------|
| `packaging/install.sh`                | Attached to each release (one-line installer)   |
| `packaging/build-dist.sh`             | Builds release artifacts (used by CI)           |
| `packaging/firewalld/airpipe.xml`     | `/usr/lib/firewalld/services/`                  |
| `packaging/systemd/airpipe.service`   | `/usr/lib/systemd/user/`                        |

Package contents are declared in `Cargo.toml` under
`[package.metadata.deb]` and `[package.metadata.generate-rpm]`; keep both
lists in sync. The default UDP ports appear in `src/config.rs`,
`packaging/firewalld/airpipe.xml` and the README; keep them in sync too.

### Building packages locally

```sh
cargo install cargo-deb cargo-generate-rpm
cargo build --release
packaging/build-dist.sh dist          # tarball, .deb, .rpm and SHA256SUMS in dist/
sh packaging/install.sh --from-dir dist --dry-run
```

### Releasing

1. Bump `version` in `Cargo.toml`, run `cargo build` to update
   `Cargo.lock`, and commit.
2. `git tag v0.2.0 && git push origin v0.2.0`.
3. `.github/workflows/release.yml` checks that the tag matches the version,
   runs the tests, builds on native x86_64 and ARM64 runners, and publishes
   the release with packages, the installer and `SHA256SUMS`.

Binaries are built on Ubuntu 24.04, so they need glibc ≥ 2.39 and
libpipewire ≥ 1.0 at runtime.

## Pull request checklist

* [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and
      `cargo test` pass.
* [ ] New wire formats, crypto, volume or settings logic have unit tests.
* [ ] No new dependency, or the PR explains why one is needed.
* [ ] User-visible changes are reflected in `README.md`; settings changes
      also in the `config.toml` template (`src/config.rs`).
* [ ] Tested on real speakers if the protocol changed, at low volume. Say
      which device.
