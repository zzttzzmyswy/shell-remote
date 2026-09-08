#!/bin/bash
# One-command release build for all 4 shipped platforms, producing BOTH:
#   dist/shell-remote-<arch>        CLI 二进制（relay + agent 仅终端转发，lean）
#   dist/shell-remote-ui-<arch>     UI 二进制（agent 终端 + 桌面转发）
#
# CLI 二进制（--no-default-features）不编译任何桌面代码：无 openh264/aom/
# str0m/x11rb 等重依赖，纯 Rust + ring(C)，无需 libstdc++ 与 libaom。
# UI 二进制需要 OpenH264(C++) 与 libaom(AV1)：沿用 libstdc++ 静态库 + stub
# 方案（见下方说明）。
#
# aarch64/armv7 用 musl.cc 真 musl 工具链：UI 构建中 str0m 引入的 aws-lc-sys
# C 代码用 glibc cross-gcc 编译会泄入 __isoc23_* glibc 符号、musl 链接失败
# （MYS-931 gate）；CLI 构建的 ring(C/asm) 交叉编译同样用它。工具链解压到
# $HOME/.cache/muslcc-<arch>（musl.cc 发布，GCC 11.2.1 + musl 1.2.3）。
# x86_64 用系统 musl-gcc（默认 cc + crt-static 亦等价）；Windows 用 mingw。
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

MUSLCC_AARCH64="${MUSLCC_AARCH64:-$HOME/.cache/muslcc-aarch64/bin/aarch64-linux-musl-}"
MUSLCC_ARMV7="${MUSLCC_ARMV7:-$HOME/.cache/muslcc-armv7/bin/arm-linux-musleabihf-}"
if [ ! -x "${MUSLCC_AARCH64}gcc" ] || [ ! -x "${MUSLCC_ARMV7}gcc" ]; then
  echo "ERROR: aarch64/armv7 需 musl.cc 工具链（UI 的 aws-lc-sys / CLI 的 ring
C 代码交叉编译 + 链接）。请下载并解压到 \$HOME/.cache/muslcc-{aarch64,armv7}
（https://musl.cc/aarch64-linux-musl-cross.tgz 与
arm-linux-musleabihf-cross.tgz），或设 MUSLCC_AARCH64 / MUSLCC_ARMV7 指向
<prefix>（需含 gcc/g++/ar）。" >&2
  exit 2
fi
# target | cc | cxx | ar | dist name
PLATFORMS=(
  "x86_64-unknown-linux-musl|musl-gcc|g++|ar|shell-remote-x86_64"
  "aarch64-unknown-linux-musl|${MUSLCC_AARCH64}gcc|${MUSLCC_AARCH64}g++|${MUSLCC_AARCH64}ar|shell-remote-aarch64"
  "armv7-unknown-linux-musleabihf|${MUSLCC_ARMV7}gcc|${MUSLCC_ARMV7}g++|${MUSLCC_ARMV7}ar|shell-remote-armv7"
  "x86_64-pc-windows-gnu|x86_64-w64-mingw32-gcc|x86_64-w64-mingw32-g++|x86_64-w64-mingw32-ar|shell-remote-x86_64.exe"
)

for entry in "${PLATFORMS[@]}"; do
  IFS='|' read -r target cc cxx ar dist_name <<<"$entry"
  echo "== $target =="
  dir="$CACHE/${dist_name%.exe}"
  mkdir -p "$dir/x"

  # ---- CLI 二进制（lean）：无桌面依赖，无需 libstdc++ stub / libaom ----
  echo "-- $target: shell-remote (CLI, --no-default-features) --"
  env CC="$cc" RUSTFLAGS="-C link-arg=-L$dir" \
      cargo build --release --bin shell-remote --no-default-features \
      --target "$target" --manifest-path "$ROOT/Cargo.toml"

  # ---- UI 二进制（desktop）：libstdc++ stub 合并 + 静态 libaom ----
  # 桌面编码的 OpenH264 引入 C++，链接需要 C++ 运行时。四个目标都链接
  # distro/gcc sysroot 的 glibc libstdc++.a，其引用 glibc-only 符号
  # （__*_chk、fopen64…），musl 链接失败 → 合并 glibc-cxx-stubs.c 的 stub。
  # musl.cc 工具链的 libstdc++ 虽为 musl 构建，但 CACHE 里的 libaom 静态库
  # 是旧 glibc 工具链预编译的，同样引用 fopen64 等 glibc 符号 → stub 也合入。
  rm -f "$dir/stub.o" "$dir/libstdc++.a"
  stdlib=$("$cxx" -print-file-name=libstdc++.a)
  echo "libstdc++: $stdlib ($cc)"
  "$cc" -O2 -c "$STUB_SRC" -o "$dir/stub.o"
  ( cd "$dir/x" && rm -f ./*.o 2>/dev/null; "$ar" x "$stdlib" )
  "$ar" rc "$dir/libstdc++.a" "$dir"/x/*.o "$dir/stub.o"

  LIBXAOM_FLAGS=()
  if [ -d "$CACHE/libaom-$target" ]; then
    LIBXAOM_FLAGS=("LIBXAOM_DIR=$CACHE/libaom-$target")
    echo "static libaom: $CACHE/libaom-$target"
  else
    echo "WARNING: no static libaom for $target ($CACHE/libaom-$target) — building without AV1"
    LIBXAOM_FLAGS=()
  fi

  # musl libstdc++ 的 guard 引用 __sync_synchronize，已在 glibc-cxx-stubs.c
  # 中为 arm/aarch64 提供了内联 dmb 屏障实现（stub.o 已合入 libstdc++.a）。
  # 不链 -lgcc：musl libgcc 的 linux-atomic.o 与 rust compiler_builtins 在
  # arm 下存在 __sync_fetch_and_add_* 重复符号定义。

  # UI 二进制的窗口 GUI（eframe）依赖 x11-dl/dlib 无条件 `-ldl`；musl 把 dl
  # 并入 libc 无独立 libdl。x86_64 恰好自动兼容，aarch64/armv7 需要一个空
  # libdl.a stub（与本文件的 glibc-cxx-stubs.c 同套路）。
  GUI_LDL_ARG=""
  if [[ "$target" == aarch64-* || "$target" == armv7-* ]]; then
    printf '' > "$dir/dl_empty.c"
    "$cc" -O2 -c "$dir/dl_empty.c" -o "$dir/dl_empty.o"
    "$ar" rcs "$dir/libdl.a" "$dir/dl_empty.o"
    GUI_LDL_ARG="-C link-arg=-L$dir"
  fi

  echo "-- $target: shell-remote-ui (desktop agent + window GUI) --"
  env CC="$cc" CXX="$cxx" AR="$ar" "${LIBXAOM_FLAGS[@]}" \
      RUSTFLAGS="-C link-arg=-L$dir $GUI_LDL_ARG" \
      cargo build --release --bin shell-remote-ui \
      --target "$target" --manifest-path "$ROOT/Cargo.toml"
done

# Windows agent 嵌入 requireAdministrator manifest: 让 SendInput 能操作
# 提权窗口/多数弹窗(360 等)。UAC 安全桌面本身仍需服务级组件(后续)。
# 仅 UI 二进制需要（桌面键鼠注入）；CLI 二进制不嵌入。
echo "== embedding windows manifest (UI only) =="
x86_64-w64-mingw32-windres "$ROOT/build/embed-manifest.rc" -O coff -o "$ROOT/build/agent_manifest.o"
WIN_AOM_DIR="$CACHE/libaom-x86_64-pc-windows-gnu"
WIN_LIBXAOM=()
if [ -d "$WIN_AOM_DIR" ]; then
  WIN_LIBXAOM=("LIBXAOM_DIR=$WIN_AOM_DIR")
fi
if [ -n "${WIN_LIBXAOM[*]}" ]; then
  # 触碰 build.rs 强制重跑 build script: cargo 按 (package, RUSTFLAGS) 缓存
  # build-script 输出, manifest 段 RUSTFLAGS 与主循环不同, LIBXAOM_DIR
  # 的值相同时不会触发 rerun-if-env-changed, 复用无 aom 的旧输出 →
  # 链接缺 -laom。
  touch "$ROOT/build.rs"
fi
# 静态 C++ 运行时: 不加 -static-libstdc++ 与 -L$dir 时, -lstdc++ 落到 mingw
# 的动态导入库 libstdc++-6.dll.a → exe 运行时报"找不到 libstdc++-6.dll"
# （用户实拍 MYS-886）。-static-libstdc++ + 打包的静态 libstdc++.a 双保险。
WIN_STD="$CACHE/shell-remote-x86_64"
RUSTFLAGS="-C link-args=$ROOT/build/agent_manifest.o -C target-feature=+crt-static \
-C link-arg=-L$WIN_STD -C link-arg=-static-libstdc++" \
  env "${WIN_LIBXAOM[@]}" \
  cargo build --release --bin shell-remote-ui \
  --target x86_64-pc-windows-gnu --manifest-path "$ROOT/Cargo.toml"

echo "== staging releases =="
mkdir -p "$ROOT/dist"
# 逐个拷贝；某目标正被使用(如本地 relay 跑着 dist 二进制)时 cp 会 ETXTBSY,
# 不应中止其余目标(该目标可稍后停进程再补拷)。
stage() { cp "$1" "$2" && echo "staged $2" || { echo "WARN: 未能覆盖 $2 (可能仍被运行中的进程占用)"; failed=1; }; }
failed=0
# CLI 二进制
stage "$ROOT"/target/x86_64-unknown-linux-musl/release/shell-remote \
      "$ROOT/dist/shell-remote-x86_64"
stage "$ROOT"/target/aarch64-unknown-linux-musl/release/shell-remote \
      "$ROOT/dist/shell-remote-aarch64"
stage "$ROOT"/target/armv7-unknown-linux-musleabihf/release/shell-remote \
      "$ROOT/dist/shell-remote-armv7"
stage "$ROOT"/target/x86_64-pc-windows-gnu/release/shell-remote.exe \
      "$ROOT/dist/shell-remote-x86_64.exe"
# UI 二进制
stage "$ROOT"/target/x86_64-unknown-linux-musl/release/shell-remote-ui \
      "$ROOT/dist/shell-remote-ui-x86_64"
stage "$ROOT"/target/aarch64-unknown-linux-musl/release/shell-remote-ui \
      "$ROOT/dist/shell-remote-ui-aarch64"
stage "$ROOT"/target/armv7-unknown-linux-musleabihf/release/shell-remote-ui \
      "$ROOT/dist/shell-remote-ui-armv7"
stage "$ROOT"/target/x86_64-pc-windows-gnu/release/shell-remote-ui.exe \
      "$ROOT/dist/shell-remote-ui-x86_64.exe"

echo "Built:"
ls -lh "$ROOT/dist/"
file "$ROOT"/dist/shell-remote*
