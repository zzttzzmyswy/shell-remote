#!/bin/sh
set -e

# shell-remote one-line agent install script
# DO NOT run directly — use:
#   run:      curl -fsSL <relay>/agent/install | sh
#   download: curl -fsSL <relay>/agent/install | sh -s -- --download-only
#
# Sources are tried in order: GitHub release directly, then the GitHub CDN
# mirrors (same list and order as DEFAULT_CDN_BASE_URLS in
# src/relay/artifact_store.rs), then this relay's own /download endpoint (only
# served when the relay runs with --download-dir). The downloader's progress
# output is kept on stderr, so % / bytes / speed stay live even when this
# script itself is piped into sh. A source that fails or stalls is abandoned
# and the next one is tried.
#
# Uses curl if available, otherwise wget (GNU or busybox).

RELAY_URL="__RELAY_URL__"

ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64)   BIN_ARCH="x86_64" ;;
    aarch64|arm64)  BIN_ARCH="aarch64" ;;
    armv7l|armv7)   BIN_ARCH="armv7" ;;
    *) echo "[shell-remote] unsupported architecture: $ARCH"; exit 1 ;;
esac

# --download-only: save the binary to the current directory and do not run it.
DOWNLOAD_ONLY=0
for a in "$@"; do
    [ "$a" = "--download-only" ] && DOWNLOAD_ONLY=1
done

if [ "$DOWNLOAD_ONLY" = "1" ]; then
    BIN="./shell-remote"
else
    TMPDIR="/dev/shm"
    if [ ! -w "$TMPDIR" ]; then
        TMPDIR="/tmp"
    fi
    BIN="$TMPDIR/shell-remote-$$"
fi
PART="$BIN.part"

BASE="https://github.com/zzttzzmyswy/shell-remote/releases/latest/download"
ASSET="shell-remote-${BIN_ARCH}"

# Sources as "<label>|<url>", tried top to bottom. A CDN entry is a prefix
# mirror: mirror origin + the full official URL (`https://gh-proxy.com/${BASE}`),
# not origin + relative path. Bare github.com needs no mirror, so it is source
# #1 instead of a member of that list. Label "relay" is expanded to
# "relay <host>" when printed.
#
# SHELL_REMOTE_INSTALL_URLS overrides the whole list with space-separated full
# URLs (label = URL host). TESTING ONLY; unset in production.
if [ -n "${SHELL_REMOTE_INSTALL_URLS:-}" ]; then
    SOURCES=""
    for u in $SHELL_REMOTE_INSTALL_URLS; do
        SOURCES="${SOURCES}$(printf '%s' "$u" | cut -d/ -f3)|${u}
"
    done
else
    SOURCES="GitHub|${BASE}/${ASSET}
CDN gh-proxy.com|https://gh-proxy.com/${BASE}/${ASSET}
CDN edgeone.gh-proxy.org|https://edgeone.gh-proxy.org/${BASE}/${ASSET}
CDN hk.gh-proxy.org|https://hk.gh-proxy.org/${BASE}/${ASSET}
CDN ghproxy.net|https://ghproxy.net/${BASE}/${ASSET}
relay|${RELAY_URL}/download/${ASSET}
"
fi

TOTAL=0
while IFS= read -r line; do
    [ -n "$line" ] && TOTAL=$((TOTAL + 1))
done <<SOURCES
$SOURCES
SOURCES

# Prefer curl, fall back to wget. busybox wget knows neither --progress nor -q
# behaviour we want: it prints a progress bar with speed by default.
DL_NAME=""
if command -v curl >/dev/null 2>&1; then
    DL_NAME="curl"
elif command -v wget >/dev/null 2>&1; then
    DL_NAME="wget"
else
    echo "[shell-remote] error: neither curl nor wget is available"
    exit 1
fi
echo "[shell-remote] using $DL_NAME for download"

WGET_BUSYBOX=0
if [ "$DL_NAME" = "wget" ] && wget --help 2>&1 | grep -q BusyBox; then
    WGET_BUSYBOX=1
fi

# fetch <url> <dest> -> downloader exit status. 600s is only a last-resort cap
# (a 22MB UI package on a slow link legitimately needs minutes): the useful
# thresholds are the 10s connect timeout and curl's 20s low-speed window.
# stdin is detached so a downloader can never eat the source list.
fetch() {
    if [ "$DL_NAME" = "curl" ]; then
        # -f rejects HTTP errors, -L follows the release redirect, -S keeps the
        # error text; the progress meter (curl's own) goes to stderr.
        curl -f -L -S --connect-timeout 10 --speed-limit 10240 --speed-time 20 \
            --max-time 600 -o "$2" "$1" </dev/null
    elif [ "$WGET_BUSYBOX" = "1" ]; then
        # busybox wget: no --progress, prints "N% +speed" itself; -T is per-read.
        wget -T 20 -O "$2" "$1" </dev/null
    elif command -v timeout >/dev/null 2>&1; then
        timeout 600 wget --progress=bar:force:noscroll -T 20 --tries=1 \
            -O "$2" "$1" </dev/null
    else
        wget --progress=bar:force:noscroll -T 20 --tries=1 -O "$2" "$1" </dev/null
    fi
}

# Human-readable byte count; integer math only (no awk on last-resort systems).
fmt_size() {
    b=$1
    if [ "$b" -ge 1048576 ]; then
        printf '%d.%d MB' $((b / 1048576)) $((b % 1048576 * 10 / 1048576))
    elif [ "$b" -ge 1024 ]; then
        printf '%d KB' $((b / 1024))
    else
        printf '%d B' "$b"
    fi
}

# One-line failure reason from the downloader's exit status.
fail_reason() {
    case "$DL_NAME:$1" in
        curl:6)  printf 'could not resolve host' ;;
        curl:7)  printf 'failed to connect' ;;
        curl:22) printf 'http error' ;;
        curl:28) printf 'timed out or too slow' ;;
        curl:35) printf 'tls handshake failed' ;;
        curl:56) printf 'receive error' ;;
        wget:1)  printf 'generic error' ;;
        wget:4)  printf 'network failure' ;;
        wget:8)  printf 'server error' ;;
        *:124)   printf 'timed out' ;;
        *)       printf 'exit %s' "$1" ;;
    esac
}

# Reject an HTML error page / truncated download that happens to be non-empty.
# Linux/macOS binaries start with the 4-byte ELF magic (\x7fELF).
is_elf() {
    [ -f "$1" ] || return 1
    magic=$(dd if="$1" bs=4 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
    [ "$magic" = "7f454c46" ]
}

echo "[shell-remote] downloading for $ARCH ($BIN_ARCH)..."

# Each iteration downloads into $PART, validates it, and only then renames it
# into place. Nothing to clean up on success: $PART is gone after the move.
FOUND=""
i=0
while IFS='|' read -r label url; do
    [ -n "$url" ] || continue
    i=$((i + 1))
    host=$(printf '%s' "$url" | cut -d/ -f3)
    [ "$label" = "relay" ] && label="relay $host"

    echo "[shell-remote] [$i/$TOTAL] trying $label ($host)"
    rm -f "$PART"
    start=$(date +%s)
    status=0
    fetch "$url" "$PART" || status=$?
    end=$(date +%s)

    if [ "$status" -ne 0 ]; then
        rm -f "$PART"
        echo "[shell-remote] $label failed: $(fail_reason "$status"), trying next source"
        continue
    fi
    if ! is_elf "$PART"; then
        rm -f "$PART"
        echo "[shell-remote] $label failed: not an ELF binary (mirror error page?), trying next source"
        continue
    fi

    size=$(wc -c <"$PART" | tr -d ' ')
    secs=$((end - start))
    [ "$secs" -lt 1 ] && secs=1
    mv -f "$PART" "$BIN"
    FOUND="$label"
    echo "[shell-remote] downloaded $(fmt_size "$size") in ${secs}s (avg $(fmt_size $((size / secs)))/s) from $FOUND"
    break
done <<SOURCES
$SOURCES
SOURCES

if [ -z "$FOUND" ]; then
    echo "[shell-remote] download failed — no valid binary obtained from any source"
    exit 1
fi

chmod +x "$BIN"

if [ "$DOWNLOAD_ONLY" = "1" ]; then
    echo "[shell-remote] saved to $BIN (not executed)"
    exit 0
fi

cleanup() {
    rm -f "$BIN"
    echo "[shell-remote] cleaned up"
}
trap cleanup EXIT INT TERM

echo "[shell-remote] starting agent..."
exec "$BIN" agent --relay-url "$RELAY_URL" "$@"
