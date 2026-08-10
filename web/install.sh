#!/bin/sh
set -e

# shell-remote one-line agent install script
# DO NOT run directly — use:
#   run:      curl -fsSL <relay>/agent/install | sh
#   download: curl -fsSL <relay>/agent/install | sh -s -- --download-only
# Uses curl if available, otherwise falls back to wget; tries a list of
# GitHub mirrors so at least one source is reachable in most networks.

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

BASE="https://github.com/zzttzzmyswy/shell-remote/releases/latest/download"
URLS="
${BASE}/shell-remote-${BIN_ARCH}
https://edgeone.gh-proxy.com/${BASE}/shell-remote-${BIN_ARCH}
https://hk.gh-proxy.com/${BASE}/shell-remote-${BIN_ARCH}
https://gh-proxy.com/${BASE}/shell-remote-${BIN_ARCH}
https://gh.llkk.cc/${BASE}/shell-remote-${BIN_ARCH}
"

# Prefer curl, fall back to wget. Mirrors combined with a timeout mean a
# non-curl-only environment and a flaky single mirror still make progress.
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

# download <url> -> 0 on success, non-zero otherwise
download() {
    url="$1"
    if [ "$DL_NAME" = "curl" ]; then
        curl -fsSL --connect-timeout 5 --max-time 60 -o "$BIN" "$url" 2>/dev/null
    else
        wget -q -T 60 --tries 1 -O "$BIN" "$url" 2>/dev/null
    fi
}

echo "[shell-remote] downloading for $ARCH ($BIN_ARCH)..."

for url in $URLS; do
    if download "$url" && [ -s "$BIN" ]; then
        echo "[shell-remote] downloaded via $(echo "$url" | cut -d/ -f3)"
        break
    fi
done

if [ ! -f "$BIN" ] || [ ! -s "$BIN" ]; then
    echo "[shell-remote] download failed — all sources unreachable"
    exit 1
fi

chmod +x "$BIN"

# Sanity check: reject an HTML error page / truncated download that happens to
# be non-empty. Linux/macOS binaries start with the 4-byte ELF magic
# (\x7fELF); a non-ELF file is a mirror's error page. If the first download is
# invalid, try the remaining mirrors.
is_elf() {
    [ -f "$1" ] || return 1
    magic=$(dd if="$1" bs=4 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')
    [ "$magic" = "7f454c46" ]
}

if ! is_elf "$BIN"; then
    echo "[shell-remote] downloaded file is not a valid binary, trying next source..."
    for url in $URLS; do
        case "$url" in
            *"/shell-remote-${BIN_ARCH}") ;;
            *) continue ;;
        esac
        rm -f "$BIN"
        if download "$url" && [ -s "$BIN" ]; then
            chmod +x "$BIN"
            if is_elf "$BIN"; then
                echo "[shell-remote] downloaded via $(echo "$url" | cut -d/ -f3)"
                break
            fi
        fi
    done
fi

if [ ! -x "$BIN" ] || ! is_elf "$BIN"; then
    rm -f "$BIN"
    echo "[shell-remote] download failed — no valid binary obtained"
    exit 1
fi

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
