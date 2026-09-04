#!/bin/bash
# One-command release build for all 4 shipped platforms.
#
# OpenH264 adds C++ (and thus the linked binary needs a C++ stdlib). All four
# targets here link the distro/gcc sysroot's glibc libstdc++.a, which drags in
# glibc-only symbols that musl does not have; we append a small stub archive
# (tools/glibc-cxx-stubs.c) that maps those to plain C calls.
#
# Usage:  tools/build-dist.sh
# Env:    CACHE_DIR overrides $HOME/.cache/shell-remote-dist
#
# Requires: rustup targets + toolchains, see BUILD.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${CACHE_DIR:-$HOME/.cache/shell-remote-dist}"
mkdir -p "$CACHE"
STUB_SRC="$ROOT/tools/glibc-cxx-stubs.c"

# target | cc | cxx | ar | dist name
PLATFORMS=(
  "x86_64-unknown-linux-musl|musl-gcc|g++|ar|shell-remote-x86_64"
  "aarch64-unknown-linux-musl|aarch64-linux-gnu-gcc|aarch64-linux-gnu-g++|aarch64-linux-gnu-ar|shell-remote-aarch64"
  "armv7-unknown-linux-musleabihf|arm-linux-gnueabihf-gcc|arm-linux-gnueabihf-g++|arm-linux-gnueabihf-ar|shell-remote-armv7"
  "x86_64-pc-windows-gnu|x86_64-w64-mingw32-gcc|x86_64-w64-mingw32-g++|x86_64-w64-mingw32-ar|shell-remote-x86_64.exe"
)

for entry in "${PLATFORMS[@]}"; do
  IFS='|' read -r target cc cxx ar dist_name <<<"$entry"
  echo "== $target =="
  dir="$CACHE/${dist_name%.exe}"
  mkdir -p "$dir/x"
  rm -f "$dir/stub.o" "$dir/libstdc++.a"
  "$cc" -O2 -c "$STUB_SRC" -o "$dir/stub.o"
  stdlib=$("$cxx" -print-file-name=libstdc++.a)
  echo "glibc libstdc++: $stdlib"
  ( cd "$dir/x" && rm -f ./*.o 2>/dev/null; ar x "$stdlib" )
  ar rc "$dir/libstdc++.a" "$dir"/x/*.o "$dir/stub.o"

  # VP9 (libvpx): prefer a prebuilt static libvpx for this target (built by
  # tools/build-libvpx.sh → $CACHE/libvpx-<target>/). Fall back to the default
  # vp9 feature pulling pkg-config libvpx (dev builds). When neither exists,
  # build with vp9 disabled so the binary still links.
  LIBVPX_FLAGS=()
  if [ -d "$CACHE/libvpx-$target" ]; then
    LIBVPX_FLAGS=("LIBVPX_DIR=$CACHE/libvpx-$target")
    echo "static libvpx: $CACHE/libvpx-$target"
  else
    echo "WARNING: no static libvpx for $target ($CACHE/libvpx-$target) — building without VP9"
    LIBVPX_FLAGS=()
  fi

  env CC="$cc" CXX="$cxx" AR="$ar" "${LIBVPX_FLAGS[@]}" \
      RUSTFLAGS="-C link-arg=-L$dir" \
      cargo build --release --target "$target" --manifest-path "$ROOT/Cargo.toml"
done

# Windows agent 嵌入 requireAdministrator manifest: 让 SendInput 能操作
# 提权窗口/多数弹窗(360 等)。UAC 安全桌面本身仍需服务级组件(后续)。
echo "== embedding windows manifest =="
x86_64-w64-mingw32-windres "$ROOT/build/embed-manifest.rc" -O coff -o "$ROOT/build/agent_manifest.o"
WIN_DIR="$CACHE/libvpx-x86_64-pc-windows-gnu"
WIN_LIBVPX=()
if [ -d "$WIN_DIR" ]; then
  WIN_LIBVPX=("LIBVPX_DIR=$WIN_DIR")
  # 触碰 build.rs 强制重跑 build script: cargo 按 (package, RUSTFLAGS) 缓存
  # build-script 输出, manifest 段 RUSTFLAGS 与主循环不同, LIBVPX_DIR 的值
  # 相同时不会触发 rerun-if-env-changed, 复用无 vpx 的旧输出 → 链接缺 -lvpx。
  touch "$ROOT/build.rs"
fi
RUSTFLAGS="-C link-args=$ROOT/build/agent_manifest.o -C target-feature=+crt-static" \
  env "${WIN_LIBVPX[@]}" \
  cargo build --release --target x86_64-pc-windows-gnu --manifest-path "$ROOT/Cargo.toml"

echo "== staging releases =="
mkdir -p "$ROOT/dist"
cp "$ROOT"/target/x86_64-unknown-linux-musl/release/shell-remote            "$ROOT/dist/shell-remote-x86_64"
cp "$ROOT"/target/aarch64-unknown-linux-musl/release/shell-remote           "$ROOT/dist/shell-remote-aarch64"
cp "$ROOT"/target/armv7-unknown-linux-musleabihf/release/shell-remote       "$ROOT/dist/shell-remote-armv7"
cp "$ROOT"/target/x86_64-pc-windows-gnu/release/shell-remote.exe            "$ROOT/dist/shell-remote-x86_64.exe"

echo "Built:"
ls -lh "$ROOT/dist/"
file "$ROOT"/dist/shell-remote-*