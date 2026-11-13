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

# libaom 源码准备（MYS-2828）：下载/解压失败绝不留下"看起来可用"的半成品。
#
# 旧版直接 `curl -o $AOM_TARBALL` 再 `tar xzf`：curl 中途断掉会留下截断的
# tar.gz，tar 失败后目录里已经躺着一半源码；下一次 `[ ! -d "$SRC" ]` 判为
# "已存在"直接复用 → 半成品源码被当成有效依赖，编出来的 libaom 可能静默缺
# 功能（甚至缺 AV1）。这里先下载到 .part、用 gzip -t 校验、解压到临时目录、
# 结构校验通过后原子 rename；任一步失败都清理干净。
libaom_src_ok() {
  [ -f "$1/CMakeLists.txt" ] && [ -f "$1/aom/aom_codec.h" ] && [ -f "$1/aom/aomcx.h" ]
}

ensure_src() {
  if libaom_src_ok "$SRC"; then return 0; fi
  if [ -e "$SRC" ]; then
    echo "已有 libaom 源码结构不完整，清理后重新解压：$SRC" >&2
    rm -rf "$SRC"
  fi
  if [ -f "$AOM_TARBALL" ] && ! gzip -t "$AOM_TARBALL" 2>/dev/null; then
    echo "已有 $AOM_TARBALL 校验失败（截断/损坏），重新下载" >&2
    rm -f "$AOM_TARBALL"
  fi
  if [ ! -f "$AOM_TARBALL" ]; then
    echo "downloading libaom $AOM_VER ..."
    # aom 官方在 googlesource（GitHub 无官方镜像）。
    if ! curl -fsSL --retry 3 --retry-delay 2 -o "$AOM_TARBALL.part" \
        "https://aomedia.googlesource.com/aom/+archive/$AOM_VER.tar.gz"; then
      rm -f "$AOM_TARBALL.part"
      echo "ERROR: libaom 下载失败（无半成品缓存残留）" >&2; exit 1
    fi
    if ! gzip -t "$AOM_TARBALL.part" 2>/dev/null; then
      rm -f "$AOM_TARBALL.part"
      echo "ERROR: libaom 下载内容不是有效 gzip（无半成品缓存残留）" >&2; exit 1
    fi
    mv -f "$AOM_TARBALL.part" "$AOM_TARBALL"
  fi
  tmp="$SRC.tmp.$$"
  rm -rf "$tmp"; mkdir -p "$tmp"
  if ! tar xzf "$AOM_TARBALL" -C "$tmp"; then
    rm -rf "$tmp"; echo "ERROR: libaom 解压失败（已清理临时目录）" >&2; exit 1
  fi
  if ! libaom_src_ok "$tmp"; then
    rm -rf "$tmp"; echo "ERROR: libaom 解压后结构异常（已清理）" >&2; exit 1
  fi
  : > "$tmp/.extracted-ok"
  rm -rf "$SRC"; mv -f "$tmp" "$SRC"
}

ensure_src

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
    # x86 SIMD（MYS-886 卡顿修复）：此前两个独立问题导致 HAVE_SSE2/AVX2 全 0
    # 的纯 C 构建——① ENABLE_NASM 默认 OFF；② 交叉编译下 CMAKE_SYSTEM_PROCESSOR
    # 探测失败 → AOM_TARGET_CPU=generic（日志 "architecture is not supported"）。
    # 显式传 AOM_TARGET_CPU + ENABLE_NASM=ON 双保险；nasm 缺失时 cmake 配置
    # 直接报错（fail fast），不会静默回退纯 C。
    -DAOM_TARGET_CPU=$(case "$target" in x86_64*) echo x86_64;; aarch64*) echo arm64;; armv7*) echo armv7;; *) echo "";; esac)
    -DENABLE_NASM=ON
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
  # armv7 gnueabihf 默认 -mfloat-abi=softfp 不带 NEON 使能 → NEON intrinsics
  # always_inline 报 target mismatch。显式 -mfpu=neon（与 libvpx 脚本的
  # neon 能力对齐）；gnueabihf 的 ABI 本身硬浮点，直接可用。
  if [[ "$target" == armv7* ]]; then
    CMAKE_ARGS+=(-DCMAKE_C_FLAGS="-mfpu=neon -mfloat-abi=hard")
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
    # 静态库必须真的含 AV1 编码器符号：否则 build.rs 会照常链接一个没有
    # AV1 的 aom，发布产物就静默降级成只有 H.264（MYS-2828）。
    if ! grep -q 'aom_codec_av1_cx' <<<"$(nm -g --defined-only "$out/lib/libaom.a" 2>/dev/null)"; then
      echo "ERROR: $target 的 libaom.a 不含 aom_codec_av1_cx（CONFIG_AV1_ENCODER 未生效）" >&2
      exit 1
    fi
    echo "staged $out/lib/libaom.a: $(stat -c%s "$out/lib/libaom.a") bytes"
  else
    # 旧版只 WARNING 就继续，最后产出无 AV1 的制品。缺 AV1 静态库是硬错误。
    echo "ERROR: $target 未产出 libaom.a —— 拒绝继续（会让该平台发布产物静默无 AV1）" >&2
    exit 1
  fi
done

echo "Done. libaom static libs are in \$CACHE/libaom-<target>/"
echo "Set LIBXAOM_DIR=<dir> when invoking cargo build for the corresponding target."