#!/bin/bash
# 低空间编译测试入口（MYS-2836 阶段1）。
#
# 目的：把「日常跑测试」固定成一条可复现、空间可控、不会误用旧二进制的命令，
# 而不是靠每个人记住一串环境变量。
#
# 空间口径来自 Cargo.toml 的 [profile.dev]/[profile.test]：
#   本仓库 crate  line-tables-only  （保留 文件:行号，砍掉变量级 DWARF）
#   依赖          debug = false     （依赖内部调试信息日常用不到）
#   incremental   false             （约占 target 的 40%，按需用 --incremental 打开）
#
# 用法:
#   tools/test_lib.sh                       # 默认 feature（含 desktop/gui/av1/tui）
#   tools/test_lib.sh --no-default-features # lean（CLI 形态，不编译桌面依赖）
#   tools/test_lib.sh --full-debug          # 完整调试（变量级 DWARF，含依赖）
#   tools/test_lib.sh --incremental         # 打开增量编译（边改边编时用）
#   tools/test_lib.sh --no-run              # 只编译不运行（量测用）
#   tools/test_lib.sh --space               # 跑完打印 target 空间分项
#   tools/test_lib.sh -- <额外 cargo 参数>  # 透传给 cargo test
#
# 环境变量:
#   CARGO_TARGET_DIR   与 cargo 完全一致地生效（本脚本不覆盖它）。默认 <repo>/target。
#   SR_XTEST_DISPLAY   传给 X11 桌面捕获测试的 display；未设置时测试回落到 :99。
#   SR_MIN_FREE_MB     开工前要求的最小可用空间（默认 4096）。不满足直接失败，
#                      而不是编到一半写爆磁盘。
#
# 退出码：0 = cargo test 全部通过；非 0 = 编译或测试失败（原样传播）。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

FEATURES_ARGS=()
PROFILE_ARGS=()
RUN=1
SPACE=0
EXTRA=()

while [ $# -gt 0 ]; do
  case "$1" in
    --no-default-features) FEATURES_ARGS+=(--no-default-features) ;;
    --full-debug)          PROFILE_ARGS+=(--profile test-full) ;;
    --incremental)         export CARGO_INCREMENTAL=1 ;;
    --no-run)              RUN=0 ;;
    --space)               SPACE=1 ;;
    --)                    shift; EXTRA+=("$@"); break ;;
    -h|--help)             sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)                     EXTRA+=("$1") ;;
  esac
  shift
done

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
MIN_FREE_MB="${SR_MIN_FREE_MB:-4096}"

# ── 开工前空间检查 ────────────────────────────────────────────────
# 用 target 所在分区判断，而不是 /tmp；建不出来时按父目录回退。
probe="$TARGET_DIR"
while [ ! -d "$probe" ] && [ "$probe" != "/" ]; do probe="$(dirname "$probe")"; done
free_mb=$(df -Pm "$probe" 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${free_mb:-}" ] && [ "$free_mb" -lt "$MIN_FREE_MB" ]; then
  echo "FAIL: $probe 可用空间 ${free_mb}MB < 要求的 ${MIN_FREE_MB}MB" >&2
  echo "      先清理本任务自己的 target（不要删别的任务的），或调低 SR_MIN_FREE_MB" >&2
  exit 1
fi

# ── 构建口径回显：让"这次到底按什么配置编的"可留证 ──────────────
profile_name="test"
[ ${#PROFILE_ARGS[@]} -gt 0 ] && profile_name="test-full"
feature_desc="default (desktop,av1,tui,gui)"
[ ${#FEATURES_ARGS[@]} -gt 0 ] && feature_desc="none (lean CLI)"

echo "─── 低空间编译测试 ───"
echo "repo:        $ROOT"
echo "target_dir:  $TARGET_DIR"
echo "profile:     $profile_name"
echo "features:    $feature_desc"
echo "incremental: ${CARGO_INCREMENTAL:-0 (profile 默认关闭)}"
echo "toolchain:   $(cargo --version 2>/dev/null) / $(rustc --version 2>/dev/null)"
echo "free:        ${free_mb:-?} MB on $probe"
echo

# ── 编译 + 运行 ──────────────────────────────────────────────────
# 一律走 cargo 而不是直接执行 target 里的测试二进制：cargo 会按指纹判断
# 是否需要重编，从根上避免"旧二进制被当成新构建结果"。
cargo_args=(test --lib)
[ ${#PROFILE_ARGS[@]} -gt 0 ] && cargo_args+=("${PROFILE_ARGS[@]}")
[ ${#FEATURES_ARGS[@]} -gt 0 ] && cargo_args+=("${FEATURES_ARGS[@]}")
[ "$RUN" -eq 0 ] && cargo_args+=(--no-run)
[ ${#EXTRA[@]} -gt 0 ] && cargo_args+=("${EXTRA[@]}")

start=$(date +%s)
CARGO_TARGET_DIR="$TARGET_DIR" cargo "${cargo_args[@]}"
rc=$?
elapsed=$(( $(date +%s) - start ))

echo
echo "─── 结果 ───"
echo "exit:     $rc"
echo "wall:     ${elapsed}s"
if [ -d "$TARGET_DIR" ]; then
  total_kb=$(du -sk "$TARGET_DIR" 2>/dev/null | awk '{print $1}')
  echo "target:   ${total_kb:-?} KB ($(awk -v k="${total_kb:-0}" 'BEGIN{printf "%.2f", k/1048576}') GB)"
  if [ "$SPACE" -eq 1 ]; then
    echo "--- 分项 (du -sk) ---"
    du -sk "$TARGET_DIR"/* 2>/dev/null | sort -rn
    for sub in debug/deps debug/incremental debug/build; do
      [ -d "$TARGET_DIR/$sub" ] && echo "$(du -sk "$TARGET_DIR/$sub" | awk '{print $1}') $sub"
    done
  fi
fi

exit "$rc"
