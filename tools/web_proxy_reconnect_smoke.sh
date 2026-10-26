#!/bin/bash
# MYS-2383 阶段2 回归门槛：≥8 个 agent 高频并发注册/重连，期间 web proxy 正常。
#
# 本卡改动了**会话注册路径**（capabilities 每次注册覆盖）与 **relay 连接路径**
# （agent SSE 断开时撤销 ticket / 取消在途网页请求），所以必须验证：
#   1. 8 个 agent 同时冷注册 → 全部获得 session + token；
#   2. 每个都能建立 events SSE（= agent 通道可达）；
#   3. 期间对其中一个会话的 web proxy 请求正常返回远端页面；
#   4. 全部 kill -9 后同时重连 → 收敛，ticket 失效后重新签发仍可用；
#   5. relay 全程无 panic、agent 进程无异常退出。
#
# 用法: tools/web_proxy_reconnect_smoke.sh
# 环境: BIN=./target/debug/shell-remote N=8 RELAY_PORT=18840 SITE_PORT=18841
set -u

BIN="${BIN:-./target/debug/shell-remote}"
N="${N:-8}"
RELAY_PORT="${RELAY_PORT:-18840}"
SITE_PORT="${SITE_PORT:-18841}"
BASE="http://127.0.0.1:${RELAY_PORT}"
SITE="http://127.0.0.1:${SITE_PORT}"
PW="smoke-pass"
WORKDIR="$(mktemp -d)"

RELAY_PID=""
SITE_PID=""
declare -a APIDS
fail=0

cleanup() {
  for p in "${APIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  [ -n "$SITE_PID" ] && kill "$SITE_PID" 2>/dev/null
  trap - EXIT
}
trap cleanup EXIT

ok()   { echo "  ✓ $*"; }
bad()  { echo "  ✗ $*"; fail=1; }
check() { # $1 描述, $2.. 命令
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi
}

[ -x "$BIN" ] || { echo "FAIL: 无二进制 $BIN（先 cargo build --bin shell-remote）"; exit 1; }

# ── 只监听 127.0.0.1 的测试站（等价于设备自身的页面服务）──
# 写成独立文件而不是 heredoc：heredoc 与 `&` 组合在部分 shell 下拿不到
# stdin，python 会读到空脚本直接退出。
cat > "$WORKDIR/site.py" <<'PYSITE'
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
BODY = b"<html><body>smoke-remote-page</body></html>"
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PYSITE
python3 "$WORKDIR/site.py" "$SITE_PORT" > "$WORKDIR/site.log" 2>&1 &
SITE_PID=$!

"$BIN" relay --bind "127.0.0.1:${RELAY_PORT}" --auth "$PW" --no-tls \
  --settings-file "$WORKDIR/settings.json" >> "$WORKDIR/relay.log" 2>&1 &
RELAY_PID=$!

start_agent() { # $1 = index
  "$BIN" agent --relay-url "$BASE" --key "smokekey$1" --session-id "smokeag$1" \
    --shell /bin/sh --root "$WORKDIR" --no-cmd-log >> "$WORKDIR/agent$1.log" 2>&1 &
  APIDS[$1]=$!
  disown "$!" 2>/dev/null
}

# 一次问出「几个 agent 的 events SSE 已可用」。
#
# 不能用 `curl -m N /agent/session/sse`：SSE 是永不结束的流，curl 会一直等到
# 超时，8 个串行就是几十秒——那量的是超时值而不是收敛时间。这里用裸 socket
# 只读到响应头就断开，单次检查是毫秒级。
cat > "$WORKDIR/online.py" <<'PYONLINE'
import socket, sys
base_port, pw, n = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
ok = 0
for i in range(1, n + 1):
    try:
        s = socket.create_connection(("127.0.0.1", base_port), timeout=2)
        s.sendall((
            f"GET /agent/session/sse?auth={pw}&token=smokekey{i} HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{base_port}\r\n"
            "Accept: text/event-stream\r\nConnection: close\r\n\r\n"
        ).encode())
        if s.recv(32).startswith(b"HTTP/1.1 200"):
            ok += 1
        s.close()
    except OSError:
        pass
print(ok)
PYONLINE

# 收敛时间用毫秒计：$SECONDS 是整秒，8 个 agent 冷启动其实 <1s，用整秒只会
# 显示 "0s"（看起来像没测）。
now_ms() { date +%s%3N; }

wait_all_online() { # $1 = timeout s
  local t0=$(now_ms) ready=0
  while [ $(( ($(now_ms) - t0) / 1000 )) -lt "$1" ]; do
    ready=$(python3 "$WORKDIR/online.py" "$RELAY_PORT" "$PW" "$N" 2>/dev/null || echo 0)
    if [ "${ready:-0}" -eq "$N" ]; then
      echo "$(( $(now_ms) - t0 ))ms"
      return 0
    fi
    sleep 0.2
  done
  echo "超时(在线=${ready:-0}/$N)"
  return 1
}

# 一次完整的网页代理往返：取 ticket → 拉页面 → 断言正文。
web_proxy_ok() { # $1 = index
  local i="$1" ticket body
  ticket=$(curl -s -m 5 -X POST "$BASE/agent/web/ticket" \
    -H "X-Auth: ${PW}" -H "X-SR-Token: smokekey$i" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin).get("ticket",""))' 2>/dev/null)
  [ -n "$ticket" ] || { echo "no-ticket"; return 1; }
  body=$(curl -s -m 15 "$BASE/agent/web/proxy?ticket=${ticket}&url=$(python3 -c \
    "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "$SITE/")")
  case "$body" in *smoke-remote-page*) return 0;; *) echo "${body:0:120}"; return 1;; esac
}

echo "── 网页代理 + 并发注册重连冒烟：N=$N（日志 $WORKDIR）──"

for _ in $(seq 1 100); do
  curl -s -o /dev/null -m 1 "$BASE/" && break
  sleep 0.2
done
curl -s -o /dev/null -m 1 "$BASE/" || { echo "FAIL: relay 未启动"; exit 1; }
# python 解释器启动 + 绑定端口需要一点时间，单独等（此前与 relay 共用一次
# 检查，测试站尚未 bind 就被判失败）。
site_up=0
for _ in $(seq 1 100); do
  if curl -s -o /dev/null -m 1 "$SITE/"; then site_up=1; break; fi
  sleep 0.2
done
[ "$site_up" -eq 1 ] || { echo "FAIL: 测试站未启动（$WORKDIR/site.log）"; exit 1; }

echo "[1] 冷启动 $N 个 agent 并发注册"
for i in $(seq 1 "$N"); do start_agent "$i"; done
if r=$(wait_all_online 60); then ok "$N 个 agent 全部注册并建立 events SSE（$r）"
else bad "并发注册未收敛（$r）"; fi

echo "[2] 并发注册期间 web proxy 正常"
for i in 1 2 "$N"; do
  if r=$(web_proxy_ok "$i"); then ok "agent$i 的 web proxy 返回远端页面"
  else bad "agent$i 的 web proxy 失败（$r）"; fi
done

echo "[3] $N 个 agent 同时 kill -9 后并发重连"
TICKET_BEFORE=$(curl -s -m 5 -X POST "$BASE/agent/web/ticket" \
  -H "X-Auth: ${PW}" -H "X-SR-Token: smokekey1" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin).get("ticket",""))' 2>/dev/null)
for i in $(seq 1 "$N"); do kill -9 "${APIDS[$i]}" 2>/dev/null; done
# 等 relay 察觉断线并撤销 ticket。
sleep 3
code=$(curl -s -o /dev/null -m 5 -w '%{http_code}' \
  "$BASE/agent/web/proxy?ticket=${TICKET_BEFORE}&url=$(python3 -c \
  "import urllib.parse;print(urllib.parse.quote('$SITE/',safe=''))")")
[ "$code" = "403" ] && ok "断线后旧 ticket 已撤销（403）" || bad "断线后旧 ticket 仍可用（$code）"

for i in $(seq 1 "$N"); do start_agent "$i"; done
if r=$(wait_all_online 90); then ok "$N 个 agent 全部重连（$r）"
else bad "重连未收敛（$r）"; fi

echo "[4] 重连后 web proxy 仍正常（重新签发 ticket）"
for i in 1 2 "$N"; do
  if r=$(web_proxy_ok "$i"); then ok "agent$i 重连后 web proxy 正常"
  else bad "agent$i 重连后 web proxy 失败（$r）"; fi
done

echo "[5] 高频抖动：3 轮 × 全部 $N 个 agent 随机 kill/重启"
for round in 1 2 3; do
  for i in $(seq 1 "$N"); do
    kill -9 "${APIDS[$i]}" 2>/dev/null
    start_agent "$i"
  done
  sleep 0.5
  if r=$(wait_all_online 60); then ok "第 $round 轮抖动后收敛（$r）"
  else bad "第 $round 轮抖动未收敛（$r）"; fi
done

echo "[6] 收敛后 web proxy 抽查"
for i in 1 $((N / 2 + 1)) "$N"; do
  if r=$(web_proxy_ok "$i"); then ok "agent$i 抖动后 web proxy 正常"
  else bad "agent$i 抖动后 web proxy 失败（$r）"; fi
done

echo "[7] 进程健康"
dead=0
for i in $(seq 1 "$N"); do kill -0 "${APIDS[$i]}" 2>/dev/null || dead=$((dead + 1)); done
[ "$dead" -eq 0 ] && ok "agent 进程全部存活" || bad "$dead 个 agent 进程退出"
if kill -0 "$RELAY_PID" 2>/dev/null; then ok "relay 存活"; else bad "relay 已退出"; fi
panics=$(grep -l 'panicked' "$WORKDIR"/*.log 2>/dev/null | wc -l)
[ "$panics" -eq 0 ] && ok "日志无 panic" || bad "$panics 个日志含 panic"
web_errs=$(grep -c "web proxy" "$WORKDIR/relay.log" 2>/dev/null || true)
echo "    （relay 日志中 web proxy 相关行：${web_errs:-0}）"

echo
if [ $fail -eq 0 ]; then echo "PASS"; else echo "FAIL（日志保留在 $WORKDIR）"; fi
exit $fail
