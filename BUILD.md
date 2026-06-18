# Cross-Compilation Guide

Build musl-static single binaries for 3 Linux architectures (x86_64, aarch64, armv7).

## Prerequisites

```bash
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf

# musl-gcc headers for cross-compilation
# Debian/Ubuntu:
sudo apt install musl-tools

# Fedora:
sudo dnf install musl-gcc

# Alpine:
apk add musl-dev

# Cross-compilation libc toolchains (for aarch64/armv7)
# Debian/Ubuntu:
sudo apt install gcc-aarch64-linux-gnu gcc-arm-linux-gnueabihf
```

## Build All Architectures

```bash
# x86_64 (native)
cargo build --release --target x86_64-unknown-linux-musl

# aarch64
CC=aarch64-linux-gnu-gcc cargo build --release --target aarch64-unknown-linux-musl

# armv7
CC=arm-linux-gnueabihf-gcc cargo build --release --target armv7-unknown-linux-musleabihf
```

## Rename Binaries for Distribution

```bash
mkdir -p releases

cp target/x86_64-unknown-linux-musl/release/shell-remote releases/shell-remote-x86_64
cp target/aarch64-unknown-linux-musl/release/shell-remote releases/shell-remote-aarch64
cp target/armv7-unknown-linux-musleabihf/release/shell-remote releases/shell-remote-armv7

# Verify they are static
file releases/shell-remote-*
```

## One-Command Build Script

```bash
#!/bin/bash
set -euo pipefail

ARCHS=(
  "x86_64-unknown-linux-musl"
  "aarch64-unknown-linux-musl"
  "armv7-unknown-linux-musleabihf"
)

CC_MAP=(
  "aarch64-unknown-linux-musl:aarch64-linux-gnu-gcc"
  "armv7-unknown-linux-musleabihf:arm-linux-gnueabihf-gcc"
)

mkdir -p releases

cargo build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/shell-remote releases/shell-remote-x86_64

for target in aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf; do
  cc=""
  for mapping in "${CC_MAP[@]}"; do
    key="${mapping%%:*}"
    val="${mapping##*:}"
    [ "$key" = "$target" ] && cc="$val" && break
  done
  env CC="$cc" cargo build --release --target "$target"
  bin_name="${target%%-unknown-linux-musl*}"
  cp "target/$target/release/shell-remote" "releases/shell-remote-${bin_name}"
done

echo "Built:"
ls -lh releases/
file releases/*
```
