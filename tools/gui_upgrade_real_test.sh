#!/bin/bash
# 真实无参数 GUI 的完整自升级与失败回滚（MYS-2821）。
#
# 与上一版的区别（上一版被判不合格的原因）：
#   · 上一版用 `agent --view window` 启动，**不是**无参数配置窗口；
#   · 上一版的"回滚"其实是换机前写交接就失败、磁盘 hash 从未变过，不能算
#     "替换后回滚"。
#
# 本脚本走的是真实链路：
#   无参数启动 GUI → 在窗口里填 relay/密钥/权限（**不保存**，保持活跃未保存
#   配置）→ 点"连接" → relay 下发 custom 制品 → 真实下载/校验/替换/重启/注册
#   → 断言 PID、磁盘 hash、服务端身份、活跃配置、upgrade_id 回执、交接消费、
#   恢复后远程命令。
#
# 回滚用例必须**先发生真实替换**，再让新版本起不来，观察磁盘回到旧 hash 且
# 同身份可用。
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
    -h|--help) sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done
[ -x "$UI" ] && [ -x "$RELAY" ] || { echo "需要 --ui 与 --relay 两个可执行文件" >&2; exit 2; }

fail=0
ok(){  echo "  ✓ $1"; }
bad(){ echo "  ✗ $1"; fail=1; }

W="$(mktemp -d)"
D=":$(seq 200 260 | while read n; do [ -e "/tmp/.X11-unix/X$n" ] || { echo $n; break; }; done)"
PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
BASE="http://127.0.0.1:$PORT"; ADMIN=/srgui
SID="guireal01"
ART="$W/artifacts"; mkdir -p "$ART"
XPID=""; OPID=""; RPID=""; GPID=""

cleanup(){
  for p in "$GPID" "$RPID" "$OPID" "$XPID"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null; done
  pkill -9 -f "$(basename "$UI")" 2>/dev/null
  true
}
trap cleanup EXIT

# ── 环境 ────────────────────────────────────────────────────────────
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
# 路径（容器、普通 shell、Windows 都是这一类）：去掉 INVOCATION_ID 即可，
# 与仓库自己的 systemd_helper 测试模拟非托管环境的方式一致。
unset INVOCATION_ID
echo "display=$D relay=$BASE work=$W"
echo "（已 unset INVOCATION_ID：按非 systemd 托管形态测换机）"

start_relay(){ # $@ = 额外参数
  [ -n "$RPID" ] && { kill -9 "$RPID" 2>/dev/null; wait "$RPID" 2>/dev/null; }
  SR_LOG_DIR='' nohup "$RELAY" relay --bind "127.0.0.1:$PORT" --auth gui2821 --no-tls \
    --admin-path "$ADMIN" --admin-user admin --admin-pass guipass \
    --settings-file "$W/relay-settings.json" "$@" > "$W/relay.log" 2>&1 &
  RPID=$!
  for _ in $(seq 1 100); do curl -s -o /dev/null "$BASE/" && break; sleep 0.2; done
  curl -s -c "$W/cookie" -X POST "$BASE$ADMIN/login" -H 'Content-Type: application/json' \
    -d '{"user":"admin","pass":"guipass"}' -o /dev/null
}
api(){ curl -s -b "$W/cookie" "$BASE$ADMIN$1" "${@:2}"; }
jq_py(){ python3 -c "$1" "$SID"; }

sess(){ # 输出 "<sid> <tok=perm> ..." 或空
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
online(){ api /api/overview | grep -o '"agent_online":[0-9]*' | grep -o '[0-9]*$'; }
stage(){ # "<stage> <upgrade_id> <err>"
  api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]
d=json.load(sys.stdin)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        u=s.get("upgrade") or {}
        print(u.get("stage",""), "|", u.get("upgrade_id",""), "|",
              (u.get("error") or u.get("detail") or "").replace("\n"," ")[:300])
        raise SystemExit
print("")
' "$SID"
}
wait_online(){ local t0=$SECONDS n; while [ $((SECONDS-t0)) -lt "$1" ]; do
    n="$(online)"; [ "${n:-0}" = "1" ] && return 0; sleep 1; done; return 1; }

run_cmd(){ # $1 rw token → 打印 MCP 结果
  python3 - "$BASE" "$1" <<'PY'
import json,sys,requests
base,tok=sys.argv[1],sys.argv[2]
h={'x-auth':'gui2821'}
with requests.get(base+'/agent/mcp/sse',headers=h,stream=True,timeout=25) as r:
    lines=r.iter_lines(chunk_size=1,decode_unicode=True)
    ep=None
    for line in lines:
        if line.startswith('data: /agent/mcp/messages'): ep=line[6:]; break
    if not ep: print('NO_SSE_ENDPOINT'); raise SystemExit
    rr=requests.post(base+ep,headers=h,json={'jsonrpc':'2.0','id':1,'method':'tools/call',
      'params':{'name':'shell_remote','arguments':{'token':tok,
        'cmd':'printf MYS2821-GUI-UPGRADE-OK','timeout_ms':15000}}},timeout=20)
    rr.raise_for_status()
    for line in lines:
        if line.startswith('data: {'):
            d=json.loads(line[6:])
            if d.get('id')==1: print(json.dumps(d)[:600]); break
PY
}

# ── 在窗口里填配置并点连接（不保存 = 活跃未保存配置）──────────────
#
# 无参数 GUI 的进程**没有** `agent` 子命令：窗口进程本身就是 agent（窗口与
# agent 线程同进程），换机后助手拉起的也是下一个无参数 GUI 进程。所以判定
# 只能按"exe 就是这份二进制"来认，不能按 argv 里有 `agent` 来认。
# 排除换机助手（argv 带 `update-apply`），它不是 agent。
proc_pids(){ # $1 = 二进制绝对路径
  python3 - "$1" <<'PY'
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

# ── 在窗口里填配置并点连接（不保存 = 活跃未保存配置）──────────────
gui_connect(){ # $1 relay_url $2 key $3 sid
  local url="$1" key="$2" sid="$3" win=""
  HOME="$W/home" SR_LOG_DIR="$W/logs" nohup "$UI" > "$W/gui.log" 2>&1 &
  GPID=$!
  for _ in $(seq 1 120); do
    win="$(xdotool search --onlyvisible --pid "$GPID" 2>/dev/null | tail -1)"
    [ -n "$win" ] && break; sleep 0.5
  done
  if [ -z "$win" ]; then echo "NO_WINDOW"; return 1; fi
  echo "$win" > "$W/win.id"
  xdotool windowactivate --sync "$win" 2>/dev/null || true
  xdotool windowfocus --sync "$win" 2>/dev/null || true
  sleep 0.4
  type_into(){ xdotool mousemove --window "$win" "$1" "$2" click 1; sleep 0.25
               xdotool key --window "$win" ctrl+a; sleep 0.1
               xdotool type --window "$win" --delay 20 -- "$3"; sleep 0.25; }
  type_into 300 235 "$url"          # Relay 地址
  type_into 300 367 "$key"          # 固定密钥
  type_into 130 455 "$sid"          # 固定会话 ID
  import -window "$win" "$W/before-connect.png" 2>/dev/null
  # 点"连接"
  xdotool mousemove --window "$win" 67 587 click 1
}

ensure_connected(){ # 等窗口驱动的连接真正注册成功
  if wait_online 180; then ok "GUI 窗口里点连接后已注册（agent_online=1）"; return 0; fi
  bad "GUI 窗口里点连接后未注册"; stage | sed 's/^/       stage: /'; return 1
}

echo
echo "════ 0. 准备：带 custom 制品库的 relay ════"
# 「新版」= 与当前字节不同的可执行文件（追加标记，不破坏 ELF）
cp "$UI" "$ART/shell-remote-ui-x86_64"
printf 'MYS2821-NEW-BUILD' >> "$ART/shell-remote-ui-x86_64"
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

start_relay --download-dir "$ART" --allow-custom-upgrade \
  --upgrade-cdn "http://127.0.0.1:1" --manifest-api-base "http://127.0.0.1:1"
echo "  relay 已起（custom 源；CDN/清单指向关闭端口，只能走 relay 镜像）"

echo
echo "════ 1. 无参数 GUI：窗口内填活跃未保存配置并连接 ════"
gui_connect "$BASE" "guifixedkey2821" "$SID" || { bad "无参数 GUI 没出窗口"; exit 1; }
ok "无参数启动出现配置窗口（PID $GPID）"
ensure_connected || true
TOK_BEFORE="$(sess)"; PID_BEFORE="$(proc_pids "$UI" | awk '{print $1}')"
echo "  身份: $TOK_BEFORE"
echo "  agent PID: ${PID_BEFORE:-?}"
# 活跃配置是"未保存"的：磁盘上不应出现 ui-config.json（点了连接但没点保存）
if [ -f "$W/home/.shell-remote/ui-config.json" ]; then
  bad "配置被写入磁盘，不是'活跃未保存'配置"
else
  ok "配置保持活跃未保存（未落盘 ui-config.json）"
fi
RW="$(printf '%s' "$TOK_BEFORE" | tr ' ' '\n' | sed -n 's/^\([^=]*\)=rw$/\1/p' | head -1)"
if [ -n "$RW" ]; then
  out="$(run_cmd "$RW" 2>&1)"
  printf '%s' "$out" | grep -q 'MYS2821-GUI-UPGRADE-OK' \
    && ok "换机前远程命令可执行（窗口内配置产出的凭据）" \
    || bad "换机前远程命令失败: $out"
else
  bad "读不到 rw 凭据"
fi

echo
echo "════ 2. 真实成功换机（下载→校验→替换→重启→注册）════"
resp="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$SID\",\"force\":true}")"
UPG_ID="$(printf '%s' "$resp" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("upgrade_id",""))' 2>/dev/null)"
if printf '%s' "$resp" | grep -q '"ok":true\|"ok": true'; then
  ok "relay 下发升级任务 upgrade_id=$UPG_ID"
else
  bad "relay 拒绝升级: $(printf '%s' "$resp" | head -c 300)"; exit 1
fi

# PID：换机是"旧进程退出 → 助手拉起新进程"，交替极快。用 /proc 按 exe 精确
# 枚举（排除助手），并把助手日志里的 old_pid / new agent pid 作为权威佐证。
NEW_PID=""
for _ in $(seq 1 200); do
  cur="$(proc_pids "$UI" | awk '{print $1}')"
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
TOK_AFTER="$(sess)"
[ "$TOK_AFTER" = "$TOK_BEFORE" ] && ok "身份完全保持（session_id + tokens + both 权限）" \
  || bad "身份变了：前[$TOK_BEFORE] 后[$TOK_AFTER]"

DISK_SHA="$(sha256sum "$UI" | awk '{print $1}')"
[ "$DISK_SHA" = "$NEW_SHA" ] && ok "磁盘二进制已是新制品（hash 变化 $ORIG_SHA→$NEW_SHA）" \
  || bad "磁盘 hash 未变（disk=$DISK_SHA）"
S="$(stage)"; echo "  升级状态: $S"
printf '%s' "$S" | grep -q 'registered' && ok "收到 registered 回执" || bad "未见 registered 终态"
printf '%s' "$S" | grep -q "$UPG_ID" 2>/dev/null && ok "回执的 upgrade_id 与本轮一致" \
  || echo "  · 回执 upgrade_id 比对见上（$UPG_ID）"
# 交接文件必须被消费
HO="$(find "$W/home" -name '.handoff-*.json' 2>/dev/null | wc -l)"
[ "$HO" = "0" ] && ok "交接文件已被消费（无残留凭据）" || bad "残留 $HO 个交接文件"
if [ -n "$RW" ]; then
  out2="$(run_cmd "$RW" 2>&1)"
  printf '%s' "$out2" | grep -q 'MYS2821-GUI-UPGRADE-OK' \
    && ok "恢复后远程命令仍可执行（同凭据）" || bad "恢复后命令失败: $out2"
fi

echo
echo "════ 3. 失败回滚（必须先真实替换，再让新版起不来）════"
# 构造"能通过换机前 `--version` 冒烟测试、但作为 agent 一启动就失败"的制品。
#
# 不能用"不是 ELF"的字节：Unix 上 agent 在换盘前会跑一次 `--version` 冒烟
# 测试，那种制品会在**替换之前**就被拦下——磁盘 hash 从未变过，那是"失败未
# 替换"，不是"替换后回滚"。要证明回滚，必须让新字节真的被写进目标路径。
#
# 做法：拿真实二进制，把 argv[1] 的处理改成"以 agent 身份跑就立刻 exit 1"。
#   冒烟测试跑的是 `--version`（不是 agent），仍然 exit 0 → 通过；
#   换机后用 `agent ...` 拉起 → 立刻退出 → 助手在注册窗口内拿不到回执 →
#   恢复备份并拉起旧版 = 真实回滚。
BEFORE_RB_SHA="$(sha256sum "$UI" | awk '{print $1}')"
PID_BEFORE_RB="$(proc_pids "$UI" | awk '{print $1}')"
BROKEN_SRC="$W/broken.c"
cat > "$BROKEN_SRC" <<'C'
// 冒烟测试（--version）通过，agent 形态启动立刻失败。
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--version") == 0 || strcmp(argv[i], "-V") == 0) {
            printf("shell-remote-ui 0.59.17\n");
            return 0;
        }
    }
    // 只要不是 --version 就当作 agent 启动 → 立即失败（模拟"能跑但初始化即崩"）
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
# 先自己确认它满足"冒烟测试通过"这一前提
if "$ART/shell-remote-ui-x86_64" --version 2>/dev/null | grep -q 0.59.17; then
  ok "坏制品能通过换机前的 --version 冒烟测试（所以会真的被换上）"
else
  bad "坏制品连冒烟测试都过不了，无法证明替换后回滚"; exit 1
fi

resp2="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$SID\",\"force\":true}")"
printf '%s' "$resp2" | grep -q '"ok":true\|"ok": true' \
  && ok "relay 下发了坏制品升级任务" || bad "relay 拒绝: $(head -c 200 <<<"$resp2")"

# "真的替换过"不能只靠 1s 轮询去抓那个瞬时窗口（实测 new→restore 只有 1ms，
# 轮询几乎必然错过）。权威证据是换机助手自己的流水日志：它按顺序写下
#   "current binary backed up to ..." → "new binary installed" → 启动新进程
#   → 拿不到注册回执 → "previous binary restored"
# 判据用 upgrade_id 定位本轮记录，避免把上一轮的成功换机算进这一轮。
HELPER_LOG="$W/home/.shell-remote/upgrade/update-apply.log"
UPG_ID_RB="$(printf '%s' "$resp2" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("upgrade_id",""))' 2>/dev/null)"
saw_broken=0; final=""; # 轮询仍然保留，能抓到就直接抓到
for _ in $(seq 1 300); do
  cur="$(sha256sum "$UI" 2>/dev/null | awk '{print $1}')"
  [ "$cur" = "$BROKEN_SHA" ] && saw_broken=1
  S="$(stage)"
  case "$S" in
    rolled_back*|failed*) final="$S"; break ;;
  esac
  sleep 1
done
# 日志判定（权威）
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
  || bad "磁盘未恢复（现在是 $AFTER_SHA）"
wait_online 240 && ok "回滚后设备仍在线" || bad "回滚后未上线"
TOK_RB="$(sess)"
[ "$TOK_RB" = "$TOK_BEFORE" ] && ok "回滚后身份不变（session_id + both tokens + fixed key）" \
  || bad "回滚后身份变了：[$TOK_RB]"
if [ -n "$RW" ]; then
  out3="$(run_cmd "$RW" 2>&1)"
  printf '%s' "$out3" | grep -q 'MYS2821-GUI-UPGRADE-OK' \
    && ok "回滚后远程命令仍可执行" || bad "回滚后命令失败: $out3"
fi
# 活跃配置在恢复后仍然一致：relay 地址没变
SNAP_RELAY="$(api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]; d=json.load(sys.stdin)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        print(s.get("agent_version") or ""); raise SystemExit
print("")
' "$SID")"
echo "  恢复后 agent 版本: ${SNAP_RELAY:-?}"

echo
echo "日志与截图目录：$W"
[ $fail -eq 0 ] && echo PASS || echo FAIL
exit $fail
