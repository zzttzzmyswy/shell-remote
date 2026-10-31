#!/bin/bash
# MYS-2425-E 真实临时 systemd unit 验证：`Restart=always` + 非 root + 无 polkit 授权。
#
# 覆盖四条路径（验收标准 3）：
#   A. 成功路径（KillMode=control-group）：升级后 service 仍 active、MainPID 是新进程、
#      NRestarts 恰好 +1、/proc/<MainPID>/exe 指向 target、磁盘 hash 为新制品、
#      relay 显示 registered、session_id/Token 不变、同 session 只 1 个活进程、backup 已删。
#   B. 成功路径（KillMode=mixed）：同上。
#   C. 自回滚：Environment=SR_UPGRADE_SELF_ROLLBACK_SECS=30，换机后 relay 停机 >30s，
#      磁盘恢复旧 hash、service 仍 active 且 MainPID 是旧版进程、relay 最终 failed。
#   D. Restart=on-failure + 未授权：换机前 failed、磁盘未动、旧 agent 在线、原因含 Restart=always。
#
# 只操作本脚本自己建的 unit（sr-mys2425e-*）。前台一次跑完，trap 清理。
set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# 被换的**已安装**二进制。必须是**带本特性**的版本（v0.59.7），否则它不具备
# SystemdRestart 能力、换机会被 fail-closed 拒绝——这正是"首次部署"的
# chicken-and-egg：v0.59.6 只能靠人工/SOP 升上来，一旦跑到 v0.59.7，之后每次
# 升级都能免权限自升级。
OLD="${OLD:-$REPO/target/release/shell-remote}"
# relay 与升级目标：0.59.8（比已安装的高一个补丁号）。relay 用自身
# `CARGO_PKG_VERSION` 当升级目标版本，所以 relay 必须是 0.59.8 那份构建——否则
# 它会说"无需升级"。制品就是 0.59.8 的 relay 二进制 + 尾部标记串，所以"磁盘
# hash 变成新制品"仍然是被证明的。
NEW="${NEW:-/tmp/sr-next-target/release/shell-remote}"
# relay 监听 UPSTREAM；agent 连 AGENT_PORT，中间是本脚本起的 TCP 代理。
# 用代理而不是直接连 relay，是为了能在 C 用例里"剪断链路"而**不丢 relay 内存
# 里的任务状态**（杀掉 relay 会连同 upgrade_id 一起消失，就没有 failed 可收敛）。
PORT="${PORT:-3131}"
UPSTREAM_PORT="${UPSTREAM_PORT:-$((PORT + 1))}"
UPSTREAM="http://127.0.0.1:${UPSTREAM_PORT}"
BASE="http://127.0.0.1:${PORT}"
PROXY_PID=""
ADMIN_PATH=/sr-admin-cus
W="$(mktemp -d)"
# 安装位置：换机真的会 rename 掉这个文件，所以必须在可写目录里，不能直接用
# 仓库里的构建产物。
INSTALLED="$W/bin/shell-remote"
BIN="$INSTALLED"
UNITDIR="$HOME/.config/systemd/user"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
RELAY_PID=""
FAIL=0

ok()  { echo "    ✓ $1"; }
bad() { echo "    ✗ $1"; FAIL=1; }
chk() { if [ "$1" = "1" ]; then ok "$2"; else bad "$2 (实际: ${3:-})"; fi; }

cleanup() {
  set +e
  echo
  echo "════ 清理 ════"
  cut_link
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null && echo "  relay $RELAY_PID 已停止"
  for u in "$UNITDIR"/sr-mys2425e-*.service; do
    [ -e "$u" ] || continue
    u="$(basename "$u")"
    systemctl --user stop "$u" 2>/dev/null
    systemctl --user reset-failed "$u" 2>/dev/null
    rm -f "$UNITDIR/$u"
    echo "  unit $u 已停止并删除"
  done
  systemctl --user daemon-reload 2>/dev/null
  for f in "$W"/agent*.log; do
    [ -e "$f" ] || continue
    pid="$(grep -o 'agent pid=[0-9]*' "$f" 2>/dev/null | tail -1 | cut -d= -f2)"
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
  done
  echo "  残留 sr-mys2425e 进程: $(pgrep -fc 'sr-mys2425e' 2>/dev/null || echo 0)"
  echo "  workdir $W 已删除"
  rm -rf "$W"
}
trap cleanup EXIT

# ── 工具 ────────────────────────────────────────────────────────────

login() {
  for _ in $(seq 1 40); do
    curl -s -c "$W/cookie" -X POST "$UPSTREAM${ADMIN_PATH}/login" \
      -H 'Content-Type: application/json' -d '{"user":"admin","pass":"cspass"}' \
      -o /dev/null 2>/dev/null
    grep -q sr_admin "$W/cookie" 2>/dev/null && return 0
    sleep 0.25
  done
  return 1
}
# 管理面直连 relay（UPSTREAM）：C 用例会剪断 agent↔relay 的链路，管理面不能
# 跟着断——否则读不到 relay 持有的任务状态。
api() { local p="$1"; shift; curl -s -b "$W/cookie" "$UPSTREAM${ADMIN_PATH}${p}" "$@"; }

# relay 必须跑**新版**（0.59.7）：它用自身的 `CARGO_PKG_VERSION` 作为升级目标
# 版本，旧版 relay 会说"无需升级"。
# 起/停 agent→relay 的 TCP 代理（剪断=杀掉代理进程）。
start_proxy() {
  [ -n "$PROXY_PID" ] && return 0
  python3 - "$PORT" "$UPSTREAM_PORT" <<'PYPROXY' > "$W/proxy.log" 2>&1 &
import socket, sys, threading
listen, target = int(sys.argv[1]), int(sys.argv[2])
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", listen)); srv.listen(64)
def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    except OSError:
        pass
    finally:
        try: b.shutdown(socket.SHUT_WR)
        except OSError: pass
while True:
    try: c, _ = srv.accept()
    except OSError: break
    try: u = socket.create_connection(("127.0.0.1", target))
    except OSError:
        c.close(); continue
    threading.Thread(target=pipe, args=(c, u), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c), daemon=True).start()
PYPROXY
  PROXY_PID=$!
  for _ in $(seq 1 40); do (echo > /dev/tcp/127.0.0.1/$PORT) 2>/dev/null && return 0; sleep 0.2; done
  return 1
}

cut_link() {
  [ -n "$PROXY_PID" ] && kill -9 "$PROXY_PID" 2>/dev/null
  wait "$PROXY_PID" 2>/dev/null
  PROXY_PID=""
}

restore_link() { start_proxy; }

start_relay() { # $1 = artifact dir (optional)
  local art="${1:-}"
  if [ -n "$art" ]; then
    "$NEW" relay --bind "127.0.0.1:${UPSTREAM_PORT}" --auth cus-pass --no-tls \
      --admin-path "$ADMIN_PATH" --admin-user admin --admin-pass cspass \
      --download-dir "$art" --allow-custom-upgrade \
      --upgrade-cdn "http://127.0.0.1:1" >> "$W/relay.log" 2>&1 &
  else
    "$NEW" relay --bind "127.0.0.1:${UPSTREAM_PORT}" --auth cus-pass --no-tls \
      --admin-path "$ADMIN_PATH" --admin-user admin --admin-pass cspass \
      >> "$W/relay.log" 2>&1 &
  fi
  RELAY_PID=$!
  for _ in $(seq 1 80); do curl -s -o /dev/null "$UPSTREAM/" && break; sleep 0.2; done
  login
}

# 某 session 当前的 tokens（身份对账单位）。
sess_tokens() {
  api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]
try: d=json.load(sys.stdin)
except Exception: sys.exit(1)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        print(" ".join(sorted(t["token"] for t in s.get("tokens",[]))))
        sys.exit(0)
sys.exit(1)' "$1"
}

# 某 session 的升级状态（relay 侧）。
upgrade_stage() {
  api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]
try: d=json.load(sys.stdin)
except Exception: sys.exit(1)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        u=s.get("upgrade") or {}
        print(u.get("stage") or "", "|", (u.get("error") or "")[:400])
        sys.exit(0)
sys.exit(1)' "$1"
}

# 按精确 sid 数**活着**的 agent 进程（排除 Z）。
live_for_sid() {
  python3 - "$1" <<'PY'
import os,sys
sid=sys.argv[1]; n=0
for pid in os.listdir('/proc'):
    if not pid.isdigit(): continue
    try:
        with open('/proc/'+pid+'/cmdline','rb') as f:
            argv=[a.decode('utf-8','replace') for a in f.read().split(b'\x00') if a]
    except OSError: continue
    if not any('shell-remote' in a for a in argv[:1]): continue
    hit=any((a=='--session-id' and j+1<len(argv) and argv[j+1]==sid) or a=='--session-id='+sid
            for j,a in enumerate(argv))
    if not hit: continue
    try:
        with open('/proc/'+pid+'/stat') as f: st=f.read().rsplit(')',1)[1].split()[0]
        if st=='Z': continue
    except OSError: continue
    n+=1
print(n)
PY
}

write_unit() { # $1=name  $2=restart  $3=killmode  $4=extra env (optional)
  local name="$1" restart="$2" killmode="$3" extra="${4:-}"
  mkdir -p "$UNITDIR"
  cat > "$UNITDIR/$name.service" <<UNIT
[Unit]
Description=MYS-2425-E verification ($name)
After=network-online.target

[Service]
Type=simple
WorkingDirectory=$W
ExecStart=$BIN agent --relay-url $BASE --key veri-pass-$name --session-id veri$5 --shell /bin/sh --root $W
Restart=$restart
RestartSec=5
KillMode=$killmode
Environment=HOME=$W
Environment=SR_UPGRADE_SELF_ROLLBACK_SECS=${extra:-180}
StandardOutput=append:$W/$name.log
StandardError=append:$W/$name.log

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
}

# 等 unit 回到 active（且有 MainPID）。
wait_active() { # $1=name  $2=timeout
  local t0=$SECONDS
  while [ $((SECONDS - t0)) -lt "$2" ]; do
    if [ "$(systemctl --user show "$1.service" -p ActiveState --value)" = active ] \
       && [ "$(systemctl --user show "$1.service" -p MainPID --value)" != 0 ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

show_unit() { # $1=name
  systemctl --user show "$1.service" -p ActiveState,MainPID,NRestarts,KillMode,Restart 2>/dev/null | tr '\n' ' '; echo
}

exe_of() { readlink "/proc/$1/exe" 2>/dev/null; }

# ── 制品：一份与 v0.59.6 不同的"新版"字节 ────────────────────────────
#
# 用旧版二进制 + 尾部标记串：hash 不同（能证明真的换过盘），仍然可执行
# （`--version` 冒烟能过）。这样"磁盘 hash 变成新制品"才有意义。
prepare_artifacts() {
  ART="$W/artifacts"; mkdir -p "$ART"
  case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64 ;; aarch64|arm64) ARCH=aarch64 ;; *) ARCH=unknown ;;
  esac
  ART_NAME="shell-remote-${ARCH}"
  [ -x "$OLD" ] || { echo "FAIL: 旧版二进制不存在: $OLD"; exit 1; }
  [ -x "$NEW" ] || { echo "FAIL: 新版二进制不存在: $NEW"; exit 1; }
  echo "  旧版: $("$OLD" --version 2>&1)  新版: $("$NEW" --version 2>&1)"
  cp "$NEW" "$ART/$ART_NAME"
  printf 'MYS2425E-%s' "$(date +%s%N)" >> "$ART/$ART_NAME"
  chmod +x "$ART/$ART_NAME"
  ART_SHA="$(sha256sum "$ART/$ART_NAME" | cut -d' ' -f1)"
  ART_SIZE="$(stat -c %s "$ART/$ART_NAME")"
  "$ART/$ART_NAME" --version >/dev/null 2>&1 || { echo "FAIL: 制品不可执行"; exit 1; }

  echo "  制品 $ART_NAME sha=${ART_SHA:0:16}… size=$ART_SIZE"
}

# 触发升级并等到 relay 给出终结状态。
trigger_upgrade() { # $1 = sid
  local resp
  resp="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
    -d "{\"session_id\":\"$1\",\"force\":true}")"
  printf '%s' "$resp" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("ok") is True else 1)' 2>/dev/null
}

wait_stage() { # $1=sid  $2=期望 stage（可空=任意终结）  $3=timeout
  local t0=$SECONDS st
  while [ $((SECONDS - t0)) -lt "$3" ]; do
    st="$(upgrade_stage "$1")"
    case "$st" in
      registered*|failed*|rolled_back*)
        # 期望为空 = 任意终结态
        if [ -z "$2" ]; then echo "$st"; return 0; fi
        case "$st" in "$2"*) echo "$st"; return 0;; esac
        ;;
      *)
        # 期望的终结态一旦出现就返回（含尚未进入终态的匹配，如等待 failed）
        if [ -n "$2" ]; then
          case "$st" in "$2"*) echo "$st"; return 0;; esac
        fi
        ;;
    esac
    sleep 1
  done
  echo "timeout | $(upgrade_stage "$1")"
  return 1
}

echo "════ MYS-2425-E 真实临时 unit 验证 ════"
echo "非 root 账户: $(id -un) (uid=$(id -u))；unit 目录 $UNITDIR"
echo "systemd --user manager: $(systemctl --user is-system-running 2>&1)"
echo

echo "── 准备制品 ──"
prepare_artifacts
mkdir -p "$(dirname "$INSTALLED")"
cp "$OLD" "$INSTALLED"; chmod +x "$INSTALLED"
OLD_SHA="$(sha256sum "$INSTALLED" | cut -d' ' -f1)"
echo "  安装旧版到 $INSTALLED sha=${OLD_SHA:0:16}…"
[ "$OLD_SHA" != "$ART_SHA" ] || { echo "FAIL: 新旧制品 hash 相同，换机无法被证明"; exit 1; }
start_relay "$ART" || { echo "FAIL: relay 未启动"; exit 1; }
start_proxy || { echo "FAIL: TCP 代理未启动"; exit 1; }
echo "  relay 已起在 $UPSTREAM，agent 经代理连 $BASE"

# ══════════════════════════════════════════════════════════════════
# A / B：成功路径（KillMode=control-group / mixed）
# ══════════════════════════════════════════════════════════════════
# 每个用例前把磁盘恢复成"已安装旧版"的状态。
#
# 必须先停掉**所有**本脚本的 unit 再覆盖：上一轮的新 agent 还在跑时，直接 `cp`
# 到同一个路径会撞 ETXTBSY（"Text file busy"），覆盖失败但又没有报错退出，
# 于是本用例从一个错误的磁盘状态开始（实测把 C 用例的断言全带偏了）。
reinstall_old() {
  local u
  for u in "$UNITDIR"/sr-mys2425e-*.service; do
    [ -e "$u" ] || continue
    u="$(basename "$u")"
    systemctl --user stop "$u" 2>/dev/null
    systemctl --user reset-failed "$u" 2>/dev/null
  done
  # 等所有本脚本的 agent 进程退出，否则仍是 ETXTBSY。
  local t0=$SECONDS
  while [ $((SECONDS - t0)) -lt 20 ]; do
    [ -z "$(pgrep -f "sr-mys2425e" | grep -v $$ 2>/dev/null)" ] && break
    sleep 0.3
  done
  rm -f "$(dirname "$INSTALLED")"/*.sr-old-*
  # 先写临时文件再 rename：与真实换机同一手法，避免任何 ETXTBSY 窗口。
  cp "$OLD" "$INSTALLED.new"
  chmod +x "$INSTALLED.new"
  mv -f "$INSTALLED.new" "$INSTALLED"
  OLD_SHA="$(sha256sum "$INSTALLED" | cut -d' ' -f1)"
}

run_success_case() { # $1=tag  $2=killmode  $3=sid-index
  local tag="$1" km="$2" idx="$3" name="sr-mys2425e-$1" sid="veri$3"
  echo
  echo "════ A/B. 成功路径：KillMode=$km ════"
  reinstall_old
  write_unit "$name" always "$km" "" "$idx"
  systemctl --user start "$name.service" 2>/dev/null
  for _ in $(seq 1 60); do sess_tokens "$sid" >/dev/null 2>&1 && break; sleep 1; done
  local before_tok before_line pid0 nr0
  before_tok="$(sess_tokens "$sid")"
  [ -n "$before_tok" ] || { bad "$tag 升级前读不到身份"; return 1; }
  echo "  升级前: $(show_unit "$name")"
  local old_sha; old_sha="$(sha256sum "$INSTALLED" | cut -d' ' -f1)"
  [ "$old_sha" = "$OLD_SHA" ] || { bad "$tag 升级前磁盘不是旧版"; return 1; }
  pid0="$(systemctl --user show "$name.service" -p MainPID --value)"
  nr0="$(systemctl --user show "$name.service" -p NRestarts --value)"
  echo "  升级前身份 token=${before_tok:0:16}… pid=$pid0 NRestarts=$nr0"

  trigger_upgrade "$sid" && ok "$tag relay 下发了升级任务" || { bad "$tag relay 未下发任务"; return 1; }
  local st; st="$(wait_stage "$sid" "" 180)"
  echo "  relay 终结状态: $st"
  case "$st" in registered*) ok "$tag relay 显示 registered";; *) bad "$tag relay 未显示 registered: $st";; esac

  sleep 2
  local now pid1 nr1 exe sha tok1 live backup
  now="$(show_unit "$name")"
  echo "  升级后: $now"
  pid1="$(systemctl --user show "$name.service" -p MainPID --value)"
  nr1="$(systemctl --user show "$name.service" -p NRestarts --value)"
  exe="$(exe_of "$pid1")"
  sha="$(sha256sum "$INSTALLED" 2>/dev/null | cut -d' ' -f1)"
  tok1="$(sess_tokens "$sid")"
  live="$(live_for_sid "$sid")"
  backup="$(ls "$(dirname "$INSTALLED")"/*.sr-old-* 2>/dev/null | wc -l)"

  chk "$([ "$(systemctl --user show "$name.service" -p ActiveState --value)" = active ] && echo 1)" "$tag ActiveState=active"
  chk "$([ "$pid1" != "$pid0" ] && [ "$pid1" != 0 ] && echo 1)" "$tag MainPID 是新进程（$pid0 → $pid1）"
  chk "$([ "$nr1" = "$((nr0 + 1))" ] && echo 1)" "$tag NRestarts 恰好 +1（$nr0 → $nr1）"
  chk "$([ "$exe" = "$INSTALLED" ] && echo 1)" "$tag /proc/$pid1/exe 指向 target（$exe）"
  chk "$([ "$sha" = "$ART_SHA" ] && echo 1)" "$tag 磁盘 hash 为新制品（${sha:0:16}…）"
  chk "$([ "$tok1" = "$before_tok" ] && echo 1)" "$tag session_id/Token 不变"
  chk "$([ "$live" = 1 ] && echo 1)" "$tag 同 session 只有 1 个活进程（$live）"
  chk "$([ "$backup" = 0 ] && echo 1)" "$tag backup 已删除"
  return 0
}

run_success_case "cg" control-group 1
run_success_case "mx" mixed 2

# ══════════════════════════════════════════════════════════════════
# C：自回滚（SR_UPGRADE_SELF_ROLLBACK_SECS=30，新 agent 注册不上）
#
# 用**TCP 代理**在 relay 与 agent 之间剪断链路，而不是把 relay 杀掉：
#   - 杀掉 relay 会连同它内存里的任务状态一起消失，重启后没有 `failed` 可收敛；
#   - 代理剪断后 relay 仍持有本轮 `upgrade_id` 与 `swapping` 状态，旧版回滚后
#     重连时 relay 才能按"重连版本低于目标"给出 `failed`——这正是要验证的收敛。
# ══════════════════════════════════════════════════════════════════
echo
echo "════ C. 自回滚路径（窗口 30s，新 agent 注册不上）════"
name="sr-mys2425e-rb"; sid="veri3"
reinstall_old
write_unit "$name" always control-group 30 3
systemctl --user start "$name.service" 2>/dev/null
for _ in $(seq 1 60); do sess_tokens "$sid" >/dev/null 2>&1 && break; sleep 1; done
old_sha_pre="$(sha256sum "$INSTALLED" 2>/dev/null | cut -d' ' -f1)"
chk "$([ "$old_sha_pre" = "$(sha256sum "$OLD" | cut -d' ' -f1)" ] && echo 1)" "C 升级前磁盘是已安装旧版"
pid_old="$(systemctl --user show "$name.service" -p MainPID --value)"
echo "  升级前: $(show_unit "$name") hash=${old_sha_pre:0:16}…"

trigger_upgrade "$sid" && ok "C relay 下发了升级任务" || bad "C relay 未下发任务"

# 换盘完成（磁盘已是新 hash）后立刻剪断链路，让新 agent 在 30s 窗口内注册不上。
t0=$SECONDS; swapped=0
while [ $((SECONDS - t0)) -lt 60 ]; do
  cur="$(sha256sum "$INSTALLED" 2>/dev/null | cut -d' ' -f1)"
  [ "$cur" = "$ART_SHA" ] && { swapped=1; break; }
  sleep 1
done
chk "$swapped" "C 换盘完成（磁盘已是新制品）"
cut_link
echo "  链路已剪断，等自回滚窗口（30s）到期…"

# 等磁盘回到旧 hash（新旧 hash 不同，所以这是明确判据）。
t0=$SECONDS; rolled=0
while [ $((SECONDS - t0)) -lt 120 ]; do
  cur="$(sha256sum "$INSTALLED" 2>/dev/null | cut -d' ' -f1)"
  if [ "$cur" = "$old_sha_pre" ]; then rolled=1; break; fi
  sleep 2
done
chk "$rolled" "C 磁盘恢复为旧 hash（${old_sha_pre:0:16}…）"

# systemd 在 `RestartSec` 内会把 MainPID 置 0、ActiveState 置 activating：
# 必须等它稳定，否则断言读到的是"正在重启"这个中间态而不是结论。
wait_active "$name" 60
now="$(show_unit "$name")"
echo "  回滚后: $now"
pid_rb="$(systemctl --user show "$name.service" -p MainPID --value)"
exe_rb="$(exe_of "$pid_rb")"
chk "$([ "$(systemctl --user show "$name.service" -p ActiveState --value)" = active ] && echo 1)" "C service 仍 active"
chk "$([ -n "$pid_rb" ] && [ "$pid_rb" != 0 ] && [ "$pid_rb" != "$pid_old" ] && echo 1)" "C MainPID 是新拉起的旧版进程（$pid_old → $pid_rb）"
chk "$([ "$exe_rb" = "$INSTALLED" ] && echo 1)" "C /proc/$pid_rb/exe 指向 target"
chk "$([ "$(sha256sum "$INSTALLED" | cut -d' ' -f1)" = "$(sha256sum "$OLD" | cut -d' ' -f1)" ] && echo 1)" "C 磁盘是已安装旧版的字节"

# 恢复链路：旧版重连后，relay 按"重连版本低于目标"给出 failed。
restore_link
echo "  链路已恢复，等 relay 收敛终态…"
st="$(wait_stage "$sid" "" 120)"
echo "  relay 最终状态: $st"
case "$st" in failed*) ok "C relay 最终显示 failed";; *) bad "C relay 最终不是 failed: $st";; esac
systemctl --user stop "$name.service" 2>/dev/null

# ══════════════════════════════════════════════════════════════════
# D：Restart=on-failure + 未授权 ⇒ 换机前拒绝
# ══════════════════════════════════════════════════════════════════
echo
echo "════ D. Restart=on-failure + 未授权 ⇒ 换机前拒绝 ════"
name="sr-mys2425e-of"; sid="veri4"
reinstall_old
write_unit "$name" on-failure control-group "" 4
systemctl --user start "$name.service" 2>/dev/null
for _ in $(seq 1 60); do sess_tokens "$sid" >/dev/null 2>&1 && break; sleep 1; done
sha_before="$(sha256sum "$INSTALLED" | cut -d' ' -f1)"
pid_before="$(systemctl --user show "$name.service" -p MainPID --value)"
echo "  升级前: $(show_unit "$name")"
trigger_upgrade "$sid" && ok "D relay 下发了任务（拒绝发生在 agent 侧）" || bad "D relay 未下发任务"
st="$(wait_stage "$sid" "failed" 120)"
echo "  relay 状态: $st"
case "$st" in failed*) ok "D 换机前 failed";; *) bad "D 未 failed: $st";; esac
case "$st" in *"Restart=always"*) ok "D 原因包含 Restart=always";; *) bad "D 原因缺少 Restart=always: $st";; esac
sleep 1
sha_after="$(sha256sum "$INSTALLED" | cut -d' ' -f1)"
pid_after="$(systemctl --user show "$name.service" -p MainPID --value)"
chk "$([ "$sha_before" = "$sha_after" ] && echo 1)" "D 磁盘未动（hash 不变）"
chk "$([ "$pid_before" = "$pid_after" ] && [ "$pid_after" != 0 ] && echo 1)" "D 旧 agent 仍在线（MainPID $pid_after）"
systemctl --user stop "$name.service" 2>/dev/null

echo
if [ "$FAIL" = 0 ]; then echo "════ 全部通过 ════"; else echo "════ 有失败项 ════"; fi
exit "$FAIL"
