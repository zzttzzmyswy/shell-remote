#!/bin/sh
set -e

# shell-remote one-line agent install script
# DO NOT run directly — use:
#   run:      curl -fsSL <relay>/agent/install | sh
#   download: curl -fsSL <relay>/agent/install | sh -s -- --download-only

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

echo "[shell-remote] downloading for $ARCH ($BIN_ARCH)..."

for url in $URLS; do
    if curl -fsSL --connect-timeout 5 --max-time 60 -o "$BIN" "$url" 2>/dev/null; then
        echo "[shell-remote] downloaded via $(echo "$url" | cut -d/ -f3)"
        break
    fi
done

if [ ! -f "$BIN" ] || [ ! -s "$BIN" ]; then
    echo "[shell-remote] download failed — all sources unreachable"
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
