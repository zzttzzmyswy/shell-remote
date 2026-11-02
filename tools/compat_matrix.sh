#!/bin/bash
# 跨版本兼容矩阵（MYS-2553 兼容 1b / MYS-2562）。
#
# 用**真实的已发布旧二进制**验证两个方向：
#   A = 新 relay + 旧 agent
#   B = 旧 relay + 新 agent
#
# 每个组合检查 6 项：agent 注册、MCP 执行（含 UTF-8）、文件传输（64KB 往返
# sha256 一致）、网页终端（SSE 输入 → terminal:output 回显）、relay 重启后
# agent 在 60s 内重新注册且 MCP 再次可用、收尾无残留进程。
#
# 用法:
#   tools/compat_matrix.sh [--new <新二进制>] [--versions "<版本列表>"]
#
#   --new       默认 target/release/shell-remote（不存在则提示先 cargo build）
#   --versions  默认 "0.53.0 0.59.4 0.59.8 latest"；latest = GitHub 最新 release，
#               当它等于新二进制的版本时自动跳过（那样两个方向都退化成同版本）
#
# 退出码：全部 PASS/GAP 为 0，任意一项 FAIL 为 1。
# 详见 docs/compat-matrix.md。
set -uo pipefail

REPO="${SR_COMPAT_REPO:-zzttzzmyswy/shell-remote}"
ASSET="shell-remote-x86_64"
MIRROR_PREFIX="https://gh-proxy.com/"
DEFAULT_VERSIONS="0.53.0 0.59.4 0.59.8 latest"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROBE="$SCRIPT_DIR/compat/sr_probe.py"
GAPS_FILE="$SCRIPT_DIR/compat/known-gaps.txt"

NEW_BIN="target/release/shell-remote"
VERSIONS="$DEFAULT_VERSIONS"

while [ $# -gt 0 ]; do
  case "$1" in
    --new)
      [ $# -ge 2 ] || { echo "--new 需要一个路径" >&2; exit 2; }
      NEW_BIN="$2"; shift 2 ;;
    --versions)
      [ $# -ge 2 ] || { echo "--versions 需要一个版本列表" >&2; exit 2; }
      VERSIONS="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *)
      echo "未知参数: $1（-h 看用法）" >&2; exit 2 ;;
  esac
done

RUN_START=$(date +%s)
CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/shell-remote-compat"
WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/sr-compat.XXXXXX")"
RESULTS="$WORK_ROOT/results.tsv"
REPORT="$PWD/compat-report.md"
: > "$RESULTS"

CHECK_NAMES=(agent_register mcp_exec file_transfer web_terminal reconnect cleanup)
CHECK_TITLES=(注册 MCP 文件 网页终端 重连 清理)

# 本脚本起过的进程（清理与残留检查都只针对这些）
LIVE_PIDS=()
RELAY_PID=""
AGENT_PID=""

log()  { printf '%s\n' "$*"; }
warn() { printf '⚠ %s\n' "$*" >&2; }

# trap 里间接调用，shellcheck 看不到调用点。
# shellcheck disable=SC2329
cleanup_all() {
  local p
  for p in ${LIVE_PIDS[@]+"${LIVE_PIDS[@]}"}; do
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
  done
  LIVE_PIDS=()
  RELAY_PID=""
  AGENT_PID=""
}

free_port() {
  python3 -c 'import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()'
}

# 子命令是否支持某参数：不同版本的 CLI 不同，一律先看 --help 再决定传不传。
has_opt() { # $1 二进制 $2 子命令 $3 参数名
  "$1" "$2" --help 2>&1 | grep -q -- "$3"
}

relay_running() { # $1 base url
  curl -s -o /dev/null --max-time 2 "$1/" 2>/dev/null
}

record() { # $1 版本 $2 组合 $3 检查项 $4 PASS|FAIL|GAP $5 说明
  printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" >> "$RESULTS"
}

gap_reason() { # $1 版本 $2 组合 $3 检查项 —— 命中返回原因（stdout），否则空
  [ -f "$GAPS_FILE" ] || return 0
  local ver="$1" combo="$2" check="$3"
  local g_ver g_combo g_check g_reason
  while read -r g_ver g_combo g_check g_reason; do
    case "$g_ver" in ''|'#'*) continue ;; esac
    [ "$g_ver" = "$ver" ] || continue
    { [ "$g_combo" = "$combo" ] || [ "$g_combo" = "both" ] || [ "$g_combo" = "*" ]; } || continue
    { [ "$g_check" = "$check" ] || [ "$g_check" = "*" ]; } || continue
    printf '%s' "${g_reason:-未注明原因}"
    return 0
  done < "$GAPS_FILE"
}

# ── 旧二进制获取 ────────────────────────────────────────────────────────────

fetch_old_binary() { # $1 版本（形如 0.59.8）→ stdout 打印路径
  local ver="$1"
  local dir="$CACHE_ROOT/v$ver"
  local bin="$CACHE_ROOT/v$ver/$ASSET"
  local url="https://github.com/$REPO/releases/download/v$ver/$ASSET"
  local digest tmp actual

  if [ -x "$bin" ]; then
    printf '%s' "$bin"
    return 0
  fi
  mkdir -p "$dir"

  # GitHub release API 的 digest 是唯一的官方校验来源；没有就只能记录「未校验」。
  digest="$(gh release view "v$ver" --repo "$REPO" --json assets \
    --jq ".assets[] | select(.name == \"$ASSET\") | .digest" 2>/dev/null \
    | sed -n 's/^sha256://p')"

  tmp="$dir/.$ASSET.part"
  if ! curl -fsSL --connect-timeout 20 --max-time 600 -o "$tmp" "$url" 2>/dev/null; then
    warn "github.com 直连失败，改走镜像 $MIRROR_PREFIX"
    if ! curl -fsSL --connect-timeout 20 --max-time 600 -o "$tmp" \
        "${MIRROR_PREFIX}${url}" 2>/dev/null; then
      rm -f "$tmp"
      warn "下载 v$ver 失败：$url"
      return 1
    fi
  fi

  actual="$(sha256sum "$tmp" | awk '{print $1}')"
  if [ -n "$digest" ]; then
    if [ "$actual" != "$digest" ]; then
      rm -f "$tmp"
      warn "v$ver 校验失败：期望 $digest，实得 $actual"
      return 1
    fi
    printf '%s\t校验通过\n' "$actual" > "$dir/$ASSET.sha256"
  else
    warn "v$ver 的 release 资产没有 digest，SHA256 未校验（报告里会标出）"
    printf '%s\t未校验\n' "$actual" > "$dir/$ASSET.sha256"
  fi

  chmod +x "$tmp"
  mv "$tmp" "$bin"
  printf '%s' "$bin"
}

# ── 单个组合 ────────────────────────────────────────────────────────────────

# 全局：DIR VER COMBO PORT BASE PW KEY SID RELAY_BIN AGENT_BIN RELAY_ARGS AGENT_ARGS
start_relay() { # $1 日志文件
  env HOME="$DIR/relay-home" "$RELAY_BIN" "${RELAY_ARGS[@]}" > "$1" 2>&1 &
  RELAY_PID=$!
  LIVE_PIDS+=("$RELAY_PID")
  local n
  for n in $(seq 1 40); do
    if relay_running "$BASE"; then return 0; fi
    sleep 0.25
  done
  : "$n"
  warn "relay 未在 10s 内就绪（$1）"
  return 1
}

start_agent() { # $1 日志文件
  env HOME="$DIR/agent-home" "$AGENT_BIN" "${AGENT_ARGS[@]}" > "$1" 2>&1 &
  AGENT_PID=$!
  LIVE_PIDS+=("$AGENT_PID")
}

stop_pid() { # $1 pid
  local p="$1" n
  [ -n "$p" ] || return 0
  kill "$p" 2>/dev/null
  for n in $(seq 1 40); do
    kill -0 "$p" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -9 "$p" 2>/dev/null
  sleep 0.2
}

probe() { python3 "$PROBE" "$@"; }

chk_agent_register() {
  probe wait-sse --base "$BASE" --auth "$PW" --token "$KEY" --timeout 45 >/dev/null 2>&1 || return 1
  grep -q "session created" "$DIR/relay-1.log" 2>/dev/null || return 1
  grep -q "agent session established" "$DIR/agent-1.log" 2>/dev/null || return 1
  # relay 侧权威视角：admin overview 里该会话 online=true（该版本没有面板时
  # 上面两条日志断言已经是充分证据）。
  if [ "${ADMIN_ENABLED:-0}" = "1" ]; then
    probe session-online --base "$BASE" --token "$KEY" --sid "$SID" \
      --admin-path "$ADMIN_PATH" --admin-user "$ADMIN_USER" --admin-pass "$ADMIN_PASS" \
      >/dev/null 2>&1 || return 1
  fi
  return 0
}

chk_mcp_exec() {
  local out
  out="$(probe mcp-call --base "$BASE" --auth "$PW" --token "$KEY" \
    --cmd "echo $MCP_EXPECT" --expect "$MCP_EXPECT" --timeout 30 2>"$DIR/mcp.err")" || return 1
  [ "$out" = "$MCP_EXPECT" ] || return 1
  return 0
}

chk_file_transfer() {
  local src="$DIR/up-64k.bin" dst="$DIR/down-64k.bin" remote="$DIR/xfer"
  local put get
  head -c 65536 /dev/urandom > "$src"
  put="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 30 -X PUT \
    --data-binary "@$src" -H "X-SR-Token: $KEY" \
    "$BASE/agent/mcp/put?path=$remote/up-64k.bin" 2>"$DIR/xfer.err")" || return 1
  [ "$put" = "200" ] || { echo "PUT HTTP $put" >> "$DIR/xfer.err"; return 1; }
  get="$(curl -sS -o "$dst" -w '%{http_code}' --max-time 30 -H "X-SR-Token: $KEY" \
    "$BASE/agent/mcp/get?path=$remote/up-64k.bin" 2>>"$DIR/xfer.err")" || return 1
  [ "$get" = "200" ] || { echo "GET HTTP $get" >> "$DIR/xfer.err"; return 1; }
  [ -f "$dst" ] || return 1
  [ "$(sha256sum "$src" | awk '{print $1}')" = "$(sha256sum "$dst" | awk '{print $1}')" ]
}

chk_web_terminal() {
  probe term-echo --base "$BASE" --auth "$PW" --token "$KEY" \
    --marker "$TERM_MARKER" --timeout 40 >/dev/null 2>"$DIR/term.err"
}

chk_reconnect() {
  local t0 deadline
  stop_pid "$RELAY_PID"
  # 同端口同参数重启：旧 agent 的退避上限是 60s（R5#9），这里给 60s 窗口。
  t0="$(date +%s)"
  start_relay "$DIR/relay-2.log" || return 1
  deadline=$(( t0 + 60 ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if probe wait-sse --base "$BASE" --auth "$PW" --token "$KEY" --timeout 3 >/dev/null 2>&1; then
      printf '重连耗时约 %ss\n' "$(( $(date +%s) - t0 ))" > "$DIR/reconnect.note"
      chk_mcp_exec
      return $?
    fi
    sleep 1
  done
  printf '60s 内未重新注册\n' > "$DIR/reconnect.note"
  return 1
}

chk_cleanup() {
  stop_pid "$AGENT_PID"
  stop_pid "$RELAY_PID"
  AGENT_PID=""
  RELAY_PID=""
  sleep 0.3
  # 只查本组合自己的进程：唯一 session id + 唯一端口。
  local left left_relay
  left="$(pgrep -af -- "--session-id $SID" 2>/dev/null | grep -v pgrep || true)"
  left_relay="$(pgrep -af -- "relay --bind 127.0.0.1:$PORT" 2>/dev/null | grep -v pgrep || true)"
  if [ -n "$left" ] || [ -n "$left_relay" ]; then
    printf '残留 agent: %s\n残留 relay: %s\n' \
      "${left:-无}" "${left_relay:-无}" > "$DIR/leftover.txt"
    return 1
  fi
  return 0
}

run_check() { # $1 版本 $2 组合 $3 检查项名
  local ver="$1" combo="$2" name="$3" reason status detail=""
  # 显式分发（而不是 $4 间接调用）：shellcheck 能看见每个检查函数的引用。
  case "$name" in
    agent_register) chk_agent_register ;;
    mcp_exec)       chk_mcp_exec ;;
    file_transfer)  chk_file_transfer ;;
    web_terminal)   chk_web_terminal ;;
    reconnect)      chk_reconnect ;;
    cleanup)        chk_cleanup ;;
    *) warn "未知检查项 $name"; return 1 ;;
  esac
  status=$?
  if [ "$status" -eq 0 ]; then
    record "$ver" "$combo" "$name" PASS ""
    log "    $(printf '%-14s %s' "$name" PASS)"
    return 0
  fi
  reason="$(gap_reason "$ver" "$combo" "$name")"
  if [ -n "$reason" ]; then
    record "$ver" "$combo" "$name" GAP "$reason"
    log "    $(printf '%-14s %s' "$name" GAP)  （$reason）"
  else
    detail="见 $DIR"
    record "$ver" "$combo" "$name" FAIL "$detail"
    log "    $(printf '%-14s %s' "$name" FAIL)  （$detail）"
  fi
  return 0
}

run_combo() { # $1 版本 $2 组合 A|B $3 relay 二进制 $4 agent 二进制
  local ver="$1" combo="$2"
  RELAY_BIN="$3"
  AGENT_BIN="$4"
  PORT="$(free_port)"
  BASE="http://127.0.0.1:$PORT"
  PW="compat-pw"
  KEY="compatkey"
  SID="cmp$(printf '%s' "$ver" | tr -cd '0-9')$(printf '%s' "$combo" | tr '[:upper:]' '[:lower:]')"
  MCP_EXPECT="compat-中文-ok"
  TERM_MARKER="term-ok"
  DIR="$WORK_ROOT/v$ver/$combo"
  mkdir -p "$DIR/relay-home" "$DIR/agent-home" "$DIR/xfer"

  # relay 参数：只传该版本 --help 里确实存在的开关。
  ADMIN_ENABLED=0
  ADMIN_PATH="/srcompat-admin"
  ADMIN_USER="admin"
  ADMIN_PASS="compat-admin"
  RELAY_ARGS=(relay --bind "127.0.0.1:$PORT" --auth "$PW")
  if has_opt "$RELAY_BIN" relay "--no-tls"; then
    RELAY_ARGS+=(--no-tls)
  fi
  if has_opt "$RELAY_BIN" relay "--admin-path" && has_opt "$RELAY_BIN" relay "--admin-pass"; then
    RELAY_ARGS+=(--admin-path "$ADMIN_PATH" --admin-pass "$ADMIN_PASS")
    ADMIN_ENABLED=1
  fi

  AGENT_ARGS=(agent --relay-url "$BASE" --key "$KEY" --root "$DIR")
  has_opt "$AGENT_BIN" agent "--session-id" && AGENT_ARGS+=(--session-id "$SID")
  has_opt "$AGENT_BIN" agent "--shell" && AGENT_ARGS+=(--shell /bin/sh)
  has_opt "$AGENT_BIN" agent "--relay-insecure" && AGENT_ARGS+=(--relay-insecure)

  log "  ── v$ver 组合 $combo（relay=$RELAY_BIN agent=$AGENT_BIN port=$PORT）"
  local t0
  t0="$(date +%s)"
  if ! start_relay "$DIR/relay-1.log"; then
    local c
    for c in "${CHECK_NAMES[@]}"; do
      record "$ver" "$combo" "$c" FAIL "relay 未能启动，见 $DIR/relay-1.log"
    done
    log "    relay 启动失败，本组合 6 项记 FAIL"
    return 0
  fi
  start_agent "$DIR/agent-1.log"
  sleep 2

  local c
  for c in "${CHECK_NAMES[@]}"; do
    run_check "$ver" "$combo" "$c"
  done

  log "    组合耗时 $(( $(date +%s) - t0 ))s"
}

# ── 前置检查 ────────────────────────────────────────────────────────────────

fail_preflight() {
  log "FAIL: $*"
  exit 1
}

trap cleanup_all EXIT INT TERM

[ -f "$PROBE" ]     || fail_preflight "缺少探针脚本 $PROBE"
[ -f "$GAPS_FILE" ] || fail_preflight "缺少豁免清单 $GAPS_FILE"
command -v python3 >/dev/null 2>&1 || fail_preflight "缺少 python3"
command -v curl    >/dev/null 2>&1 || fail_preflight "缺少 curl"
command -v sha256sum >/dev/null 2>&1 || fail_preflight "缺少 sha256sum"
command -v gh      >/dev/null 2>&1 || fail_preflight "缺少 gh（需要它读 release digest）"

if [ ! -x "$NEW_BIN" ]; then
  fail_preflight "新二进制不存在或不可执行：$NEW_BIN（先 cargo build --release）"
fi
NEW_VERSION="$("$NEW_BIN" --version 2>/dev/null | awk '{print $NF}')"
[ -n "$NEW_VERSION" ] || fail_preflight "无法从 $NEW_BIN --version 取得版本号"

# latest → 具体版本号；等于新二进制版本时跳过（两方向退化成同版本，没有意义）。
RESOLVED=()
SKIPPED=()
for v in $VERSIONS; do
  if [ "$v" = "latest" ]; then
    v="$(gh release view --repo "$REPO" --json tagName --jq '.tagName' 2>/dev/null \
      | sed 's/^v//')"
    if [ -z "$v" ]; then
      warn "取不到 GitHub 最新 release，跳过 latest"
      continue
    fi
  fi
  if [ "$v" = "$NEW_VERSION" ]; then
    SKIPPED+=("$v")
    continue
  fi
  RESOLVED+=("$v")
done

log "跨版本兼容矩阵：新二进制 v$NEW_VERSION（$NEW_BIN）"
log "工作目录：$WORK_ROOT"
if [ ${#SKIPPED[@]} -gt 0 ]; then
  log "跳过（与新二进制同版本）：${SKIPPED[*]}"
fi
[ ${#RESOLVED[@]} -gt 0 ] || fail_preflight "没有可测的旧版本"

for ver in "${RESOLVED[@]}"; do
  log ""
  log "正在准备 v$ver 旧二进制 …"
  if ! old_bin="$(fetch_old_binary "$ver")"; then
    for combo in A B; do
      for c in "${CHECK_NAMES[@]}"; do
        record "$ver" "$combo" "$c" FAIL "旧二进制下载/校验失败"
      done
    done
    continue
  fi
  log "  旧二进制：$old_bin（sha256 $(awk -F'\t' '{print $1" "$2}' "$CACHE_ROOT/v$ver/$ASSET.sha256" 2>/dev/null)）"
  # A = 新 relay + 旧 agent；B = 旧 relay + 新 agent
  run_combo "$ver" A "$NEW_BIN" "$old_bin"
  run_combo "$ver" B "$old_bin" "$NEW_BIN"
done

# ── 矩阵渲染 + 报告 ─────────────────────────────────────────────────────────

status_of() { # $1 版本 $2 组合 $3 检查项
  awk -F'\t' -v v="$1" -v c="$2" -v k="$3" \
    '$1==v && $2==c && $3==k {print $4}' "$RESULTS" | tail -1
}

n_pass=0
n_fail=0
n_gap=0
while IFS=$'\t' read -r _v _c _k st _d; do
  case "$st" in
    PASS) n_pass=$((n_pass+1)) ;;
    FAIL) n_fail=$((n_fail+1)) ;;
    GAP)  n_gap=$((n_gap+1)) ;;
  esac
done < "$RESULTS"

ELAPSED=$(( $(date +%s) - RUN_START ))

print_table() {
  local sep=""
  printf '%-10s %-4s' "版本" "组合"
  local t
  for t in "${CHECK_TITLES[@]}"; do
    printf ' %-6s' "$t"
  done
  printf '\n'
  for ver in "${RESOLVED[@]}"; do
    for combo in A B; do
      printf '%-10s %-4s' "v$ver" "$combo"
      local c
      for c in "${CHECK_NAMES[@]}"; do
        local s
        s="$(status_of "$ver" "$combo" "$c")"
        printf ' %-6s' "${s:-SKIP}"
      done
      printf '\n'
    done
  done
  : "$sep"
}

log ""
log "════ 兼容矩阵 ════"
print_table
log ""
log "PASS=$n_pass FAIL=$n_fail GAP=$n_gap  总耗时 ${ELAPSED}s"
log "日志与临时文件：$WORK_ROOT"

# markdown 报告
{
  echo "# 跨版本兼容矩阵报告"
  echo
  echo "- 生成时间：$(date '+%Y-%m-%d %H:%M:%S %Z')"
  echo "- 仓库：\`$REPO\`"
  echo "- 新二进制：\`$NEW_BIN\`（v$NEW_VERSION）"
  echo "- 测试版本：${RESOLVED[*]:-无}"
  if [ ${#SKIPPED[@]} -gt 0 ]; then
    echo "- 跳过（与新二进制同版本）：${SKIPPED[*]}"
  fi
  echo "- 结果：PASS=$n_pass FAIL=$n_fail GAP=$n_gap"
  echo "- 总耗时：${ELAPSED}s"
  echo "- 日志目录：\`$WORK_ROOT\`"
  echo
  echo "A = 新 relay + 旧 agent；B = 旧 relay + 新 agent。"
  echo
  printf '| 版本 | 组合 |'
  for t in "${CHECK_TITLES[@]}"; do printf ' %s |' "$t"; done
  printf '\n|------|------|'
  for t in "${CHECK_TITLES[@]}"; do printf '%s' '------|'; done
  printf '\n'
  for ver in "${RESOLVED[@]}"; do
    for combo in A B; do
      printf '| v%s | %s |' "$ver" "$combo"
      for c in "${CHECK_NAMES[@]}"; do
        printf ' %s |' "$(status_of "$ver" "$combo" "$c" | sed 's/^$/SKIP/')"
      done
      printf '\n'
    done
  done
  echo
  echo "## 豁免（GAP）"
  echo
  if awk -F'\t' '$4=="GAP"' "$RESULTS" | grep -q .; then
    echo "| 版本 | 组合 | 检查项 | 原因 |"
    echo "|------|------|--------|------|"
    awk -F'\t' '$4=="GAP" {printf "| v%s | %s | %s | %s |\n", $1, $2, $3, $5}' "$RESULTS"
  else
    echo "无。"
  fi
  echo
  echo "## 失败（FAIL）"
  echo
  if awk -F'\t' '$4=="FAIL"' "$RESULTS" | grep -q .; then
    echo "| 版本 | 组合 | 检查项 | 说明 |"
    echo "|------|------|--------|------|"
    awk -F'\t' '$4=="FAIL" {printf "| v%s | %s | %s | %s |\n", $1, $2, $3, $5}' "$RESULTS"
  else
    echo "无。"
  fi
  echo
  echo "## 各版本二进制来源"
  echo
  echo "| 版本 | SHA256 | 校验 |"
  echo "|------|--------|------|"
  for ver in "${RESOLVED[@]}"; do
    digest="未下载"
    mark="-"
    if [ -f "$CACHE_ROOT/v$ver/$ASSET.sha256" ]; then
      digest="$(awk -F'\t' '{print $1}' "$CACHE_ROOT/v$ver/$ASSET.sha256")"
      mark="$(awk -F'\t' '{print $2}' "$CACHE_ROOT/v$ver/$ASSET.sha256")"
    fi
    echo "| v$ver | \`$digest\` | $mark |"
  done
} > "$REPORT"

log "报告：$REPORT"

if [ "$n_fail" -gt 0 ]; then
  log "有 FAIL：日志在 $WORK_ROOT"
  exit 1
fi
exit 0
