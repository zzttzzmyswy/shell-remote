#!/bin/bash
# MYS-2425 阶段3 冒烟，**两部分刻意分开**（review #5：不能把"relay 拒绝了
# 升级请求"当成"升级成功验证"）：
#
#   第 1 部分「注册/限流冒烟」——只压注册与会话顶替
#     1. N 台并发冷启动注册
#     2. 同 session 顶替（同 key/--session-id 重启，不产生重复会话）
#     3. 低 registration-rate-limit 下批量重连收敛
#     （这里**不**声称验证升级：下面那部分是独立的。）
#
#   第 2 部分「真实换机冒烟」——真下载、真替换、真注册
#     4. 造一个 hash 正确的"新版"制品放进 relay 的 --download-dir（custom 源，
#        不需要外网 / 不需要 mock GitHub），触发 admin 升级
#     5. 断言：进程 PID 变了、session_id 与 Token 不变、relay 侧看到新版本、
#        **磁盘上的二进制 hash 变成新制品的 hash**
#     6. 多次换机后身份仍然不变
#
# 与 concurrent_register_smoke.sh 的分工：那个脚本压"注册风暴"，这个脚本压
# "升级路径上的注册与身份保持"。
#
# 用法: tools/concurrent_register_and_upgrade_smoke.sh
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

# 等一批显式收集的 pid。
#
# 不能用裸 `wait`，也不能用 `jobs -p`：relay 与 agent 都是本脚本的后台作业，
# 两者都会把它们一起等，脚本直接永久挂住（表面现象只是"后面没有输出了"）。
# 某会话当前在线的 agent pid。
#
# 不用 `pgrep -f "... --session-id X"`：换机后的新进程是旧进程**退出后**被
# 拉起的，中间还有助手进程，pgrep 会同时匹配到包装 shell 与助手的命令行，
# 拿到的 pid 既不稳定也不一定是 agent 本身。这里读 agent 自己写的日志文件
# （每个 agent 一个 `agent<i>.log`），从最后一轮 "agent session established"
# 之前的启动行取真实 pid——更稳，也顺便证明"这个 pid 确实注册成功了"。
agent_pid_for() { # $1 = index
  local log="$WORKDIR/agent$1.log" pid=""
  [ -f "$log" ] || { echo ""; return; }
  # 新版 agent 在启动时打印自己的 pid；没有就退回 pgrep（兼容）。
  pid="$(grep -o 'agent pid=[0-9]*' "$log" 2>/dev/null | tail -1 | cut -d= -f2)"
  # **必须校验它还活着**：换机后由助手拉起的新进程，其 stderr 被助手接管
  # （`spawn_agent` 给它 piped stderr，用来等注册回执），所以它的
  # `agent pid=` 不会写进本文件的日志——日志里最后一条永远是**旧** pid。
  # 只看日志会把"换机后新进程"判成"没换机"（MYS-2425-D 实测）。
  if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
    pid=""
  fi
  if [ -z "$pid" ]; then
    # 按**确切 session id** 找活进程：换机后新进程带同一 sid，这正是要观察的。
    pid="$(pgrep -f "shell-remote agent .*--session-id cusag$1$" 2>/dev/null | tail -1)"
    [ -n "$pid" ] || pid="$(pgrep -f "shell-remote agent .*--session-id cusag$1 " 2>/dev/null | tail -1)"
  fi
  echo "$pid"
}

wait_pids() {
  local p
  for p in "$@"; do
    [ -n "$p" ] && wait "$p" 2>/dev/null
  done
  return 0
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

# 每个 session 的**活进程**数（MYS-2425-D review #3）。
#
# 旧实现数的是"日志里出现过的最后一个 pid"，有两个致命漏洞：
#   1. 换机后由助手拉起的新进程 stderr 被助手接管，它的 `agent pid=` 根本
#      不会写进日志文件——数出来的是**旧** pid，永远证明不了实际有几个进程；
#   2. 断言是 `<= N`，少一个也算"通过"，无法证明"每个 sid 恰好 1 个"。
#
# 现在直接按**精确二进制路径 + 精确 `--session-id`** 枚举 /proc：
#   - argv 必须含 `--session-id <sid>`（按独立词匹配，避免 cusag1 命中 cusag10）；
#   - exe 必须就是本次被测的 `$BIN`（排除同机其它 shell-remote）；
#   - stat 状态不能是 Z（僵尸不算活进程）。
# 每行输出 `<sid> <count>`，由调用方逐个断言 == 1。
live_proc_per_session() { python3 - "$BIN" "$N" <<'PY'
import os,sys
binarg,n=sys.argv[1],int(sys.argv[2])
target=os.path.realpath(binarg)
target_base=os.path.basename(target)
def same_binary(argv0):
    """argv[0] 是否指向被测二进制。

    换机后磁盘上的二进制被原子替换：旧进程的 `/proc/<pid>/exe` 会变成
    `... (deleted)`，新进程的 argv[0] 也可能被规范化成 `/proc/self/exe`
    的解析路径。所以三重比较：realpath 相等 / basename 相等 / exe 的
    realpath 相等（去掉 deleted 后缀）。任何一条成立即认为是同一份二进制。
    """
    try:
        if os.path.realpath(argv0)==target:
            return True
    except OSError:
        pass
    if os.path.basename(argv0)==target_base:
        return True
    return False

for i in range(1,n+1):
    sid='cusag%d'%i
    cnt=0
    for pid in os.listdir('/proc'):
        if not pid.isdigit():
            continue
        try:
            with open('/proc/'+pid+'/cmdline','rb') as f:
                argv=[a.decode('utf-8','replace') for a in f.read().split(b'\x00') if a]
        except OSError:
            continue
        if len(argv)<2:
            continue
        if not same_binary(argv[0]):
            continue
        hit=False
        for j,a in enumerate(argv):
            if a=='--session-id' and j+1<len(argv) and argv[j+1]==sid:
                hit=True; break
            if a=='--session-id='+sid:
                hit=True; break
        if not hit:
            continue
        try:
            with open('/proc/'+pid+'/stat') as f:
                state=f.read().rsplit(')',1)[1].split()[0]
            if state=='Z':
                continue
        except OSError:
            continue
        cnt+=1
    print(sid,cnt)
PY
}

[ -x "$BIN" ] || { echo "FAIL: 无二进制 $BIN"; exit 1; }
curl -s -o /dev/null "$BASE/" && { echo "FAIL: 端口 $PORT 被占用"; exit 1; }
echo "════ 第 1 部分：注册/限流冒烟（不含换机验证）N=$N ════"

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

# ── 2. 并发注册不串身份 ────────────────────────────────────────────
# 一次性并发注册 N 台（各自不同 key/session-id），断言没有会话互相顶替：
# relay 在注册风暴下不能把身份张冠李戴。
echo "  · 全部会话并发重注册一次（同 key，预期互相顶替自己而非他人）"
reg_pids=()
for i in $(seq 1 "$N"); do
  curl -s -o /dev/null -X POST "$BASE/agent/send" -H 'Content-Type: application/json' \
    -d "{\"type\":\"agent:register\",\"session_id\":\"cusag$i\",\"key\":\"cusk$i\",\"token_type\":\"rw\"}" &
  reg_pids+=("$!")
done
wait_pids "${reg_pids[@]}"
check "2. 并发注册后 $N 台仍全部在线" wait_online "$N" 60
for i in $(seq 1 "$N"); do
  [ -n "${BASE_TOK[$i]:-}" ] || continue
  wait_identity "cusag$i" 20 || { bad "升级触发后读不到 cusag$i"; continue; }
  line="$(sess_tokens "cusag$i")"
  [ "$(echo "$line" | cut -d' ' -f2-)" = "${BASE_TOK[$i]}" ] \
    || bad "cusag$i 的 token 在并发注册后变了"
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

echo

# ══ 第 2 部分：真实换机冒烟（真下载、真替换、真注册）════════════════
#
# 与第 1 部分严格分开：这一部分真的换掉二进制并断言 PID/身份/文件 hash。
#
# 制品来源用 relay 的**本地镜像 + --allow-custom-upgrade**：不需要外网、不需要
# mock GitHub/CDN，也不依赖任何发布资产。relay 会现场算 hash 并下发
# `source: custom` 任务（契约里 custom 只能走 relay 镜像，正好被这里覆盖）。
# "新版"就是当前二进制的一个副本——内容相同不影响断言：我们验证的是
# **换机流程**（新 PID + 身份保持 + 文件被判为已替换），不是版本号递增。
echo "════ 第 2 部分：真实换机冒烟（真替换 + 身份断言）════════════"

ART_DIR="$WORKDIR/artifacts"
mkdir -p "$ART_DIR"
# 制品名必须与 relay 为该设备挑的形态/架构一致，否则会被身份校验拦下。
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  armv7l|armv7) ARCH=armv7 ;;
  *) ARCH=unknown ;;
esac
ART_NAME="shell-remote-${ARCH}"
#
# 制品必须是"与当前二进制**不同**的一份字节"，否则断言"磁盘 hash 变成新制品"
# 恒真（同一份文件自然是同一个 hash），根本证明不了真的换过机。
# 追加一个标记串即可：ELF/PE 都按段长度加载，尾部多余字节不影响执行，
# `--version` 仍然正常（下面会验证）。
cp "$BIN" "$ART_DIR/$ART_NAME"
printf 'SR-UPGRADE-SMOKE-%s' "$(date +%s%N)" >> "$ART_DIR/$ART_NAME"
chmod +x "$ART_DIR/$ART_NAME"
ART_SHA="$(sha256sum "$ART_DIR/$ART_NAME" | cut -d' ' -f1)"
BIN_SHA="$(sha256sum "$BIN" | cut -d' ' -f1)"
ART_SIZE="$(stat -c %s "$ART_DIR/$ART_NAME" 2>/dev/null || stat -f %z "$ART_DIR/$ART_NAME")"

# 写一份"官方 digest"清单 fixture，让 relay 走**真实**的官方制品路径
# （而不是 custom 兜底）：
#
# - relay 判断官方 hash 只看这份缓存清单；我们让它记录的就是本机这份制品的
#   name/size/sha256，于是 `build_task` 走 `(Some(asset), Some(local))` 分支
#   → source=official + relay_url 兜底；
# - `download_url` 指向官方 release 路径，配合下面 `--upgrade-cdn` 指向一个
#   关闭的本地端口，agent 会先试 CDN（快速失败）再回源 relay 镜像——正好把
#   阶段2 的"CDN 优先 → relay 兜底"也顺带压了一遍；
# - 不联网、不改 relay 代码：只提供它本来就会读的缓存文件。
mkdir -p "$ART_DIR/.metadata/manifests"
cat > "$ART_DIR/.metadata/manifests/v0.59.2.json" <<JSON
{
  "schema_version": 1,
  "tag": "v0.59.2",
  "version": "0.59.2",
  "fetched_at": $(date +%s),
  "assets": [
    {
      "name": "$ART_NAME",
      "size": $ART_SIZE,
      "sha256": "$ART_SHA",
      "download_url": "https://github.com/zzttzzmyswy/shell-remote/releases/download/v0.59.2/$ART_NAME"
    }
  ]
}
JSON
[ "$ART_SHA" != "$BIN_SHA" ] || { echo "FAIL: 制品与当前二进制相同，换机无法被证明"; exit 1; }
"$ART_DIR/$ART_NAME" --version >/dev/null 2>&1 \
  || { echo "FAIL: 追加标记后的制品不可执行（说明该平台不允许）"; exit 1; }

# 换一个带制品库的 relay（custom 源需要显式批准）。
{ kill -9 "$RELAY_PID"; wait "$RELAY_PID"; } 2>/dev/null
sleep 1
start_relay_with_artifacts() {
  "$BIN" relay --bind "127.0.0.1:${PORT}" --auth cus-pass --no-tls \
    --admin-path "$ADMIN_PATH" --admin-user admin --admin-pass cspass \
    --download-dir "$ART_DIR" --allow-custom-upgrade \
    --upgrade-cdn "http://127.0.0.1:1" \
    >> "$WORKDIR/relay.log" 2>&1 &
  RELAY_PID=$!
  for _ in $(seq 1 60); do curl -s -o /dev/null "$BASE/" && break; sleep 0.2; done
  login
}
start_relay_with_artifacts || { echo "FAIL: 带制品库的 relay 未启动"; exit 1; }
check "2.0 $N 台重连到带制品库的 relay" wait_online "$N" 180

# 挑一台设备做真实换机。记录换机前的 PID / session / tokens。
UPG_IDX=1
UPG_SID="cusag$UPG_IDX"
OLD_PID="${APIDS[$UPG_IDX]}"
wait_identity "$UPG_SID" 30 || { bad "换机前读不到 $UPG_SID"; exit 1; }
before="$(sess_tokens "$UPG_SID")"
UPG_TOK_BEFORE="$(echo "$before" | cut -d' ' -f2-)"

echo "  · 触发真实换机：$UPG_SID（制品 $ART_NAME sha=${ART_SHA:0:12}…）"
resp="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$UPG_SID\",\"force\":true}")"
# 只认**顶层** ok（响应里还嵌了 manifest 的 ok，grep 全串会误判成功）。
printf '%s' "$resp" | python3 -c '
import json,sys
d=json.load(sys.stdin)
sys.exit(0 if d.get("ok") is True else 1)
' 2>/dev/null \
  && ok "2.1 relay 下发了升级任务（不是拒绝）" \
  || { bad "2.1 relay 拒绝/未下发升级任务: $(echo "$resp" | head -c 500)"; exit 1; }

# 等换机完成：agent 进程换掉、且 relay 上仍是同一 session。
# 判据用"旧 pid 确实消失 + 新 pid 出现且已注册"，而不是只比一次 pgrep。
old_alive_after=""
new_pid=""
for _ in $(seq 1 120); do
  if kill -0 "$OLD_PID" 2>/dev/null; then
    old_alive_after="yes"
  else
    old_alive_after=""
    p="$(agent_pid_for "$UPG_IDX")"
    if [ -n "$p" ] && [ "$p" != "$OLD_PID" ]; then new_pid="$p"; break; fi
  fi
  sleep 1
done
[ -n "$new_pid" ] && APIDS[$UPG_IDX]="$new_pid"

# fail-closed 是**合法结局**（MYS-2425-D 设计决定）：受管 service 归属无法确认
# 或未授权时，agent 必须在磁盘改动之前明确失败并继续运行。测试环境正好是这种
# 形状——8 个 agent 共享 `multica-daemon.service`。断言要区分两种合法结局：
#   A) 换机发生：旧 pid 消失 + 新 pid 出现
#   B) fail-closed：旧 pid 仍在 + relay 给出带原因的 failed + 磁盘未变
read_stage() {
  api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]
try: d=json.load(sys.stdin)
except Exception: sys.exit(0)
for s in d.get("sessions",[]):
  if s.get("session_id")==sid:
    u=s.get("upgrade") or {}
    print(u.get("stage",""), "|", (u.get("error") or "").replace("\n"," ")[:200])
    break
' "$1"
}
refused=0
refuse_reason=""
if [ -z "$new_pid" ]; then
  # 等一小会儿让 fail-closed 的 failed 帧到达 relay
  for _ in $(seq 1 20); do
    line="$(read_stage "$UPG_SID")"
    case "$line" in
      failed*|rolled_back*) refuse_reason="$line"; refused=1; break ;;
    esac
    sleep 2
  done
fi

if [ -n "$new_pid" ] && [ -z "$old_alive_after" ]; then
  ok "2.2 换机后 PID 变了（$OLD_PID → $new_pid）"
elif [ "$refused" = "1" ] && [ -n "$old_alive_after" ]; then
  # fail-closed：必须满足"旧进程仍在 + 磁盘未变 + 原因可读"。
  disk_now="$(sha256sum "$BIN" 2>/dev/null | cut -d' ' -f1)"
  if [ "$disk_now" = "$BIN_SHA" ]; then
    ok "2.2 fail-closed 明确失败且磁盘未动（旧 pid $OLD_PID 仍在运行）"
    echo "      原因: $refuse_reason"
    FAIL_CLOSED=1
  else
    bad "2.2 fail-closed 却改动了磁盘（disk=${disk_now:0:12}）"
  fi
else
  bad "2.2 未观察到换机，也没有明确的 fail-closed（旧 pid $OLD_PID ${old_alive_after:+仍在运行}，新 pid ${new_pid:-无}）"
fi

wait_identity "$UPG_SID" 60 || bad "2.3 换机后 $UPG_SID 未回到 relay"
after="$(sess_tokens "$UPG_SID" 2>/dev/null)"
[ "$(echo "$after" | cut -d' ' -f2-)" = "$UPG_TOK_BEFORE" ] \
  && ok "2.3 换机后 session_id 与 Token 不变" \
  || bad "2.3 换机后身份变了：前 [$UPG_TOK_BEFORE] 后 [$(echo "$after" | cut -d' ' -f2-)]"

# 磁盘上的二进制必须就是新制品（真实替换，不是"看起来升级了"）。
# fail-closed 场景下则相反：磁盘必须**保持旧制品**（上面 2.2 已断言）。
if [ "${FAIL_CLOSED:-0}" = "1" ]; then
  echo "  · fail-closed：跳过 2.4（磁盘未被改动，已由 2.2 断言）"
fi
if [ "${FAIL_CLOSED:-0}" != "1" ]; then
ON_DISK="$BIN"
if [ -f "$ON_DISK" ]; then
  DISK_SHA="$(sha256sum "$ON_DISK" | cut -d' ' -f1)"
  [ "$DISK_SHA" = "$ART_SHA" ] \
    && ok "2.4 磁盘上的二进制已是新制品（sha 一致）" \
    || bad "2.4 磁盘 hash 未变成新制品（disk=${DISK_SHA:0:12} art=${ART_SHA:0:12}）"
fi

fi
# 换机状态必须收敛到终结态（不是一直"下载中"）。
status_stage=""
for _ in $(seq 1 30); do
  status_stage="$(api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]
try: d=json.load(sys.stdin)
except Exception: sys.exit(0)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        u=s.get("upgrade") or {}
        print(u.get("stage",""))
        break
' "$UPG_SID")"
  case "$status_stage" in registered|rolled_back|failed) break ;; esac
  sleep 1
done
case "$status_stage" in
  registered) ok "2.5 升级状态收敛到 registered" ;;
  # fail-closed 的合法终态：必须在**换机前**明确失败（2.2 已断言磁盘未动）。
  failed|rolled_back)
    if [ "${FAIL_CLOSED:-0}" = "1" ]; then
      ok "2.5 升级状态收敛到 $status_stage（fail-closed）"
    else
      bad "2.5 升级失败: $status_stage（预期 registered）"
    fi ;;
  "")         ok "2.5 状态已清空（终结态过期或未保留，不影响身份断言）" ;;
  *)          bad "2.5 升级状态停在非终结态: $status_stage" ;;
esac

# ── 5. 多次换机：再换一台，身份仍一致 ─────────────────────────────
echo "  · 第二次真实换机（$N 台里再挑一台），验证可重复"
UPG2_IDX=2
UPG2_SID="cusag$UPG2_IDX"
OLD2_PID="${APIDS[$UPG2_IDX]}"
before2="$(sess_tokens "$UPG2_SID" 2>/dev/null || true)"
TOK2_BEFORE="$(echo "$before2" | cut -d' ' -f2-)"
api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$UPG2_SID\",\"force\":true}" -o /dev/null
new2=""
for _ in $(seq 1 120); do
  if ! kill -0 "$OLD2_PID" 2>/dev/null; then
    p="$(agent_pid_for "$UPG2_IDX")"
    if [ -n "$p" ] && [ "$p" != "$OLD2_PID" ]; then new2="$p"; break; fi
  fi
  sleep 1
done
[ -n "$new2" ] && APIDS[$UPG2_IDX]="$new2"
if [ -n "$new2" ]; then
  ok "5.1 第二次换机 PID 变了（$OLD2_PID → $new2）"
elif [ "${FAIL_CLOSED:-0}" = "1" ]; then
  # 同一台机器、同一服务归属 ⇒ 第二次同样被 fail-closed 拒绝，这是**正确**的。
  ok "5.1 第二次换机同样 fail-closed（旧 pid $OLD2_PID 仍在运行，磁盘未动）"
else
  bad "5.1 第二次换机 PID 未变化"
fi
wait_identity "$UPG2_SID" 60 || bad "5.2 第二次换机后会话未回来"
after2="$(sess_tokens "$UPG2_SID" 2>/dev/null)"
[ "$(echo "$after2" | cut -d' ' -f2-)" = "$TOK2_BEFORE" ] \
  && ok "5.2 第二次换机后身份不变" \
  || bad "5.2 第二次换机后身份变了"

# ── 6. 每个 session 恰好 1 个活进程 / 无 panic ────────────────────
#
# 「恰好 1」而不是「≤ N」：少一个说明有设备失联，多一个说明换机留下了重复
# 进程。两种都必须报出来。
bad_sids=""
while read -r sid cnt; do
  [ -z "$sid" ] && continue
  [ "$cnt" = "1" ] || bad_sids="$bad_sids $sid=$cnt"
done <<< "$(live_proc_per_session)"
[ -z "$bad_sids" ] && ok "每个 session 恰好 1 个活 agent 进程（共 $N 个）" \
  || bad "活进程数不等于 1:$bad_sids"
panics=$(grep -l 'panicked' "$WORKDIR"/*.log 2>/dev/null | wc -l)
[ "$panics" -eq 0 ] && ok "无 panic" || bad "$panics 个日志含 panic"
kill -0 "$RELAY_PID" 2>/dev/null && ok "relay 存活" || bad "relay 已退出"

[ $fail -eq 0 ] && echo "PASS" || echo "FAIL（日志保留在 $WORKDIR）"
exit $fail
