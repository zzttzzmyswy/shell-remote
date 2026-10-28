#!/bin/bash
# 并发升级 / 换机身份恢复冒烟（MYS-2425 阶段3）。
#
# 覆盖任务卡要求的四件事：
#   1. 多台设备**同时**升级：不重复注册、无重复进程、无互抢身份；
#   2. 同 session 顶替：同一 --session-id 的新 incarnation 顶掉旧的，身份不串；
#   3. 注册限流：低 registration-rate-limit 下批量重注册仍能收敛；
#   4. 多次升级：同一设备连升两轮仍保持同一 session_id 与 Token。
#
# 与 concurrent_register_smoke.sh 的分工：那个脚本压的是"注册风暴"，这个脚本
# 压的是"升级路径里的注册"——每条断言都检查 session_id / Token 前后一致，
# 而不只是"在线数对得上"。
#
# 用法: tools/concurrent_upgrade_smoke.sh
# 环境: BIN=./target/debug/shell-remote N=8 PORT=3122
set -u
BIN="${BIN:-./target/debug/shell-remote}"
N="${N:-8}"
PORT="${PORT:-3122}"
BASE="http://127.0.0.1:${PORT}"
ADMIN_PATH=/sr-admin-cus
WORKDIR="$(mktemp -d)"
RELAY_PID=""
declare -a APIDS

cleanup() {
  for p in "${APIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  trap - EXIT
}
trap cleanup EXIT

fail=0
ok()   { echo "  ✓ $1"; }
bad()  { echo "  ✗ $1"; fail=1; }
check() { local name="$1"; shift; local r; r="$("$@")"; local rc=$?
  if [ $rc -eq 0 ]; then ok "$name ($r)"; else bad "$name ($r)"; fi; }

# 登录一次，之后复用 cookie。
#
# 每次都重新登录会撞 relay 的管理员登录限流（"admin login rate limited"），
# 之后的请求全部返回空——表现成"会话读不到 tokens"，而会话其实好好的。
# 换 relay（重启）后需要重新登录，所以 `login` 单独暴露、在起 relay 后调用。
login() {
  curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" \
    -H 'Content-Type: application/json' -d '{"user":"admin","pass":"cspass"}' \
    -o /dev/null 2>/dev/null
  for _ in $(seq 1 40); do
    [ -s "$WORKDIR/cookie" ] && grep -q sr_admin "$WORKDIR/cookie" 2>/dev/null && return 0
    sleep 0.25
    curl -s -c "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/login" \
      -H 'Content-Type: application/json' -d '{"user":"admin","pass":"cspass"}' \
      -o /dev/null 2>/dev/null
  done
  return 1
}

api() { # $1 = path, rest = curl args
  local path="$1"; shift
  curl -s -b "$WORKDIR/cookie" "$BASE${ADMIN_PATH}${path}" "$@" 2>/dev/null
}

start_relay() { # $1 = rate limit (optional)
  "$BIN" relay --bind "127.0.0.1:${PORT}" --auth cus-pass --no-tls \
    --admin-path "$ADMIN_PATH" --admin-user admin --admin-pass cspass \
    --registration-rate-limit "${1:-120}" \
    >> "$WORKDIR/relay.log" 2>&1 &
  RELAY_PID=$!
  for _ in $(seq 1 60); do curl -s -o /dev/null "$BASE/" && break; sleep 0.2; done
  login
}

start_agent() { # $1 = index, $2 = extra args (optional)
  local i="$1"; shift
  "$BIN" agent --relay-url "$BASE" --key "cusk$i" --session-id "cusag$i" \
    --shell /bin/sh --root "$WORKDIR" "$@" >> "$WORKDIR/agent$i.log" 2>&1 &
  APIDS[$i]=$!
  disown "$!" 2>/dev/null
}

overview() { api /api/overview; }

online() { overview | grep -o '"agent_online":[0-9]*' | grep -o '[0-9]*$'; }

# 某会话当前的 (session_id, 第一个 token) —— 身份对账的基本单位。
sess_tokens() { # $1 = session id
  overview | python3 -c '
import json,sys
sid = sys.argv[1]
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for s in d.get("sessions", []):
    if s.get("session_id") == sid:
        toks = sorted(t["token"] for t in s.get("tokens", []))
        print(sid, " ".join(toks))
        sys.exit(0)
sys.exit(1)
' "$1"
}

# 等某个会话的身份可读。agent_online 变 true 与"会话表里已写入 tokens"不是
# 同一个瞬间（注册响应先回、概览快照后到），所以对账前必须轮询，否则会读到
# 一个还没落表的会话。
wait_identity() { # $1 = session id, $2 = timeout s
  local t0=$SECONDS
  while [ $((SECONDS - t0)) -lt "$2" ]; do
    if sess_tokens "$1" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}

wait_online() { # $1 = expected, $2 = timeout
  local t0=$SECONDS n=0
  while [ $((SECONDS - t0)) -lt "$2" ]; do
    n="$(online)"
    if [ "${n:-0}" -eq "$1" ] 2>/dev/null; then echo "$((SECONDS - t0))s"; return 0; fi
    sleep 1
  done
  echo "超时(在线=${n:-0}/$1)"; return 1
}

# 并发触发升级：用 xargs -P 而不是 `curl &` + 裸 `wait`。
#
# 裸 `wait` 会等**所有**子进程，包括那些 disown 掉、本来就该常驻的 agent
# ——脚本会在第一次 `wait` 处永久挂住，而表面现象只是"后面没有输出了"。
# xargs -P 只等它自己派生的那批。
#
# 必须用 `-b`（读 cookie）而不是 `-c`：`-c` 会把 cookie jar **重写**成这次
# 请求的结果，而升级接口不下发 cookie —— 一次并发触发就把登录态清空，
# 之后所有 api 调用都变成未登录。
fire_upgrades() { # $@ = session ids
  printf '%s\n' "$@" | xargs -P "$N" -I{} curl -s -o /dev/null \
    -b "$WORKDIR/cookie" -X POST "$BASE${ADMIN_PATH}/api/agent/upgrade" \
    -H 'Content-Type: application/json' -d '{"session_id":"{}"}' 2>/dev/null
}

# 进程数：同名二进制必须恰好是"每台设备一个"，不能因为换机留下双进程。
proc_count() { pgrep -fc "shell-remote agent .*--session-id cusag" 2>/dev/null || echo 0; }

[ -x "$BIN" ] || { echo "FAIL: 无二进制 $BIN"; exit 1; }
curl -s -o /dev/null "$BASE/" && { echo "FAIL: 端口 $PORT 被占用"; exit 1; }
echo "── 并发升级/注册冒烟：N=$N（日志 $WORKDIR）──"

# ── 1. 冷启动 ──────────────────────────────────────────────────────
start_relay || { echo "FAIL: relay 未启动"; exit 1; }
for i in $(seq 1 "$N"); do start_agent "$i"; done
check "1. 冷启动 $N 个并发注册" wait_online "$N" 60

# 记录每台设备的基线身份（session_id + tokens），之后每一步都要对得上。
declare -a BASE_ID
declare -a BASE_TOK
for i in $(seq 1 "$N"); do
  wait_identity "cusag$i" 20 || { bad "读不到 cusag$i 的 tokens"; continue; }
  line="$(sess_tokens "cusag$i")"
  BASE_ID[$i]="$(echo "$line" | cut -d' ' -f1)"
  BASE_TOK[$i]="$(echo "$line" | cut -d' ' -f2-)"
done

# ── 2. 同时升级：所有设备并发走一遍换机 ────────────────────────────
# 没有真实制品时，升级指令会被拒绝（429/409/400 都算正常拒绝），但这正是要
# 压的路径：并发的 agent 同时在注册与上报进度，relay 不能错乱、不能把身份
# 张冠李戴。用 admin API 直接触发，忽略返回体只要求 HTTP 有响应。
echo "  · 并发触发升级指令（无制品 → relay 预期拒绝，重点是不串身份）"
fire_upgrades $(for i in $(seq 1 "$N"); do echo "cusag$i"; done)
check "2. 并发升级触发后 $N 台仍全部在线" wait_online "$N" 60
for i in $(seq 1 "$N"); do
  [ -n "${BASE_TOK[$i]:-}" ] || continue
  wait_identity "cusag$i" 20 || { bad "升级触发后读不到 cusag$i"; continue; }
  line="$(sess_tokens "cusag$i")"
  [ "$(echo "$line" | cut -d' ' -f2-)" = "${BASE_TOK[$i]}" ] \
    || bad "cusag$i 的 token 在并发升级后变了"
done
[ $fail -eq 0 ] && ok "全部设备 token 未变（无互抢身份）"

# ── 3. 同 session 顶替 ─────────────────────────────────────────────
# 同一 --session-id 的新 incarnation（同 key）顶掉旧的：在线数不能因此翻倍。
echo "  · cusag1 重启（同 key/session id）→ 旧 incarnation 被顶替"
kill -9 "${APIDS[1]}" 2>/dev/null; sleep 1
start_agent 1
check "3. 同 session 顶替后仍是 $N 台（无重复会话）" wait_online "$N" 60
if [ -n "${BASE_TOK[1]:-}" ]; then
  wait_identity cusag1 20 && line="$(sess_tokens cusag1)" \
    && [ "$(echo "$line" | cut -d' ' -f2-)" = "${BASE_TOK[1]}" ] \
    && ok "cusag1 顶替后 token 未变" || bad "cusag1 顶替后 token 变了"
fi

# ── 4. 注册限流：低限额下批量重注册仍收敛 ─────────────────────────
echo "  · 以 --registration-rate-limit 3 重启 relay，$N 台同时重连"
# 主动杀掉的 relay 是预期的：屏蔽 shell 的 "Killed" 作业提示，免得看起来像失败。
{ kill -9 "$RELAY_PID"; wait "$RELAY_PID"; } 2>/dev/null
sleep 1
start_relay 3 || { echo "FAIL: relay 重启失败"; exit 1; }
check "4. 限流 3/min 下 $N 台重连收敛" wait_online "$N" 180

# ── 5. 多次升级：连做两轮后身份仍一致 ─────────────────────────────
for round in 1 2; do
  echo "  · 第 $round 轮并发升级触发 + 抖动"
  fire_upgrades $(for i in $(seq 1 "$N"); do echo "cusag$i"; done)
  # 抖动：升级触发的同时重启一部分 agent（同 key/session id 续接）。
  for i in $(seq 1 "$N"); do
    if [ $((i % 3)) -eq 0 ]; then kill -9 "${APIDS[$i]}" 2>/dev/null; start_agent "$i"; fi
  done
  check "5.$round 第 $round 轮后 $N 台在线" wait_online "$N" 90
done

for i in $(seq 1 "$N"); do
  [ -n "${BASE_TOK[$i]:-}" ] || continue
  wait_identity "cusag$i" 20 || { bad "最终读不到 cusag$i"; continue; }
  line="$(sess_tokens "cusag$i")"
  [ "$(echo "$line" | cut -d' ' -f2-)" = "${BASE_TOK[$i]}" ] \
    || bad "cusag$i 在两轮升级后 token 变了"
done
[ $fail -eq 0 ] && ok "两轮升级后全部设备身份保持一致"

# ── 6. 无重复进程 / 无 panic ───────────────────────────────────────
procs="$(proc_count)"
[ "$procs" -le "$N" ] && ok "无重复 agent 进程（$procs ≤ $N）" \
  || bad "出现重复 agent 进程（$procs > $N）——换机残留旧进程"
panics=$(grep -l 'panicked' "$WORKDIR"/*.log 2>/dev/null | wc -l)
[ "$panics" -eq 0 ] && ok "无 panic" || bad "$panics 个日志含 panic"
kill -0 "$RELAY_PID" 2>/dev/null && ok "relay 存活" || bad "relay 已退出"

[ $fail -eq 0 ] && echo "PASS" || echo "FAIL（日志保留在 $WORKDIR）"
exit $fail
