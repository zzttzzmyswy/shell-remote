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
ok(){  echo "  ✓ $1"; npass=$((npass+1)); nassert=$((nassert+1)); }
bad(){ echo "  ✗ $1"; fail=1; nassert=$((nassert+1)); }

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

# 只记录**本测试自己创建**的 PID。
XPID=""; OPID=""; RPID=""; GPID=""; AGENT_PIDS=()
cleanup(){
  local p
  for p in "$GPID" "$RPID" "$OPID" "$XPID"; do
    [ -n "$p" ] && kill -9 "$p" 2>/dev/null
  done
  # 换机助手与它拉起的新 agent：只杀 /proc 里 exe 精确等于本测试副本的进程。
  for p in "${AGENT_PIDS[@]:-}"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null; done
  local stale
  stale="$(leftover_for "$UI" 0)"
  if [ -n "$stale" ]; then
    # shellcheck disable=SC2086
    kill -9 $stale 2>/dev/null
    # 等它们真的退出（最多 10s）
    for _ in $(seq 1 20); do
      [ -z "$(leftover_for "$UI" 0)" ] && break
      sleep 0.5
    done
  fi
  rm -rf "$W"
}
trap cleanup EXIT

export DISPLAY="$D"
Xvfb "$D" -screen 0 1280x900x24 -nolisten tcp > "$W/xvfb.log" 2>&1 & XPID=$!
sleep 1.5
openbox > "$W/openbox.log" 2>&1 & OPID=$!
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
# 只按 **/proc/<pid>/exe 精确等于给定路径** 认领，绝不用 basename 匹配。
leftover_for(){ # $1 bin $2 skip-pid → 空格分隔的 PID 列表
  python3 - "$1" "$2" <<'PY'
import os, sys
target = os.path.realpath(sys.argv[1])
skip = int(sys.argv[2])
out = []
for pid in filter(str.isdigit, os.listdir('/proc')):
    if int(pid) == skip:
        continue
    try:
        exe = os.path.realpath('/proc/%s/exe' % pid)
    except OSError:
        continue
    if exe.endswith(' (deleted)'):
        exe = exe[:-len(' (deleted)')]
    if exe == target:
        out.append(int(pid))
print(' '.join(str(p) for p in sorted(out)))
PY
}

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

# ── GUI 操作 ────────────────────────────────────────────────────────
# 无参数 GUI 的进程**没有** `agent` 子命令：窗口进程本身就是 agent。
WIN=""; WX=0; WY=0
gui_open(){ # 启动无参数 GUI 并等窗口
  HOME="$W/home" SR_LOG_DIR="$W/logs" SHELL=/bin/bash \
    nohup "$UI" > "$W/gui.log" 2>&1 &
  GPID=$!
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
# 故意起一个**同名**（basename 一样）的无关进程，放在另一个目录。上一版脚本的
# `pkill -f basename` 会把它杀掉；本版必须让它活到最后。
SENTINEL_DIR="$W/sentinel"; mkdir -p "$SENTINEL_DIR"
cp "$UI" "$SENTINEL_DIR/shell-remote-ui-x86_64"
cat > "$SENTINEL_DIR/run.sh" <<'EOS'
#!/bin/sh
# 独立同名哨兵：什么也不做，只证明自己没被别人的清理误杀。
while :; do sleep 5; done
EOS
chmod +x "$SENTINEL_DIR/run.sh"
SR_LOG_DIR="$W/sentinel-logs" nohup "$SENTINEL_DIR/run.sh" > "$SENTINEL_DIR/out.log" 2>&1 &
SENTINEL_PID=$!
AGENT_PIDS+=("$SENTINEL_PID")
sleep 0.5
if kill -0 "$SENTINEL_PID" 2>/dev/null; then
  ok "独立同名哨兵已启动（PID $SENTINEL_PID，basename 与 UI 相同）"
else
  bad "独立同名哨兵起不来，无法验证清理范围"
fi

start_relay --download-dir "$ART" --allow-custom-upgrade \
  --upgrade-cdn "http://127.0.0.1:1" --manifest-api-base "http://127.0.0.1:1"
echo "  relay 已起（custom 源；CDN/清单指向关闭端口，只能走 relay 镜像）"

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

# 2.4 shell 的活跃配置：网页终端 PTY 的 argv[0] 必须是配置的 shell。
# 通过 /proc 看 agent 的子进程，命令行的第一个词即 PTY 起的 shell。
SHELL_SEEN=""
if [ -n "$PID_BEFORE" ]; then
  SHELL_SEEN="$(descendants_of "$PID_BEFORE" | tr ' ' '\n' | while read -r p; do
      [ -n "$p" ] || continue
      [ -r "/proc/$p/cmdline" ] || continue
      tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | awk '{print $1}'
    done | sort -u | tr '\n' ' ')"
fi
if printf '%s' "$SHELL_SEEN" | grep -qF "$ACTIVE_SHELL"; then
  ok "PTY shell 是窗口里配置的 $ACTIVE_SHELL（子进程实测：$SHELL_SEEN）"
else
  bad "没在 agent 子进程里看到配置的 shell $ACTIVE_SHELL（实测：${SHELL_SEEN:-无}）"
fi

# 2.5 no_cmd_log 的活跃配置：窗口里勾了「关闭命令日志」，则命令日志文件不应
# 因这几次 MCP 调用而多出 "[MCP] exec" 行。这是该开关唯一的外部可观测点。
#
# 日志文件是 `$SR_LOG_DIR/shell-remote.log.<日期>-<小时>`（RollingFileAppender
# 的 HOURLY 轮转），所以按前缀匹配，不能猜固定文件名。
cmdlog_lines(){ grep -h '\[MCP\] exec' "$W"/logs/shell-remote.log* 2>/dev/null | wc -l; }
CMDSTAT_BEFORE="$(cmdlog_lines)"
if [ -n "$RW_BEFORE" ]; then
  run_cmd "$RW_BEFORE" 'printf MYS2827-CMDLOG-PROBE' > /dev/null
fi
sleep 1.0
CMDSTAT_AFTER="$(cmdlog_lines)"
# 判据要能区分"没写命令日志"与"日志管道根本没工作"：先确认日志文件里**有**
# 内容（agent 启动时总会写若干 info 行）。否则 0→0 是空断言，不能算通过。
LOG_LINES="$(cat "$W"/logs/shell-remote.log* 2>/dev/null | wc -l)"
if ! ls "$W"/logs/shell-remote.log* >/dev/null 2>&1; then
  bad "找不到日志文件（$W/logs/shell-remote.log*），命令日志断言无从判定"
elif [ "$LOG_LINES" -lt 5 ]; then
  bad "日志文件只有 $LOG_LINES 行，日志管道未正常工作，无法据此判断 no_cmd_log"
elif [ "$CMDSTAT_AFTER" = "$CMDSTAT_BEFORE" ]; then
  ok "关闭命令日志生效（日志共 $LOG_LINES 行，MCP 调用未新增 [MCP] exec 行：$CMDSTAT_BEFORE→$CMDSTAT_AFTER）"
else
  bad "命令日志仍被写入（$CMDSTAT_BEFORE→$CMDSTAT_AFTER）——no_cmd_log 未生效"
fi

# 2.6 root 的活跃配置：文件管理器按 --root 解析相对路径。
# 用 MCP 的相对路径操作落到 root 下——这是"root 真的生效"的唯一可观测证据。
ROOT_MARK="mys2827-root-$$"
if [ -n "$RW_BEFORE" ]; then
  up="$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
        -H "X-SR-Token: $RW_BEFORE" --data-binary "root-marker" \
        "$BASE/agent/mcp/put?path=$ROOT_MARK" 2>/dev/null)"
  if [ "$up" = "200" ]; then
    if [ -f "$ACTIVE_ROOT/$ROOT_MARK" ]; then
      ok "相对路径上传落在配置的 root 下（$ACTIVE_ROOT/$ROOT_MARK 存在）"
    else
      bad "上传返回 200，但文件不在配置的 root 下（$ACTIVE_ROOT/$ROOT_MARK 不存在）"
    fi
  else
    bad "相对路径上传失败（HTTP $up），无法证明 root 生效"
  fi
fi

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
# 换机后新进程也必须仍然用配置的 shell（活跃配置跨进程保持）。
PID_AFTER="$(agent_pids | awk '{print $1}')"
SHELL_AFTER=""
if [ -n "$PID_AFTER" ]; then
  SHELL_AFTER="$(descendants_of "$PID_AFTER" | tr ' ' '\n' | while read -r p; do
      [ -n "$p" ] || continue
      [ -r "/proc/$p/cmdline" ] || continue
      tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | awk '{print $1}'
    done | sort -u | tr '\n' ' ')"
fi
printf '%s' "$SHELL_AFTER" | grep -qF "$ACTIVE_SHELL" \
  && ok "换机后 PTY shell 仍是配置的 $ACTIVE_SHELL（活跃配置跨进程保持）" \
  || bad "换机后 shell 变了（实测：${SHELL_AFTER:-无}）"
# root 也必须跨进程保持。
if [ -n "$RW_BEFORE" ]; then
  MARK2="mys2827-root-after-$$"
  up2="$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
        -H "X-SR-Token: $RW_BEFORE" --data-binary "root-marker" \
        "$BASE/agent/mcp/put?path=$MARK2" 2>/dev/null)"
  if [ "$up2" = "200" ] && [ -f "$ACTIVE_ROOT/$MARK2" ]; then
    ok "换机后相对路径仍落在配置的 root 下（活跃配置跨进程保持）"
    rm -f "$ACTIVE_ROOT/$MARK2"
  else
    bad "换机后 root 配置未保持（HTTP $up2，$ACTIVE_ROOT/$MARK2 不存在）"
  fi
fi

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
saw_broken=0; final=""
for _ in $(seq 1 300); do
  cur="$(sha256sum "$UI" 2>/dev/null | awk '{print $1}')"
  [ "$cur" = "$BROKEN_SHA" ] && saw_broken=1
  S="$(stage)"
  case "$S" in
    rolled_back*|failed*) final="$S"; break ;;
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
[ -n "$final" ] && ok "relay 收敛到终结态: $final" || bad "未收敛到终结态（stage=$(stage)）"
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
# 回滚后 shell / root 也必须还是配置的那份。
PID_RB="$(agent_pids | awk '{print $1}')"
SHELL_RB=""
if [ -n "$PID_RB" ]; then
  SHELL_RB="$(descendants_of "$PID_RB" | tr ' ' '\n' | while read -r p; do
      [ -n "$p" ] || continue
      [ -r "/proc/$p/cmdline" ] || continue
      tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | awk '{print $1}'
    done | sort -u | tr '\n' ' ')"
fi
printf '%s' "$SHELL_RB" | grep -qF "$ACTIVE_SHELL" \
  && ok "回滚后 PTY shell 仍是配置的 $ACTIVE_SHELL" \
  || bad "回滚后 shell 变了（实测：${SHELL_RB:-无}）"

echo
echo "════ 5. 清理范围：只杀自己起的，同名哨兵必须活着 ════"
if kill -0 "$SENTINEL_PID" 2>/dev/null; then
  ok "独立同名哨兵全程存活（PID $SENTINEL_PID），清理没有按 basename 误杀"
else
  bad "独立同名哨兵被杀了——清理范围过大（PID $SENTINEL_PID 已不在）"
fi
# 逐级回收本测试自己起的进程：先用记录的 PID，再按 exe 精确路径兜住换机后的新
# 进程与助手。**不** pkill basename、**不** wineserver -k。
echo "  本测试进程清单（记录值）：XPID=$XPID OPID=$OPID RPID=$RPID GPID=$GPID"
for p in "$GPID" "$RPID" "$OPID" "$XPID"; do
  [ -n "$p" ] && { kill -9 "$p" 2>/dev/null; }
done
# agent + 它的子进程（PTY shell / script / 命令）都在本测试的 exe 路径下。
for p in $(agent_pids); do
  for c in $(descendants_of "$p"); do kill -9 "$c" 2>/dev/null; done
  kill -9 "$p" 2>/dev/null
done
# 助手：exe 是 ~/.shell-remote 下的 update-apply 副本（本测试的 HOME 内）。
for p in $(leftover_for "$W/home/.shell-remote/update-apply" 0); do kill -9 "$p" 2>/dev/null; done
# 等待真正退出（不是 sleep 一下就当清理完了）
CLEAN_OK=1
for _ in $(seq 1 40); do
  left="$(leftover_for "$UI" 0)"
  if [ -z "$left" ]; then CLEAN_OK=0; break; fi
  sleep 0.5
done
if [ "$CLEAN_OK" = "0" ]; then
  ok "本测试的 UI 进程已全部退出（按 exe 路径核验）"
else
  bad "仍有本测试的 UI 进程存活：$(leftover_for "$UI" 0)"
fi
# relay 必须离线：端口不再可连。
REL_LEFT=0
for _ in $(seq 1 20); do
  curl -s -o /dev/null --max-time 1 "$BASE/" && { REL_LEFT=1; sleep 0.5; } || { REL_LEFT=0; break; }
done
[ "$REL_LEFT" = "0" ] && ok "relay 已离线（端口不可连）" || bad "relay 仍然在线（端口 $PORT 可连）"
# 哨兵最后必须还在
if kill -0 "$SENTINEL_PID" 2>/dev/null; then
  ok "清理完成后同名哨兵依然存活"
  kill -9 "$SENTINEL_PID" 2>/dev/null
else
  bad "清理完成后同名哨兵已不在"
fi

echo
echo "日志与截图目录：$W"
echo "断言：PASS=$npass / 共 $nassert"
[ $fail -eq 0 ] && echo PASS || echo FAIL
exit $fail
