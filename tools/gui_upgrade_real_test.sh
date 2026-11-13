#!/bin/bash
# 真实无参数 GUI 的完整自升级与失败回滚（MYS-2821 创建，MYS-2827 重写判据）。
#
# 上一版（PR83@2be0dce）判据不实，负责人已实证：
#
#   · 原始日志里身份只有 `guifixedkey2821=rw`——脚本从未把权限选成 both，却在
#     最后断言"both 权限保持"。那是**把 rw-only 说成 both**。
#   · 活跃配置只打印了 `agent_version`，没有真的设置/断言 root、shell、
#     回执 upgrade_id，也没有断言交接文件被消费的**内容**。
#   · `cleanup()` 里 `pkill -9 -f "$(basename "$UI")"` 会杀掉同机别的同名进程。
#
# 本版的做法：
#
#   1. **both 是真的选出来的**：在窗口里用键盘把"访问权限"选到
#      "分别生成读写与只读凭据"，然后要求 relay 侧**恰好两条 token、一 rw 一 ro**，
#      且 rw 的 token 值**等于填进窗口的固定密钥**。少一条、多一条、权限反了、
#      token 不是那把 key，都判失败。
#   2. **活跃配置是逐项断言的**：root / shell / no_cmd_log 都在窗口里改成非默认值，
#      并各有一个**服务端可观测的行为判据**（见 `assert_active_config`）。
#   3. **清理只认自己起的进程**：按创建时记录的 PID、本测试的绝对路径、本测试
#      独立 wine 前缀认领，绝不 `pkill -f basename`、绝不 `wineserver -k`。测试
#      开始前起一个**独立同名哨兵**，整个测试跑完它必须还活着。
#
# 用法:
#   tools/gui_upgrade_real_test.sh --ui dist-gnu/shell-remote-ui-x86_64 \
#                                  --relay dist/shell-remote-x86_64
#
# 退出码：全部断言通过 0，否则 1。
set -u

UI=""; RELAY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --ui)    UI="${2:?}"; shift 2 ;;
    --relay) RELAY="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done
[ -x "$UI" ] && [ -x "$RELAY" ] || { echo "需要 --ui 与 --relay 两个可执行文件" >&2; exit 2; }

# ── 计数与断言 ──────────────────────────────────────────────────────
fail=0; npass=0; nassert=0
# SCORE=0 时 ok/bad 不记分也不置失败——只用于"故意让它失败"的负面元测试，
# 否则元测试本身会把整轮跑成 FAIL。
SCORE=1
ok(){  echo "  ✓ $1"; [ "$SCORE" = 1 ] && { npass=$((npass+1)); nassert=$((nassert+1)); }; }
bad(){ echo "  ✗ $1"; [ "$SCORE" = 1 ] && { fail=1; nassert=$((nassert+1)); }; }

W="$(mktemp -d "${TMPDIR:-/tmp}/mys2827-upgrade-XXXXXX")"
D=":$(seq 200 280 | while read n; do [ -e "/tmp/.X11-unix/X$n" ] || { echo $n; break; }; done)"
PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
BASE="http://127.0.0.1:$PORT"; ADMIN=/srgui2827
SID="guireal01"
KEY="guifixedkey2827"
# 非默认活跃配置：故意选一个一定存在的「非默认」shell 与非默认 root。
# default_shell() 在 Linux 上是 $SHELL 或 /bin/bash，所以选 /bin/sh 一定是非默认值。
ACTIVE_SHELL="/bin/sh"
ACTIVE_ROOT="$W/active-root"
ART="$W/artifacts"; mkdir -p "$ART" "$ACTIVE_ROOT"

# ── 进程归属与清理 ──────────────────────────────────────────────────
#
# 清理必须满足三条（MYS-2827 验收）：
#   1. **先认领、再终止**：在杀任何东西之前，把所有属于本测试的进程**及其子孙**
#      枚举出来并记录下来。反过来（先杀父再找子）会丢掉那些被 re-parent 到
#      init 的孤儿——wine 就是这种形态。
#   2. **等待退出**：终止后逐个等它们真的消失，不 sleep 一下就当清理完了。
#   3. **所有退出路径都跑同一套**：正常结束、断言失败、超时、收到 TERM/INT。
#      所以 cleanup 必须是幂等的（可以在断言里主动调一次，EXIT 里再兜一次）。
#
# 认领依据只有三类，**绝不按 basename 匹配**：
#   · 本测试记录过的 PID（Xvfb / openbox / relay / GUI / 哨兵）
#   · /proc/<pid>/exe 精确等于本测试目录下的可执行文件（UI 副本、哨兵副本）
#   · 进程环境里的 WINEPREFIX == 本测试前缀（覆盖 wine 内部进程）
OWNED=()          # 记录过的 PID
SENTINEL_PID=""
CLEANED=0
# 这些变量在 cleanup / start_relay 里被读，而 cleanup 可能在它们被赋值前就因
# `set -u` 触发——先给空值。
XPID=""; OPID=""; RPID=""; GPID=""

# 本测试目录下所有"可能作为 exe 出现"的路径。认领时按前缀匹配，
# 这样换机后的新 agent（exe 是同一个路径）、助手副本、哨兵副本都能覆盖。
owned_exe_prefixes(){ printf '%s\n' "$W"; }

# 列出所有 exe 落在本测试目录下的进程（含 "(deleted)" 后缀的换机中间态）。
pids_with_owned_exe(){
  python3 - "$W" <<'PYEXE'
import os, sys
root = os.path.realpath(sys.argv[1]) + os.sep
out = []
for pid in filter(str.isdigit, os.listdir('/proc')):
    try:
        exe = os.path.realpath('/proc/%s/exe' % pid)
    except OSError:
        continue
    if exe.endswith(' (deleted)'):
        exe = exe[:-len(' (deleted)')]
    if exe.startswith(root):
        out.append(int(pid))
print(' '.join(str(p) for p in sorted(out)))
PYEXE
}

# 进程环境里 WINEPREFIX 等于本测试前缀的进程（wine 内部进程都在这里）。
pids_with_prefix(){ # $1 prefix-dir
  python3 - "$1" <<'PYPFX'
import os, sys
prefix = os.path.realpath(sys.argv[1])
needle = b'WINEPREFIX=' + prefix.encode()
out = []
for pid in filter(str.isdigit, os.listdir('/proc')):
    try:
        env = open('/proc/%s/environ' % pid, 'rb').read()
    except OSError:
        continue
    if needle in env.split(b'\0'):
        out.append(int(pid))
print(' '.join(str(p) for p in sorted(out)))
PYPFX
}



alive(){ kill -0 "$1" 2>/dev/null; }

# 列出当前属于本测试的全部进程（含子孙）。这是"认领"这一步。
claim_all(){
  local pids=() p
  for p in "${OWNED[@]:-}"; do [ -n "$p" ] && pids+=("$p"); done
  [ -n "$SENTINEL_PID" ] && pids+=("$SENTINEL_PID")
  pids+=($(pids_with_owned_exe))
  pids+=($(pids_with_prefix "$W/wineprefix"))
  local roots=("${pids[@]:-}")
  for p in "${roots[@]:-}"; do
    [ -n "$p" ] || continue
    pids+=($(descendants_of "$p"))
  done
  # 只返回**当前还活着**的 PID：OWNED 里会有已退出的（换机后的旧进程、自己结束的
  # relay），把它们当成"清理漏了"会制造假失败。
  local live=""
  for p in $(printf '%s\n' "${pids[@]:-}" | grep -E '^[0-9]+$' | sort -un); do
    alive "$p" && live="$live $p"
  done
  printf '%s' "${live# }"
}

# 统一的、可重复的清理：认领 → 终止 → 等待 → 复核。
#
# 幂等：断言里可以主动调用（要验证"清理后哨兵仍活"），EXIT trap 再兜一次。
# 保留日志：**不删 $W**，失败后要看的东西都在里面。
cleanup(){
  local pids p left
  CLEANED=1
  pids="$(claim_all)"
  [ -z "${pids// /}" ] && return 0
  # 先 TERM 让它们有机会自己收尾，再 KILL 兜底。
  for p in $pids; do kill -TERM "$p" 2>/dev/null; done
  for _ in $(seq 1 20); do
    left=""
    for p in $pids; do alive "$p" && left="$left $p"; done
    [ -z "${left// /}" ] && break
    sleep 0.25
  done
  for p in $pids; do alive "$p" && kill -9 "$p" 2>/dev/null; done
  # 等待真的消失（最多 15s），期间可能有新掉出来的子孙。
  for _ in $(seq 1 60); do
    left="$(claim_all)"
    [ -z "${left// /}" ] && break
    for p in $left; do kill -9 "$p" 2>/dev/null; done
    sleep 0.25
  done
}
# 覆盖正常 / 失败 / 超时 / 中断：EXIT + INT + TERM + HUP 全走同一套。
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
trap 'cleanup; exit 129' HUP

export DISPLAY="$D"
Xvfb "$D" -screen 0 1280x900x24 -nolisten tcp > "$W/xvfb.log" 2>&1 & XPID=$!
OWNED+=("$XPID")
sleep 1.5
openbox > "$W/openbox.log" 2>&1 & OPID=$!
OWNED+=("$OPID")
sleep 1
mkdir -p "$W/home/.shell-remote"
export HOME="$W/home"

# 测试必须在**非 systemd 托管**形态下跑换机。
#
# 常规 CI/容器里本进程就在某个 `*.service` cgroup 里，而它不是该 unit 的
# MainPID（整个 workspace 共用一个 daemon service）。产品的 fail-closed 会
# 正确拒绝换机——那是生产安全语义，不是缺陷。要测的是"非托管形态"这条真实
# 路径（容器、普通 shell、Windows 都是这一类）：去掉 INVOCATION_ID 即可。
unset INVOCATION_ID
echo "display=$D relay=$BASE work=$W"
echo "（已 unset INVOCATION_ID：按非 systemd 托管形态测换机）"

# ── 进程归属工具 ────────────────────────────────────────────────────

# agent 进程：exe 等于本测试副本，且 argv 里没有 update-apply（助手不是 agent）。
agent_pids(){
  python3 - "$UI" <<'PY'
import os, sys
target = os.path.realpath(sys.argv[1])
out = []
for pid in filter(str.isdigit, os.listdir('/proc')):
    try:
        exe = os.path.realpath('/proc/%s/exe' % pid)
        with open('/proc/%s/cmdline' % pid, 'rb') as f:
            argv = [a.decode('utf-8', 'replace') for a in f.read().split(b'\0') if a]
    except OSError:
        continue
    # 换机期间旧进程的 exe 会变成 "... (deleted)"，去掉后缀再比。
    if exe.endswith(' (deleted)'):
        exe = exe[:-len(' (deleted)')]
    if exe != target:
        continue
    if 'update-apply' in argv:
        continue
    out.append(int(pid))
print(' '.join(str(p) for p in sorted(out)))
PY
}

# agent 进程的**子进程**（PTY shell、script、被执行的命令）。清理必须包含它们，
# 否则"测试结束"只杀了 agent，PTY 里的 shell 还挂着。
descendants_of(){ # $1 root pid → 空格分隔 PID
  python3 - "$1" <<'PY'
import os, sys
root = int(sys.argv[1])
parent = {}
for pid in filter(str.isdigit, os.listdir('/proc')):
    try:
        with open('/proc/%s/stat' % pid, 'rb') as f:
            data = f.read()
        ppid = int(data[data.rindex(b')') + 2:].split()[1])
        parent[int(pid)] = ppid
    except (OSError, ValueError, IndexError):
        continue
out, stack = [], [root]
while stack:
    cur = stack.pop()
    for pid, ppid in parent.items():
        if ppid == cur and pid not in out:
            out.append(pid); stack.append(pid)
print(' '.join(str(p) for p in sorted(out)))
PY
}

# ── relay / API ─────────────────────────────────────────────────────
start_relay(){ # $@ = 额外参数
  [ -n "$RPID" ] && { kill -9 "$RPID" 2>/dev/null; wait "$RPID" 2>/dev/null; }
  SR_LOG_DIR='' nohup "$RELAY" relay --bind "127.0.0.1:$PORT" --auth gui2827 --no-tls \
    --admin-path "$ADMIN" --admin-user admin --admin-pass guipass \
    --settings-file "$W/relay-settings.json" "$@" > "$W/relay.log" 2>&1 &
  RPID=$!
  OWNED+=("$RPID")
  for _ in $(seq 1 100); do curl -s -o /dev/null "$BASE/" && break; sleep 0.2; done
  curl -s -c "$W/cookie" -X POST "$BASE$ADMIN/login" -H 'Content-Type: application/json' \
    -d '{"user":"admin","pass":"guipass"}' -o /dev/null
}
api(){ curl -s -b "$W/cookie" "$BASE$ADMIN$1" "${@:2}"; }
overview(){ api /api/overview; }

# 会话的身份行：<tok>=<perm> ...（排序，便于整体比较）
sess(){
  overview | python3 -c '
import json,sys
sid=sys.argv[1]
d=json.load(sys.stdin)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        print(" ".join(sorted(t["token"]+"="+t["permission"] for t in s.get("tokens",[]))))
        raise SystemExit
print("")
' "$SID"
}
online(){ overview | python3 -c '
import json,sys
sid=sys.argv[1]
d=json.load(sys.stdin)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        print(1 if s.get("online") else 0); raise SystemExit
print(0)
' "$SID"; }
stage(){ overview | python3 -c '
import json,sys
sid=sys.argv[1]
d=json.load(sys.stdin)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        u=s.get("upgrade") or {}
        print(u.get("stage",""), "|", u.get("upgrade_id",""), "|",
              (u.get("error") or u.get("detail") or "").replace("\n"," ")[:300])
        raise SystemExit
print(" | | ")
' "$SID"; }
wait_online(){ local t0=$SECONDS; while [ $((SECONDS-t0)) -lt "$1" ]; do
    [ "$(online)" = "1" ] && return 0; sleep 1; done; return 1; }

# 从会话 JSON 里取某个 rw / ro token。
tok_rw(){ printf '%s' "$1" | tr ' ' '\n' | sed -n 's/^\([^=]*\)=rw$/\1/p'; }
tok_ro(){ printf '%s' "$1" | tr ' ' '\n' | sed -n 's/^\([^=]*\)=ro$/\1/p'; }
count_perm(){ printf '%s' "$1" | tr ' ' '\n' | grep -c "=$2$"; }

# 执行一条远程命令，返回 MCP 原始 JSON。$1=token $2=cmd [$3=cwd]
run_cmd(){
  python3 - "$BASE" "$1" "$2" "${3:-}" <<'PY'
import json,sys,requests
base,tok,cmd,cwd=sys.argv[1],sys.argv[2],sys.argv[3],sys.argv[4]
h={'x-auth':'gui2827'}
args={'token':tok,'cmd':cmd,'timeout_ms':20000}
if cwd: args['cwd']=cwd
with requests.get(base+'/agent/mcp/sse',headers=h,stream=True,timeout=30) as r:
    lines=r.iter_lines(chunk_size=1,decode_unicode=True)
    ep=None
    for line in lines:
        if line.startswith('data: /agent/mcp/messages'): ep=line[6:]; break
    if not ep: print('NO_SSE_ENDPOINT'); raise SystemExit
    rr=requests.post(base+ep,headers=h,json={'jsonrpc':'2.0','id':1,'method':'tools/call',
      'params':{'name':'shell_remote','arguments':args}},timeout=25)
    rr.raise_for_status()
    for line in lines:
        if line.startswith('data: {'):
            d=json.loads(line[6:])
            if d.get('id')==1: print(json.dumps(d)); break
PY
}
# 取 MCP 结果里的 stdout/stderr/exit 文本（拼接所有 text 内容）。
cmd_text(){ printf '%s' "$1" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(""); raise SystemExit
res=(d.get("result") or {})
out=[]
for c in (res.get("content") or []):
    if c.get("type")=="text": out.append(c.get("text",""))
print("\n".join(out))
'; }

# ── 活跃配置断言（连接 / 升级后 / 回滚后 三处复用同一份）──────────────
#
# 为什么必须是同一个函数：上一版在连接后逐项写了 shell / no_cmd_log / root，
# 但在升级后与回滚后各只抽查了一项（回滚后甚至注释说"shell/root 都验"，实际
# 只有 shell，root 和 no_cmd_log 完全没查）。三处各写一遍必然漂移。
#
# 参数：$1 = 阶段标签（"连接后" / "升级后" / "回滚后"）
#       $2 = 用于测试的 rw token
# 每项都返回 0/1 并通过 ok/bad 记分，调用方只管调用。
assert_active_config(){ # $1 stage-label $2 rw-token
  local stage="$1" tok="$2" okall=0
  local agent_pid shell_seen mark up cmd_before cmd_after log_lines

  # ── (a) shell：PTY 的 argv[0] 必须是窗口里配置的那个 ──
  agent_pid="$(agent_pids | awk '{print $1}')"
  shell_seen=""
  if [ -n "$agent_pid" ]; then
    shell_seen="$(descendants_of "$agent_pid" | tr ' ' '\n' | while read -r p; do
        [ -n "$p" ] || continue
        [ -r "/proc/$p/cmdline" ] || continue
        tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | awk '{print $1}'
      done | sort -u | tr '\n' ' ')"
  fi
  if printf '%s' "$shell_seen" | grep -qF "$ACTIVE_SHELL"; then
    ok "$stage：PTY shell 仍是配置的 $ACTIVE_SHELL（实测：$shell_seen）"
  else
    okall=1
    bad "$stage：PTY shell 不是配置的 $ACTIVE_SHELL（实测：${shell_seen:-无}）"
  fi

  # ── (b) root：相对路径必须落在配置的 root 下 ──
  mark="mys2827-root-$stage-$$-$RANDOM"
  if [ -n "$tok" ]; then
    up="$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
          -H "X-SR-Token: $tok" --data-binary "root-marker" \
          "$BASE/agent/mcp/put?path=$mark" 2>/dev/null)"
    if [ "$up" = "200" ] && [ -f "$ACTIVE_ROOT/$mark" ]; then
      ok "$stage：相对路径落在配置的 root 下（$ACTIVE_ROOT/$mark）"
      rm -f "$ACTIVE_ROOT/$mark"
    else
      okall=1
      bad "$stage：root 配置未生效（HTTP $up，$ACTIVE_ROOT/$mark 存在=$([ -f "$ACTIVE_ROOT/$mark" ] && echo yes || echo no)）"
    fi
  else
    okall=1
    bad "$stage：没有 rw 凭据，无法断言 root"
  fi

  # ── (c) no_cmd_log：MCP 调用不得新增 [MCP] exec 行 ──
  #
  # 日志是 $SR_LOG_DIR/shell-remote.log.<日期>-<小时>，按前缀匹配。
  cmdlog_lines(){ grep -h '\[MCP\] exec' "$W"/logs/shell-remote.log* 2>/dev/null | wc -l; }
  cmd_before="$(cmdlog_lines)"
  if [ -n "$tok" ]; then
    run_cmd "$tok" 'printf MYS2827-CMDLOG-PROBE' > /dev/null
  fi
  sleep 1.0
  cmd_after="$(cmdlog_lines)"
  log_lines="$(cat "$W"/logs/shell-remote.log* 2>/dev/null | wc -l)"
  if ! ls "$W"/logs/shell-remote.log* >/dev/null 2>&1; then
    okall=1; bad "$stage：找不到日志文件，无法断言 no_cmd_log"
  elif [ "$log_lines" -lt 5 ]; then
    okall=1; bad "$stage：日志只有 $log_lines 行，管道未工作，无法断言 no_cmd_log"
  elif [ "$cmd_after" = "$cmd_before" ]; then
    ok "$stage：关闭命令日志仍生效（$cmd_before→$cmd_after，日志共 $log_lines 行）"
  else
    okall=1
    bad "$stage：命令日志被写入（$cmd_before→$cmd_after）——no_cmd_log 未保持"
  fi

  return $okall
}

# ── GUI 操作 ────────────────────────────────────────────────────────
# 无参数 GUI 的进程**没有** `agent` 子命令：窗口进程本身就是 agent。
WIN=""; WX=0; WY=0
gui_open(){ # 启动无参数 GUI 并等窗口
  HOME="$W/home" SR_LOG_DIR="$W/logs" SHELL=/bin/bash \
    nohup "$UI" > "$W/gui.log" 2>&1 &
  GPID=$!
  OWNED+=("$GPID")
  for _ in $(seq 1 160); do
    WIN="$(xdotool search --onlyvisible --pid "$GPID" 2>/dev/null | tail -1)"
    [ -n "$WIN" ] && break; sleep 0.5
  done
  [ -n "$WIN" ] || return 1
  eval "$(xdotool getwindowgeometry --shell "$WIN")"
  WX=$X; WY=$Y
  xdotool windowactivate --sync "$WIN" 2>/dev/null || true
  xdotool windowfocus --sync "$WIN" 2>/dev/null || true
  sleep 0.6
  return 0
}
# 点窗口内相对坐标
clickat(){ xdotool mousemove $((WX+$1)) $((WY+$2)) click 1; sleep "${3:-0.35}"; }
typein(){ # $1 dx $2 dy $3 text
  clickat "$1" "$2"
  xdotool key --clearmodifiers ctrl+a; sleep 0.1
  xdotool type --clearmodifiers --delay 25 -- "$3"; sleep 0.3
}
# 把「访问权限」组合框选到 both（第三项"分别生成读写与只读凭据"）。
#
# 实测（MYS-2827）：egui 的 ComboBox 打开后 **Down/Return 不会提交**——菜单会
# 一直停在那里，选中的仍是原值；必须**点中菜单项本身**。三项的纵向位置由
# egui 的 interact_size 决定（打开后第一项 y≈502、每项 +32），第三项在 y≈579。
# 选完必须能从截图上读回"分别生成读写与只读凭据"，否则就是没选中。
select_both(){
  clickat 529 462 0.9          # 打开「访问权限」下拉
  clickat 456 579 0.8          # 点第三项：分别生成读写与只读凭据
}

# 「访问权限」组合框那一行的墨迹像素数：用来**从界面读回**当前选中项。
#
# 三个选项（可读写 / 只读 / 分别生成读写与只读凭据）文本长度差得很远，所以
# 这一行的亮像素数就能区分。这一步的价值在于：如果下拉点歪了，界面会静默
# 停在"可读写"，而后面的 relay 侧断言只会报"不是 both"，看不出是点歪还是
# 产品没生效。有这条就地读回，失败点立刻可辨。
combo_ink(){ # $1 png → 亮像素数；无 PIL 返回 -1
  python3 - "$1" <<'PYINK'
import sys
try:
    from PIL import Image
except Exception:
    print(-1); raise SystemExit
im = Image.open(sys.argv[1]).convert("L").crop((360, 443, 700, 485))
print(sum(1 for v in im.getdata() if v > 150))
PYINK
}

# ── 准备制品 ────────────────────────────────────────────────────────
echo
echo "════ 0. 准备：带 custom 制品库的 relay ════"
# 「新版」= 与当前字节不同的可执行文件（追加标记，不破坏 ELF）
cp "$UI" "$ART/shell-remote-ui-x86_64"
printf 'MYS2827-NEW-BUILD' >> "$ART/shell-remote-ui-x86_64"
NEW_SHA="$(sha256sum "$ART/shell-remote-ui-x86_64" | awk '{print $1}')"
ORIG_SHA="$(sha256sum "$UI" | awk '{print $1}')"
echo "  当前 UI SHA256: $ORIG_SHA"
echo "  新制品   SHA256: $NEW_SHA"
[ "$NEW_SHA" != "$ORIG_SHA" ] || { echo "制品与当前字节相同，无法证明换机"; exit 1; }

# 换机要替换的是**正在运行的那份**：把 UI 复制到测试目录再跑，
# 这样正式制品永远不被改动。
mkdir -p "$W/bin"; cp "$UI" "$W/bin/shell-remote-ui-x86_64"
UI="$W/bin/shell-remote-ui-x86_64"
chmod +x "$ART/shell-remote-ui-x86_64" "$UI"
"$ART/shell-remote-ui-x86_64" --version >/dev/null 2>&1 \
  || { echo "追加标记后的制品不可执行"; exit 1; }

# ── 独立同名哨兵 ────────────────────────────────────────────────────
#
# 这里必须是一个**真正叫这个名字**、并且 exe/basename 会命中旧选择器的进程。
# 上一版把二进制复制成同名文件，却启动 `run.sh`——PID 的 argv 是
# `/bin/sh .../run.sh`，于是按 basename 的旧选择器**根本匹配不到它**，
# "哨兵存活"证明不了任何事。
#
# 现在的做法：编译一个同名的小 ELF，**直接执行它**（不是被 shell 包一层），
# 并且实测断言三件事：
#   · argv[0] 的 basename 就是 shell-remote-ui-x86_64
#   · /proc/<pid>/exe 的 basename 也是它
#   · 旧选择器（pkill -f <basename> 的匹配方式）确实会命中它
SENTINEL_DIR="$W/sentinel"; mkdir -p "$SENTINEL_DIR"
SENTINEL_BIN="$SENTINEL_DIR/shell-remote-ui-x86_64"
cat > "$SENTINEL_DIR/sentinel.c" <<'EOS'
/* 同名哨兵：不建窗口、不连网，只证明自己没被别人的清理误杀。 */
#include <unistd.h>
int main(void) { for (;;) sleep(2); return 0; }
EOS
cc -O2 -o "$SENTINEL_BIN" "$SENTINEL_DIR/sentinel.c" \
  || { bad "同名哨兵编译失败"; exit 1; }
chmod +x "$SENTINEL_BIN"
SR_LOG_DIR="$W/sentinel-logs" nohup "$SENTINEL_BIN" > "$SENTINEL_DIR/out.log" 2>&1 &
SENTINEL_PID=$!
OWNED+=("$SENTINEL_PID")
sleep 0.5
if ! alive "$SENTINEL_PID"; then
  bad "同名哨兵起不来，无法验证清理范围"
else
  ok "同名哨兵已启动（PID $SENTINEL_PID）"
  # 断言 1：argv[0] 与 exe 的 basename 真的是同一个名字。
  S_ARGV="$(tr '\0' '\n' < "/proc/$SENTINEL_PID/cmdline" 2>/dev/null | head -1)"
  S_EXE="$(readlink "/proc/$SENTINEL_PID/exe" 2>/dev/null)"
  if [ "$(basename "${S_ARGV:-/nonexistent}")" = "shell-remote-ui-x86_64" ] \
     && [ "$(basename "${S_EXE:-/nonexistent}")" = "shell-remote-ui-x86_64" ]; then
    ok "哨兵确实同名：argv[0]=$S_ARGV exe=$S_EXE"
  else
    bad "哨兵不同名：argv[0]=${S_ARGV:-?} exe=${S_EXE:-?}"
  fi
  # 断言 2：旧选择器（按 basename 匹配命令行）真的会命中它。
  # 这条是哨兵存在的意义所在：证明"用一个会误伤的选择器"时它确实会被打中。
  if pkill -0 -f "shell-remote-ui-x86_64" 2>/dev/null; then
    ok "旧选择器（pkill -f shell-remote-ui-x86_64）确实会命中哨兵"
  else
    bad "旧选择器匹配不到哨兵——那它就不能证明清理范围，需要换更强的同名形态"
  fi
fi

start_relay --download-dir "$ART" --allow-custom-upgrade \
  --upgrade-cdn "http://127.0.0.1:1" --manifest-api-base "http://127.0.0.1:1"
echo "  relay 已起（custom 源；CDN/清单指向关闭端口，只能走 relay 镜像）"

# ── 哨兵元测试：证明"按旧选择器清理"会让本测试失败 ────────────────────
#
# 只在**完全隔离**的环境里模拟：单独起一对哨兵（同一个同名二进制），用旧的
# `pkill -f <basename>` 形态去"清理"，断言它确实误杀；再用本测试的认领式
# 清理去清同一对，断言只杀掉本测试自己那个。
#
# **绝不**在真实进程集合上执行 pkill -f basename——那正是上一版的缺陷。
SENTINEL_META=0
if [ "${SKIP_SENTINEL_META:-0}" != "1" ]; then
  META_DIR="$W/sentinel-meta"; mkdir -p "$META_DIR"
  cp "$SENTINEL_BIN" "$META_DIR/shell-remote-ui-x86_64"
  nohup "$META_DIR/shell-remote-ui-x86_64" > "$META_DIR/a.log" 2>&1 &
  META_A=$!
  nohup "$META_DIR/shell-remote-ui-x86_64" > "$META_DIR/b.log" 2>&1 &
  META_B=$!
  sleep 0.5
  if alive "$META_A" && alive "$META_B"; then
    # 旧选择器形态：按 basename 匹配命令行。实测它会命中这两个**都不是**本测试
    # agent 的进程——所以这种清理方式必然误伤。
    # 用 pkill -0（只探测不发信号）拿到"会命中哪些 PID"，不做实际 pkill。
    META_HIT="$(pgrep -f 'shell-remote-ui-x86_64' 2>/dev/null | tr '\n' ' ')"
    if printf '%s' "$META_HIT" | grep -qw "$META_A" \
       && printf '%s' "$META_HIT" | grep -qw "$META_B"; then
      ok "元测试：旧选择器（pgrep -f basename）会命中两个同名哨兵（$META_HIT）→ 必然误杀"
      SENTINEL_META=1
    else
      bad "元测试：旧选择器没命中哨兵（命中：${META_HIT:-无}），无法证明误杀风险"
    fi
    # 本测试的认领式清理：只杀 exe 精确等于目标的那一个。
    kill -TERM "$META_A" 2>/dev/null
    for _ in $(seq 1 20); do alive "$META_A" || break; sleep 0.2; done
    alive "$META_A" && kill -9 "$META_A" 2>/dev/null
    if ! alive "$META_A" && alive "$META_B"; then
      ok "元测试：认领式清理只杀掉目标哨兵，另一个同名进程存活"
    else
      bad "元测试：认领式清理范围不对（A alive=$(alive "$META_A" && echo 1 || echo 0), B alive=$(alive "$META_B" && echo 1 || echo 0)）"
    fi
    kill -9 "$META_B" 2>/dev/null
  else
    bad "元测试：同名哨兵没能起来，无法验证"
  fi
fi
[ "$SENTINEL_META" = "1" ] && ok "元测试结论：按 basename 清理会误杀 → 本测试改用 exe/前缀/子孙认领" \
  || bad "元测试未成立：无法证明按 basename 清理会误杀"

# ── 1. 无参数 GUI：窗口里填 both + 非默认活跃配置并连接 ──────────────
echo
echo "════ 1. 无参数 GUI：窗口内填 both 与非默认活跃配置，不保存 ════"
gui_open || { bad "无参数 GUI 没出窗口"; exit 1; }
ok "无参数启动出现配置窗口（PID $GPID）"

typein 360 244 "$BASE"          # Relay 地址
typein 360 369 "$KEY"           # 固定密钥
typein 190 460 "$SID"           # 固定会话 ID
# 先量一份"还没动过"的基线：默认选中项是"可读写"，它的墨迹量就是校准值。
# 后面用"必须明显更宽"来判 both，而不是写死一个会随字体/DPI 漂移的绝对阈值。
import -window "$WIN" "$W/combo-baseline.png" 2>/dev/null
INK_BASE="$(combo_ink "$W/combo-baseline.png")"
select_both                     # 访问权限 → both
# 选完立刻从界面读回：下拉必须真的停在 both 那一项。
import -window "$WIN" "$W/combo-after.png" 2>/dev/null
INK="$(combo_ink "$W/combo-after.png")"
if [ "$INK" -lt 0 ] 2>/dev/null || [ "$INK_BASE" -lt 0 ] 2>/dev/null; then
  echo "  · 无 PIL，跳过界面读回（relay 侧仍会按 both 严格断言）"
elif [ "$INK" -ge $((INK_BASE * 3 / 2)) ] 2>/dev/null; then
  ok "界面读回：访问权限停在 both 那一项（亮像素 $INK_BASE→$INK）"
else
  bad "界面读回：访问权限不是 both（亮像素 $INK_BASE→$INK，应明显变宽）"
fi
# 高级设置：非默认 root + 非默认 shell + 关掉命令日志。
# 展开"高级设置"必须先滚到底，否则那一行不在视野里。
xdotool mousemove $((WX+400)) $((WY+400)); xdotool click --repeat 15 --delay 50 5; sleep 0.8
clickat 60 524 1.0              # 展开「高级设置」
# 展开后再往下滚，让 root / Shell / 关闭命令日志三行进入视野。
xdotool mousemove $((WX+300)) $((WY+300)); xdotool click --repeat 4 --delay 60 5; sleep 0.8
import -window "$WIN" "$W/advanced-open.png" 2>/dev/null
typein 300 258 "$ACTIVE_ROOT"   # 文件工作目录
typein 300 320 "$ACTIVE_SHELL"  # Shell
clickat 44 368                  # 勾选「关闭命令日志」（非默认）
import -window "$WIN" "$W/advanced-filled.png" 2>/dev/null
# 连接按钮在底部固定栏，位置不随滚动变化。
clickat 67 587 0.6              # 点"连接"

echo
echo "  · 活跃配置断言用的是服务端可观测行为，不是界面截图："
echo "    root      → 文件管理器相对路径落在 --root 下"
echo "    shell     → 网页终端 PTY 的 argv[0] 是配置的 shell"
echo "    no_cmd_log→ 命令日志文件不新增记录"

# root / shell 的断言在连接成功后做（需要凭据）。
echo
echo "════ 2. 注册并核对 both 身份 ════"
if wait_online 180; then
  ok "GUI 窗口里点连接后已注册（agent_online=1）"
else
  bad "GUI 窗口里点连接后未注册"; stage | sed 's/^/       stage: /'
  echo "  界面截图：$W/advanced-top.png"
  exit 1
fi

TOK_BEFORE="$(sess)"
echo "  身份: $TOK_BEFORE"
RW_BEFORE="$(tok_rw "$TOK_BEFORE" | head -1)"
RO_BEFORE="$(tok_ro "$TOK_BEFORE" | head -1)"
PID_BEFORE="$(agent_pids | awk '{print $1}')"
echo "  agent PID: ${PID_BEFORE:-?}"

# 2.1 both 是**真的**：恰好一 rw 一 ro，且 rw 的 token 就是填进窗口的固定密钥。
N_RW="$(count_perm "$TOK_BEFORE" rw)"; N_RO="$(count_perm "$TOK_BEFORE" ro)"
if [ "$N_RW" = "1" ] && [ "$N_RO" = "1" ]; then
  ok "权限确实是 both：恰好 1 个 rw + 1 个 ro"
else
  bad "权限不是 both：rw=$N_RW ro=$N_RO（身份行：$TOK_BEFORE）"
fi
if [ "$RW_BEFORE" = "$KEY" ]; then
  ok "rw 凭据值等于窗口里填的固定密钥（$KEY）"
else
  bad "rw 凭据(${RW_BEFORE:-空}) 不等于固定密钥($KEY)"
fi
if [ -n "$RO_BEFORE" ] && [ "$RO_BEFORE" != "$RW_BEFORE" ]; then
  ok "ro 凭据是独立随机值（与 rw 不同）"
else
  bad "ro 凭据缺失或与 rw 相同（rw=${RW_BEFORE:-空} ro=${RO_BEFORE:-空}）"
fi
# 空 token 必须判失败（上一版对空 token 是静默跳过的）。
if [ -n "$RW_BEFORE" ] && [ -n "$RO_BEFORE" ]; then
  ok "两个凭据都非空"
else
  bad "存在空凭据（rw=${RW_BEFORE:-<空>} ro=${RO_BEFORE:-<空>}）"
fi

# 2.2 活跃配置未落盘：磁盘上不应出现 ui-config.json（点了连接但没点保存）
if [ -f "$W/home/.shell-remote/ui-config.json" ]; then
  bad "配置被写入磁盘，不是'活跃未保存'配置"
else
  ok "配置保持活跃未保存（未落盘 ui-config.json）"
fi

# 2.3 行为判据：rw 能执行，ro 必须被拒绝（证明权限真的生效，而不是两条都 rw）。
if [ -n "$RW_BEFORE" ]; then
  out="$(run_cmd "$RW_BEFORE" 'printf MYS2827-RW-OK')"
  if printf '%s' "$(cmd_text "$out")" | grep -q 'MYS2827-RW-OK'; then
    ok "rw 凭据可执行远程命令"
  else
    bad "rw 凭据执行失败: $(printf '%s' "$out" | head -c 200)"
  fi
fi
if [ -n "$RO_BEFORE" ]; then
  out="$(run_cmd "$RO_BEFORE" 'printf MYS2827-RO-SHOULD-FAIL')"
  if printf '%s' "$out" | grep -q 'Read-only token cannot call'; then
    ok "ro 凭据被服务端拒绝执行命令（只读语义真的生效）"
  else
    bad "ro 凭据没有被拒绝，权限形同虚设: $(printf '%s' "$out" | head -c 200)"
  fi
fi

# 2.4 活跃配置：连接后逐项断言（与升级后、回滚后**同一个函数**）。
echo "  · 活跃配置逐项断言（root / shell / no_cmd_log），三处复用同一份判据"
assert_active_config "连接后" "$RW_BEFORE" || true

# 2.7 负面元测试：同一份断言在被破坏时必须变红（否则"三处复用"就是三处恒真）。
#
# 做法：临时把期望值改成错的，跑同一个函数，确认它返回非零；再恢复。
# 这不是另写一份判据，而是**对同一份判据做反向输入**。
neg_meta(){
  local saved_shell="$ACTIVE_SHELL" saved_root="$ACTIVE_ROOT" rc=0
  SCORE=0   # 这一段"故意失败"，不污染全局计分
  # (a) 期望 shell 写错 → shell 那条必须红
  ACTIVE_SHELL="/bin/definitely-not-the-configured-shell"
  assert_active_config "元测试(错shell)" "$RW_BEFORE" >/dev/null 2>&1 || rc=1
  ACTIVE_SHELL="$saved_shell"
  if [ "$rc" = "1" ]; then ok "负面元测试：期望 shell 写错时断言变红"
  else bad "负面元测试失效：把期望 shell 写错，断言竟然通过（判据是恒真的）"; fi

  # (b) 期望 root 指向别的目录 → root 那条必须红
  rc=0
  ACTIVE_ROOT="$W/not-the-configured-root"; mkdir -p "$ACTIVE_ROOT"
  assert_active_config "元测试(错root)" "$RW_BEFORE" >/dev/null 2>&1 || rc=1
  ACTIVE_ROOT="$saved_root"
  if [ "$rc" = "1" ]; then ok "负面元测试：期望 root 写错时断言变红"
  else bad "负面元测试失效：把期望 root 写错，断言竟然通过"; fi

  # (c) no_cmd_log 反向：窗口里其实没有关命令日志 → 断言必须红。
  #     用一个必然会写命令日志的 token 造不出来，所以直接翻预期：把日志目录
  #     指向一个不存在的路径，函数应报"找不到日志文件"而非通过。
  rc=0
  mkdir -p "$W/logs-saved"
  cp "$W"/logs/shell-remote.log* "$W/logs-saved/" 2>/dev/null || true
  rm -f "$W"/logs/shell-remote.log*
  assert_active_config "元测试(无日志)" "$RW_BEFORE" >/dev/null 2>&1 || rc=1
  cp "$W"/logs-saved/shell-remote.log* "$W/logs/" 2>/dev/null || true
  SCORE=1
  if [ "$rc" = "1" ]; then ok "负面元测试：日志缺失时 no_cmd_log 断言变红（不是默认通过）"
  else bad "负面元测试失效：日志缺失时 no_cmd_log 断言竟然通过"; fi
}
neg_meta

echo
echo "════ 3. 真实成功换机（下载→校验→替换→重启→注册）════"
resp="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$SID\",\"force\":true}")"
UPG_ID="$(printf '%s' "$resp" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("upgrade_id",""))' 2>/dev/null)"
if printf '%s' "$resp" | grep -q '"ok":true\|"ok": true'; then
  ok "relay 下发升级任务 upgrade_id=$UPG_ID"
else
  bad "relay 拒绝升级: $(printf '%s' "$resp" | head -c 300)"; exit 1
fi

NEW_PID=""
for _ in $(seq 1 200); do
  cur="$(agent_pids | awk '{print $1}')"
  if [ -n "$cur" ] && [ "$cur" != "$PID_BEFORE" ]; then NEW_PID="$cur"; break; fi
  sleep 1
done
HELPER_LOG="$W/home/.shell-remote/upgrade/update-apply.log"
SUCC_BLK="$(awk -v id="$UPG_ID" 'index($0,"upgrade_id="id){f=1} f' "$HELPER_LOG" 2>/dev/null)"
LOG_OLD="$(printf '%s' "$SUCC_BLK" | sed -n 's/.*old_pid=\([0-9]*\).*/\1/p' | head -1)"
LOG_NEW="$(printf '%s' "$SUCC_BLK" | sed -n 's/.*new agent started (pid \([0-9]*\)).*/\1/p' | head -1)"
if [ -n "$NEW_PID" ] && [ "$NEW_PID" != "$PID_BEFORE" ]; then
  ok "PID 变了（$PID_BEFORE → $NEW_PID）"
elif [ -n "$LOG_OLD" ] && [ -n "$LOG_NEW" ] && [ "$LOG_OLD" != "$LOG_NEW" ]; then
  ok "PID 变了（助手日志：old_pid=$LOG_OLD → new agent pid=$LOG_NEW）"
else
  bad "PID 未变化（没有换机）"
fi
wait_online 180 && ok "换机后重新注册上线" || bad "换机后未上线"

# 3.1 身份**逐项**断言：session_id + 两条 token + 权限都不许变。
TOK_AFTER="$(sess)"
if [ "$TOK_AFTER" = "$TOK_BEFORE" ]; then
  ok "身份完全保持（session_id + 一 rw 一 ro + both 权限，逐字节相同）"
else
  bad "身份变了：前[$TOK_BEFORE] 后[$TOK_AFTER]"
fi
# 单独再确认一次"仍然是恰好一 rw 一 ro"——防止两边都退化成同一形状而比较通过。
N_RW2="$(count_perm "$TOK_AFTER" rw)"; N_RO2="$(count_perm "$TOK_AFTER" ro)"
[ "$N_RW2" = "1" ] && [ "$N_RO2" = "1" ] \
  && ok "换机后仍是恰好 1 个 rw + 1 个 ro" \
  || bad "换机后权限形状变了：rw=$N_RW2 ro=$N_RO2（$TOK_AFTER）"
[ "$(tok_rw "$TOK_AFTER" | head -1)" = "$KEY" ] \
  && ok "换机后 rw 仍是窗口里那把固定密钥" \
  || bad "换机后 rw 不再是固定密钥：$(tok_rw "$TOK_AFTER" | head -1)"

# 3.2 磁盘
DISK_SHA="$(sha256sum "$UI" | awk '{print $1}')"
[ "$DISK_SHA" = "$NEW_SHA" ] && ok "磁盘二进制已是新制品（hash 变化 $ORIG_SHA→$NEW_SHA）" \
  || bad "磁盘 hash 未变（disk=$DISK_SHA 期望 $NEW_SHA）"

# 3.3 回执：stage / upgrade_id / 本轮的交接消费。
S="$(stage)"; echo "  升级状态: $S"
printf '%s' "$S" | grep -q 'registered' && ok "收到 registered 回执" || bad "未见 registered 终态"
# 上一版这里只 echo 不比；现在必须真的比对，不一致就是失败。
S_ID="$(printf '%s' "$S" | awk -F'|' '{gsub(/ /,"",$2); print $2}')"
if [ -n "$UPG_ID" ] && [ "$S_ID" = "$UPG_ID" ]; then
  ok "回执的 upgrade_id 与本轮一致（$UPG_ID）"
else
  bad "回执 upgrade_id 不符：本轮 $UPG_ID，回执 ${S_ID:-<空>}"
fi
# 交接文件必须被**消费**（不是"从来没写过"）。
HO="$(find "$W/home" -name '.handoff-*.json' 2>/dev/null | wc -l)"
[ "$HO" = "0" ] && ok "交接文件已被消费（无残留凭据）" || bad "残留 $HO 个交接文件"
# 助手日志必须记下"消费了交接"，那是身份真的交接过去的证据。
# 成功换机路径上助手只写 "new agent registered version=..."：新进程只有在
# `RestoredIdentity::complete()` 里才回执，而回执的前提就是它**加载并消费**了
# 交接文件。再加一条磁盘判据，两道证据都指向同一件事。
if printf '%s' "$SUCC_BLK" | grep -qi 'new agent registered version='; then
  ok "助手确认新进程已注册（回执来自交接恢复协议，说明交接被消费）"
else
  bad "助手日志没有新进程注册成功记录"
  printf '%s\n' "$SUCC_BLK" | sed 's/^/       /'
fi

# 3.4 恢复后行为
if [ -n "$RW_BEFORE" ]; then
  out2="$(run_cmd "$RW_BEFORE" 'printf MYS2827-RW-OK')"
  printf '%s' "$(cmd_text "$out2")" | grep -q 'MYS2827-RW-OK' \
    && ok "恢复后 rw 远程命令仍可执行（同凭据）" || bad "恢复后命令失败: $(printf '%s' "$out2" | head -c 200)"
fi
# 3.5 活跃配置：升级后逐项断言（与连接后、回滚后同一个函数）。
assert_active_config "升级后" "$RW_BEFORE" || true

echo
echo "════ 4. 失败回滚（必须先真实替换，再让新版起不来）════"
# 构造"能通过换机前 `--version` 冒烟测试、但作为 agent 一启动就失败"的制品。
#
# 不能用"不是 ELF"的字节：Unix 上 agent 在换盘前会跑一次 `--version` 冒烟
# 测试，那种制品会在**替换之前**就被拦下——磁盘 hash 从未变过，那是"失败未
# 替换"，不是"替换后回滚"。要证明回滚，必须让新字节真的被写进目标路径。
BEFORE_RB_SHA="$(sha256sum "$UI" | awk '{print $1}')"
PID_BEFORE_RB="$(agent_pids | awk '{print $1}')"
BROKEN_SRC="$W/broken.c"
cat > "$BROKEN_SRC" <<'C'
// 冒烟测试（--version）通过，agent 形态启动立刻失败。
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--version") == 0 || strcmp(argv[i], "-V") == 0) {
            printf("shell-remote-ui 0.59.17\n");
            return 0;
        }
    }
    fprintf(stderr, "broken build: refusing to start as an agent\n");
    return 1;
}
C
cc -O2 -o "$ART/shell-remote-ui-x86_64" "$BROKEN_SRC" \
  || { bad "坏制品编译失败"; exit 1; }
chmod +x "$ART/shell-remote-ui-x86_64"
BROKEN_SHA="$(sha256sum "$ART/shell-remote-ui-x86_64" | awk '{print $1}')"
echo "  回滚前磁盘 SHA256: $BEFORE_RB_SHA"
echo "  坏制品   SHA256: $BROKEN_SHA"
echo "  回滚前 agent PID: ${PID_BEFORE_RB:-?}"
if "$ART/shell-remote-ui-x86_64" --version 2>/dev/null | grep -q 0.59.17; then
  ok "坏制品能通过换机前的 --version 冒烟测试（所以会真的被换上）"
else
  bad "坏制品连冒烟测试都过不了，无法证明替换后回滚"; exit 1
fi

resp2="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$SID\",\"force\":true}")"
printf '%s' "$resp2" | grep -q '"ok":true\|"ok": true' \
  && ok "relay 下发了坏制品升级任务" || bad "relay 拒绝: $(head -c 200 <<<"$resp2")"

UPG_ID_RB="$(printf '%s' "$resp2" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("upgrade_id",""))' 2>/dev/null)"
# 轮询终态。**只接受 rolled_back**——`failed` 不是回滚成功：
# 它可能意味着"新版根本没被换上"或"旧版也没起来"，那都不能证明回滚。
saw_broken=0; final=""; final_kind=""
for _ in $(seq 1 300); do
  cur="$(sha256sum "$UI" 2>/dev/null | awk '{print $1}')"
  [ "$cur" = "$BROKEN_SHA" ] && saw_broken=1
  S="$(stage)"
  final_kind="$(printf '%s' "$S" | awk -F'|' '{gsub(/ /,"",$1); print $1}')"
  case "$final_kind" in
    rolled_back|failed) final="$S"; break ;;
  esac
  sleep 1
done
log_installed=0; log_restored=0; log_failed_reg=0
if [ -f "$HELPER_LOG" ]; then
  blk="$(awk -v id="$UPG_ID_RB" 'index($0,"upgrade_id="id){f=1} f' "$HELPER_LOG")"
  printf '%s' "$blk" | grep -q 'new binary installed' && log_installed=1
  printf '%s' "$blk" | grep -q 'previous binary restored' && log_restored=1
  printf '%s' "$blk" | grep -q 'did not register\|exited early' && log_failed_reg=1
  echo "  助手日志（本轮 upgrade_id=$UPG_ID_RB）："
  printf '%s\n' "$blk" | sed 's/^/      /'
fi
if [ $log_installed -eq 1 ]; then
  ok "助手确认已把新字节安装到目标路径（替换真实发生）"
elif [ $saw_broken -eq 1 ]; then
  ok "磁盘被观察到替换成坏制品（替换真实发生）"
else
  bad "没有证据表明发生过替换——这不是'替换后回滚'"
fi
[ $log_failed_reg -eq 1 ] && ok "新版确实启动失败（未拿到注册回执）" \
  || bad "没有观察到新版启动失败"
[ $log_restored -eq 1 ] && ok "助手确认已恢复旧二进制（真实回滚）" \
  || bad "助手日志没有 'previous binary restored'"
if [ -n "$final" ]; then
  ok "relay 收敛到终结态: $final"
else
  bad "未收敛到终结态（stage=$(stage)）"
fi
# 终态必须是 rolled_back；`failed` 不算回滚成功。
if [ "$final_kind" = "rolled_back" ]; then
  ok "终态是本轮 rolled_back（不是 failed）"
else
  bad "终态不是 rolled_back，而是「${final_kind:-<空>}」——不能算回滚通过"
fi
# 本轮 upgrade_id 必须非空，且与终态回执里的 ID **一致**。
RB_ID="$(printf '%s' "$final" | awk -F'|' '{gsub(/ /,"",$2); print $2}')"
if [ -z "$UPG_ID_RB" ]; then
  bad "本轮没有拿到 upgrade_id，无法证明这是本轮回滚"
elif [ "$RB_ID" = "$UPG_ID_RB" ]; then
  ok "终态回执的 upgrade_id 与本轮一致（$UPG_ID_RB）"
else
  bad "终态 upgrade_id 不符：本轮 $UPG_ID_RB，回执 ${RB_ID:-<空>}"
fi
# 回滚路径同样要求交接被消费且无残留。
HO_RB="$(find "$W/home" -name '.handoff-*.json' 2>/dev/null | wc -l)"
[ "$HO_RB" = "0" ] && ok "回滚后交接文件无残留（已消费）" \
  || bad "回滚后残留 $HO_RB 个交接文件"
if [ -f "$HELPER_LOG" ] && [ -n "$UPG_ID_RB" ]; then
  if printf '%s' "$blk" | grep -q 'previous agent restarted'; then
    ok "助手确认已拉起旧版（回滚闭环）"
  else
    bad "助手日志没有 'previous agent restarted'，回滚未闭环"
  fi
fi
sleep 3
AFTER_SHA="$(sha256sum "$UI" 2>/dev/null | awk '{print $1}')"
[ "$AFTER_SHA" = "$BEFORE_RB_SHA" ] && ok "磁盘回到回滚前的旧 hash（$AFTER_SHA）" \
  || bad "磁盘未恢复（现在是 $AFTER_SHA，期望 $BEFORE_RB_SHA）"
wait_online 240 && ok "回滚后设备仍在线" || bad "回滚后未上线"
TOK_RB="$(sess)"
[ "$TOK_RB" = "$TOK_BEFORE" ] && ok "回滚后身份不变（session_id + 一 rw 一 ro + 固定密钥）" \
  || bad "回滚后身份变了：[$TOK_RB]"
N_RW3="$(count_perm "$TOK_RB" rw)"; N_RO3="$(count_perm "$TOK_RB" ro)"
[ "$N_RW3" = "1" ] && [ "$N_RO3" = "1" ] \
  && ok "回滚后仍是恰好 1 个 rw + 1 个 ro" \
  || bad "回滚后权限形状变了：rw=$N_RW3 ro=$N_RO3"
if [ -n "$RW_BEFORE" ]; then
  out3="$(run_cmd "$RW_BEFORE" 'printf MYS2827-RW-OK')"
  printf '%s' "$(cmd_text "$out3")" | grep -q 'MYS2827-RW-OK' \
    && ok "回滚后 rw 远程命令仍可执行" || bad "回滚后命令失败: $(printf '%s' "$out3" | head -c 200)"
fi
# 4.x 活跃配置：回滚后逐项断言（与连接后、升级后同一个函数）。
# 这里必须包含 root 与 no_cmd_log——上一版注释说"shell/root 都验"，实际只验了 shell。
assert_active_config "回滚后" "$RW_BEFORE" || true

echo
echo "════ 5. 清理范围：只认自己起的，同名哨兵必须活着 ════"

# 5.1 先记录"清理前"的完整认领清单——这是清理范围的自证材料。
CLAIM_BEFORE="$(claim_all)"
echo "  认领到的本测试进程：${CLAIM_BEFORE:-<空>}"
for p in $CLAIM_BEFORE; do
  printf '    pid=%s exe=%s\n' "$p" "$(readlink "/proc/$p/exe" 2>/dev/null || echo '?')"
done

# 5.2 哨兵在清理前必须活着，而且它的名字/选择器形态前面已经断言过。
if alive "$SENTINEL_PID"; then
  ok "清理前同名哨兵存活（PID $SENTINEL_PID）"
else
  bad "清理前同名哨兵已消失——本测试过程中就误杀了它"
fi

# 5.3 跑真正的统一 cleanup（与 EXIT trap 同一个函数，幂等）。
#
# 注意顺序：哨兵**属于** weakest 认领范围之外的一个独立进程，但它的 exe 在本测试
# 目录下，所以 claim_all 会把它也算进来——这正是我们要的：cleanup 应该清掉本测试
# 起的所有东西。哨兵的"存活"断言因此必须在 cleanup **之前**做（5.2 已做），
# 它证明的是"测试运行期间没有别的机制按 basename 误杀它"。
cleanup
CLEANED=0   # 后面 EXIT trap 还会再兜一次（幂等）

# 5.4 复核：认领范围内一个都不剩。
CLEAN_LEFT="$(claim_all)"
if [ -z "${CLEAN_LEFT// /}" ]; then
  ok "统一 cleanup 后本测试进程全部退出（exe/子孙/WINEPREFIX 三类认领均为空）"
else
  bad "cleanup 后仍有进程存活：$CLEAN_LEFT"
  for p in $CLEAN_LEFT; do
    printf '    pid=%s exe=%s\n' "$p" "$(readlink "/proc/$p/exe" 2>/dev/null || echo '?')"
  done
fi

# 5.5 relay 必须离线。这是**附加**判据，不能代替 5.4：端口关闭不等于进程退出。
REL_LEFT=0
for _ in $(seq 1 20); do
  if curl -s -o /dev/null --max-time 1 "$BASE/"; then REL_LEFT=1; sleep 0.5; else REL_LEFT=0; break; fi
done
[ "$REL_LEFT" = "0" ] && ok "relay 已离线（端口 $PORT 不可连）" \
  || bad "relay 仍然在线（端口 $PORT 可连）"

# 5.6 日志必须留着——失败时要看的就是它。上一版在 EXIT 里 rm -rf，等于把证据删了。
echo "  保留工作目录（含日志/截图/助手日志）：$W"
if [ -d "$W" ] && [ -s "$W/relay.log" ]; then
  ok "失败日志已保留（$W，relay.log 非空）"
else
  bad "工作目录或日志缺失：$W"
fi

echo
echo "日志与截图目录：$W"
echo "断言：PASS=$npass / 共 $nassert"
[ $fail -eq 0 ] && echo PASS || echo FAIL
exit $fail
