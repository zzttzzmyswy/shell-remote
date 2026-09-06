#!/bin/bash
# 4-top 动态基准验收（R4 戊172 / R5 戊 4-top 基准）：标准动态高刷新画面
# （4 个 0.1s 刷新 top 终端）下验证 agent 桌面流"动态内容满帧编码"——
# 用户铁律：动态画面不降帧（fps 仅由内容活动 + 解码背压驱动）。
#
# 流程（不依赖浏览器）：
#   起 4-top 画面(Xvfb) → 起 relay → 起 agent(X11 捕获动态场景)
#   → admin KPI 采样（心跳快照 fps/bitrate）→ 断言动态 fps ≥ 15 且
#   bitrate > 0 → PASS/FAIL 报告 → cleanup
#
# 用法:
#   tools/bench_top4_verify.sh
# 环境: DISPLAY_NUM=:98（需已起 Xvfb）、BIN/PORT/ADMIN_*/KEY/SID 同其它验收脚本
set -u
BIN="${BIN:-./target/debug/shell-remote}"
PORT="${PORT:-3103}"
BASE="${BASE:-http://127.0.0.1:${PORT}}"
ADMIN_PATH="${ADMIN_PATH:-/sr-admin-test}"
ADMIN_USER="${ADMIN_USER:-admin}"
ADMIN_PASS="${ADMIN_PASS:-srpass}"
AUTH="${AUTH:-sr-smoke-pass}"
KEY="${KEY:-b4k}"
SID="${SID:-b4sess1}"
DISPLAY_NUM="${DISPLAY_NUM:-:98}"
WORKDIR="$(mktemp -d)"
RELAY_PID=""; AGENT_PID=""; BQ_PID=""

cleanup() {
  [ -n "$AGENT_PID" ] && kill "$AGENT_PID" 2>/dev/null
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  [ -n "$BQ_PID" ] && kill "$BQ_PID" 2>/dev/null
  pkill -f 'top -b -d 0.1' 2>/dev/null
  pkill -f 'xterm -T top' 2>/dev/null
  rm -rf "$WORKDIR"
  trap - EXIT
}
trap cleanup EXIT

echo "── 4-top 动态基准验收（relay ${BASE}，画面 ${DISPLAY_NUM}）──"
if [ ! -x "$BIN" ]; then echo "FAIL: 二进制不存在 $BIN"; exit 1; fi
if ! DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then echo "FAIL: Xvfb ${DISPLAY_NUM} 未运行"; exit 1; fi

# 1) 标准动态画面：四象限高速字符行块（无字体依赖——本环境 Xvfb 无 misc
#    字体，xterm/top 起不来；bench_draw_quad.c 模拟 4 个 0.1s 刷新的动态
#    终端，整体熵 ≈ 连续刷新终端）。
BQ="$WORKDIR/bench_draw_quad"
if ! gcc -O2 -o "$BQ" tools/bench_draw_quad.c -lX11 2>"$WORKDIR/gcc.log"; then
  echo "FAIL: 编译 tools/bench_draw_quad.c 失败（看 $WORKDIR/gcc.log）"
  tail -3 "$WORKDIR/gcc.log"; exit 1
fi
DISPLAY="$DISPLAY_NUM" "$BQ" 18 >/dev/null 2>&1 &
BQ_PID=$!
sleep 2
echo "  动态画面: bench_draw_quad 已在 ${DISPLAY_NUM} 运行（18fps 四象限字符块）"

# 2) relay
"$BIN" relay --bind "127.0.0.1:${PORT}" --auth "$AUTH" --no-tls \
  --admin-path "$ADMIN_PATH" --admin-user "$ADMIN_USER" --admin-pass "$ADMIN_PASS" \
  > "$WORKDIR/relay.log" 2>&1 & RELAY_PID=$!
for i in $(seq 1 20); do curl -s -o /dev/null "$BASE/" 2>/dev/null && break; sleep 1; done

# 3) agent：X11 捕获动态画面
"$BIN" agent --relay-url "$BASE" --key "$KEY" --session-id "$SID" \
  --desktop-capture x11 --desktop-display "$DISPLAY_NUM" \
  > "$WORKDIR/agent.log" 2>&1 & AGENT_PID=$!
# 3.5) 桌面由浏览器命令驱动：模拟浏览器经 /agent/session/send 发 desktop:start。
for i in $(seq 1 20); do grep -q 'agent session established' "$WORKDIR/agent.log" 2>/dev/null && break; sleep 1; done
curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" \
  -H 'Content-Type: application/json' \
  -d "{\"user\":\"${ADMIN_USER}\",\"pass\":\"${ADMIN_PASS}\"}" -o /dev/null 2>/dev/null
TOKEN="$(curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}/api/overview" 2>/dev/null | python3 -c '
import json,sys
d=json.load(sys.stdin)
sid="'$SID'"
s=[x for x in d.get("sessions",[]) if x.get("session_id")==sid]
t=s[0].get("tokens",[]) if s else []
print(t[0].get("token","") if t else "")')"
if [ -z "$TOKEN" ]; then echo "FAIL: 取不到会话 token（admin overview）"; exit 1; fi
curl -s -X POST "$BASE/agent/session/send" -H 'Content-Type: application/json' \
  -d "{\"token\":\"${TOKEN}\",\"type\":\"desktop:start\",\"payload\":{}}" -o /dev/null
# 等捕获启动（agent 日志 "capture started"）
for i in $(seq 1 30); do
  grep -q 'capture started' "$WORKDIR/agent.log" 2>/dev/null && break
  sleep 1
done
grep -q 'capture started' "$WORKDIR/agent.log" 2>/dev/null || {
  echo "FAIL: agent 捕获未启动（看 $WORKDIR/agent.log）"; tail -3 "$WORKDIR/agent.log"; exit 1;
}

# 4) admin KPI 采样：等编码器同步（bitrate>0 的样本；启动首心跳可能 0）。
echo "  等待心跳 KPI 样本（编码器同步 bitrate>0，≤45s）…"
SAMPLES=""
for i in $(seq 1 45); do
  curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" \
    -H 'Content-Type: application/json' \
    -d "{\"user\":\"${ADMIN_USER}\",\"pass\":\"${ADMIN_PASS}\"}" -o /dev/null 2>/dev/null
  SAMPLES="$(curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}/api/session/kpi/$SID" 2>/dev/null)"
  BR_OK="$(printf '%s' "$SAMPLES" | python3 -c '
import json,sys
d=json.load(sys.stdin)
s=d.get("samples", [])
br=[x.get("bitrate_kbps",0) for x in s if isinstance(x.get("bitrate_kbps",0),(int,float))]
print(1 if any(b>0 for b in br) else 0)' 2>/dev/null)"
  if [ "${BR_OK:-0}" = 1 ]; then break; fi
  sleep 1
done

# 5) 断言：动态 fps 中值 ≥15 且 bitrate > 0
REPORT="$(printf '%s' "$SAMPLES" | python3 -c '
import json,sys,statistics
d=json.load(sys.stdin)
s=d.get("samples", [])
fps=[x.get("fps",0) for x in s if isinstance(x.get("fps",0),(int,float))]
br=[x.get("bitrate_kbps",0) for x in s if isinstance(x.get("bitrate_kbps",0),(int,float))]
if not fps:
    print("NO_SAMPLES"); sys.exit(0)
med=statistics.median(fps)
peak=max(br) if br else 0
print(f"fps_median={med:.1f} fps_min={min(fps)} fps_max={max(fps)} bitrate_kbps_peak={peak} samples={len(fps)}")
')"
echo "  KPI: $REPORT"
case "$REPORT" in
  NO_SAMPLES) echo "FAIL: 无 KPI 样本（agent 心跳未采样）"; exit 1;;
esac
FPS_MED="$(printf '%s' "$REPORT" | sed -n 's/.*fps_median=\([0-9.]*\).*/\1/p')"
BIT_PEAK="$(printf '%s' "$REPORT" | sed -n 's/.*bitrate_kbps_peak=\([0-9.]*\).*/\1/p')"
FPS_OK="$(python3 -c "print(1 if float('${FPS_MED}') >= 15 else 0)")"
BIT_OK="$(python3 -c "print(1 if float('${BIT_PEAK}') > 0 else 0)")"
if [ "$FPS_OK" = 1 ] && [ "$BIT_OK" = 1 ]; then
  echo "PASS: 动态 4-top 画面 fps 中值 ${FPS_MED} ≥15、bitrate ${BIT_PEAK} kbps（动态内容满帧，未降帧）"
  exit 0
else
  echo "FAIL: fps 中值 ${FPS_MED}（需≥15）、bitrate ${BIT_PEAK} kbps（需>0）"
  exit 1
fi