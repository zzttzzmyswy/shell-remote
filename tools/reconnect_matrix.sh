#!/bin/bash
# 重连矩阵验收（R5#40 批次验收 / R3 戊165）：进程级重连矩阵——不依赖浏览器。
#
# 破坏源 × N 轮，每轮破坏后验证会话/桌面自动恢复（绕过浏览器，用 relay
# admin overview 在线数 + agent 日志 "session established" 做验证点）：
#   agent<  — kill -9 agent → 重启同 --key/--session-id → register_existing 按
#             key 续接（#10/#23 崩溃重启续接）：overview 在线恢复
#   relay<  — kill relay → 同端口同配置重启 → agent 指数退避（1→…→60s 上限，
#             R5#9）自动重连（#18 发送失败即重连）：overview 在线恢复
#   flaps   — agent 连续 kill-重启（幂等替换 #10 + 退避上限不崩）：最终稳定
#
# 用法:
#   tools/reconnect_matrix.sh <N>     # N = 每破坏源轮数（默认 2）
# 环境变量:
#   BIN=./target/debug/shell-remote   BASE 由 PORT 推导
#   PORT=3101  AUTH=sr-smoke-pass  ADMIN_PATH=/sr-admin-test
#   ADMIN_USER=admin  ADMIN_PASS=srpass
#   KEY=rmx1  SID=rmx-sess1  CAPTURE=none
set -u
N="${1:-2}"
BIN="${BIN:-./target/debug/shell-remote}"
PORT="${PORT:-3101}"
BASE="${BASE:-http://127.0.0.1:${PORT}}"
ADMIN_PATH="${ADMIN_PATH:-/sr-admin-test}"
ADMIN_USER="${ADMIN_USER:-admin}"
ADMIN_PASS="${ADMIN_PASS:-srpass}"
AUTH="${AUTH:-sr-smoke-pass}"
KEY="${KEY:-rmx1}"
SID="${SID:-rmxsess1}"
CAPTURE="${CAPTURE:-none}"
WORKDIR="$(mktemp -d)"
RELAY_PID=""
AGENT_PID=""

cleanup() {
  [ -n "$AGENT_PID" ] && kill "$AGENT_PID" 2>/dev/null
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  rm -rf "$WORKDIR"
  trap - EXIT
}
trap cleanup EXIT

echo "── 重连矩阵（破坏源: agent/relay/flaps × ${N}，relay ${BASE}）──"

relay_cmd() {
  "$BIN" relay --bind "127.0.0.1:${PORT}" --auth "$AUTH" --no-tls \
    --admin-path "$ADMIN_PATH" --admin-user "$ADMIN_USER" --admin-pass "$ADMIN_PASS"
}
agent_cmd() {
  "$BIN" agent --relay-url "$BASE" --key "$KEY" --session-id "$SID" \
    --desktop-capture "$CAPTURE"
}

start_relay() { relay_cmd > "$WORKDIR/relay.log" 2>&1 & RELAY_PID=$!; }
start_agent() { agent_cmd > "$WORKDIR/agent.log" 2>&1 & AGENT_PID=$!; }

# admin cookie + overview 在线 agent 数（agent_online 顶层字段）
overview_online() {
  curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" \
    -H 'Content-Type: application/json' \
    -d "{\"user\":\"${ADMIN_USER}\",\"pass\":\"${ADMIN_PASS}\"}" -o /dev/null 2>/dev/null
  curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}/api/overview" 2>/dev/null \
    | grep -o '"agent_online":[0-9]*' | grep -o '[0-9]*$'
}

# 等待在线会话数 == 期望值，最多 timeout 秒。返回 0=达标。
wait_online() {
  local expected="$1" timeout="$2" t=0 n=0
  while [ "$t" -lt "$timeout" ]; do
    n="$(overview_online 2>/dev/null)"
    [ "${n:-0}" -eq "$expected" ] 2>/dev/null && return 0
    sleep 1; t=$((t+1))
  done
  echo "  [超时 ${timeout}s] 在线=${n:-0}（期望 $expected）"
  return 1
}

agent_registered() {
  grep -q "agent session established" "$WORKDIR/agent.log" 2>/dev/null
}

fail=0
run_case() { # $1 场景名；整个函数块期望 0=恢复
  if "$@"; then echo "  ✓ $1"; else echo "  ✗ $1"; fail=1; fi
}

# ── 预检：端口空闲 + 二进制存在 ──
if [ ! -x "$BIN" ]; then echo "FAIL: 二进制不存在 $BIN（先 cargo build）"; exit 1; fi
if curl -s -o /dev/null "$BASE/" 2>/dev/null; then echo "FAIL: 端口 ${PORT} 已被占用"; exit 1; fi

# ── 启动 relay + agent，baseline ──
start_relay
for i in $(seq 1 20); do curl -s -o /dev/null "$BASE/" 2>/dev/null && break; sleep 1; done
start_agent
if ! wait_online 1 30; then
  echo "FAIL: baseline 未就绪（relay/agent 启动失败，看 $WORKDIR/*.log）"
  exit 1
fi
agent_registered || { echo "FAIL: baseline agent 未注册"; exit 1; }
echo "  baseline: 在线=1（agent ${AGENT_PID} / relay ${RELAY_PID}）"

# ── 场景 1：agent 被杀 → 重启续接（#10/#23）──
echo "-- 场景 1: agent 崩溃重启（register_existing 续接）x${N}"
for i in $(seq 1 "$N"); do
  kill -9 "$AGENT_PID" 2>/dev/null; sleep 2
  start_agent
  run_case wait_online 1 30
  run_case agent_registered
done

# ── 场景 2：relay 重启 → agent 退避重连（#18/#9）──
echo "-- 场景 2: relay 重启（agent 自动重连）x${N}"
for i in $(seq 1 "$N"); do
  kill -9 "$RELAY_PID" 2>/dev/null; sleep 3
  start_relay
  for t in $(seq 1 20); do curl -s -o /dev/null "$BASE/" 2>/dev/null && break; sleep 1; done
  # 上限：agent 指数退避最高 60s（R5#9）+ 重注册，90s 兜底
  run_case wait_online 1 90
done

# ── 场景 3：连续 flap（幂等替换 + 退避不崩）──
echo "-- 场景 3: agent 连续 3 次 kill-重启（幂等替换）"
for i in 1 2 3; do
  kill -9 "$AGENT_PID" 2>/dev/null; sleep 1
  start_agent
  run_case wait_online 1 30
done

if [ "$fail" = 0 ]; then echo "── 重连矩阵完成（全过）──"; else echo "── 重连矩阵完成（有失败）──"; fi
exit "$fail"