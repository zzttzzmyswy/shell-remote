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
# 代价（已写入 docs/build.md / docs/install.md 相关说明）：GNU UI 需要
# glibc ≥2.35 与 X11/GL/XKB 运行库，**不能在 Alpine/musl 上直接运行**；
# CLI 不受影响。
#
# aarch64/armv7 的 **CLI** 用 musl.cc 真 musl 工具链（ring 的 C/asm 交叉编译
# 需要它）。工具链解压到 $HOME/.cache/muslcc-<arch>（musl.cc 发布，
# GCC 11.2.1 + musl 1.2.3）。x86_64 CLI 用系统 musl-gcc；Windows 用 mingw。
#
# ── 构建合同（MYS-2828）──────────────────────────────────────────────
#
# 1) 取件目录 = cargo 实际使用的目录。cargo 遵守 CARGO_TARGET_DIR，本脚本
#    就读同一个变量，绝不固定读 $ROOT/target 而从别处取旧文件。
# 2) 八件产物全部通过门禁（存在 / 版本 / 架构 / 链接形态 / AV1 / Windows
#    subsystem + manifest）后才会落到 dist/；任一失败即非零退出。
# 3) 落盘是"先整批复制到 staging 目录，再逐个 rename 到 dist/"。任何一步
#    失败都不会留下"新旧混合却看起来齐全"的 dist/（rename 也天然避开
#    正在运行的同名二进制 ETXTBSY）。
# 4) 不删除、不改动任何已有 target 目录内容（musl UI 的旧构建产物留着无妨，
#    取件路径只认 GNU UI 目录，且 UI 门禁要求动态链接——静态 musl UI 无法冒充）。
#
# Usage:  tools/build-dist.sh
# Env:    CACHE_DIR       覆盖 $HOME/.cache/shell-remote-dist（libaom / 工具链缓存）
#         DIST_DIR        覆盖输出目录（默认 <repo>/dist）
#         GNU_OUT_DIR     覆盖 Linux GUI 中间产物目录（默认 <repo>/dist-gnu）
#         CARGO_TARGET_DIR 覆盖 cargo target 目录（取件路径随之变化）
#         GNU_IMAGE       覆盖 UI 构建镜像（默认 ubuntu:22.04）
#         GNU_RUST_TOOLCHAIN 覆盖容器内固定的 Rust 版本（默认见 build-gnu-ui.sh）
#         WIN_LIBXAOM_DIR 覆盖 Windows 静态 libaom 目录
#         SKIP_GNU_UI=1   复用 GNU_OUT_DIR 里已有的 GNU UI（仅调试用；门禁照旧）
#
# Requires: rustup targets + toolchains + docker（Linux UI），见 docs/build.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="${CACHE_DIR:-$HOME/.cache/shell-remote-dist}"
mkdir -p "$CACHE"

DIST="${DIST_DIR:-$ROOT/dist}"
GNU_OUT="${GNU_OUT_DIR:-$ROOT/dist-gnu}"
GNU_IMAGE="${GNU_IMAGE:-ubuntu:22.04}"

# 有效 target 目录。cargo 的优先级就是：命令行 --target-dir > 环境变量
# CARGO_TARGET_DIR > <workspace>/target。本脚本不传 --target-dir，因此这里
# 只要与 CARGO_TARGET_DIR 对齐即可；**CLI/Windows 的取件路径全部由
# $TARGET_DIR 派生**——旧版固定读 $ROOT/target，一旦设置了 CARGO_TARGET_DIR
# 就会从另一个目录取旧文件冒充本次构建。
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
# GNU UI 单独一个 target 目录（target-gnu/，已在 .gitignore）：
#   1) 它的 cargo/rustc 是基线容器里的工具链，与宿主 musl/Windows 缓存混在
#      一起会互相失效重建；
#   2) 容器以 root 运行，若与宿主共用 target/ 会写出 root 所有的
#      .rustc_info.json 等文件，让宿主下一次构建直接 EACCES。
# GNU UI 的**取件目录是 $GNU_OUT**（build-gnu-ui.sh 在 ABI 门禁通过后发布），
# 不读 target，因此这里与 CARGO_TARGET_DIR 解耦不会带来"取旧文件"的风险。
GNU_TARGET_DIR="${GNU_CARGO_TARGET_DIR:-$ROOT/target-gnu}"

# 版本唯一源：Cargo.toml（与 tools/release.sh 一致，拒绝 tag/二进制漂移）。
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
if [ -z "$VERSION" ]; then
  echo "ERROR: 无法从 $ROOT/Cargo.toml 读出 version" >&2
  exit 2
fi
# 版本串匹配：前后不能紧邻数字或小数点，避免 "0.59.17" 命中 "0.59.170"。
VERSION_RE="(^|[^0-9.])${VERSION//./\\.}([^0-9.]|$)"

STUB_SRC="$ROOT/tools/glibc-cxx-stubs.c"

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

# Windows UI 的静态 libaom 必须在**开工前**确认（MYS-2828）：旧版在目录不存在
# 时静默跳过 LIBXAOM_DIR，build.rs 只打一条 warning，最后产出一个没有 AV1 的
# Windows UI，而末尾门禁只查 Linux 三件——等于允许静默降级发布。
WIN_AOM_DIR="${WIN_LIBXAOM_DIR:-$CACHE/libaom-x86_64-pc-windows-gnu}"
if [ ! -f "$WIN_AOM_DIR/lib/libaom.a" ]; then
  echo "ERROR: Windows UI 缺少静态 libaom：$WIN_AOM_DIR/lib/libaom.a 不存在。" >&2
  echo "  Windows UI 与 Linux UI 一样必须带 AV1，不允许产出无 AV1 的制品。" >&2
  echo "  先跑：CACHE_DIR=$CACHE tools/build-libaom.sh" >&2
  echo "  或用 WIN_LIBXAOM_DIR=<含 lib/libaom.a 的目录> 指向已有构建。" >&2
  exit 3
fi

if [ "${SKIP_GNU_UI:-0}" != "1" ] && ! command -v docker >/dev/null; then
  echo "ERROR: Linux UI 需要 docker（在 $GNU_IMAGE 基线容器内构建）。" >&2
  exit 2
fi

echo "== shell-remote $VERSION 发布构建 =="
echo "  源码:      $(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo '<非 git 工作树>')"
echo "  target:    $TARGET_DIR"
echo "  GNU target: $GNU_TARGET_DIR"
echo "  输出:      $DIST"
echo "  GNU 中间:  $GNU_OUT（镜像 $GNU_IMAGE）"
echo "  缓存:      $CACHE"
echo "  rustc:     $(rustc --version 2>/dev/null || echo '<缺失>')"
echo "  cargo:     $(cargo --version 2>/dev/null || echo '<缺失>')"

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
# 触碰 build.rs 强制重跑 build script: cargo 按 (package, RUSTFLAGS) 缓存
# build-script 输出, manifest 段 RUSTFLAGS 与主循环不同, LIBXAOM_DIR
# 的值相同时不会触发 rerun-if-env-changed, 复用无 aom 的旧输出 →
# 链接缺 -laom。
touch "$ROOT/build.rs"
# 静态 C++ 运行时: 不加 -static-libstdc++ 与 -L$dir 时, -lstdc++ 落到 mingw
# 的动态导入库 libstdc++-6.dll.a → exe 运行时报"找不到 libstdc++-6.dll"
# （用户实拍 MYS-886）。-static-libstdc++ + 打包的静态 libstdc++.a 双保险。
WIN_STD="$WIN_DIR"
RUSTFLAGS="-C link-args=$CACHE/agent_manifest.o -C target-feature=+crt-static \
-C link-arg=-L$WIN_STD -C link-arg=-static-libstdc++" \
  env LIBXAOM_DIR="$WIN_AOM_DIR" \
  cargo build --release --bin shell-remote-ui \
  --target x86_64-pc-windows-gnu --manifest-path "$ROOT/Cargo.toml"

# ── 3. Linux UI ×3（GNU/glibc 动态，固定基线容器）──────────────────
if [ "${SKIP_GNU_UI:-0}" = "1" ]; then
  echo "== Linux UI: 复用 $GNU_OUT（SKIP_GNU_UI=1，仍走完整门禁）=="
else
  echo "== Linux UI: 用 $GNU_IMAGE 产出 GNU/glibc 构建（基线 glibc 2.35）=="
  # build-gnu-ui.sh 自己负责：清掉本目标名的旧产物 → 逐目标构建 → ABI 门禁
  # → 只在成功时把新文件放进 OUT_DIR。它非零退出时本脚本立即中止。
  IMAGE="$GNU_IMAGE" OUT_DIR="$GNU_OUT" CACHE_DIR="$CACHE" \
    CARGO_TARGET_DIR="$GNU_TARGET_DIR" \
    bash "$ROOT/tools/build-gnu-ui.sh"
fi

# ── 4. 制品清单 ─────────────────────────────────────────────────────
# 每项：dist 文件名 | 来源绝对路径 | 类别(cli|ui) | 形态(static|dynamic|peconsole|pegui)
ARTIFACTS=(
  "shell-remote-x86_64|$TARGET_DIR/x86_64-unknown-linux-musl/release/shell-remote|cli|static"
  "shell-remote-aarch64|$TARGET_DIR/aarch64-unknown-linux-musl/release/shell-remote|cli|static"
  "shell-remote-armv7|$TARGET_DIR/armv7-unknown-linux-musleabihf/release/shell-remote|cli|static"
  "shell-remote-x86_64.exe|$TARGET_DIR/x86_64-pc-windows-gnu/release/shell-remote.exe|cli|peconsole"
  "shell-remote-ui-x86_64|$GNU_OUT/shell-remote-ui-x86_64|ui|dynamic"
  "shell-remote-ui-aarch64|$GNU_OUT/shell-remote-ui-aarch64|ui|dynamic"
  "shell-remote-ui-armv7|$GNU_OUT/shell-remote-ui-armv7|ui|dynamic"
  "shell-remote-ui-x86_64.exe|$TARGET_DIR/x86_64-pc-windows-gnu/release/shell-remote-ui.exe|ui|pegui"
)

# ── 5. 门禁（全部通过才允许落盘）────────────────────────────────────
gate_fail=0
gate_err() { echo "  ✗ $*" >&2; gate_fail=1; }

aom_hits() { strings -a "$1" 2>/dev/null | grep -c 'aom_codec_av1_cx' || true; }
ver_hits() { strings -a "$1" 2>/dev/null | grep -cE "$VERSION_RE" || true; }

check_elf() { # f arch static|dynamic label
  local f="$1" want_arch="$2" want_link="$3" label="$4" hdr phdr
  if ! hdr="$(readelf -h "$f" 2>/dev/null)"; then
    gate_err "$label 不是可解析的 ELF"; return
  fi
  # 全部用 bash 模式匹配 / heredoc，不走 `生成器 | grep -q`：
  # pipefail 下 grep -q 提前退出会给上游发 SIGPIPE，把"匹配上了"误判成失败。
  case "$want_arch" in
    x86_64)  [[ $hdr == *"Machine:"*"X86-64"* ]]  || gate_err "$label 架构错：应为 x86_64";;
    aarch64) [[ $hdr == *"Machine:"*"AArch64"* ]] || gate_err "$label 架构错：应为 aarch64";;
    armv7)   [[ $hdr == *"Machine:"*"ARM"* ]]     || gate_err "$label 架构错：应为 armv7";;
  esac
  phdr="$(readelf -l "$f" 2>/dev/null)"
  if [ "$want_link" = dynamic ]; then
    [[ $phdr == *INTERP* ]] \
      || gate_err "$label 不是动态链接（无 PT_INTERP）—— musl 静态 UI 混入？"
  else
    [[ $phdr == *INTERP* ]] && gate_err "$label 不是静态链接（含 PT_INTERP）"
  fi
  return 0
}

check_pe() { # f want(gui|console) label
  local f="$1" want="$2" label="$3" desc
  desc="$(file -b "$f" 2>/dev/null)"
  [[ $desc == *PE32+* ]] || { gate_err "$label 不是 PE32+：$desc"; return; }
  [[ $desc == *x86-64* ]] || gate_err "$label 架构错：应为 x86-64"
  case "$want" in
    gui)     [[ $desc == *"(GUI)"* ]]     || gate_err "$label subsystem 错：应为 GUI";;
    console) [[ $desc == *"(console)"* ]] || gate_err "$label subsystem 错：应为 console";;
  esac
  return 0
}

echo "== 制品门禁（版本 $VERSION / 架构 / 链接形态 / AV1）=="
for entry in "${ARTIFACTS[@]}"; do
  IFS='|' read -r name src kind form <<<"$entry"
  label="$name"
  if [ ! -f "$src" ]; then
    gate_err "$label 缺目标文件：$src（构建未产出，或取件目录不是本次 cargo 使用的 target）"
    continue
  fi
  if [ ! -s "$src" ]; then
    gate_err "$label 目标文件为空：$src"
    continue
  fi
  echo "-- $label ← $src"
  hits="$(ver_hits "$src")"
  [ "${hits:-0}" -ge 1 ] || gate_err "$label 版本串不含 $VERSION（实际产物版本不符）"

  case "$form" in
    static)    check_elf "$src" "$(case "$name" in *aarch64) echo aarch64;; *armv7) echo armv7;; *) echo x86_64;; esac)" static "$label";;
    dynamic)   check_elf "$src" "$(case "$name" in *aarch64) echo aarch64;; *armv7) echo armv7;; *) echo x86_64;; esac)" dynamic "$label";;
    peconsole) check_pe "$src" console "$label";;
    pegui)     check_pe "$src" gui "$label";;
  esac

  if [ "$kind" = ui ]; then
    # AV1 绝不能因为"没找到库"悄悄消失：四件 UI（含 Windows）都必须能看到
    # AV1 编码器符号。用 grep -c 而不是 grep -q：grep -q 命中即退出会让上游
    # strings 收到 SIGPIPE，在 pipefail 下整条管道判失败——看起来像"没有 AV1"，
    # 其实是管道被自己的提前退出打断了。
    a="$(aom_hits "$src")"
    [ "${a:-0}" -ge 1 ] || gate_err "$label 里没有 AV1 编码器符号（aom_codec_av1_cx）——拒绝静默降级"
  else
    a="$(aom_hits "$src")"
    [ "${a:-0}" -eq 0 ] || gate_err "$label 是 lean CLI，不应包含 AV1 编码器符号"
  fi

  if [ "$form" = pegui ]; then
    # 用 heredoc 而不是管道：`strings | grep -q` 在 pipefail 下会因 grep
    # 提前退出给 strings 发 SIGPIPE，把"确实有 manifest"误判成失败。
    grep -q 'requireAdministrator' <<<"$(strings -a "$src")" \
      || gate_err "$label 缺少 requireAdministrator manifest"
  fi
  printf '  ✓ %s：版本串命中 %s 次，AV1 符号 %s 处\n' "$label" "${hits:-0}" "${a:-0}"
done

if [ "$gate_fail" -ne 0 ]; then
  echo "ERROR: 制品门禁未通过，dist/ 未做任何改动。" >&2
  exit 4
fi

# ── 6. 落盘：整批 staging 成功后逐个 rename ─────────────────────────
mkdir -p "$DIST"
STAGE_DIR="$(mktemp -d "$DIST/.staging.XXXXXX")"
cleanup_stage() { [ -n "${STAGE_DIR:-}" ] && rm -rf "$STAGE_DIR"; }
trap cleanup_stage EXIT

echo "== staging（$STAGE_DIR）=="
staged=()
for entry in "${ARTIFACTS[@]}"; do
  IFS='|' read -r name src _ _ <<<"$entry"
  if cp -- "$src" "$STAGE_DIR/$name"; then
    chmod +x "$STAGE_DIR/$name"
    staged+=("$name")
    echo "  staged $name"
  else
    echo "ERROR: 复制失败：$src → $STAGE_DIR/$name" >&2
    echo "dist/ 保持原样（未做任何替换）。" >&2
    exit 5
  fi
done

# staging 齐全后才动 dist/：失败时 dist/ 仍是上一版，不会出现"新旧混合但看起来齐全"。
echo "== 落盘（rename 到 $DIST）=="
for name in "${staged[@]}"; do
  if ! mv -f "$STAGE_DIR/$name" "$DIST/$name"; then
    echo "ERROR: 无法写入 $DIST/$name" >&2
    exit 5
  fi
  echo "  installed $name"
done
rmdir "$STAGE_DIR" 2>/dev/null || true
trap - EXIT

# ── 7. 构建记录（可追溯：源码 / 工具链 / 来源 / 大小 / SHA256）──────
INFO="$DIST/BUILD-INFO.txt"
{
  echo "# shell-remote $VERSION 发布构建记录（tools/build-dist.sh 生成）"
  echo "source_commit: $(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "source_branch: $(git -C "$ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
  echo "built_at:      $(date -Iseconds)"
  echo "cargo_target:  $TARGET_DIR"
  echo "gnu_target:    $GNU_TARGET_DIR"
  echo "cache_dir:     $CACHE"
  echo "gnu_out:       $GNU_OUT"
  echo "gnu_image:     $GNU_IMAGE"
  echo "rustc:         $(rustc --version 2>/dev/null || echo unknown)"
  echo "cargo:         $(cargo --version 2>/dev/null || echo unknown)"
  echo "mingw_gcc:     $(x86_64-w64-mingw32-gcc --version 2>/dev/null | head -1 || echo unknown)"
  echo "musl_gcc:      $(musl-gcc --version 2>/dev/null | head -1 || echo unknown)"
  echo
  printf '%-30s %12s %-12s %s\n' artifact size form sha256
  for entry in "${ARTIFACTS[@]}"; do
    IFS='|' read -r name _ _ form <<<"$entry"
    f="$DIST/$name"
    printf '%-30s %12s %-12s %s\n' "$name" "$(stat -c%s "$f")" "$form" \
      "$(sha256sum "$f" | cut -d' ' -f1)"
  done
  echo
  echo "# 来源（与 dist/ 逐字节一致）"
  for entry in "${ARTIFACTS[@]}"; do
    IFS='|' read -r name src _ _ <<<"$entry"
    printf '%s <- %s\n' "$name" "$src"
  done
} > "$INFO"

echo
echo "Built:"
ls -lh "$DIST/"
file "$DIST"/shell-remote* | sed 's/, BuildID[^,]*//'
echo
echo "记录：$INFO"
