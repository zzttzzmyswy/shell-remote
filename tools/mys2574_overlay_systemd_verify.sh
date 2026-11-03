#!/bin/bash
# MYS-2574 真实临时 systemd unit 验证：**目标目录不可写**时的 overlay 升级
# （`Restart=always` + 非 root + 无 polkit 授权 + root 所有的安装目录的等价物）。
#
# 覆盖验收标准 3 要求的那一条：
#   unit 的 `ExecStart` 指向 **0555 目录**里的 base、`Restart=always`；
#   relay 下发升级 ⇒ agent 装进 overlay ⇒ 退出 ⇒ systemd 拉起 base ⇒
#   base 交接给 overlay 版本 ⇒ 重新注册为新版本。
#   断言：service 仍 active、受 systemd 管、`/proc/<MainPID>/exe` 指向 overlay
#   二进制、版本已升级、同 session 只有 1 个活进程、base 二进制字节未变、
#   重启（`systemctl restart`）后依然是新版本。
#
# 只操作本脚本自己建的 unit（sr-mys2574-*）。前台一次跑完，trap 清理。
#
# 用法：
#   SR_OVERLAY_BASE_BIN=/path/to/0.59.12 \
#   SR_OVERLAY_NEW_BIN=/path/to/0.59.13 \
#   tools/mys2574_overlay_systemd_verify.sh
#
# 两个二进制必须是**版本号不同**的两份构建（relay 用自身版本当升级目标，
# 所以 `NEW` 必须高于 `BASE`）。没有就跳过并说明原因。
set -u

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# 已安装的 base：放在 **0555** 目录里，模拟 root 所有的 /usr/local/bin。
BASE_BIN="${SR_OVERLAY_BASE_BIN:-}"
# relay 与升级目标（必须是更高版本的那份构建）。
NEW_BIN="${SR_OVERLAY_NEW_BIN:-}"

PORT="${PORT:-3257}"
BASE_URL="http://127.0.0.1:${PORT}"
ADMIN_PATH=/sr-admin-ovl
UNITDIR="$HOME/.config/systemd/user"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
W="$(mktemp -d "${TMPDIR:-/tmp}/sr-ovl-sysd.XXXXXX")"
INSTALL_DIR="$W/bin"          # 0555
INSTALLED="$INSTALL_DIR/shell-remote"
export HOME_OVL="$W/home"     # 设备上的 HOME（overlay 落在这里）
export XDG_DATA_HOME="$W/xdg"
OVERLAY_ROOT="$XDG_DATA_HOME/shell-remote/overlay"
RELAY_PID=""
FAIL=0

ok()  { echo "  ✅ $*"; }
bad() { echo "  ❌ $*"; FAIL=$((FAIL + 1)); }
chk() { [ "$1" = 1 ] && ok "$2" || bad "$2"; }

cleanup() {
  local u
  for u in "$UNITDIR"/sr-mys2574-*.service; do
    [ -e "$u" ] || continue
    systemctl --user stop "$(basename "$u")" 2>/dev/null
    systemctl --user reset-failed "$(basename "$u")" 2>/dev/null
    rm -f "$u"
  done
  systemctl --user daemon-reload 2>/dev/null
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  # 0555 目录里的残留要先恢复权限才删得掉。
  [ -d "$INSTALL_DIR" ] && chmod 0755 "$INSTALL_DIR" 2>/dev/null
  rm -rf "$W"
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

# 前置条件
[ -n "$BASE_BIN" ] || { echo "跳过：未设置 SR_OVERLAY_BASE_BIN"; exit 0; }
[ -n "$NEW_BIN" ] || { echo "跳过：未设置 SR_OVERLAY_NEW_BIN"; exit 0; }
[ -x "$BASE_BIN" ] || { echo "FAIL: SR_OVERLAY_BASE_BIN 不可执行: $BASE_BIN"; exit 1; }
[ -x "$NEW_BIN" ] || { echo "FAIL: SR_OVERLAY_NEW_BIN 不可执行: $NEW_BIN"; exit 1; }
if [ "$(id -u)" = 0 ]; then
  echo "跳过：以 root 运行时 chmod 0555 不生效，无法构造"目录不可写"的现场"
  exit 0
fi
if ! systemctl --user is-system-running >/dev/null 2>&1; then
  echo "跳过：没有可用的用户级 systemd manager（XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR）"
  exit 0
fi
BASE_VER="$("$BASE_BIN" --version 2>&1 | awk '{print $2}')"
NEW_VER="$("$NEW_BIN" --version 2>&1 | awk '{print $2}')"
if [ "$BASE_VER" = "$NEW_VER" ]; then
  echo "FAIL: 两个二进制版本相同（$BASE_VER），relay 会判定"无需升级""
  exit 1
fi

echo "════ MYS-2574 overlay 升级的真实 systemd unit 验证 ════"
echo "非 root 账户: $(id -un) (uid=$(id -u))；unit 目录 $UNITDIR"
echo "systemd --user manager: $(systemctl --user is-system-running 2>&1)"
echo "base=$BASE_VER（装在 0555 目录）  new=$NEW_VER（relay + 升级目标）"
echo

# ── 制品：新版二进制放进 relay 的制品库 ────────────────────────────
ART="$W/artifacts"; mkdir -p "$ART"
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;; aarch64|arm64) ARCH=aarch64 ;;
  armv7l|armv7) ARCH=armv7 ;; *) ARCH=unknown ;;
esac
ART_NAME="shell-remote-${ARCH}"
cp "$NEW_BIN" "$ART/$ART_NAME"
chmod +x "$ART/$ART_NAME"
ART_SHA="$(sha256sum "$ART/$ART_NAME" | cut -d' ' -f1)"
echo "  制品 $ART_NAME sha=${ART_SHA:0:16}…"

# ── base 装进 0555 目录 ──────────────────────────────────────────
mkdir -p "$INSTALL_DIR" "$HOME_OVL" "$XDG_DATA_HOME" "$W/logs"
cp "$BASE_BIN" "$INSTALLED"
chmod 0755 "$INSTALLED"
BASE_SHA="$(sha256sum "$INSTALLED" | cut -d' ' -f1)"
chmod 0555 "$INSTALL_DIR"
echo "  安装目录 $INSTALL_DIR 权限 $(stat -c '%a' "$INSTALL_DIR")（不可写）"
# 自检：确认真的不可写——否则整条用例没有意义。
if ( : > "$INSTALL_DIR/.probe" ) 2>/dev/null; then
  rm -f "$INSTALL_DIR/.probe"
  echo "FAIL: $INSTALL_DIR 仍然可写，构造不出目标目录不可写的现场"
  exit 1
fi
echo "  确认写不进 $INSTALL_DIR ✅"
echo

# ── relay ────────────────────────────────────────────────────────
echo "── relay ──"
"$NEW_BIN" relay --bind "127.0.0.1:${PORT}" --auth ovl-pass --no-tls \
  --admin-path "$ADMIN_PATH" --admin-user admin --admin-pass ovlpass \
  --download-dir "$ART" --allow-custom-upgrade \
  --upgrade-cdn "http://127.0.0.1:1" > "$W/relay.log" 2>&1 &
RELAY_PID=$!
for _ in $(seq 1 80); do curl -s -o /dev/null "$BASE_URL/" && break; sleep 0.2; done
COOKIE="$(curl -s -i -X POST "$BASE_URL$ADMIN_PATH/login" -H 'Content-Type: application/json' \
  -d '{"user":"admin","pass":"ovlpass"}' | sed -n 's/^set-cookie: *sr_admin=\([^;]*\).*/\1/ip')"
[ -n "$COOKIE" ] || { echo "FAIL: admin 登录失败"; exit 1; }
api() { curl -s -H "cookie: sr_admin=$COOKIE" "$BASE_URL$ADMIN_PATH$1" "${@:2}"; }
echo "  relay 已起在 $BASE_URL"
echo

# ── unit：ExecStart 指向 0555 目录里的 base，Restart=always ───────
NAME="sr-mys2574-ovl"
SID="ovlverify1"
mkdir -p "$UNITDIR"
cat > "$UNITDIR/$NAME.service" <<UNIT
[Unit]
Description=MYS-2574 overlay upgrade verification
After=network-online.target

[Service]
Type=simple
WorkingDirectory=$W
ExecStart=$INSTALLED agent --relay-url $BASE_URL --key ovl-pass-$NAME --session-id $SID --shell /bin/sh --root $W
Restart=always
RestartSec=2
KillMode=control-group
Environment=HOME=$HOME_OVL
Environment=XDG_DATA_HOME=$XDG_DATA_HOME
StandardOutput=append:$W/$NAME.log
StandardError=append:$W/$NAME.log

[Install]
WantedBy=default.target
UNIT
systemctl --user daemon-reload
echo "════ A. 目录不可写 + Restart=always ════"
systemctl --user start "$NAME.service" 2>/dev/null
for _ in $(seq 1 60); do
  sess_ver "$SID" >/dev/null 2>&1 && break
  sleep 1
done

sess_ver() { # $1 = sid → stdout 版本（空 = 没找到/离线）
  api /api/overview | python3 -c '
import json,sys
sid=sys.argv[1]
try: d=json.load(sys.stdin)
except Exception: sys.exit(1)
for s in d.get("sessions",[]):
    if s.get("session_id")==sid and s.get("online"):
        print(s.get("agent_version") or "")
        sys.exit(0)
sys.exit(1)' "$1"
}

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
    if not argv or 'shell-remote' not in argv[0]: continue
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

wait_ver() { # $1=sid  $2=期望版本前缀  $3=timeout
  local t0=$SECONDS
  while [ $((SECONDS - t0)) -lt "$3" ]; do
    case "$(sess_ver "$1" 2>/dev/null)" in "$2"*) return 0;; esac
    sleep 1
  done
  return 1
}

wait_stage() { # $1=sid  $2=timeout
  local t0=$SECONDS st
  while [ $((SECONDS - t0)) -lt "$2" ]; do
    st="$(api /api/overview | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: sys.exit(0)
for s in d.get("sessions",[]):
    if s.get("session_id")==sys.argv[1]:
        print(((s.get("upgrade") or {}).get("stage") or "")); break' "$1")"
    case "$st" in registered|failed|rolled_back) echo "$st"; return 0;; esac
    sleep 1
  done
  echo "timeout"
  return 1
}

v0="$(sess_ver "$SID")"
chk "$([ -n "$v0" ] && echo 1)" "升级前 base 已注册上线（版本 $v0）"
chk "$([ "${v0}" = "$BASE_VER" ] && echo 1)" "升级前版本是 base（$BASE_VER）"
pid0="$(systemctl --user show "$NAME.service" -p MainPID --value)"
echo "  升级前: ActiveState=$(systemctl --user show "$NAME.service" -p ActiveState --value) MainPID=$pid0"

# ── 触发升级 ─────────────────────────────────────────────────────
RESP="$(api /api/agent/upgrade -X POST -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$SID\"}")"
chk "$(printf '%s' "$RESP" | python3 -c 'import json,sys;print(1 if json.load(sys.stdin).get("ok") is True else 0)' 2>/dev/null)" "relay 下发了升级任务"

st="$(wait_stage "$SID" 240)"
echo "  relay 终结状态: $st"
upgraded=0
wait_ver "$SID" "$NEW_VER" 180 && upgraded=1
chk "$upgraded" "升级后 relay 上版本是 $NEW_VER（实际 $(sess_ver "$SID" 2>/dev/null)）"

sleep 2
pid1="$(systemctl --user show "$NAME.service" -p MainPID --value)"
exe1="$(readlink "/proc/$pid1/exe" 2>/dev/null)"
sha_now="$(sha256sum "$INSTALLED" 2>/dev/null | cut -d' ' -f1)"
live="$(live_for_sid "$SID")"
ovl_ver="$(python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));print(d["version"],d["healthy"],d["attempts"])' "$OVERLAY_ROOT/current.json" 2>/dev/null)"

chk "$([ "$(systemctl --user show "$NAME.service" -p ActiveState --value)" = active ] && echo 1)" "service 仍 active（受 systemd 托管）"
chk "$([ "$pid1" != 0 ] && [ "$pid1" != "$pid0" ] && echo 1)" "MainPID 是新进程（$pid0 → $pid1）"
chk "$([ "${exe1##*/}" != "shell-remote" ] && echo 1)" "/proc/$pid1/exe 指向 overlay 二进制（$exe1）"
chk "$([ "$sha_now" = "$BASE_SHA" ] && echo 1)" "base 二进制一个字节没动"
chk "$([ "$live" = 1 ] && echo 1)" "同 session 只有 1 个活进程（$live）"
chk "$(case "$ovl_ver" in "$NEW_VER True"*) echo 1;; *) echo 0;; esac)" "指针健康（$ovl_ver）"

# ── 重启后依然是新版本 ───────────────────────────────────────────
echo
echo "════ B. systemctl restart 之后依然是新版本 ════"
systemctl --user restart "$NAME.service" 2>/dev/null
still=0
wait_ver "$SID" "$NEW_VER" 120 && still=1
chk "$still" "重启后版本仍是 $NEW_VER（实际 $(sess_ver "$SID" 2>/dev/null)）"
pid2="$(systemctl --user show "$NAME.service" -p MainPID --value)"
exe2="$(readlink "/proc/$pid2/exe" 2>/dev/null)"
chk "$([ "${exe2##*/}" != "shell-remote" ] && echo 1)" "重启后 /proc/$pid2/exe 仍指向 overlay（$exe2）"
chk "$([ "$(systemctl --user show "$NAME.service" -p ActiveState --value)" = active ] && echo 1)" "重启后 service 仍 active"

echo
echo "  unit 最终状态: $(systemctl --user show "$NAME.service" -p ActiveState,MainPID,NRestarts,Restart --value 2>/dev/null | tr '\n' ' ')"
echo "  设备日志尾部:"
tail -6 "$W/$NAME.log" 2>/dev/null | sed 's/^/    /'

echo
if [ "$FAIL" = 0 ]; then echo "════ 全部通过 ════"; else echo "════ 有 $FAIL 项失败 ════"; fi
exit "$FAIL"
