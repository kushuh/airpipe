# AirPlay (airpipe) for Noctalia

A [Noctalia](https://noctalia.dev) shell plugin for
[airpipe](https://github.com/kushuh/airpipe): turn airpipe on and off, set the
volume, choose your AirPlay speakers and change every airpipe setting, from
the top bar. It is an add-on: airpipe works the same without it, and the
plugin only uses airpipe's command line (`--json` output and `config`
commands), `systemctl --user` and `pactl`.

## What you get

- **Bar widget:** the AirPlay icon, highlighted while a speaker plays. Click
  to open the panel; scroll to change the volume (with Noctalia's volume
  OSD). Optional label with the speakers and volume.
- **Panel:** on/off switch, volume and mute, the speakers in use with their
  state, per-speaker settings (start volume, maximum volume, sync offset),
  a network scan to add speakers, and every setting of `config.toml`
  (buffering, keep connected, start at login...).
- **Control Center tile:** AirPlay on/off; right-click opens the panel.

It follows your Noctalia theme: it uses the shell's own controls and colour
roles.

Noctalia's built-in audio panel has no extension points, so the plugin
cannot live inside it; put the widget next to the volume widget instead. The
AirPlay output itself still shows in the audio panel like any other output.

## Requirements

- airpipe 0.4 or newer (for `--json`), running as `airpipe.service`.
- Noctalia 5.4 or newer (plugin API 43).

## Install

The plugin has its own releases in the airpipe repository (tags
`noctalia-vX.Y.Z`); it is not in Noctalia's plugin catalogs. As your own
user (no root), run the installer of the release you want, for example:

```sh
curl -fsSL https://github.com/kushuh/airpipe/releases/download/noctalia-v0.1.0/install-noctalia.sh | sh
```

It downloads the plugin, checks its SHA-256 checksum, installs it in
`~/.local/share/noctalia/plugins/airpipe` and enables it. Then add the
**AirPlay (airpipe)** widget to your bar (Settings → Bar, next to the volume
widget), and optionally the AirPlay tile in Settings → Control Center.

Run a newer release's installer to update. To remove the plugin:

```sh
curl -fsSL https://github.com/kushuh/airpipe/releases/download/noctalia-v0.1.0/install-noctalia.sh | sh -s -- --uninstall
```

If airpipe is not on the shell's `PATH` (for example a build in
`~/.cargo/bin`), set its path in the plugin's settings.

### From a checkout

```sh
mkdir -p ~/.local/share/noctalia/plugins
ln -s "$PWD/noctalia/airpipe" ~/.local/share/noctalia/plugins/airpipe
noctalia msg plugins enable kushuh/airpipe
```

The installer replaces this link when you later install a release.

## Develop

```sh
noctalia plugins lint noctalia/airpipe        # Noctalia's own checks
python3 noctalia/check.py                     # manifest and strings (also in CI)
noctalia msg panel-toggle kushuh/airpipe:panel
noctalia msg plugin kushuh/airpipe:service all refresh
```

`.luau` edits reload live; after editing `plugin.toml`, run
`noctalia msg config-reload`. Every string shown is looked up in
`translations/en.json`; keys built at runtime are listed in `check.py`.

To release: bump `version` in `plugin.toml` and the version in the install
commands above, merge, then push a tag `noctalia-vX.Y.Z`
(`.github/workflows/noctalia-release.yml` publishes it).
