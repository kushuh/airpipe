#!/bin/sh
# Installer for the airpipe plugin of the Noctalia shell (AirPlay in the top
# bar). Attached to each plugin release (tags noctalia-vX.Y.Z):
#
#   curl -fsSL https://github.com/kushuh/airpipe/releases/download/noctalia-vX.Y.Z/install-noctalia.sh | sh
#
# What it does, as the current user (no root):
#   1. Downloads the plugin from the GitHub release and verifies its SHA-256
#      checksum.
#   2. Installs it in Noctalia's plugin folder,
#      ~/.local/share/noctalia/plugins/airpipe (replacing an older copy).
#   3. Enables it in the running Noctalia.
#
# Options:
#   --version X.Y.Z   install that plugin release (default: this script's)
#   --from-dir DIR    install from local release files (needs SHA256SUMS)
#   --no-enable       do not enable the plugin in the running Noctalia
#   --uninstall       disable and remove the plugin
#
# Environment: AIRPIPE_REPO=owner/name overrides the GitHub repository;
# NOCTALIA_DATA_HOME or XDG_DATA_HOME move the plugin folder like Noctalia does.

set -eu

REPO="${AIRPIPE_REPO:-kushuh/airpipe}"
PLUGIN_ID="kushuh/airpipe"
# Set by the release workflow; a copy from the repository needs --version.
VERSION="@VERSION@"
FROM_DIR=""
ENABLE=1
MODE="install"
# The plugin uses airpipe's JSON interface, added in 0.4.
MIN_AIRPIPE="0.4.0"

say() { printf '%s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() {
    printf '\033[31merror:\033[0m %s\n' "$*" >&2
    exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

download() { # url dest
    if have curl; then
        curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
    elif have wget; then
        wget -q --https-only -O "$2" "$1"
    else
        die "need curl or wget"
    fi
}

# True if version $1 is at least $2 (X.Y.Z).
version_at_least() {
    [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]
}

usage() {
    cat <<'EOF_USAGE'
Installs the airpipe plugin for the Noctalia shell, for the current user.

Options:
  --version X.Y.Z   install that plugin release (default: this script's)
  --from-dir DIR    install from local release files (needs SHA256SUMS)
  --no-enable       do not enable the plugin in the running Noctalia
  --uninstall       disable and remove the plugin
EOF_USAGE
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || die "--version needs a value"
            VERSION="${2#noctalia-}"
            VERSION="${VERSION#v}"
            shift 2
            ;;
        --from-dir)
            [ $# -ge 2 ] || die "--from-dir needs a value"
            FROM_DIR="$2"
            shift 2
            ;;
        --no-enable) ENABLE=0 && shift ;;
        --uninstall) MODE="uninstall" && shift ;;
        -h | --help) usage ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

[ "$(id -u)" != 0 ] || die "run this as your own user, not root: Noctalia plugins are per user"

DATA_HOME="${NOCTALIA_DATA_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/noctalia}"
PLUGINS="$DATA_HOME/plugins"
DEST="$PLUGINS/airpipe"

noctalia_running() { have noctalia && noctalia msg plugins list >/dev/null 2>&1; }

if [ "$MODE" = uninstall ]; then
    if noctalia_running; then
        noctalia msg plugins disable "$PLUGIN_ID" >/dev/null 2>&1 || true
    fi
    if [ -L "$DEST" ]; then
        rm -f "$DEST"
    elif [ -d "$DEST" ]; then
        rm -rf "${DEST:?}"
    else
        say "The plugin is not installed in $PLUGINS."
        exit 0
    fi
    say "Removed the airpipe plugin from $PLUGINS."
    exit 0
fi

if [ -z "$FROM_DIR" ] && [ "$VERSION" = "@VERSION@" ]; then
    die "this copy is not from a release: pass --version X.Y.Z or --from-dir DIR"
fi

# airpipe itself is not required to install the plugin, but it is to use it.
if have airpipe; then
    installed=$(airpipe --version 2>/dev/null | awk '{print $2}')
    if [ -n "$installed" ] && ! version_at_least "$installed" "$MIN_AIRPIPE"; then
        warn "airpipe $installed is installed; the plugin needs $MIN_AIRPIPE or newer"
    fi
else
    warn "airpipe is not installed; the plugin needs airpipe $MIN_AIRPIPE or newer"
fi
have noctalia || warn "noctalia is not on PATH; the plugin is installed, but nothing loads it yet"

mkdir -p "$PLUGINS"
TMP=$(mktemp -d "$PLUGINS/.airpipe-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

if [ -n "$FROM_DIR" ]; then
    [ -f "$FROM_DIR/SHA256SUMS" ] || die "$FROM_DIR/SHA256SUMS not found"
    cp "$FROM_DIR/SHA256SUMS" "$TMP/"
    FILE=$(awk '{print $2}' "$TMP/SHA256SUMS" | sed 's/^\*//' | grep -E '^airpipe-noctalia-.*\.tar\.gz$' | head -n1 || true)
    [ -n "$FILE" ] || die "no airpipe-noctalia-*.tar.gz in $FROM_DIR/SHA256SUMS"
    cp "$FROM_DIR/$FILE" "$TMP/"
else
    FILE="airpipe-noctalia-$VERSION.tar.gz"
    BASE="https://github.com/$REPO/releases/download/noctalia-v$VERSION"
    say "Downloading the airpipe plugin $VERSION..."
    download "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "could not download $BASE/SHA256SUMS"
    download "$BASE/$FILE" "$TMP/$FILE" || die "could not download $BASE/$FILE"
fi
(cd "$TMP" && grep -E "[ *]$FILE\$" SHA256SUMS | sha256sum -c --quiet -) || die "checksum mismatch for $FILE; aborting"
say "Verified $FILE (SHA-256)."

mkdir "$TMP/unpacked"
tar -xzf "$TMP/$FILE" -C "$TMP/unpacked"
[ -f "$TMP/unpacked/airpipe/plugin.toml" ] || die "$FILE does not contain airpipe/plugin.toml"
grep -q "^id = \"$PLUGIN_ID\"" "$TMP/unpacked/airpipe/plugin.toml" || die "$FILE is not the $PLUGIN_ID plugin"

# Replace the old copy (or a development symlink) in one step.
if [ -L "$DEST" ]; then
    rm -f "$DEST"
elif [ -e "$DEST" ]; then
    mv "$DEST" "$TMP/previous"
fi
mv "$TMP/unpacked/airpipe" "$DEST"
say "Installed the airpipe plugin in $DEST."

if [ "$ENABLE" = 1 ] && noctalia_running; then
    # Picks up a changed plugin.toml; scripts reload by themselves.
    noctalia msg config-reload >/dev/null 2>&1 || true
    if noctalia msg plugins enable "$PLUGIN_ID" >/dev/null 2>&1; then
        say "Enabled $PLUGIN_ID in Noctalia."
    else
        warn "could not enable it; run: noctalia msg plugins enable $PLUGIN_ID"
    fi
elif [ "$ENABLE" = 1 ]; then
    say "Noctalia is not running; enable the plugin later with: noctalia msg plugins enable $PLUGIN_ID"
fi

cat <<EOF

Next: add the "AirPlay (airpipe)" widget to your bar in Noctalia's
Settings → Bar (next to the volume widget works well), and, if you like,
the AirPlay tile in Settings → Control Center.
EOF
