# airpipe

[![CI](https://github.com/kushuh/airpipe/actions/workflows/ci.yml/badge.svg)](https://github.com/kushuh/airpipe/actions/workflows/ci.yml)

Play your Linux computer's sound on AirPlay 2 speakers.

airpipe adds an output called **"AirPlay: <your speaker>"** to your sound
settings. Pick it like any other output, and everything you play (music,
videos, games, calls) comes out of your AirPlay speakers. It works on any
desktop (KDE, GNOME, XFCE, tiling window managers...) and runs from the
terminal or in the background.

## What it does

* **Any app, any desktop:** it is a normal sound output; there is nothing
  to set up in your apps.
* **Volume just works:** your volume keys and slider control the speakers.
  The slider covers each speaker's whole range, and each speaker starts at
  a safe level (lower for powerful speakers); an optional per-speaker limit
  caps it.
* **Several speakers in sync:** play to multiple AirPlay speakers at once,
  perfectly in time (multi-room).
* **Now playing:** speakers and their apps show the title, artist and
  artwork of what is playing.
* **Speaker buttons work:** play, pause, next and previous on the speaker
  (or its app) control your music player.
* **Video stays in sync:** video players automatically delay the picture to
  match the speakers.
* **Plays nicely with others:** airpipe only uses a speaker while sound is
  playing, so your phone or other computers can use it the rest of the time.
* **One-way only:** airpipe only *sends* sound. It never accepts audio or
  video from the network, and never appears as an AirPlay receiver.

## Requirements

* Linux with **PipeWire** 1.0 or newer: the default on current Fedora,
  openSUSE, Ubuntu, Debian, Arch and most other distributions. The
  ready-made packages need Ubuntu 24.04+, Debian 13+, Fedora 40+ or openSUSE
  Tumbleweed (or equivalent); on older systems, build from source.
* **Avahi** (`avahi-daemon`), which is installed and running on nearly every
  desktop Linux.
* **AirPlay 2** speakers on the same network. Older AirPlay-1-only devices
  are not supported (PipeWire already has built-in support for those).
* A firewall that lets your speakers reach your computer on three UDP
  ports; the installer can set this up for you.

## Install

```sh
curl -fsSL https://github.com/kushuh/airpipe/releases/latest/download/install.sh | sh
```

The installer:

1. Downloads the right package for your system (openSUSE, Fedora, Debian,
   Ubuntu, or a generic package for others; x86_64 or ARM64).
2. Checks that the download is intact (SHA-256).
3. Installs it with your package manager. It asks for your password once.
4. Offers to open the firewall to your **local network only**.
5. Offers to start airpipe automatically for a speaker of your choice.

It asks before every optional step. You can
[read it first](packaging/install.sh) if you like.

**Uninstall:**

```sh
curl -fsSL https://github.com/kushuh/airpipe/releases/latest/download/install.sh | sh -s -- --uninstall
```

Your settings in `~/.config/airpipe/` are kept; delete that folder to remove
them too.

**Manual install:** every [release](../../releases) has `.rpm` and `.deb`
packages and a `.tar.gz` for x86_64 and ARM64, plus a `SHA256SUMS` file. To
build from source, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Quick start

airpipe runs in the background, and you control it with short commands.
The installer offers to set this up. To do it by hand:

```sh
airpipe list                                  # show the AirPlay speakers on your network
airpipe use Kitchen                           # choose the speaker to play to
systemctl --user enable --now airpipe.service # run airpipe now and at every login
```

Now choose **AirPlay: Kitchen** as the output in your sound settings and
play something. Sound starts on the speaker after about a second.

Several speakers in sync:

```sh
airpipe use Kitchen "Living Room"
```

Everyday commands:

| Command                       | What it does                                              |
|-------------------------------|-----------------------------------------------------------|
| `airpipe status`              | Show speakers, their volume, and any error                |
| `airpipe use <speaker>...`    | Play to these speakers (remembered in `config.toml`)      |
| `airpipe add <speaker>`       | Add a speaker                                             |
| `airpipe remove <speaker>`    | Stop using a speaker                                      |
| `airpipe gain <speaker> <dB>` | Make one speaker of a group quieter for now, e.g. `-6`    |
| `airpipe reload`              | Apply speaker changes in `config.toml`, edits to `devices.toml` |
| `airpipe stop`                | Stop airpipe; sound goes back to your previous output     |

If airpipe is not running, these commands tell you how to start it. To
run it in a terminal instead of in the background, use `airpipe run`
(Ctrl-C stops it). Only one airpipe runs at a time.

### Speakers that ask for a PIN or password

Most speakers need nothing. If `airpipe list` says a speaker needs pairing:

```sh
airpipe pair Kitchen                      # the speaker shows a PIN; type it in
airpipe pair Kitchen --password <secret>  # speakers protected by a password
```

This only needs to be done once.

## Configuration

Settings live in two files in `~/.config/airpipe/`, created on first run.
`airpipe config` shows their location and the current values.

### `config.toml`: general settings

Created with comments explaining every option. airpipe only ever changes
the speaker list in it (`airpipe use`, `add`, `remove`), keeping everything
else and your comments. The most useful settings:

```toml
[output]
speakers = ["Kitchen"]         # set by `airpipe use`

[audio]
latency = "balanced"           # "low-latency", "balanced" or "robust"
idle_timeout_secs = 30         # free the speakers after this much silence

[volume]
follow_system = true           # volume keys and slider control the speakers
start = "device-default"       # start at each speaker's default volume
                               # ("last" keeps the volume from last time)
```

Speaker changes apply with `airpipe reload`; other settings when airpipe
restarts (`systemctl --user restart airpipe`).

**Latency:** sound always arrives in full quality. Latency is only the delay
before you hear it, and how much Wi-Fi trouble it can absorb:

| Setting       | Delay  | Best for                               |
|---------------|--------|----------------------------------------|
| `low-latency` | ~0.5 s | Wired network or excellent Wi-Fi       |
| `balanced`    | ~1 s   | Most homes (default)                   |
| `robust`      | ~2 s   | Busy or weak Wi-Fi, if you hear cuts   |

Video players compensate for the delay automatically.

For less delay, set an exact value, from 1 to 10000 ms, e.g.
`latency_ms = 200`. AirPlay always buffers on the speaker. This buffer
absorbs Wi-Fi hiccups and keeps several speakers in sync, so near-instant
sound like a wired speaker is not possible. Go down step by step: below
about 250 ms, sound cuts out unless your network is excellent, and some
speakers keep their own minimum delay whatever you set.

### `devices.toml`: one entry per speaker

airpipe adds each speaker the first time it plays to it. You can edit the
values; airpipe keeps your changes and comments. Each entry is one AirPlay
speaker as it appears on the network: a stereo pair that shows up as one
speaker is one entry.

```toml
[devices."50:5B:C2:9C:79:E5"]
name = "Kitchen"
model = "Devialet Phantom 103 dB"
default_volume = 25         # % volume set when airpipe starts
max_volume = 100            # % never exceeded (100 = no limit, 0 = silent)
latency_offset_ms = 0       # delay this speaker to line it up with others
```

After editing, run `airpipe reload`.

## Volume

Your system volume controls the speakers, on every desktop, with no setup.
The volume slider covers each speaker's whole range: **40% is 40% of what
the speaker can do**, and 100% is its full power, which is dangerously loud
on some speakers. So:

* **When airpipe starts, the volume is set to the speaker's
  `default_volume`** (and again when speakers are added), so a
  fresh start is never too loud. Speakers known to be very powerful
  (Devialet, Bang & Olufsen, Klipsch, KEF, McIntosh) start at 25%, others
  (TVs, soundbars, small speakers) at 50%. Change it per speaker in
  `devices.toml`.
* **`max_volume`** in `devices.toml` is a hard limit for one speaker: airpipe
  never plays it louder, whatever the slider says. It is 100% (no limit)
  unless you lower it.
* For a one-off quiet session: `airpipe stop`, then
  `airpipe run --max-volume 10`.

## Troubleshooting

**`airpipe list` finds no speakers**

* Check that the speaker works from a phone on the same Wi-Fi.
* Check that Avahi is running: `systemctl status avahi-daemon`.
* Your firewall must allow mDNS (UDP 5353) from your network. Run the
  installer again and accept the firewall step, or see
  [Firewall](#firewall).
* Some Wi-Fi routers isolate devices from each other ("client isolation" or
  "AP isolation"); turn it off for your home network.

**The speaker is found but stays silent**

* Make sure **AirPlay: \<speaker\>** is the selected output and its volume
  is not at zero or muted.
* Your firewall must allow UDP 6111 and 6112 from your network (see
  [Firewall](#firewall)).
* Run `airpipe status` to see errors.

**Sound cuts out**

Set `latency = "robust"` in `config.toml`. Wi-Fi interference is the usual
cause.

**The beginning of short sounds is cut**

The first ~0.5–1 s after a silence is lost while the speaker wakes up.
Increase `idle_timeout_secs` to keep the speaker connected longer.

**"requires pairing first"**

Run `airpipe pair <speaker>` once (see
[above](#speakers-that-ask-for-a-pin-or-password)).

**Where are the logs?**

For the background service: `journalctl --user -u airpipe`. `airpipe run`
in a terminal prints to the terminal. For more detail, run with
`RUST_LOG=airpipe=debug`.

**"airpipe is already running"**

Only one airpipe runs at a time. `airpipe stop` stops the running one
(background or terminal).

**Upgrading from 0.1**

Run the installer again: it moves `airpipe@<speaker>.service` setups to
`airpipe.service` with the same speakers. Volumes in `devices.toml` change
from dB (`max_volume_db`, `default_volume_db`) to percentages
(`default_volume`, `max_volume`); airpipe replaces the old keys with the
new defaults the first time it starts. **100% volume now means the
speaker's full power:** the old -20 dB ceiling of powerful speakers is
gone, and they start at 25% instead. To keep a hard limit, set
`max_volume` (e.g. `max_volume = 33` is the old -20 dB).

### Firewall

Speakers must be able to reach your computer on:

| Port      | Why                                  |
|-----------|--------------------------------------|
| UDP 5353  | Finding speakers (mDNS)              |
| UDP 6111  | Keeping speakers in time             |
| UDP 6112  | Re-sending audio lost on Wi-Fi       |

Allow them **from your local network only**. With firewalld (openSUSE,
Fedora), replacing `192.168.1.0/24` with your network:

```sh
for s in mdns airpipe; do
  sudo firewall-cmd --permanent --zone=public \
    --add-rich-rule="rule family=ipv4 source address=192.168.1.0/24 service name=$s accept"
done
sudo firewall-cmd --reload
```

With ufw (Ubuntu, Debian):

```sh
sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp
sudo ufw allow from 192.168.1.0/24 to any port 6111:6112 proto udp
```

None of these ports accept audio or video: airpipe cannot be streamed to.

## Compatibility

Tested with:

| Speaker                                         | Result |
|-------------------------------------------------|--------|
| Devialet Phantom 103 dB (stereo pair)           | Works  |

Most AirPlay 2 speakers (Sonos, HomePod, Bose, Bang & Olufsen, Denon,
Marantz, Yamaha, LG and Samsung TVs...) use the same protocol and are
expected to work. If yours does or doesn't, please
[open an issue](../../issues) with the output of `airpipe info <speaker>`.

Not supported: video and screen mirroring, and speakers that only support
the original AirPlay (version 1).

## Privacy and security

* airpipe talks only to speakers on your local network. It makes no
  internet connections.
* It only browses for speakers and never advertises itself, so nothing on
  the network can send audio or video to your computer through it.
* The ports it uses are opened to your local network only, and only carry
  timing and retransmission requests.
* Pairing keys are stored in `~/.config/airpipe/devices.toml`, readable only
  by you.
* Releases come with checksums and a signed build attestation.

See [SECURITY.md](SECURITY.md) for the full security model and how to report
a vulnerability.

## Contributing

Bug reports, speaker test reports and code are welcome. See
[CONTRIBUTING.md](CONTRIBUTING.md) to get started.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT), at your option.

AirPlay is a trademark of Apple Inc. airpipe is an independent project, not
affiliated with or endorsed by Apple. It was written from scratch, using
[OwnTone](https://github.com/owntone/owntone-server),
[shairport-sync](https://github.com/mikebrady/shairport-sync), PipeWire's
RAOP module and [Emanuele Cozzi's notes](https://emanuelecozzi.net/docs/airplay2)
as protocol references.
