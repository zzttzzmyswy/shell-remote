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
  # AV1 (libaom): same pattern (tools/build-libaom.sh → $CACHE/libaom-<target>/).
  LIBXAOM_FLAGS=()
  if [ -d "$CACHE/libaom-$target" ]; then
    LIBXAOM_FLAGS=("LIBXAOM_DIR=$CACHE/libaom-$target")
    echo "static libaom: $CACHE/libaom-$target"
  else
    echo "WARNING: no static libaom for $target ($CACHE/libaom-$target) — building without AV1"
    LIBXAOM_FLAGS=()
  fi

  env CC="$cc" CXX="$cxx" AR="$ar" "${LIBVPX_FLAGS[@]}" "${LIBXAOM_FLAGS[@]}" \
      RUSTFLAGS="-C link-arg=-L$dir" \
      cargo build --release --target "$target" --manifest-path "$ROOT/Cargo.toml"
done

# Windows agent 嵌入 requireAdministrator manifest: 让 SendInput 能操作
# 提权窗口/多数弹窗(360 等)。UAC 安全桌面本身仍需服务级组件(后续)。
echo "== embedding windows manifest =="
x86_64-w64-mingw32-windres "$ROOT/build/embed-manifest.rc" -O coff -o "$ROOT/build/agent_manifest.o"
WIN_DIR="$CACHE/libvpx-x86_64-pc-windows-gnu"
WIN_LIBVPX=()
WIN_AOM_DIR="$CACHE/libaom-x86_64-pc-windows-gnu"
WIN_LIBXAOM=()
if [ -d "$WIN_DIR" ]; then
  WIN_LIBVPX=("LIBVPX_DIR=$WIN_DIR")
fi
if [ -d "$WIN_AOM_DIR" ]; then
  WIN_LIBXAOM=("LIBXAOM_DIR=$WIN_AOM_DIR")
fi
if [ -n "${WIN_LIBVPX[*]}" ] || [ -n "${WIN_LIBXAOM[*]}" ]; then
  # 触碰 build.rs 强制重跑 build script: cargo 按 (package, RUSTFLAGS) 缓存
  # build-script 输出, manifest 段 RUSTFLAGS 与主循环不同, LIBVPX/LIBXAOM_DIR
  # 的值相同时不会触发 rerun-if-env-changed, 复用无 vpx/aom 的旧输出 →
  # 链接缺 -lvpx/-laom。
  touch "$ROOT/build.rs"
fi
# 静态 C++ 运行时: 不加 -static-libstdc++ 与 -L$dir 时, -lstdc++ 落到 mingw
# 的动态导入库 libstdc++-6.dll.a → exe 运行时报"找不到 libstdc++-6.dll"
# （用户实拍 MYS-886）。-static-libstdc++ + 打包的静态 libstdc++.a 双保险。
WIN_STD="$CACHE/shell-remote-x86_64"
RUSTFLAGS="-C link-args=$ROOT/build/agent_manifest.o -C target-feature=+crt-static \
-C link-arg=-L$WIN_STD -C link-arg=-static-libstdc++" \
  env "${WIN_LIBVPX[@]}" "${WIN_LIBXAOM[@]}" \
  cargo build --release --target x86_64-pc-windows-gnu --manifest-path "$ROOT/Cargo.toml"

echo "== staging releases =="
mkdir -p "$ROOT/dist"
# 逐个拷贝；某目标正被使用(如本地 relay 跑着 dist 二进制)时 cp 会 ETXTBSY,
# 不应中止其余目标(该目标可稍后停进程再补拷)。
stage() { cp "$1" "$2" && echo "staged $2" || { echo "WARN: 未能覆盖 $2 (可能仍被运行中的进程占用)"; failed=1; }; }
failed=0
stage "$ROOT"/target/x86_64-unknown-linux-musl/release/shell-remote \
      "$ROOT/dist/shell-remote-x86_64"
stage "$ROOT"/target/aarch64-unknown-linux-musl/release/shell-remote \
      "$ROOT/dist/shell-remote-aarch64"
stage "$ROOT"/target/armv7-unknown-linux-musleabihf/release/shell-remote \
      "$ROOT/dist/shell-remote-armv7"
stage "$ROOT"/target/x86_64-pc-windows-gnu/release/shell-remote.exe \
      "$ROOT/dist/shell-remote-x86_64.exe"

echo "Built:"
ls -lh "$ROOT/dist/"
file "$ROOT"/dist/shell-remote-*