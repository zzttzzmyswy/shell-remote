#!/bin/bash
# Cross-compile static libvpx for shell-remote's shipped platforms.
#
# VP9 software encoding needs libvpx linked into the single static binaries.
# This script builds `libvpx.a` for each target using the same cross toolchain
# that build-dist.sh uses, then build.rs links via LIBVPX_DIR.
#
# Usage:  tools/build-libvpx.sh
# Env:    LIBVPX_SRC  libvpx source dir (default: $HOME/.cache/shell-remote-dist/libvpx)
#         CACHE_DIR   overrides $HOME/.cache/shell-remote-dist
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${CACHE_DIR:-$HOME/.cache/shell-remote-dist}"
mkdir -p "$CACHE"
SRC="${LIBVPX_SRC:-$CACHE/libvpx-1.17.0}"
LIBVPX_TARBALL="$CACHE/libvpx-1.17.0.tar.gz"

if [ ! -d "$SRC" ]; then
  if [ ! -f "$LIBVPX_TARBALL" ]; then
    echo "downloading libvpx 1.17.0 ..."
    curl -sL -o "$LIBVPX_TARBALL" https://codeload.github.com/webmproject/libvpx/tar.gz/refs/tags/v1.17.0
  fi
  tar xzf "$LIBVPX_TARBALL" -C "$CACHE"
fi

# target | cc | cxx | configure target | extra configure args
# armv7:
#   - gnueabihf gcc 的 specs 自动注入 -latomic_asneeded（本地没有别名）→
#     LDFLAGS=-fno-link-libatomic 跳过该注入
#   - 老 binutils 汇编器不识别 -march=armv7-a（neon 手写 asm）→ 禁用
#     neon-asm，用 C 实现（armv7 设备出货少，代价可接受）
PLATFORMS=(
  "x86_64-unknown-linux-musl|musl-gcc|musl-g++|x86_64-linux-gcc|"
  "aarch64-unknown-linux-musl|aarch64-linux-musl-gcc|aarch64-linux-musl-g++|arm64-linux-gcc|"
  "armv7-unknown-linux-musleabihf|arm-linux-gnueabihf-gcc|arm-linux-gnueabihf-g++|armv7-linux-gcc|--disable-neon-asm"
  "x86_64-pc-windows-gnu|x86_64-w64-mingw32-gcc|x86_64-w64-mingw32-g++|x86_64-win64-gcc|"
)

for entry in "${PLATFORMS[@]}"; do
  IFS='|' read -r target cc cxx cfg_target extra_args <<<"$entry"
  echo "== libvpx for $target =="
  out="$CACHE/libvpx-$target"
  rm -rf "$out"
  mkdir -p "$out/lib" "$out/include"
  # armv7 gnueabihf 的 specs 自动注入 -latomic_asneeded（本地没有别名）→
  # LDFLAGS=-fno-link-libatomic 跳过该注入。其它平台（musl 交叉 gcc 等）
  # 不认这个选项，必须留空——否则 configure 的 check_ld 直接失败。
  LDFLAGS=""
  if [[ "$cc" == *gnueabihf* ]]; then LDFLAGS="-fno-link-libatomic"; fi
  (
    cd "$SRC"
    make distclean >/dev/null 2>&1 || true
    # LD 必须指向交叉编译器：libvpx configure 的 check_ld 回落宿主 gcc，
    # 否则链接测试产物格式不对 → "Toolchain is unable to link executables"。
    CC="$cc" CXX="$cxx" LD="$cc" \
    LDFLAGS="$LDFLAGS" \
    ./configure \
      --target="$cfg_target" \
      --disable-shared --enable-static \
      --disable-tools --disable-docs --disable-examples \
      --disable-unit-tests --disable-webm-io \
      --disable-vp9-decoder \
      $extra_args
    make -j"$(nproc)" libvpx.a
  )
  cp "$SRC/libvpx.a" "$out/lib/"
  cp -r "$SRC/vpx" "$out/include/vpx"
  echo "staged $out"
done

echo "Done. libvpx static libs are in $CACHE/libvpx-<target>/"
echo "Set LIBVPX_DIR=<dir> when invoking cargo build for the corresponding target."