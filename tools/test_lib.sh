#!/bin/bash
# 低空间编译测试入口（MYS-2836 阶段1）。
#
# 目的：把「日常跑测试」固定成一条可复现、空间可控、不会误用旧二进制的命令，
# 而不是靠每个人记住一串环境变量。
#
# 空间口径来自 Cargo.toml 的 [profile.dev]/[profile.test]：
#   本仓库 crate  line-tables-only  （保留 文件:行号，砍掉变量级 DWARF）
#   依赖          debug = false     （依赖内部调试信息日常用不到）
#   incremental   false             （约占基线 target 的 25%，按需用 --incremental 打开）
#
# 用法:
#   tools/test_lib.sh                       # 默认 feature（含 desktop/gui/av1/tui）
#   tools/test_lib.sh --no-default-features # lean（CLI 形态，不编译桌面依赖）
#   tools/test_lib.sh --full-debug          # 完整调试（变量级 DWARF，含依赖）
#   tools/test_lib.sh --incremental         # 打开增量编译（边改边编时用）
#   tools/test_lib.sh --no-run              # 只编译不运行（量测用）
#   tools/test_lib.sh --space               # 跑完打印 target 空间分项
#   tools/test_lib.sh -- --target-dir <B>   # 指定 target 目录（等价 CARGO_TARGET_DIR）
#   tools/test_lib.sh -- <额外 cargo 参数>  # 其余参数透传给 cargo test
#
# target 目录解析（预检 / 构建 / 统计三处永远同一个路径）:
#   --target-dir <B> / --target-dir=<B>  >  CARGO_TARGET_DIR  >  <repo>/target
#   前者与 cargo 自身的优先级一致。脚本会把它统一收敛成 CARGO_TARGET_DIR
#   再调 cargo，因此不存在"回显 A、实际写 B"。
#   不支持 --manifest-path（会改变 cargo 的 target 默认根，与本脚本的仓库绑定
#   假设冲突），显式拒绝而不是猜。
#
# 环境变量:
#   CARGO_TARGET_DIR   见上；被 --target-dir 覆盖时以命令行为准。
#   SR_XTEST_DISPLAY   传给 X11 桌面捕获测试的 display；未设置时测试回落到 :99。
#   SR_MIN_FREE_MB     开工前要求的最小可用空间（默认 4096）。不满足直接失败，
#                      而不是编到一半写爆磁盘。
#
# 退出码：0 = cargo test 全部通过；非 0 = 编译/测试失败（原样传播）或参数被拒。
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

die() { echo "FAIL: $*" >&2; exit 2; }

FEATURES_ARGS=()
PROFILE_ARGS=()
RUN=1
SPACE=0
FULL_DEBUG=0
EXTRA=()

while [ $# -gt 0 ]; do
  case "$1" in
    --no-default-features) FEATURES_ARGS+=(--no-default-features); shift ;;
    --full-debug)          PROFILE_ARGS+=(--profile test-full); FULL_DEBUG=1; shift ;;
    --incremental)         export CARGO_INCREMENTAL=1; shift ;;
    --no-run)              RUN=0; shift ;;
    --space)               SPACE=1; shift ;;
    -h|--help)             sed -n '2,33p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --)                    shift; EXTRA+=("$@"); break ;;
    *)                     EXTRA+=("$1"); shift ;;
  esac
done

# ── 透传参数归一化 ────────────────────────────────────────────────
# 目的：把会影响"产物落在哪个目录 / 按什么 profile 编"的参数从透传里摘出来，
# 由脚本自己解析成唯一口径，再喂给 cargo。
CLI_TARGET_DIR=""
CLI_TARGET_TRIPLE=""
CLI_PROFILE=""
CLI_FEATURES=""
CLI_ALL_FEATURES=0
PASSTHRU=()

i=0
n=${#EXTRA[@]}
while [ "$i" -lt "$n" ]; do
  a="${EXTRA[$i]}"
  case "$a" in
    --target-dir)
      i=$((i+1)); [ "$i" -lt "$n" ] || die "--target-dir 缺少参数"
      CLI_TARGET_DIR="${EXTRA[$i]}" ;;
    --target-dir=*)   CLI_TARGET_DIR="${a#--target-dir=}" ;;
    --target)
      i=$((i+1)); [ "$i" -lt "$n" ] || die "--target 缺少参数"
      CLI_TARGET_TRIPLE="${EXTRA[$i]}"; PASSTHRU+=("--target" "${EXTRA[$i]}") ;;
    --target=*)       CLI_TARGET_TRIPLE="${a#--target=}"; PASSTHRU+=("$a") ;;
    --profile)
      i=$((i+1)); [ "$i" -lt "$n" ] || die "--profile 缺少参数"
      CLI_PROFILE="${EXTRA[$i]}"; PASSTHRU+=("--profile" "${EXTRA[$i]}") ;;
    --profile=*)      CLI_PROFILE="${a#--profile=}"; PASSTHRU+=("$a") ;;
    --release)        CLI_PROFILE="release"; PASSTHRU+=("$a") ;;
    --all-features)   CLI_ALL_FEATURES=1; PASSTHRU+=("$a") ;;
    --features)
      i=$((i+1)); [ "$i" -lt "$n" ] || die "--features 缺少参数"
      CLI_FEATURES="${EXTRA[$i]}"; PASSTHRU+=("--features" "${EXTRA[$i]}") ;;
    --features=*)     CLI_FEATURES="${a#--features=}"; PASSTHRU+=("$a") ;;
    --manifest-path|--manifest-path=*)
      die "不支持 --manifest-path：它会改变 cargo 解析 target 根目录的基准，
      本脚本的预检/统计与 build-dist.sh 的取件路径都绑定当前仓库。
      需要别的 target 请用 --target-dir 显式指定。" ;;
    *)                PASSTHRU+=("$a") ;;
  esac
  i=$((i+1))
done

# --full-debug 与显式 --profile/--release 同时给会互相覆盖，直接拒绝。
if [ "$FULL_DEBUG" -eq 1 ] && [ -n "$CLI_PROFILE" ]; then
  die "--full-debug 与 --profile/--release 不能同时使用"
fi

# ── 有效 target 目录 / profile 的唯一解析 ────────────────────────
# 优先级与 cargo 一致：命令行 --target-dir > $CARGO_TARGET_DIR > 默认。
# 解析结果同时用于：① 空间预检 ② 传给 cargo ③ 事后统计。
if [ -n "$CLI_TARGET_DIR" ]; then
  TARGET_DIR="$CLI_TARGET_DIR"
elif [ -n "${CARGO_TARGET_DIR:-}" ]; then
  TARGET_DIR="$CARGO_TARGET_DIR"
else
  TARGET_DIR="$ROOT/target"
fi
# 相对路径按仓库根解析，与 cargo 的 cwd 一致；realpath -m 允许目录尚不存在。
TARGET_DIR="$(realpath -m -- "$TARGET_DIR")"

# cargo test 的默认 profile 是 test，产物落在 debug/ 下。
if [ -n "$CLI_PROFILE" ]; then
  EFF_PROFILE="$CLI_PROFILE"
elif [ ${#PROFILE_ARGS[@]} -gt 0 ]; then
  EFF_PROFILE="test-full"
else
  EFF_PROFILE="test"
fi
case "$EFF_PROFILE" in
  dev|test)       PROFILE_DIR="debug" ;;
  release|bench)  PROFILE_DIR="release" ;;
  *)              PROFILE_DIR="$EFF_PROFILE" ;;
esac
# 交叉编译时产物再下一层：<target>/<triple>/<profile>/
if [ -n "$CLI_TARGET_TRIPLE" ]; then
  OUT_ROOT="$TARGET_DIR/$CLI_TARGET_TRIPLE"
else
  OUT_ROOT="$TARGET_DIR"
fi

if [ "$CLI_ALL_FEATURES" -eq 1 ]; then
  FEATURE_DESC="all-features"
elif [ -n "$CLI_FEATURES" ]; then
  FEATURE_DESC="--features $CLI_FEATURES"
  [ ${#FEATURES_ARGS[@]} -gt 0 ] && FEATURE_DESC="$FEATURE_DESC --no-default-features"
elif [ ${#FEATURES_ARGS[@]} -gt 0 ]; then
  FEATURE_DESC="none (lean CLI)"
else
  FEATURE_DESC="default (desktop,av1,tui,gui)"
fi

MIN_FREE_MB="${SR_MIN_FREE_MB:-4096}"

# ── 开工前空间检查（对解析出的 TARGET_DIR 所在分区）────────────────
probe="$TARGET_DIR"
while [ ! -d "$probe" ] && [ "$probe" != "/" ]; do probe="$(dirname "$probe")"; done
free_mb=$(df -Pm "$probe" 2>/dev/null | awk 'NR==2 {print $4}')
if [ -n "${free_mb:-}" ] && [ "$free_mb" -lt "$MIN_FREE_MB" ]; then
  echo "FAIL: $probe 可用空间 ${free_mb}MB < 要求的 ${MIN_FREE_MB}MB" >&2
  echo "      目标目录: $TARGET_DIR" >&2
  echo "      先清理本任务自己的 target（不要删别的任务的），或调低 SR_MIN_FREE_MB" >&2
  exit 1
fi

# ── 构建口径回显：让"这次到底按什么配置编的、写到哪"可留证 ────────
echo "─── 低空间编译测试 ───"
echo "repo:        $ROOT"
echo "target_dir:  $TARGET_DIR"
echo "profile:     $EFF_PROFILE (产出 $PROFILE_DIR/)"
echo "features:    $FEATURE_DESC"
[ -n "$CLI_TARGET_TRIPLE" ] && echo "target:      $CLI_TARGET_TRIPLE"
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
[ ${#PASSTHRU[@]} -gt 0 ] && cargo_args+=("${PASSTHRU[@]}")

start=$(date +%s)
# --target-dir 已在上面收敛成 TARGET_DIR；这里只用环境变量传，避免两处口径并存。
CARGO_TARGET_DIR="$TARGET_DIR" cargo "${cargo_args[@]}"
rc=$?
elapsed=$(( $(date +%s) - start ))

# ── 事后一致性校验（对应 build_contract_test.sh 的 A12 思路）──────
# cargo 成功却没有任何产物落在我们解析出来的目录 → 说明解析与实际不一致，
# 绝不能静默按一个空目录报告"空间 0"。
if [ "$rc" -eq 0 ] && [ ! -d "$TARGET_DIR" ]; then
  echo "FAIL: cargo 成功，但解析出的 target 目录不存在: $TARGET_DIR" >&2
  echo "      说明目录解析与实际写入位置不一致，拒绝给出空间统计。" >&2
  exit 3
fi

echo
echo "─── 结果 ───"
echo "exit:     $rc"
echo "wall:     ${elapsed}s"
if [ -d "$OUT_ROOT" ]; then
  total_kb=$(du -sk "$OUT_ROOT" 2>/dev/null | awk '{print $1}')
  echo "target:   ${total_kb:-?} KiB ($(awk -v k="${total_kb:-0}" 'BEGIN{printf "%.2f", k/1048576}') GiB) [$OUT_ROOT]"
  if [ "$SPACE" -eq 1 ]; then
    echo "--- 分项 (du -sk，KiB) ---"
    du -sk "$OUT_ROOT"/* 2>/dev/null | sort -rn
    for sub in "$PROFILE_DIR/deps" "$PROFILE_DIR/incremental" "$PROFILE_DIR/build"; do
      [ -d "$OUT_ROOT/$sub" ] && echo "$(du -sk "$OUT_ROOT/$sub" | awk '{print $1}') $sub"
    done
  fi
fi

exit "$rc"
