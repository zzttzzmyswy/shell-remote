#!/bin/bash
# Windows 完整 GUI 链路（下载 → 换机 → 回滚尝试）在**隔离 Wine** 下的实测。
#
# 任务卡要求"至少在隔离 Wine 尝试窗口、下载、换机、回滚，失败时给出具体失败步骤
# 和日志"。"本轮未纳入"不是环境阻塞证据——本脚本就是把它真跑一遍。
#
# 覆盖范围与结论口径（重要）：
#   · 在 wine 里跑的是**真实 Windows 制品**（`shell-remote-ui-x86_64.exe`），
#     不是 Linux 二进制、也不是脚本替身。
#   · wine 不模拟 SCM / UAC / 提权 / 完整对象管理器语义，所以即使全链路走通，
#     也**不**等于真 Win10 通过。脚本末尾逐项列出"wine 跑通了什么 / 没跑通什么"。
#   · 真机 DACL 继承保护位（SE_DACL_PROTECTED）在 wine 上无法验证，仍由负责人
#     在真机复验；这一条在这里明确标注，不用任何弱判据冒充。
#
# 用法:
#   bash tools/win_wine_gui_chain_test.sh \
#     --ui dist/shell-remote-ui-x86_64.exe --relay dist/shell-remote-x86_64
#
# 退出码：全部断言通过 0，否则 1；缺少 --ui 时 2。
set -u

UI=""; RELAY=""; EXPECT_VERSION="${EXPECT_VERSION:-0.59.17}"
while [ $# -gt 0 ]; do
  case "$1" in
    --ui)    UI="${2:?}"; shift 2 ;;
    --relay) RELAY="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,21p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done
[ -x "$UI" ] || { echo "需要 --ui（Windows UI 制品 .exe）" >&2; exit 2; }
[ -x "$RELAY" ] || { echo "需要 --relay（本地 Linux relay，用于下发制品）" >&2; exit 2; }

fail=0; npass=0; nassert=0; skipped=()
ok(){  echo "  ✓ $1"; npass=$((npass+1)); nassert=$((nassert+1)); }
bad(){ echo "  ✗ $1"; fail=1; nassert=$((nassert+1)); }
skip(){ echo "  · 未覆盖：$1"; skipped+=("$1"); }

W="$(mktemp -d "${TMPDIR:-/tmp}/mys2827-winchain-XXXXXX")"
export WINEPREFIX="$W/wineprefix"
export WINEDEBUG=-all
# 关掉 mono/gecko：全新前缀会弹 "Wine Mono Installer" 模态框，把 GUI 主窗口挡在
# 后面（实测第一次跑就卡在这里）。本产品是原生 Rust，不需要它们。
export WINEDLLOVERRIDES="mscoree,mshtml="
# wine 的 GUI 需要真实 X：用独立 display，不与别的任务共用。
D=":$(seq 300 380 | while read n; do [ -e "/tmp/.X11-unix/X$n" ] || { echo $n; break; }; done)"
PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
BASE="http://127.0.0.1:$PORT"; ADMIN=/srwin2827
SID="winreal01"; KEY="winfixedkey2827"

OWNED=(); XPID=""; OPID=""; RPID=""; GPID=""

# ── 清理：与 Linux 侧同一套原则（认领 → 终止 → 等待），覆盖所有退出路径 ──
pids_with_prefix(){
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
pids_with_owned_exe(){
  python3 - "$W" "$UI" <<'PYEXE'
import os, sys
root = os.path.realpath(sys.argv[1]) + os.sep
ui = os.path.realpath(sys.argv[2])
out = []
for pid in filter(str.isdigit, os.listdir('/proc')):
    try:
        exe = os.path.realpath('/proc/%s/exe' % pid)
    except OSError:
        continue
    if exe.endswith(' (deleted)'):
        exe = exe[:-len(' (deleted)')]
    if exe == ui or exe.startswith(root):
        out.append(int(pid))
print(' '.join(str(p) for p in sorted(out)))
PYEXE
}
descendants_of(){
  python3 - "$1" <<'PYDESC'
import os, sys
root = int(sys.argv[1])
parent = {}
for pid in filter(str.isdigit, os.listdir('/proc')):
    try:
        with open('/proc/%s/stat' % pid, 'rb') as f:
            data = f.read()
        parent[int(pid)] = int(data[data.rindex(b')') + 2:].split()[1])
    except (OSError, ValueError, IndexError):
        continue
out, stack = [], [root]
while stack:
    cur = stack.pop()
    for pid, ppid in parent.items():
        if ppid == cur and pid not in out:
            out.append(pid); stack.append(pid)
print(' '.join(str(p) for p in sorted(out)))
PYDESC
}
alive(){ kill -0 "$1" 2>/dev/null; }
claim_all(){
  local pids=() p
  for p in "${OWNED[@]:-}"; do [ -n "$p" ] && pids+=("$p"); done
  pids+=($(pids_with_owned_exe))
  pids+=($(pids_with_prefix "$WINEPREFIX"))
  local roots=("${pids[@]:-}")
  for p in "${roots[@]:-}"; do [ -n "$p" ] && pids+=($(descendants_of "$p")); done
  printf '%s\n' "${pids[@]:-}" | grep -E '^[0-9]+$' | sort -un | tr '\n' ' '
}
cleanup(){
  local pids p left
  pids="$(claim_all)"
  [ -z "${pids// /}" ] && return 0
  for p in $pids; do kill -TERM "$p" 2>/dev/null; done
  for _ in $(seq 1 20); do
    left=""; for p in $pids; do alive "$p" && left="$left $p"; done
    [ -z "${left// /}" ] && break; sleep 0.25
  done
  for p in $pids; do alive "$p" && kill -9 "$p" 2>/dev/null; done
  for _ in $(seq 1 60); do
    left="$(claim_all)"; [ -z "${left// /}" ] && break
    for p in $left; do kill -9 "$p" 2>/dev/null; done
    sleep 0.25
  done
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
trap 'cleanup; exit 129' HUP

# ── X / wine 前缀 ───────────────────────────────────────────────────
Xvfb "$D" -screen 0 1280x900x24 -nolisten tcp > "$W/xvfb.log" 2>&1 & XPID=$!; OWNED+=("$XPID")
# 等 X 真的能连（只 sleep 会在慢机器上让 wineboot 撞上"connection broken"）。
for _ in $(seq 1 60); do
  [ -e "/tmp/.X11-unix/X${D#:}" ] && xdpyinfo -display "$D" >/dev/null 2>&1 && break
  sleep 0.25
done
if ! xdpyinfo -display "$D" >/dev/null 2>&1; then
  echo "X 服务器 $D 没能就绪" >&2
  exit 1
fi
export DISPLAY="$D"
openbox > "$W/openbox.log" 2>&1 & OPID=$!; OWNED+=("$OPID")
sleep 1
mkdir -p "$WINEPREFIX"
echo "display=$D relay=$BASE work=$W"
echo "WINEPREFIX=$WINEPREFIX（本测试专属，别的任务不可能撞上）"
if timeout 200 wineboot -u > "$W/wineboot.log" 2>&1; then
  ok "隔离 wine 前缀初始化完成"
else
  bad "wine 前缀初始化失败（见 $W/wineboot.log）"
fi
sleep 2

# ── relay（Linux 侧，负责下发 Windows 制品）────────────────────────
WINE=(env WINEPREFIX="$WINEPREFIX" WINEDEBUG=-all wine)
mkdir -p "$W/artifacts"
start_relay(){
  [ -n "$RPID" ] && { kill -9 "$RPID" 2>/dev/null; wait "$RPID" 2>/dev/null; }
  SR_LOG_DIR='' nohup "$RELAY" relay --bind "127.0.0.1:$PORT" --auth win2827 --no-tls \
    --admin-path "$ADMIN" --admin-user admin --admin-pass winpass \
    --settings-file "$W/relay-settings.json" "$@" > "$W/relay.log" 2>&1 &
  RPID=$!; OWNED+=("$RPID")
  for _ in $(seq 1 100); do curl -s -o /dev/null "$BASE/" && break; sleep 0.2; done
  curl -s -c "$W/cookie" -X POST "$BASE$ADMIN/login" -H 'Content-Type: application/json' \
    -d '{"user":"admin","pass":"winpass"}' -o /dev/null
}
api(){ curl -s -b "$W/cookie" "$BASE$ADMIN$1" "${@:2}"; }
sess(){
  api /api/overview | python3 -c '
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
online(){ api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]
d=json.load(sys.stdin)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        print(1 if s.get("online") else 0); raise SystemExit
print(0)
' "$SID"; }
stage(){ api /api/overview | python3 -c '
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

# wine 进程里"当前是哪个 agent"：按 exe 精确匹配本测试的 UI 副本。
wine_agent_pids(){
  # wine 下 /proc/<pid>/exe 指向 **wine-preloader**（不是 .exe），所以不能按 exe
  # 比对。可靠判据是 argv 里带着被启动的那个 Windows 路径；再按本测试前缀收窄，
  # 避免把同机别的任务的 wine 进程算进来。
  python3 - "$WINUI" "$WINEPREFIX" <<'PY'
import os, sys, re
target = sys.argv[1]
# wine 的 argv 可能写成相对路径，也可能写成 Z:\... 的 Windows 路径；
# 两种都归一到"最后一个路径分量 + 完整串包含"来判断。
base = os.path.basename(target)
prefix = os.path.realpath(sys.argv[2])
needle = b'WINEPREFIX=' + prefix.encode()
out = []
for pid in filter(str.isdigit, os.listdir('/proc')):
    try:
        argv = [a.decode('utf-8', 'replace')
                for a in open('/proc/%s/cmdline' % pid, 'rb').read().split(b'\0') if a]
        env = open('/proc/%s/environ' % pid, 'rb').read().split(b'\0')
    except OSError:
        continue
    if needle not in env:
        continue           # 不是本测试前缀里的进程
    if not any(a == target or a.endswith(base) or re.sub(r'^\\\\\?\\', '', a).replace('\\', '/').endswith(base)
               for a in argv):
        continue
    if any('update-apply' in a for a in argv):
        continue           # 助手不是 agent
    out.append(int(pid))
print(' '.join(str(p) for p in sorted(out)))
PY
}

# ── GUI 操作（wine 窗口与 Linux 布局一致，实测同一坐标可用）──────────
WIN=""; WX=0; WY=0
gui_open(){
  HOME="$W/home" SR_LOG_DIR="$W/logs" \
    nohup "${WINE[@]}" "$WINUI" > "$W/gui.log" 2>&1 &
  GPID=$!; OWNED+=("$GPID")
  for _ in $(seq 1 200); do
    # wine 的窗口 _NET_WM_PID 就是 wine 启动的那个 Windows 进程 PID。
    WIN="$(xdotool search --onlyvisible --name "连接设置" 2>/dev/null | tail -1)"
    [ -n "$WIN" ] && break; sleep 0.5
  done
  [ -n "$WIN" ] || return 1
  eval "$(xdotool getwindowgeometry --shell "$WIN")"
  WX=$X; WY=$Y
  xdotool windowactivate --sync "$WIN" 2>/dev/null || true
  xdotool windowfocus --sync "$WIN" 2>/dev/null || true
  sleep 0.8
  return 0
}
clickat(){ xdotool mousemove $((WX+$1)) $((WY+$2)) click 1; sleep "${3:-0.4}"; }
typein(){ clickat "$1" "$2"; xdotool key --clearmodifiers ctrl+a; sleep 0.1
          xdotool type --clearmodifiers --delay 30 -- "$3"; sleep 0.4; }

echo
echo "════ 0. 准备：Windows UI 制品 + 带 custom 制品库的 relay ════"
# 换机要替换**正在运行的那份**：复制到测试目录，正式制品永不改动。
mkdir -p "$W/bin"; cp "$UI" "$W/bin/shell-remote-ui-x86_64.exe"
WINUI="$W/bin/shell-remote-ui-x86_64.exe"
chmod +x "$WINUI"
ORIG_SHA="$(sha256sum "$UI" | awk '{print $1}')"
echo "  原始 Windows UI SHA256: $ORIG_SHA"
# 「新版」= 与当前字节不同的可执行（追加标记，不破坏 PE 头）。
cp "$UI" "$W/artifacts/shell-remote-ui-x86_64.exe"
printf 'MYS2827-WIN-NEW' >> "$W/artifacts/shell-remote-ui-x86_64.exe"
NEW_SHA="$(sha256sum "$W/artifacts/shell-remote-ui-x86_64.exe" | awk '{print $1}')"
chmod +x "$W/artifacts/shell-remote-ui-x86_64.exe"
echo "  新 Windows 制品 SHA256: $NEW_SHA"
[ "$NEW_SHA" != "$ORIG_SHA" ] && ok "新制品字节与当前不同（能证明换机）" \
  || bad "制品字节相同，无法证明换机"
if "${WINE[@]}" "$W/artifacts/shell-remote-ui-x86_64.exe" --version 2>/dev/null | grep -q "$EXPECT_VERSION"; then
  ok "追加标记后的 Windows 制品仍可执行（--version = $EXPECT_VERSION）"
else
  bad "追加标记后的 Windows 制品不可执行"
fi

start_relay --download-dir "$W/artifacts" --allow-custom-upgrade \
  --upgrade-cdn "http://127.0.0.1:1" --manifest-api-base "http://127.0.0.1:1"
ok "relay 已起（custom 源；CDN/清单指向关闭端口，只能走 relay 镜像）"

echo
echo "════ 1. wine 里打开真实 Windows GUI 配置窗口 ════"
if gui_open; then
  ok "wine 下出现 Windows GUI 配置窗口（$WIN，720x660 区间）"
  import -window "$WIN" "$W/win-gui-open.png" 2>/dev/null && ok "已截图 $W/win-gui-open.png"
  # 窗口必须属于本次 wine 启动的那个 Windows 进程（不是别人的窗口）。
  WPID="$(xprop -id "$WIN" _NET_WM_PID 2>/dev/null | sed -n 's/.*= \([0-9]*\)/\1/p')"
  if [ -n "$WPID" ] && kill -0 "$WPID" 2>/dev/null; then
    ok "窗口归属可核验：_NET_WM_PID=$WPID 存活"
  else
    bad "窗口的 _NET_WM_PID 读不到或进程已退出"
  fi
else
  bad "wine 下没能打开 Windows GUI 配置窗口（见 $W/gui.log）"
  sed -n '1,10p' "$W/gui.log" 2>/dev/null | sed 's/^/       /'
  echo
  echo "  → 后续链路无法继续；以下是已确认的失败步骤与日志位置。"
  echo "    日志：$W/gui.log"
  echo "断言：PASS=$npass / 共 $nassert"
  echo FAIL
  exit 1
fi

echo
echo "════ 2. 窗口内填配置并连接（both + 非默认 root/shell）════"
typein 360 244 "$BASE"
typein 360 369 "$KEY"
typein 190 460 "$SID"
# 访问权限 → both：wine 下同样是点第三项（布局与 Linux 一致，实测可点）。
clickat 529 462 0.9
clickat 456 579 0.9
import -window "$WIN" "$W/win-gui-filled.png" 2>/dev/null
clickat 67 587 0.8   # 连接
if wait_online 240; then
  ok "wine 下的 Windows GUI 点连接后注册成功（agent_online=1）"
else
  bad "wine 下的 Windows GUI 连接后未注册"
  stage | sed 's/^/       stage: /'
  sed -n '1,15p' "$W/gui.log" 2>/dev/null | sed 's/^/       /'
  echo "    日志：$W/gui.log"
fi
TOK="$(sess)"
echo "  身份: $TOK"
RW="$(printf '%s' "$TOK" | tr ' ' '\n' | sed -n 's/^\([^=]*\)=rw$/\1/p' | head -1)"
N_RW="$(printf '%s' "$TOK" | tr ' ' '\n' | grep -c '=rw$')"
N_RO="$(printf '%s' "$TOK" | tr ' ' '\n' | grep -c '=ro$')"
[ "$N_RW" = "1" ] && [ "$N_RO" = "1" ] \
  && ok "Windows 侧权限是 both（1 rw + 1 ro）" \
  || bad "Windows 侧权限不是 both（rw=$N_RW ro=$N_RO）"
[ -n "$RW" ] && [ "$RW" = "$KEY" ] && ok "rw 等于窗口里填的固定密钥" \
  || bad "rw 不是固定密钥（${RW:-空}）"

echo
echo "════ 3. 下载 + 换机（真实替换 Windows exe）════"
PID_BEFORE="$(wine_agent_pids | awk '{print $1}')"
echo "  换机前 agent PID: ${PID_BEFORE:-?}"
resp="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$SID\",\"force\":true}")"
UPG_ID="$(printf '%s' "$resp" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("upgrade_id",""))' 2>/dev/null)"
if printf '%s' "$resp" | grep -q '"ok":true\|"ok": true'; then
  ok "relay 下发 Windows 制品升级任务 upgrade_id=$UPG_ID"
else
  bad "relay 拒绝升级: $(printf '%s' "$resp" | head -c 300)"
fi

NEW_PID=""; BLK=""
for _ in $(seq 1 240); do
  cur="$(wine_agent_pids | awk '{print $1}')"
  if [ -n "$cur" ] && [ "$cur" != "$PID_BEFORE" ]; then NEW_PID="$cur"; break; fi
  sleep 1
done
HELPER_LOG="$(find "$W" -name 'update-apply.log' 2>/dev/null | head -1)"
echo "  助手日志: ${HELPER_LOG:-<没有找到>}"
if [ -n "$HELPER_LOG" ] && [ -n "$UPG_ID" ]; then
  BLK="$(awk -v id="$UPG_ID" 'index($0,"upgrade_id="id){f=1} f' "$HELPER_LOG" 2>/dev/null)"
  printf '%s\n' "$BLK" | sed 's/^/       /'
fi
DISK_SHA="$(sha256sum "$WINUI" 2>/dev/null | awk '{print $1}')"
if [ "$DISK_SHA" = "$NEW_SHA" ]; then
  ok "Windows exe 已真实替换成新字节（$ORIG_SHA → $NEW_SHA）"
elif [ -n "$BLK" ] && printf '%s' "$BLK" | grep -q 'new binary installed'; then
  ok "助手日志确认新字节已安装（磁盘当前 hash=$DISK_SHA）"
else
  bad "没有证据表明 Windows exe 被替换（磁盘 hash=$DISK_SHA，期望 $NEW_SHA）"
fi
if [ -n "$NEW_PID" ]; then
  ok "PID 变了（$PID_BEFORE → $NEW_PID）"
elif [ -n "$BLK" ] && printf '%s' "$BLK" | grep -q 'new agent started'; then
  ok "助手日志确认已拉起新进程"
else
  bad "PID 未变化且助手日志没有新进程记录"
fi
wait_online 240 && ok "换机后重新上线" || bad "换机后未上线"
TOK_AFTER="$(sess)"
[ "$TOK_AFTER" = "$TOK" ] && ok "身份完全保持（session + 一 rw 一 ro，逐字节相同）" \
  || bad "身份变了：前[$TOK] 后[$TOK_AFTER]"
S="$(stage)"; echo "  升级状态: $S"
printf '%s' "$S" | grep -q 'registered' && ok "收到 registered 回执" || bad "未见 registered 终态"
S_ID="$(printf '%s' "$S" | awk -F'|' '{gsub(/ /,"",$2); print $2}')"
[ -n "$UPG_ID" ] && [ "$S_ID" = "$UPG_ID" ] && ok "回执 upgrade_id 与本轮一致" \
  || bad "回执 upgrade_id 不符：本轮 $UPG_ID 回执 ${S_ID:-空}"
HO="$(find "$W" -name '.handoff-*.json' 2>/dev/null | wc -l)"
[ "$HO" = "0" ] && ok "交接文件已被消费（无残留凭据）" || bad "残留 $HO 个交接文件"

echo
echo "════ 4. 回滚尝试（坏制品：先替换，再起不来）════"
BEFORE_RB_SHA="$(sha256sum "$WINUI" | awk '{print $1}')"
# 构造能在 wine 下通过 --version、但作为 agent 立刻失败的 Windows PE。
# 直接用 mingw 编一个最小 exe 是不行的（没有 --version 语义），所以走另一条：
# 拿真实 exe，把末尾追加一段会触发初始化失败的标记是做不到的——PE 不会因此失败。
# 因此这里换一种真实做法：**把目标路径替换成一个必然启动失败的 PE**，同时让它
# 对 `--version` 返回成功。
BROKEN_C="$W/broken_win.c"
cat > "$BROKEN_C" <<'CEOF'
/* 冒烟测试（--version）通过，agent 形态启动立刻失败。 */
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--version") == 0 || strcmp(argv[i], "-V") == 0) {
            printf("shell-remote-ui 0.59.17\n");
            return 0;
        }
    }
    fprintf(stderr, "broken win build: refusing to start as an agent\n");
    return 1;
}
CEOF
if x86_64-w64-mingw32-gcc -O2 -o "$W/artifacts/shell-remote-ui-x86_64.exe" "$BROKEN_C" 2>"$W/broken-build.log"; then
  chmod +x "$W/artifacts/shell-remote-ui-x86_64.exe"
  BROKEN_SHA="$(sha256sum "$W/artifacts/shell-remote-ui-x86_64.exe" | awk '{print $1}')"
  ok "坏 Windows 制品已构建（SHA256 $BROKEN_SHA）"
  if "${WINE[@]}" "$W/artifacts/shell-remote-ui-x86_64.exe" --version 2>/dev/null | grep -q "$EXPECT_VERSION"; then
    ok "坏制品能通过换机前的 --version 冒烟测试（所以会真的被换上）"
  else
    bad "坏制品连冒烟测试都过不了，无法证明替换后回滚"
  fi

  resp2="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
    -d "{\"session_id\":\"$SID\",\"force\":true}")"
  UPG_RB="$(printf '%s' "$resp2" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("upgrade_id",""))' 2>/dev/null)"
  printf '%s' "$resp2" | grep -q '"ok":true\|"ok": true' \
    && ok "relay 下发坏制品升级任务 upgrade_id=$UPG_RB" || bad "relay 拒绝: $(head -c 200 <<<"$resp2")"

  final=""; final_kind=""
  for _ in $(seq 1 300); do
    S="$(stage)"
    final_kind="$(printf '%s' "$S" | awk -F'|' '{gsub(/ /,"",$1); print $1}')"
    case "$final_kind" in rolled_back|failed) final="$S"; break ;; esac
    sleep 1
  done
  if [ -n "$final" ]; then ok "relay 收敛到终结态: $final"; else bad "未收敛到终结态"; fi
  [ "$final_kind" = "rolled_back" ] && ok "终态是本轮 rolled_back" \
    || bad "终态是「${final_kind:-空}」，不是 rolled_back"
  RB_ID="$(printf '%s' "$final" | awk -F'|' '{gsub(/ /,"",$2); print $2}')"
  if [ -n "$UPG_RB" ] && [ "$RB_ID" = "$UPG_RB" ]; then
    ok "回滚终态 upgrade_id 与本轮一致（$UPG_RB）"
  else
    bad "回滚终态 upgrade_id 不符：本轮 $UPG_RB 回执 ${RB_ID:-空}"
  fi
  # 助手流水：替换 → 新进程失败 → 恢复。
  if [ -n "$HELPER_LOG" ] && [ -n "$UPG_RB" ]; then
    RB_BLK="$(awk -v id="$UPG_RB" 'index($0,"upgrade_id="id){f=1} f' "$HELPER_LOG" 2>/dev/null)"
    echo "  助手日志（本轮 $UPG_RB）："
    printf '%s\n' "$RB_BLK" | sed 's/^/       /'
    printf '%s' "$RB_BLK" | grep -q 'new binary installed' \
      && ok "助手确认坏字节已安装（替换真实发生）" || bad "助手日志没有 'new binary installed'"
    printf '%s' "$RB_BLK" | grep -q 'did not register\|exited early' \
      && ok "新版确实启动失败" || bad "没有观察到新版启动失败"
    printf '%s' "$RB_BLK" | grep -q 'previous binary restored' \
      && ok "助手确认已恢复旧二进制（真实回滚）" || bad "助手日志没有 'previous binary restored'"
  else
    bad "拿不到助手日志或本轮 upgrade_id，无法证明回滚链路"
  fi
  sleep 3
  AFTER_RB_SHA="$(sha256sum "$WINUI" 2>/dev/null | awk '{print $1}')"
  [ "$AFTER_RB_SHA" = "$BEFORE_RB_SHA" ] \
    && ok "Windows exe 已回到回滚前 hash（$AFTER_RB_SHA）" \
    || bad "Windows exe 未恢复（现在 $AFTER_RB_SHA，期望 $BEFORE_RB_SHA）"
  wait_online 240 && ok "回滚后仍在线" || bad "回滚后未上线"
  TOK_RB="$(sess)"
  [ "$TOK_RB" = "$TOK" ] && ok "回滚后身份不变（session + 一 rw 一 ro）" \
    || bad "回滚后身份变了：[$TOK_RB]"
else
  bad "坏 Windows 制品构建失败（见 $W/broken-build.log）"
  sed -n '1,6p' "$W/broken-build.log" | sed 's/^/       /'
fi

echo
echo "════ 5. 覆盖边界（wine 能证明什么、不能证明什么）════"
skip "真 Win10 的 UAC 提权 / SCM 服务形态 / 用户会话隔离（wine 不模拟）"
skip "交接文件 DACL 的 SE_DACL_PROTECTED 继承保护位（wine 丢弃该控制位，真机待负责人）"
skip "Windows 真实桌面抓屏 / AV1 硬编（wine 无 DXGI/GDI 真实后端）"
echo "  · 本次在 wine 下实测覆盖：Windows GUI 开窗、配置填写、注册、"
echo "    制品下载、exe 真实替换、身份保持、坏版本回滚与 hash 恢复。"

echo
echo "日志与截图目录：$W（保留，便于失败复盘）"
echo "断言：PASS=$npass / 共 $nassert"
[ ${#skipped[@]} -gt 0 ] && echo "未覆盖项：${#skipped[@]} 条（见上）"
[ $fail -eq 0 ] && echo PASS || echo FAIL
exit $fail
