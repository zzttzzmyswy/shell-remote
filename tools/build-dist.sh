#!/bin/bash
# One-command release build for all 4 shipped platforms, producing BOTH:
#   dist/shell-remote-<arch>        CLI 二进制（relay + agent 仅终端转发，lean）
#   dist/shell-remote-ui-<arch>     UI 二进制（agent 终端 + 桌面转发）
#
# ── 两种链接形态：CLI 用 musl，Linux UI 用 GNU/glibc ──────────────────
#
# CLI（--no-default-features）不编译任何桌面代码：纯 Rust + ring(C)，静态
# musl 单文件，任何发行版（含 Alpine）直接跑。
#
# Linux UI 必须用 **GNU/glibc 动态** 构建（MYS-2821）：GUI 走
# eframe→winit→`x11-dl`，窗口后端在运行时 `dlopen` libX11/libxkbcommon/libGL。
# musl 静态二进制不支持加载共享对象，窗口一定起不来：
#     Error: 窗口初始化失败: ... Failed to load one of xlib's shared libraries
# 同一份代码的 glibc 动态构建能正常开窗。因此 UI 的 Linux 三目标改由
# `tools/build-gnu-ui.sh` 在固定基线容器（Ubuntu 22.04 / glibc 2.35）里产出，
# 产物要求不超过 GLIBC_2.35 / GLIBCXX_3.4.30。
#
# 代价（已写入 docs/install.md）：GNU UI 需要 glibc ≥2.35 与 X11/GL/ XKB
# 运行库，**不能在 Alpine/musl 上直接运行**；CLI 不受影响。
#
# aarch64/armv7 的 **CLI** 用 musl.cc 真 musl 工具链（ring 的 C/asm 交叉编译
# 需要它）。工具链解压到 $HOME/.cache/muslcc-<arch>（musl.cc 发布，
# GCC 11.2.1 + musl 1.2.3）。x86_64 CLI 用系统 musl-gcc；Windows 用 mingw。
#
# Usage:  tools/build-dist.sh
# Env:    CACHE_DIR 覆盖 $HOME/.cache/shell-remote-dist
#         GNU_IMAGE 覆盖 UI 构建镜像（默认 ubuntu:22.04）
#         SKIP_GNU_UI=1 复用 dist-gnu/ 里已有的 GNU UI（仅调试用）
#
# Requires: rustup targets + toolchains + docker（Linux UI），见 docs/build.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${CACHE_DIR:-$HOME/.cache/shell-remote-dist}"
mkdir -p "$CACHE"
STUB_SRC="$ROOT/tools/glibc-cxx-stubs.c"
GNU_IMAGE="${GNU_IMAGE:-ubuntu:22.04}"
GNU_OUT="$ROOT/dist-gnu"

MUSLCC_AARCH64="${MUSLCC_AARCH64:-$HOME/.cache/muslcc-aarch64/bin/aarch64-linux-musl-}"
MUSLCC_ARMV7="${MUSLCC_ARMV7:-$HOME/.cache/muslcc-armv7/bin/arm-linux-musleabihf-}"
if [ ! -x "${MUSLCC_AARCH64}gcc" ] || [ ! -x "${MUSLCC_ARMV7}gcc" ]; then
  echo "ERROR: aarch64/armv7 CLI 需 musl.cc 工具链（ring 的 C 代码交叉编译）。
请下载并解压到 \$HOME/.cache/muslcc-{aarch64,armv7}
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

# ── 1. Linux CLI ×3（musl 静态）+ Windows CLI ───────────────────────
for entry in "${PLATFORMS[@]}"; do
  IFS='|' read -r target cc cxx ar dist_name <<<"$entry"
  echo "== $target =="
  dir="$CACHE/${dist_name%.exe}"
  mkdir -p "$dir/x"

  echo "-- $target: shell-remote (CLI, --no-default-features, musl/windows) --"
  env CC="$cc" RUSTFLAGS="-C link-arg=-L$dir" \
      cargo build --release --bin shell-remote --no-default-features \
      --target "$target" --manifest-path "$ROOT/Cargo.toml"
done

# ── 2. Windows UI（GNU/mingw，静态 C++，GUI subsystem + manifest）────
# Windows 不需要 dlopen 加载 glibc 共享库，保持原有静态形态不变。
echo "== x86_64-pc-windows-gnu: UI =="
# 独立的 Windows 静态库目录：绝不与 Linux x86_64 CLI 的缓存目录（同名
# `shell-remote-x86_64`）共用，否则 mingw 的 libstdc++.a 会把 musl 链接搞坏。
WIN_DIR="$CACHE/winui-x86_64"
mkdir -p "$WIN_DIR/x"
stdlib=$(x86_64-w64-mingw32-g++ -print-file-name=libstdc++.a)
echo "libstdc++: $stdlib (mingw)"
rm -f "$WIN_DIR/stub.o" "$WIN_DIR/libstdc++.a"
x86_64-w64-mingw32-gcc -O2 -c "$STUB_SRC" -o "$WIN_DIR/stub.o"
( cd "$WIN_DIR/x" && rm -f ./*.o 2>/dev/null; x86_64-w64-mingw32-ar x "$stdlib" )
x86_64-w64-mingw32-ar rc "$WIN_DIR/libstdc++.a" "$WIN_DIR"/x/*.o "$WIN_DIR/stub.o"

# Windows agent 嵌入 requireAdministrator manifest: 让 SendInput 能操作
# 提权窗口/多数弹窗(360 等)。UAC 安全桌面本身仍需服务级组件(后续)。
# 仅 UI 二进制需要（桌面键鼠注入）；CLI 二进制不嵌入。
echo "== embedding windows manifest (UI only) =="
x86_64-w64-mingw32-windres -I "$ROOT/windows" "$ROOT/windows/embed-manifest.rc" \
  -O coff -o "$CACHE/agent_manifest.o"
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
WIN_STD="$WIN_DIR"
RUSTFLAGS="-C link-args=$CACHE/agent_manifest.o -C target-feature=+crt-static \
-C link-arg=-L$WIN_STD -C link-arg=-static-libstdc++" \
  env "${WIN_LIBXAOM[@]}" \
  cargo build --release --bin shell-remote-ui \
  --target x86_64-pc-windows-gnu --manifest-path "$ROOT/Cargo.toml"

# ── 3. Linux UI ×3（GNU/glibc 动态，固定基线容器）──────────────────
if [ "${SKIP_GNU_UI:-0}" = "1" ] && [ -x "$GNU_OUT/shell-remote-ui-x86_64" ]; then
  echo "== Linux UI: 复用已有 $GNU_OUT（SKIP_GNU_UI=1）=="
else
  echo "== Linux UI: 用 $GNU_IMAGE 产出 GNU/glibc 构建（基线 glibc 2.35）=="
  IMAGE="$GNU_IMAGE" OUT_DIR="$GNU_OUT" CACHE_DIR="$CACHE" \
    bash "$ROOT/tools/build-gnu-ui.sh"
fi
# AV1 绝不能因为"没找到库"悄悄消失：三个 Linux UI 必须都能看到 AV1 编码器符号。
#
# 用 `grep -c` 而不是 `grep -q`：`grep -q` 命中即退出，会让上游 `strings`
# 收到 SIGPIPE，在 `set -o pipefail` 下整条管道判失败——看起来像"没有 AV1"，
# 其实是管道被自己的提前退出打断了。
for a in x86_64 aarch64 armv7; do
  f="$GNU_OUT/shell-remote-ui-$a"
  [ -x "$f" ] || { echo "ERROR: 缺少 Linux UI 制品 $f（GNU UI 构建未完成）" >&2; exit 4; }
  hits="$(strings -a "$f" | grep -c 'aom_codec_av1_cx' || true)"
  if [ "${hits:-0}" -lt 1 ]; then
    echo "ERROR: $f 里没有 AV1 编码器符号（aom_codec_av1_cx）——拒绝静默降级" >&2
    exit 4
  fi
done
echo "Linux UI AV1 检查通过（3/3 含 aom_codec_av1_cx）"

echo "== staging releases =="
mkdir -p "$ROOT/dist"
# 逐个拷贝；某目标正被使用(如本地 relay 跑着 dist 二进制)时 cp 会 ETXTBSY,
# 不应中止其余目标(该目标可稍后停进程再补拷)。
stage() { cp "$1" "$2" && echo "staged $2" || { echo "WARN: 未能覆盖 $2 (可能仍被运行中的进程占用)"; failed=1; }; }
failed=0
# CLI 二进制（musl 静态 ×3 + Windows）
stage "$ROOT"/target/x86_64-unknown-linux-musl/release/shell-remote \
      "$ROOT/dist/shell-remote-x86_64"
stage "$ROOT"/target/aarch64-unknown-linux-musl/release/shell-remote \
      "$ROOT/dist/shell-remote-aarch64"
stage "$ROOT"/target/armv7-unknown-linux-musleabihf/release/shell-remote \
      "$ROOT/dist/shell-remote-armv7"
stage "$ROOT"/target/x86_64-pc-windows-gnu/release/shell-remote.exe \
      "$ROOT/dist/shell-remote-x86_64.exe"
# UI 二进制：Linux 三件来自 GNU 构建，Windows 来自 mingw
stage "$GNU_OUT/shell-remote-ui-x86_64"  "$ROOT/dist/shell-remote-ui-x86_64"
stage "$GNU_OUT/shell-remote-ui-aarch64" "$ROOT/dist/shell-remote-ui-aarch64"
stage "$GNU_OUT/shell-remote-ui-armv7"   "$ROOT/dist/shell-remote-ui-armv7"
stage "$ROOT"/target/x86_64-pc-windows-gnu/release/shell-remote-ui.exe \
      "$ROOT/dist/shell-remote-ui-x86_64.exe"

# 清理上一版遗留的 musl UI 构建产物：它们已不是发布形态，留着容易被误拷。
rm -rf "$ROOT"/target/x86_64-unknown-linux-musl/release/shell-remote-ui \
       "$ROOT"/target/aarch64-unknown-linux-musl/release/shell-remote-ui \
       "$ROOT"/target/armv7-unknown-linux-musleabihf/release/shell-remote-ui 2>/dev/null || true

echo "Built:"
ls -lh "$ROOT/dist/"
file "$ROOT"/dist/shell-remote*
