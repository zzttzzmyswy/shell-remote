#!/bin/bash
# Cross-compile static libaom for shell-remote's shipped platforms.
#
# AV1 software encoding needs libaom linked into the single static binaries
# (build.rs links via LIBXAOM_DIR). This script builds `libaom.a` for each
# target using the same cross toolchains as build-dist.sh.
#
# Usage:  tools/build-libaom.sh
# Env:    LIBXAOM_SRC  libaom source dir (default: $HOME/.cache/shell-remote-dist/libaom)
#         CACHE_DIR    overrides $HOME/.cache/shell-remote-dist
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${CACHE_DIR:-$HOME/.cache/shell-remote-dist}"
mkdir -p "$CACHE"
SRC="${LIBXAOM_SRC:-$CACHE/libaom-src}"
AOM_TARBALL="$CACHE/libaom.tar.gz"
AOM_VER="v3.15.0"

if [ ! -d "$SRC" ]; then
  if [ ! -f "$AOM_TARBALL" ]; then
    echo "downloading libaom $AOM_VER ..."
    # aom 官方在 googlesource（GitHub 无官方镜像）。
    curl -sL -o "$AOM_TARBALL" "https://aomedia.googlesource.com/aom/+archive/$AOM_VER.tar.gz"
  fi
  mkdir -p "$SRC"
  tar xzf "$AOM_TARBALL" -C "$SRC"
fi

# target | cc | cxx | ar | cmake toolchain (none = native)
# libaom 3.x cmake: 交叉编译走 -DCMAKE_SYSTEM_NAME / -DCMAKE_C_COMPILER 等。
PLATFORMS=(
  "x86_64-unknown-linux-musl|musl-gcc|musl-g++|ar||Linux"
  "aarch64-unknown-linux-musl|aarch64-linux-musl-gcc|aarch64-linux-musl-g++|ar||Linux"
  "armv7-unknown-linux-musleabihf|arm-linux-gnueabihf-gcc|arm-linux-gnueabihf-g++|ar||Linux"
  "x86_64-pc-windows-gnu|x86_64-w64-mingw32-gcc|x86_64-w64-mingw32-g++|ar||Windows"
)

for entry in "${PLATFORMS[@]}"; do
  IFS='|' read -r target cc cxx ar sysname imp <<<"$entry"
  echo "== libaom for $target =="
  out="$CACHE/libaom-$target"
  rm -rf "$out"
  mkdir -p "$out/lib" "$out/include" "$out/build"
  pushd "$out/build" >/dev/null
  CMAKE_ARGS=(
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX="$out"
    -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TESTDATA=0 -DENABLE_TESTS=0
    -DENABLE_TOOLS=0
    -DBUILD_SHARED_LIBS=0
    -DCONFIG_AV1_ENCODER=1 -DCONFIG_AV1_DECODER=0
    -DCONFIG_MULTITHREAD=1
    # 交叉工具链(尤其 arm gnueabihf)的 specs 会注入 -latomic_asneeded,
    # 可执行链接测试会失败; 我们只产静态库, 跳过链接试跑。
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY
    -DCMAKE_EXE_LINKER_FLAGS=-fno-link-libatomic
  )
  if [ -n "$imp" ]; then
    CMAKE_ARGS+=(
      -DCMAKE_SYSTEM_NAME="$imp"
      -DCMAKE_C_COMPILER="$cc"
      -DCMAKE_CXX_COMPILER="$cxx"
      -DCMAKE_AR="$(command -v "$ar" || true)"
    )
  else
    CMAKE_ARGS+=(-DCMAKE_C_COMPILER="$cc" -DCMAKE_CXX_COMPILER="$cxx")
  fi
  cmake "$SRC" "${CMAKE_ARGS[@]}" >/tmp/aom-cmake-$target.log 2>&1 || {
    echo "cmake config failed for $target (see /tmp/aom-cmake-$target.log)"; tail -20 /tmp/aom-cmake-$target.log; popd >/dev/null; exit 1;
  }
  # 只构建库 (aom target), 不构建 aomenc 等工具 —— 交叉工具链下 tools
  # 编译会因 glibc/musl 头差异失败, 而我们只需要 libaom.a。
  make -j"$(nproc)" aom >/tmp/aom-make-$target.log 2>&1 || {
    echo "make aom failed for $target (see /tmp/aom-make-$target.log)"; tail -20 /tmp/aom-make-$target.log; popd >/dev/null; exit 1;
  }
  popd >/dev/null
  if [ -f "$out/build/libaom.a" ]; then
    mkdir -p "$out/lib" "$out/include"
    cp "$out/build/libaom.a" "$out/lib/libaom.a"
    cp -r "$SRC/aom" "$out/include/aom"
    echo "staged $out/lib/libaom.a: $(stat -c%s "$out/lib/libaom.a") bytes"
  else
    echo "WARNING: no libaom.a produced for $target"
  fi
done

echo "Done. libaom static libs are in \$CACHE/libaom-<target>/"
echo "Set LIBXAOM_DIR=<dir> when invoking cargo build for the corresponding target."