#!/bin/bash
# 拆分验证冒烟：CLI 二进制（relay + agent 仅终端）与 UI 二进制（agent 终端+桌面）。
#
# 验证点：
#   1. CLI agent 无 --desktop-* 参数（终端转发专用）
#   2. CLI agent 终端会话可通（terminal:input → terminal:output 回显）
#   3. UI agent 有 --desktop-* 参数，注册后 session:join 回复 desktop:capabilities(available:true)
#
# 用法：tools/smoke_split.sh [cli_bin] [ui_bin]
#   默认 cli_bin=target/release/shell-remote  ui_bin=target/debug/shell-remote-ui
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI_BIN="${1:-$ROOT/target/release/shell-remote}"
UI_BIN="${2:-$ROOT/target/debug/shell-remote-ui}"
PORT="${PORT:-3271}"
WORK="$ROOT/target/smoke-split-$PORT"
mkdir -p "$WORK"

cleanup() {
  kill "${RELAY_PID:-}" "${CLI_PID:-}" "${UI_PID:-}" "${SSE_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

echo "== 1. CLI agent 无 --desktop-* 参数 =="
if "$CLI_BIN" agent --help 2>&1 | grep -q -- '--desktop-'; then
  echo "FAIL: CLI agent 不应有 --desktop-* 参数"; exit 1
fi
echo "OK: CLI agent 只有终端参数"

echo "== 2. UI agent 有 --desktop-* 参数 =="
if ! "$UI_BIN" agent --help 2>&1 | grep -q -- '--desktop-capture'; then
  echo "FAIL: UI agent 应有 --desktop-capture"; exit 1
fi
echo "OK: UI agent 带桌面参数"

echo "== 3. 启动 relay（CLI 二进制）于 :$PORT =="
"$CLI_BIN" relay --bind "127.0.0.1:$PORT" --auth sr-split-pass --no-tls \
  >"$WORK/relay.log" 2>&1 &
RELAY_PID=$!
sleep 1
curl -fsS "http://127.0.0.1:$PORT/" -o /dev/null || { echo "FAIL: relay 未就绪"; cat "$WORK/relay.log"; exit 1; }

echo "== 4. 启动 CLI agent（终端转发）=="
"$CLI_BIN" agent --relay-url "http://127.0.0.1:$PORT" --key sr-cli-key \
  --session-id e2ecli1 --root "$ROOT" >"$WORK/cli-agent.log" 2>&1 &
CLI_PID=$!
for i in $(seq 1 30); do
  if grep -q "agent session established" "$WORK/cli-agent.log"; then break; fi
  sleep 0.5
done
if ! grep -q "agent session established" "$WORK/cli-agent.log"; then
  echo "FAIL: CLI agent 未注册成功"; cat "$WORK/cli-agent.log"; exit 1
fi
# 日志形如 `token: <到空格为止> session=... permission=rw`；key 模式下 token 即 key。
CLI_TOKEN=$(grep -m1 -oE 'token: [^ ]+' "$WORK/cli-agent.log" | awk '{print $2}' || true)
[ -n "${CLI_TOKEN:-}" ] || { echo "FAIL: 未能从 agent 日志解析 rw token"; cat "$WORK/cli-agent.log"; exit 1; }
echo "OK: CLI agent 注册成功 session=e2ecli1 token=${CLI_TOKEN:0:8}…"

echo "== 5. CLI agent 终端会话测试 =="
curl -N -sS "http://127.0.0.1:$PORT/agent/session/sse?session_id=e2ecli1&token=$CLI_TOKEN" \
  >"$WORK/cli-sse.log" 2>&1 &
SSE_PID=$!
sleep 1
# 从 session:join 回复中取第一个 tab_id
TAB_ID=$(grep -oE '"tab_id":"[0-9a-f-]+"' "$WORK/cli-sse.log" | head -1 | sed -E 's/.*:"([0-9a-f-]+)"/\1/')
[ -n "${TAB_ID:-}" ] || { echo "FAIL: 未收到 tab_list"; head -5 "$WORK/cli-sse.log"; exit 1; }
# 发送 echo，等待回显
INPUT=$(printf 'echo SR_E2E_OK_%s\n' "$PORT" | base64 -w0)
curl -fsS -X POST "http://127.0.0.1:$PORT/agent/session/send" \
  -H 'Content-Type: application/json' \
  -d "{\"type\":\"terminal:input\",\"token\":\"$CLI_TOKEN\",\"payload\":{\"data\":\"$INPUT\",\"tab_id\":\"$TAB_ID\"}}" >/dev/null
for i in $(seq 1 20); do
  if python3 - "$WORK/cli-sse.log" "$PORT" <<'PY'
import sys, base64, re
log = open(sys.argv[1], encoding="utf-8", errors="replace").read()
marker = f"SR_E2E_OK_{sys.argv[2]}"
for m in re.finditer(r'"data":"([A-Za-z0-9+/=]+)"', log):
    try:
        dec = base64.b64decode(m.group(1)).decode("utf-8", errors="replace")
    except Exception:
        continue
    if marker in dec:
        sys.exit(0)
sys.exit(1)
PY
  then break; fi
  sleep 0.5
done
if python3 - "$WORK/cli-sse.log" "$PORT" <<'PY'
import sys, base64, re
log = open(sys.argv[1], encoding="utf-8", errors="replace").read()
marker = f"SR_E2E_OK_{sys.argv[2]}"
for m in re.finditer(r'"data":"([A-Za-z0-9+/=]+)"', log):
    try:
        dec = base64.b64decode(m.group(1)).decode("utf-8", errors="replace")
    except Exception:
        continue
    if marker in dec:
        sys.exit(0)
sys.exit(1)
PY
then
  echo "OK: CLI agent 终端回显成功"
else
  echo "FAIL: 未收到终端回显"; tail -5 "$WORK/cli-sse.log"; exit 1
fi
kill "$SSE_PID" 2>/dev/null || true; SSE_PID=""

echo "== 6. 启动 UI agent（终端 + 桌面）=="
# 默认 capture=auto：UI agent 上报桌面能力可用（headless 下不会真正开流）
"$UI_BIN" agent --relay-url "http://127.0.0.1:$PORT" --key sr-ui-key \
  --session-id e2eui1 --root "$ROOT" >"$WORK/ui-agent.log" 2>&1 &
UI_PID=$!
for i in $(seq 1 30); do
  if grep -q "agent session established" "$WORK/ui-agent.log"; then break; fi
  sleep 0.5
done
if ! grep -q "agent session established" "$WORK/ui-agent.log"; then
  echo "FAIL: UI agent 未注册成功"; cat "$WORK/ui-agent.log"; exit 1
fi
UI_TOKEN=$(grep -m1 -oE 'token: [^ ]+' "$WORK/ui-agent.log" | awk '{print $2}' || true)
[ -n "${UI_TOKEN:-}" ] || { echo "FAIL: 未能从 UI agent 日志解析 rw token"; cat "$WORK/ui-agent.log"; exit 1; }
echo "OK: UI agent 注册成功 session=e2eui1 token=${UI_TOKEN:0:8}…"

echo "== 7. UI agent 上报桌面能力 =="
curl -N -sS "http://127.0.0.1:$PORT/agent/session/sse?session_id=e2eui1&token=$UI_TOKEN" \
  >"$WORK/ui-sse.log" 2>&1 &
SSE_PID=$!
sleep 1
if grep -q '"type":"desktop:capabilities"' "$WORK/ui-sse.log" \
   && grep -q '"available":true' "$WORK/ui-sse.log"; then
  echo "OK: UI agent 上报 desktop:capabilities(available:true)"
else
  echo "FAIL: UI agent 未上报桌面能力"; tail -5 "$WORK/ui-sse.log"; exit 1
fi
kill "$SSE_PID" 2>/dev/null || true; SSE_PID=""

echo ""
echo "全部通过 ✔  CLI=$CLI_BIN UI=$UI_BIN"
