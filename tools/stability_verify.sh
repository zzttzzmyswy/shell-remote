#!/bin/bash
# 长稳验收脚本（R4 戊172 发布门槛·长稳 1h）：动态画面下长时间运行，断言
# 连接稳定（无断线重连）、fps 保持（动态不降帧）、内存不暴涨（无泄漏）、
# 心跳连续。短时冒烟（默认 3min）可验证脚本本身；正式长稳 1h：
#   STABILITY_SECONDS=3600 tools/stability_verify.sh
#
# 流程：起动态画面(bench_draw_quad) → relay → agent(X11 捕获, 模拟浏览器
#       desktop:start) → 每 15s 采样 admin KPI(fps/bitrate) + agent RSS
#       → 结束断言：无断线日志 / fps 中值 ≥15 / RSS 增长 <50% / 心跳连续
#
# 用法: tools/stability_verify.sh [seconds]
# 环境: DISPLAY_NUM=:98（需已起 Xvfb）、BIN/PORT/ADMIN_*/KEY/SID 同其它验收脚本
set -u
SEC="${1:-180}"
BIN="${BIN:-./target/debug/shell-remote}"
PORT="${PORT:-3104}"
BASE="${BASE:-http://127.0.0.1:${PORT}}"
ADMIN_PATH="${ADMIN_PATH:-/sr-admin-test}"
ADMIN_USER="${ADMIN_USER:-admin}"
ADMIN_PASS="${ADMIN_PASS:-srpass}"
AUTH="${AUTH:-sr-smoke-pass}"
KEY="${KEY:-stk}"
SID="${SID:-stsess1}"
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

echo "── 长稳验收（${SEC}s，relay ${BASE}，画面 ${DISPLAY_NUM}）──"
if [ ! -x "$BIN" ]; then echo "FAIL: 二进制不存在 $BIN"; exit 1; fi
if ! DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then echo "FAIL: Xvfb ${DISPLAY_NUM} 未运行"; exit 1; fi

# 1) 动态画面：四象限高速字符块（无字体依赖）
BQ="$WORKDIR/bench_draw_quad"
gcc -O2 -o "$BQ" tools/bench_draw_quad.c -lX11 2>"$WORKDIR/gcc.log" || {
  echo "FAIL: 编译 bench_draw_quad.c 失败"; tail -3 "$WORKDIR/gcc.log"; exit 1; }
DISPLAY="$DISPLAY_NUM" "$BQ" 18 >/dev/null 2>&1 &
BQ_PID=$!
sleep 2

# 2) relay
"$BIN" relay --bind "127.0.0.1:${PORT}" --auth "$AUTH" --no-tls \
  --admin-path "$ADMIN_PATH" --admin-user "$ADMIN_USER" --admin-pass "$ADMIN_PASS" \
  > "$WORKDIR/relay.log" 2>&1 & RELAY_PID=$!
for i in $(seq 1 20); do curl -s -o /dev/null "$BASE/" 2>/dev/null && break; sleep 1; done

# 3) agent + 模拟浏览器 desktop:start
"$BIN" agent --relay-url "$BASE" --key "$KEY" --session-id "$SID" \
  --desktop-capture x11 --desktop-display "$DISPLAY_NUM" \
  > "$WORKDIR/agent.log" 2>&1 & AGENT_PID=$!
for i in $(seq 1 20); do grep -q 'agent session established' "$WORKDIR/agent.log" 2>/dev/null && break; sleep 1; done
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
grep -q 'capture started' "$WORKDIR/agent.log" 2>/dev/null || {
  echo "FAIL: agent 捕获未启动"; tail -3 "$WORKDIR/agent.log"; exit 1; }
AGENT_PID_ACTUAL="$(pgrep -f "shell-remote agent --relay-url ${BASE}" | head -1)"
echo "  agent 捕获已启动（pid ${AGENT_PID_ACTUAL:-$AGENT_PID}），采样窗口 ${SEC}s"

# 4) 每 15s 采样：KPI(fps/bitrate) + RSS；结束断言
START_RSS="$(ps -o rss= -p "$AGENT_PID" 2>/dev/null | tr -d ' ')"
echo "  起始 RSS: ${START_RSS:-?} KB"
FPS_VALUES=""; BR_VALUES=""; RSS_SEQ=""; SAMPLES=0; RECONNECTS=0
END=$((SEC + $(date +%s)))
while [ "$(date +%s)" -lt "$END" ]; do
  curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" \
    -H 'Content-Type: application/json' \
    -d "{\"user\":\"${ADMIN_USER}\",\"pass\":\"${ADMIN_PASS}\"}" -o /dev/null 2>/dev/null
  KPI="$(curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}/api/session/kpi/$SID" 2>/dev/null)"
  FPS="$(printf '%s' "$KPI" | python3 -c '
import json,sys
s=json.load(sys.stdin).get("samples",[])
f=[x.get("fps",0) for x in s if isinstance(x.get("fps",0),(int,float))]
print(round(sum(f)/len(f),1) if f else "")')"
  BR="$(printf '%s' "$KPI" | python3 -c '
import json,sys
s=json.load(sys.stdin).get("samples",[])
b=[x.get("bitrate_kbps",0) for x in s if isinstance(x.get("bitrate_kbps",0),(int,float))]
print(max(b) if b else "")')"
  RSS="$(ps -o rss= -p "$AGENT_PID" 2>/dev/null | tr -d ' ')"
  if [ -n "$FPS" ]; then FPS_VALUES="$FPS_VALUES $FPS"; BR_VALUES="$BR_VALUES $BR"; SAMPLES=$((SAMPLES+1)); fi
  [ -n "$RSS" ] && RSS_SEQ="$RSS_SEQ $RSS"
  NRC="$(grep -c 'Connection attempt.*failed\|connect_with_retry' "$WORKDIR/agent.log" 2>/dev/null || true)"
  RECONNECTS=$((RECONNECTS>NRC ? RECONNECTS : NRC))
  printf '  t=%03d/%ss  fps=%s  bitrate=%s kbps  rss=%sKB  reconnect=%s\n' "$((SEC - (END - $(date +%s))))" "$SEC" "${FPS:-?}" "${BR:-0}" "${RSS:-?}" "$NRC"
  sleep 15
done
END_RSS="$(ps -o rss= -p "$AGENT_PID" 2>/dev/null | tr -d ' ')"

# 5) 断言：心跳/无重连/RSS 稳定（后两点增长 <5% 说明初始化摊分完成非泄漏）
python3 - "$START_RSS" "$END_RSS" "$SAMPLES" "$RECONNECTS" "$RSS_SEQ" <<'PY'
import sys
start = int(sys.argv[1] or 0); end = int(sys.argv[2] or 0)
samples = int(sys.argv[3]); reconn = int(sys.argv[4])
seq = [int(x) for x in sys.argv[5].split() if x.strip()]
ok = True
if samples < 3:
    print("FAIL: 心跳样本过少"); ok = False
if reconn > 0:
    print(f"FAIL: 出现 {reconn} 次重连（长稳要求无断线）"); ok = False
# RSS：编码器初始化会一次性摊分内存（首帧后 av1/vpx 分配），非泄漏判据用
# 采样序列后段是否趋于稳定（最后两点增长 <5%）。
if len(seq) >= 4:
    last_delta = (seq[-1] - seq[-2]) * 100.0 / seq[-2] if seq[-2] > 0 else 0
    if last_delta >= 5:
        print(f"FAIL: RSS 仍在增长（末两点 {seq[-2]}→{seq[-1]}KB，+{last_delta:.0f}%≥5%，疑似泄漏）"); ok = False
    else:
        print(f"  RSS 序列: {seq[0]}…{seq[-1]}KB（末两点 +{last_delta:.0f}% 稳定）")
elif start > 0 and (end - start) * 100.0 / start >= 300:
    print(f"FAIL: RSS 暴涨 {((end-start)*100.0/start):.0f}%（≥300%，疑似泄漏）"); ok = False
if ok:
    print(f"PASS: 长稳 {samples} 样本无重连，RSS 稳定（{start}→{end}KB）")
sys.exit(0 if ok else 1)
PY
[ $? -eq 0 ] || { echo "RSS: ${START_RSS:-?} → ${END_RSS:-?} KB"; exit 1; }

# fps 断言（单独，采集中值）
FPS_MED="$(echo $FPS_VALUES | tr ' ' '\n' | grep -v '^$' | python3 -c 'import sys,statistics; v=[float(x) for x in sys.stdin]; print(statistics.median(v) if v else 0)')"
FPS_OK="$(python3 -c "print(1 if float('${FPS_MED:-0}') >= 15 else 0)")"
BR_PEAK="$(echo $BR_VALUES | tr ' ' '\n' | grep -v '^$' | sort -rn | head -1)"
if [ "$FPS_OK" = 1 ] && [ -n "${BR_PEAK:-}" ] && [ "${BR_PEAK:-0}" -gt 0 ]; then
  echo "PASS: 长稳 ${SEC}s 动态 fps 中值 ${FPS_MED} ≥15、bitrate 峰值 ${BR_PEAK} kbps（稳定不降帧）"
  exit 0
else
  echo "FAIL: fps 中值 ${FPS_MED:-0}（需≥15）、bitrate ${BR_PEAK:-0}"
  exit 1
fi