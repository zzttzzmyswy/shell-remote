#!/bin/bash
# IPv6 全链路验收（R5 #38 KCP/白名单/IPv6 的 IPv6 子集）：不依赖浏览器，
# relay bind [::1] + agent --relay-url http://[::1] + 模拟浏览器 desktop:start，
# 经 IPv6 走完整链路（HTTP / agent 注册 WS / 桌面控制命令 / 桌面视频数据回流
# KPI），断言 fps≥15（动态画面不降帧铁律在 IPv6 通道同样成立）。
#
# 用法: tools/ipv6_verify.sh
# 环境: DISPLAY_NUM=:98（需已起 Xvfb + bench_draw_quad 可编译）、BIN/PORT 同其它验收脚本
set -u
BIN="${BIN:-./target/debug/shell-remote}"
PORT="${PORT:-3117}"
BASE="${BASE:-http://[::1]:${PORT}}"
ADMIN_PATH="${ADMIN_PATH:-/sr-admin-test}"
ADMIN_USER="${ADMIN_USER:-admin}"
ADMIN_PASS="${ADMIN_PASS:-srpass}"
AUTH="${AUTH:-sr-smoke-pass}"
KEY="${KEY:-stk}"
SID="${SID:-v6sess1}"
DISPLAY_NUM="${DISPLAY_NUM:-:98}"
WORKDIR="$(mktemp -d)"
RELAY_PID=""; AGENT_PID=""; BQ_PID=""

cleanup() {
  [ -n "$AGENT_PID" ] && kill "$AGENT_PID" 2>/dev/null
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  [ -n "$BQ_PID" ] && kill "$BQ_PID" 2>/dev/null
  rm -rf "$WORKDIR"
  trap - EXIT
}
trap cleanup EXIT

echo "── IPv6 全链路验收（relay ${BASE}，画面 ${DISPLAY_NUM}）──"
if [ ! -x "$BIN" ]; then echo "FAIL: 二进制不存在 $BIN"; exit 1; fi
if ! DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then echo "FAIL: Xvfb ${DISPLAY_NUM} 未运行"; exit 1; fi
# 本机 IPv6 loopback 可用性前置检查
if ! python3 -c "import socket; s=socket.socket(socket.AF_INET6); s.bind(('::1',0)); s.close()" 2>/dev/null; then
  echo "FAIL: 本机无 IPv6 loopback（::1）"; exit 1; fi

# 1) 动态画面（四象限字符块）
BQ="$WORKDIR/bench_draw_quad"
gcc -O2 -o "$BQ" tools/bench_draw_quad.c -lX11 2>"$WORKDIR/gcc.log" || {
  echo "FAIL: 编译 bench_draw_quad.c 失败"; tail -3 "$WORKDIR/gcc.log"; exit 1; }
DISPLAY="$DISPLAY_NUM" "$BQ" 18 >/dev/null 2>&1 & BQ_PID=$!
sleep 1

# 2) relay bind IPv6
"$BIN" relay --bind "[::1]:${PORT}" --auth "$AUTH" --no-tls \
  --admin-path "$ADMIN_PATH" --admin-user "$ADMIN_USER" --admin-pass "$ADMIN_PASS" \
  > "$WORKDIR/relay.log" 2>&1 & RELAY_PID=$!
for i in $(seq 1 20); do curl -s -o /dev/null "$BASE/" 2>/dev/null && break; sleep 1; done
curl -s -o /dev/null "$BASE/" || { echo "FAIL: relay 未监听 IPv6"; tail -3 "$WORKDIR/relay.log"; exit 1; }
echo "OK: relay 监听 IPv6 [::1]:${PORT}（HTTP 200）"

# 3) agent 经 IPv6 注册 + 桌面流
"$BIN" agent --relay-url "$BASE" --key "$KEY" --session-id "$SID" \
  --desktop-capture x11 --desktop-display "$DISPLAY_NUM" \
  > "$WORKDIR/agent.log" 2>&1 & AGENT_PID=$!
for i in $(seq 1 20); do grep -q 'agent session established' "$WORKDIR/agent.log" 2>/dev/null && break; sleep 1; done
grep -q 'agent session established' "$WORKDIR/agent.log" || {
  echo "FAIL: agent 未建立会话（IPv6）"; tail -5 "$WORKDIR/agent.log"; exit 1; }
echo "OK: agent 经 IPv6 注册会话"

curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" \
  -H 'Content-Type: application/json' \
  -d "{\"user\":\"${ADMIN_USER}\",\"pass\":\"${ADMIN_PASS}\"}" -o /dev/null 2>/dev/null
TOKEN="$(curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}/api/overview" 2>/dev/null | python3 -c '
import json,sys
d=json.load(sys.stdin); sid="'$SID'"
s=[x for x in d.get("sessions",[]) if x.get("session_id")==sid]
t=s[0].get("tokens",[]) if s else []
print(t[0].get("token","") if t else "")')"
[ -n "$TOKEN" ] || { echo "FAIL: 取不到会话 token"; exit 1; }
curl -s -X POST "$BASE/agent/session/send" -H 'Content-Type: application/json' \
  -d "{\"token\":\"${TOKEN}\",\"type\":\"desktop:start\",\"payload\":{}}" -o /dev/null
for i in $(seq 1 30); do grep -q 'capture started' "$WORKDIR/agent.log" 2>/dev/null && break; sleep 1; done
grep -q 'capture started' "$WORKDIR/agent.log" || {
  echo "FAIL: 桌面捕获未启动（IPv6）"; tail -3 "$WORKDIR/agent.log"; exit 1; }
echo "OK: 桌面流经 IPv6 启动"

# 4) KPI 采样（桌面视频数据经 IPv6 回流 relay → admin）
for i in $(seq 1 6); do sleep 3; done
KPI="$(curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}/api/session/kpi/$SID" 2>/dev/null)"
FPS="$(printf '%s' "$KPI" | python3 -c '
import json,sys
try:
    s=json.load(sys.stdin).get("samples",[])
    f=[x.get("fps",0) for x in s if isinstance(x.get("fps",0),(int,float))]
    print(round(sum(f)/len(f),1) if f else "")
except Exception: print("")')"
BR="$(printf '%s' "$KPI" | python3 -c '
import json,sys
try:
    s=json.load(sys.stdin).get("samples",[])
    b=[x.get("bitrate_kbps",0) for x in s if isinstance(x.get("bitrate_kbps",0),(int,float))]
    print(max(b) if b else "")
except Exception: print("")')"

# 5) 断言：fps 中值 ≥15 且 bitrate>0（动态画面满帧，IPv6 通道无丢帧）
python3 - "$FPS" "$BR" <<'PY'
import sys
fps = float(sys.argv[1] or 0); br = float(sys.argv[2] or 0)
ok = True
if fps < 15:
    print(f"FAIL: IPv6 通道 fps 中值 {fps} <15"); ok = False
else:
    print(f"OK: IPv6 通道 fps 中值 {fps} ≥15（动态满帧，未降帧）")
if br <= 0:
    print("FAIL: IPv6 通道无 bitrate 采样（视频数据未回流）"); ok = False
else:
    print(f"OK: IPv6 通道 bitrate 峰值 {br:.0f} kbps")
sys.exit(0 if ok else 1)
PY
exit $?
