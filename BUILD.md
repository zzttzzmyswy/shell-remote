# Cross-Compilation Guide

Build musl-static single binaries for 3 Linux architectures (x86_64, aarch64, armv7).

## Prerequisites

Since v0.19.0 the agent ships desktop H.264 encoding via OpenH264 (vendored
C++ source through the `openh264-sys2` feature). Building it needs:

1. a working C++ compiler (`g++`/`clang++`) — OpenH264's bundled `.cpp`/`.asm`
   sources are compiled by the linker/CC driver used for the target;
2. the C linker of the target (e.g. `musl-gcc` for Linux, `x86_64-w64-mingw32-g++` for Windows);
3. NASM (yasm) is not required for this build.

```bash
# C++ compiler (native)
# Debian/Ubuntu:
sudo apt install g++

rustup target add x86_64-unknown-linux-musl

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

## Static C++ stdlib for musl targets

OpenH264 ships as C++ source, so the musl-static binaries must link a C++
standard library too. The distro cross sysroots only ship a *glibc-built*
`libstdc++.a`, which pulls glibc-only symbols (`__sprintf_chk`,
`__libc_single_threaded`, `__isoc23_strtoul`) into the static link and fails
against musl. Two working recipes (both verified for the 0.19.0 releases):

1. **x86_64 native (LLVM libc++)**: merge the LLVM `libc++.a` + `libc++abi.a`
   into one `libstdc++.a` and prepend its dir with `-L`:

```bash
mkdir -p ~/.cache/musl-stdcxx/x && cd ~/.cache/musl-stdcxx/x && rm -f ./*.o 2>/dev/null
ar x /usr/lib/libc++.a && ar x /usr/lib/libc++abi.a
cd .. && ar rc libstdc++.a x/*.o
# then: RUSTFLAGS="-C link-arg=-L$HOME/.cache/musl-stdcxx" cargo build --release --target x86_64-unknown-linux-musl
```

2. **Cross targets (aarch64/armv7/Windows)**: extract the target's own
   glibc `libstdc++.a`, append a small stub archive providing the glibc-only
   fortify/`*_chk` symbols plus `__libc_single_threaded` / `__isoc23_strtoul`,
   and link with `-L` pointing at that dir:

```bash
L=$(aarch64-linux-gnu-g++ -print-file-name=libstdc++.a)
mkdir -p ~/.cache/glibc-stubs/aarch64/x && cd ~/.cache/glibc-stubs/aarch64
ar x "$L" --output=x && cp /path/to/stub.o x/ && ar rc libstdc++.a x/*.o
# then: CC=aarch64-linux-gnu-gcc CXX=aarch64-linux-gnu-g++ \
#   RUSTFLAGS="-C link-arg=-L$HOME/.cache/glibc-stubs/aarch64" \
#   cargo build --release --target aarch64-unknown-linux-musl
```

`stub.o` is a freestanding `.c` (see the build script in the repo
`tools/build-dist.sh`) compiled with the target's `-gcc`:
`__vfprintf_chk`/`__vsnprintf_chk`/`__snprintf_chk`/`__sprintf_chk`,
`__str{cat,strcpy,ncat,ncpy}_chk`, `__mem{move,cpy,set}_chk` (each just calls
its plain counterpart), plus `int __libc_single_threaded = 1;` and
`__isoc23_strtoul` → `strtoul`.

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
