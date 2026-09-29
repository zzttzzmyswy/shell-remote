#!/bin/bash
# 多 agent 高频并发注册冒烟（连接/会话改动的发布前回归门槛）。
#
# 场景（均断言 admin overview agent_online 回到 N，且无 panic）：
#   1. 冷启动：N 个 agent 同时注册
#   2. relay kill -9 → 重启：N 个 agent 同时重连（惊群 + 抖动退避）
#   3. 高频抖动：ROUNDS 轮，每轮随机 kill -9 并立即重启 FLAP 个 agent
#      （同 --key/--session-id 续接）
#
# 用法: tools/concurrent_register_smoke.sh
# 环境: BIN=./target/debug/shell-remote N=40 FLAP=10 ROUNDS=5 PORT=3111
set -u
BIN="${BIN:-./target/debug/shell-remote}"
N="${N:-40}"
FLAP="${FLAP:-10}"
ROUNDS="${ROUNDS:-5}"
PORT="${PORT:-3111}"
BASE="http://127.0.0.1:${PORT}"
ADMIN_PATH=/sr-admin-crs
WORKDIR="$(mktemp -d)"
RELAY_PID=""
declare -a APIDS

cleanup() {
  for p in "${APIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  trap - EXIT
}
trap cleanup EXIT

start_relay() {
  "$BIN" relay --bind "127.0.0.1:${PORT}" --auth crs-pass --no-tls \
    --admin-path "$ADMIN_PATH" --admin-user admin --admin-pass crspass \
    >> "$WORKDIR/relay.log" 2>&1 &
  RELAY_PID=$!
  for _ in $(seq 1 50); do curl -s -o /dev/null "$BASE/" && return 0; sleep 0.2; done
  return 1
}
start_agent() { # $1 = index
  "$BIN" agent --relay-url "$BASE" --key "crskey$1" --session-id "crsag$1" \
    --shell /bin/sh --root "$WORKDIR" >> "$WORKDIR/agent$1.log" 2>&1 &
  APIDS[$1]=$!
  disown "$!" 2>/dev/null
}
online() {
  curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" -H 'Content-Type: application/json' \
    -d '{"user":"admin","pass":"crspass"}' -o /dev/null 2>/dev/null
  curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}/api/overview" 2>/dev/null \
    | grep -o '"agent_online":[0-9]*' | grep -o '[0-9]*$'
}
wait_online() { # $1 expected, $2 timeout s → prints elapsed
  local t0=$SECONDS n=0
  while [ $((SECONDS - t0)) -lt "$2" ]; do
    n="$(online)"
    if [ "${n:-0}" -eq "$1" ] 2>/dev/null; then echo "$((SECONDS - t0))s"; return 0; fi
    sleep 1
  done
  echo "超时(在线=${n:-0}/$1)"; return 1
}

fail=0
check() { local name="$1"; shift; local r; r="$("$@")"; local rc=$?
  if [ $rc -eq 0 ]; then echo "  ✓ $name ($r)"; else echo "  ✗ $name ($r)"; fail=1; fi; }

[ -x "$BIN" ] || { echo "FAIL: 无二进制 $BIN"; exit 1; }
curl -s -o /dev/null "$BASE/" && { echo "FAIL: 端口 $PORT 被占用"; exit 1; }
echo "── 并发注册冒烟：N=$N FLAP=$FLAP ROUNDS=$ROUNDS（日志 $WORKDIR）──"

start_relay || { echo "FAIL: relay 未启动"; exit 1; }
for i in $(seq 1 "$N"); do start_agent "$i"; done
check "1. 冷启动 $N 个并发注册" wait_online "$N" 60

kill -9 "$RELAY_PID"; sleep 1
start_relay || { echo "FAIL: relay 重启失败"; exit 1; }
check "2. relay kill -9 重启后 $N 个同时重连" wait_online "$N" 120

for r in $(seq 1 "$ROUNDS"); do
  for i in $(shuf -i 1-"$N" -n "$FLAP"); do
    kill -9 "${APIDS[$i]}" 2>/dev/null
    start_agent "$i"
  done
  sleep 0.5
done
check "3. ${ROUNDS} 轮 x ${FLAP} 个 agent 高频 kill/重启后收敛" wait_online "$N" 90
# 稳定性保持：收敛后 20s 仍全部在线（捕获延迟出现的 429 / 重连级联）
sleep 20
check "4. 收敛后保持 20s 仍全部在线" wait_online "$N" 5

# 进程存活 + 无 panic
dead=0
for i in $(seq 1 "$N"); do kill -0 "${APIDS[$i]}" 2>/dev/null || dead=$((dead + 1)); done
panics=$(grep -l 'panicked' "$WORKDIR"/*.log 2>/dev/null | wc -l)
[ "$dead" -eq 0 ] && echo "  ✓ agent 进程全部存活" || { echo "  ✗ $dead 个 agent 进程退出"; fail=1; }
[ "$panics" -eq 0 ] && echo "  ✓ 无 panic" || { echo "  ✗ $panics 个日志含 panic"; fail=1; }
kill -0 "$RELAY_PID" 2>/dev/null && echo "  ✓ relay 存活" || { echo "  ✗ relay 已退出"; fail=1; }

[ $fail -eq 0 ] && echo "PASS" || echo "FAIL（日志保留在 $WORKDIR）"
exit $fail
