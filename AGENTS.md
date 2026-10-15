# Notes for AI coding agents

Read `CONTRIBUTING.md` first: its "Ground rules" are mandatory. This file
only adds what an agent needs most often.

## Commands

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS=-D warnings cargo doc --no-deps
```

All four must pass before you finish. Unit tests need no network or audio.

## Orientation

* Module map and reading order: the doc comment at the top of `src/lib.rs`.
* Protocol and threading: `docs/ARCHITECTURE.md`.
* "Where to change X": the table in `CONTRIBUTING.md`.

## Rules that are easy to break

* **Never** add code that listens for or accepts audio/video, or that
  advertises airpipe over mDNS. airpipe is a sender only.
* **Never** compute a speaker volume outside `src/group/volume.rs`, and
  never remove the clamp in `session::volume_body`.
* **Never** play audio on real speakers above 7% (about -28 dB) when
  testing. Use `airpipe tone` or `airpipe run --max-volume 7`.
* Do not add dependencies without a stated reason (see CONTRIBUTING).
* Do not copy code from OwnTone or shairport-sync (GPL).
* Keep GitHub Actions pinned to commit hashes; do not loosen workflow
  permissions.
* Never allocate, lock for long, or block in the audio thread
  (`stream/mod.rs`) or the PipeWire `process` callback (`sink.rs`).
* airpipe only writes `output.speakers` in `config.toml` (`airpipe use`);
  both files are only edited through `toml_edit` so user comments survive.
* The control socket (`src/control.rs`) is for the current user only: keep
  it in `$XDG_RUNTIME_DIR`, mode 0600, with the peer uid check.
