#!/bin/sh
# airpipe installer.
#
#   curl -fsSL https://github.com/kushuh/airpipe/releases/latest/download/install.sh | sh
#
# What it does, asking before anything optional:
#   1. Downloads the package for this machine (rpm, deb, or tarball) from the
#      GitHub release and verifies its SHA-256 checksum.
#   2. Installs it with the system package manager (zypper, dnf, apt).
#   3. Optionally opens the firewall to the local network only (firewalld/ufw).
#   4. Optionally starts an AirPlay output for a speaker at every login.
#
# Options:
#   --version X.Y.Z   install a specific release instead of the latest
#   --from-dir DIR    install from local release files (needs SHA256SUMS)
#   --no-firewall     skip the firewall step
#   --no-service      skip the auto-start step
#   --uninstall       remove airpipe (keeps ~/.config/airpipe)
#   --dry-run         print privileged commands instead of running them
#   -y, --yes         answer yes to optional steps (firewall) without a terminal
#
# Environment: AIRPIPE_REPO=owner/name overrides the GitHub repository.

set -eu

REPO="${AIRPIPE_REPO:-kushuh/airpipe}"
VERSION="latest"
FROM_DIR=""
DO_FIREWALL=1
DO_SERVICE=1
MODE="install"
DRY_RUN=0
ASSUME_YES=0

# --------------------------------------------------------------------------
# helpers

say() { printf '%s\n' "$*"; }
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() {
    printf '\033[31merror:\033[0m %s\n' "$*" >&2
    exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

# Runs a command, or prints it with --dry-run.
run() {
    if [ "$DRY_RUN" = 1 ]; then
        printf '  + %s\n' "$*" >&2
    else
        "$@"
    fi
}

# Runs a command as root.
as_root() {
    if [ "$(id -u)" = 0 ]; then
        run "$@"
    else
        run sudo "$@"
    fi
}

# Runs a read-only command as root and prints its output. Never call
# firewall-cmd without root: unprivileged calls go through polkit, which
# shows a password dialog for every single call. In --dry-run nothing
# privileged runs, so queries fail and callers fall back to defaults.
query_root() {
    [ "$DRY_RUN" = 1 ] && return 1
    if [ "$(id -u)" = 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

# True if we can ask the user questions (even when piped into sh).
interactive() { [ -t 1 ] && { : </dev/tty; } 2>/dev/null; }

# ask "question" default(y|n) -> returns 0 for yes.
# Without a terminal, optional steps are skipped unless --yes was given:
# a script must not change the firewall unattended by surprise.
ask() {
    if ! interactive; then
        if [ "$ASSUME_YES" = 1 ]; then
            return 0
        fi
        say "Skipped (no terminal to ask: $1). Re-run interactively or pass --yes."
        return 1
    fi
    if [ "$2" = y ]; then suffix="[Y/n]"; else suffix="[y/N]"; fi
    printf '%s %s ' "$1" "$suffix" >/dev/tty
    read -r answer </dev/tty || answer=""
    case "${answer:-$2}" in
        y | Y | yes | Yes) return 0 ;;
        *) return 1 ;;
    esac
}

download() { # url dest
    if have curl; then
        curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"
    elif have wget; then
        wget -q --https-only -O "$2" "$1"
    else
        die "need curl or wget"
    fi
}

usage() {
    cat <<'EOF_USAGE'
airpipe installer

Usage: install.sh [options]

  --version X.Y.Z   install a specific release instead of the latest
  --from-dir DIR    install from local release files (needs SHA256SUMS)
  --no-firewall     skip the firewall step
  --no-service      skip the auto-start step
  --uninstall       remove airpipe (keeps ~/.config/airpipe)
  --dry-run         print privileged commands instead of running them
  -y, --yes         answer yes to optional steps (firewall) without a terminal

Environment: AIRPIPE_REPO=owner/name overrides the GitHub repository.
EOF_USAGE
}

# --------------------------------------------------------------------------
# arguments

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:?--version needs a value}"; shift ;;
        --from-dir) FROM_DIR="${2:?--from-dir needs a directory}"; shift ;;
        --no-firewall) DO_FIREWALL=0 ;;
        --no-service) DO_SERVICE=0 ;;
        --uninstall) MODE="uninstall" ;;
        --dry-run) DRY_RUN=1 ;;
        -y | --yes) ASSUME_YES=1 ;;
        -h | --help) usage; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
    shift
done

# --------------------------------------------------------------------------
# platform detection

[ "$(uname -s)" = Linux ] || die "airpipe only runs on Linux"

case "$(uname -m)" in
    x86_64 | amd64) ARCH=x86_64; DEB_ARCH=amd64 ;;
    aarch64 | arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
    *) die "unsupported architecture $(uname -m); build from source instead" ;;
esac

if have zypper; then
    PKG=zypper
elif have dnf; then
    PKG=dnf
elif have apt-get; then
    PKG=apt
else
    PKG=tar
fi

if [ "$(id -u)" != 0 ] && ! have sudo && [ "$DRY_RUN" = 0 ]; then
    die "run as root or install sudo"
fi

# --------------------------------------------------------------------------
# firewall

# Prints the interface of the IPv4 default route, e.g. wlp98s0.
lan_dev() {
    ip -o -4 route show default 2>/dev/null |
        awk '{for (i = 1; i < NF; i++) if ($i == "dev") { print $(i + 1); exit }}'
}

# Prints the IPv4 network of an interface, e.g. 192.168.1.0/24.
lan_subnet() { # dev
    cidr=$(ip -o -4 addr show dev "$1" 2>/dev/null | awk '{print $4; exit}')
    [ -n "$cidr" ] || return 1
    addr=${cidr%/*}
    len=${cidr#*/}
    old_ifs=$IFS
    IFS=.
    # shellcheck disable=SC2086 # intentional word splitting on dots
    set -- $addr
    IFS=$old_ifs
    n=$((($1 << 24) | ($2 << 16) | ($3 << 8) | $4))
    mask=$(((0xFFFFFFFF << (32 - len)) & 0xFFFFFFFF))
    net=$((n & mask))
    printf '%d.%d.%d.%d/%d\n' $((net >> 24 & 255)) $((net >> 16 & 255)) $((net >> 8 & 255)) $((net & 255)) "$len"
}

setup_firewall() {
    step "Firewall"
    say "Speakers must reach this computer on UDP 6111-6112 (clock sync, packet"
    say "retransmits), and mDNS (UDP 5353) must be allowed for discovery."
    say "No other port is opened, and nothing can stream TO this computer."

    dev=$(lan_dev)
    if [ -z "$dev" ] || ! subnet=$(lan_subnet "$dev"); then
        warn "could not determine your local network; open the ports manually (see README)"
        return
    fi
    prefix=${subnet#*/}
    if [ "$prefix" -lt 16 ]; then
        warn "network $subnet is unusually large; open the ports manually if this is not your LAN"
        return
    fi

    if have firewall-cmd && systemctl is-active --quiet firewalld 2>/dev/null; then
        zone=$(query_root firewall-cmd --get-zone-of-interface="$dev" 2>/dev/null ||
            query_root firewall-cmd --get-default-zone 2>/dev/null || echo public)
        ask "Allow mDNS and airpipe from $subnet only (firewalld zone '$zone')?" y || return 0
        # Make firewalld see the service file the package just installed.
        as_root firewall-cmd --reload >/dev/null
        for svc in mdns airpipe; do
            rule="rule family=ipv4 source address=$subnet service name=$svc accept"
            if ! query_root firewall-cmd --permanent --zone="$zone" --query-rich-rule="$rule" >/dev/null 2>&1; then
                as_root firewall-cmd --permanent --zone="$zone" --add-rich-rule="$rule" >/dev/null
            fi
        done
        as_root firewall-cmd --reload >/dev/null
        say "Opened to $subnet in zone '$zone'."
        if [ "$zone" = public ]; then
            say "Note: in the 'public' zone this rule also applies to other networks using $subnet."
            say "To limit it to your home network, move that connection to the 'home' zone:"
            say "  nmcli connection modify '<your network>' connection.zone home"
        fi
    elif have ufw && as_root ufw status 2>/dev/null | grep -q '^Status: active'; then
        ask "Allow mDNS and airpipe from $subnet only (ufw)?" y || return 0
        as_root ufw allow from "$subnet" to any port 5353 proto udp comment 'mDNS (airpipe)' >/dev/null
        as_root ufw allow from "$subnet" to any port 6111:6112 proto udp comment 'airpipe' >/dev/null
        say "Opened to $subnet."
    else
        say "No active firewalld or ufw found; nothing to do."
    fi
}

remove_firewall() {
    if have firewall-cmd && systemctl is-active --quiet firewalld 2>/dev/null; then
        # One query for all zones: lines "zone|rule" for every airpipe rule.
        query_root firewall-cmd --permanent --list-all-zones 2>/dev/null |
            awk '/^[^ \t]/ { zone = $1 } /service name="airpipe"/ { sub(/^[ \t]+/, ""); print zone "|" $0 }' \
                >"$TMP/rules" || true
        while IFS='|' read -r zone rule; do
            as_root firewall-cmd --permanent --zone="$zone" --remove-rich-rule="$rule" >/dev/null
            say "Removed firewalld rule in zone '$zone': $rule"
        done <"$TMP/rules"
        as_root firewall-cmd --reload >/dev/null
        say "(mDNS rules were left in place; other software may rely on them.)"
    elif have ufw; then
        say "Remove the airpipe ufw rules with: sudo ufw status numbered, then sudo ufw delete <n>"
    fi
}

# --------------------------------------------------------------------------
# auto-start

setup_service() {
    step "Start automatically"
    if [ "$(id -u)" = 0 ]; then
        say "Skipped: run the installer as your normal user (not with sudo) to set this up."
        return
    fi
    if ! systemctl --user show-environment >/dev/null 2>&1; then
        say "Skipped: no systemd user session."
        return
    fi
    interactive || { say "Skipped (non-interactive). Later: systemctl --user enable --now 'airpipe@<speaker>'"; return; }
    say "Speakers on your network:"
    if [ "$DRY_RUN" = 1 ] || ! airpipe list; then
        say "  (could not list speakers)"
    fi
    printf 'Speaker to play to automatically at login (empty to skip): ' >/dev/tty
    read -r speaker </dev/tty || speaker=""
    [ -n "$speaker" ] || { say "Skipped."; return; }
    unit="airpipe@$(systemd-escape "$speaker").service"
    run systemctl --user daemon-reload
    run systemctl --user enable --now "$unit"
    say "Enabled $unit. Logs: journalctl --user -u '$unit'"
    say "Settings: ~/.config/airpipe/config.toml and devices.toml (see 'airpipe config')."
}

remove_services() {
    systemctl --user show-environment >/dev/null 2>&1 || return 0
    # Enabled units, whether running or not.
    for unit in $(systemctl --user list-units 'airpipe@*' 'airpipe.service' --all --plain --no-legend 2>/dev/null | awk '{print $1}'); do
        run systemctl --user disable --now "$unit" 2>/dev/null || true
    done
}

# --------------------------------------------------------------------------
# install / uninstall

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

if [ "$MODE" = uninstall ]; then
    step "Uninstalling airpipe"
    remove_services
    case "$PKG" in
        zypper) as_root zypper --non-interactive remove airpipe || true ;;
        dnf) as_root dnf remove -y airpipe || true ;;
        apt) as_root apt-get remove -y airpipe || true ;;
    esac
    # Tarball installs (and leftovers) live under /usr/local and /etc.
    for f in /usr/local/bin/airpipe /usr/local/lib/systemd/user/airpipe@.service \
        /usr/local/lib/systemd/user/airpipe.service /etc/firewalld/services/airpipe.xml; do
        [ -e "$f" ] && as_root rm -f "$f"
    done
    [ -d /usr/local/share/doc/airpipe ] && as_root rm -rf /usr/local/share/doc/airpipe
    if [ "$DO_FIREWALL" = 1 ] && ask "Remove airpipe firewall rules?" y; then
        remove_firewall
    fi
    say ""
    say "airpipe removed. Your identity and pairings remain in ~/.config/airpipe (delete it to forget them)."
    exit 0
fi

step "Downloading airpipe ($VERSION) for $ARCH ($PKG)"
if [ -n "$FROM_DIR" ]; then
    SRC="$FROM_DIR"
    [ -f "$SRC/SHA256SUMS" ] || die "$SRC/SHA256SUMS not found"
    cp "$SRC/SHA256SUMS" "$TMP/"
else
    if [ "$VERSION" = latest ]; then
        BASE="https://github.com/$REPO/releases/latest/download"
    else
        BASE="https://github.com/$REPO/releases/download/v${VERSION#v}"
    fi
    download "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "could not download $BASE/SHA256SUMS"
fi

case "$PKG" in
    zypper | dnf) pattern="\\.$ARCH\\.rpm\$" ;;
    apt) pattern="_${DEB_ARCH}\\.deb\$" ;;
    tar) pattern="-$ARCH-linux\\.tar\\.gz\$" ;;
esac
FILE=$(awk '{print $2}' "$TMP/SHA256SUMS" | sed 's/^\*//' | grep -E "$pattern" | head -n1 || true)
[ -n "$FILE" ] || die "this release has no package for $ARCH ($PKG)"

if [ -n "$FROM_DIR" ]; then
    cp "$SRC/$FILE" "$TMP/"
else
    download "$BASE/$FILE" "$TMP/$FILE" || die "could not download $FILE"
fi

(cd "$TMP" && grep -E "[ *]$FILE\$" SHA256SUMS | sha256sum -c --quiet -) || die "checksum mismatch for $FILE; aborting"
say "Verified $FILE (SHA-256)."

step "Installing"
case "$PKG" in
    zypper)
        # Release packages are not GPG-signed; integrity comes from the checksum above.
        as_root zypper --non-interactive install --allow-unsigned-rpm "$TMP/$FILE"
        ;;
    dnf) as_root dnf install -y "$TMP/$FILE" ;;
    apt) as_root apt-get install -y "$TMP/$FILE" ;;
    tar)
        tar -xzf "$TMP/$FILE" -C "$TMP"
        dir=$(find "$TMP" -mindepth 1 -maxdepth 1 -type d -name 'airpipe-*' | head -n1)
        as_root install -Dm755 "$dir/airpipe" /usr/local/bin/airpipe
        for unit in airpipe@.service airpipe.service; do
            as_root install -Dm644 "$dir/packaging/systemd/$unit" "/usr/local/lib/systemd/user/$unit"
        done
        if [ -d /etc/firewalld ]; then
            as_root install -Dm644 "$dir/packaging/firewalld/airpipe.xml" /etc/firewalld/services/airpipe.xml
        fi
        for doc in README.md CONTRIBUTING.md LICENSE-MIT LICENSE-APACHE; do
            as_root install -Dm644 "$dir/$doc" "/usr/local/share/doc/airpipe/$doc"
        done
        ;;
esac
if [ "$DRY_RUN" = 0 ]; then
    say "Installed: $(airpipe --version)"
fi

[ "$DO_FIREWALL" = 1 ] && setup_firewall
[ "$DO_SERVICE" = 1 ] && setup_service

step "Done"
say "Run 'airpipe list' to see speakers and 'airpipe run <speaker>' to start an AirPlay output."
say "Uninstall any time with the same script and --uninstall."
