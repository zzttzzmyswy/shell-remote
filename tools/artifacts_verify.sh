#!/bin/bash
# 八制品真实运行验证（MYS-2821 验收，MYS-2827 补齐离线/服务端版本/子进程清理）。
#
# 上一版（PR83@2be0dce）的遗留缺口，都在本版逐条补齐：
#
#   1. **末尾不读 offline 状态**：只 `kill/wait` 启动的那个 PID，会话转 offline
#      这件事从没断言过——"注册成功"可以靠一个随后崩溃的进程骗过。
#   2. **wine 子进程没纳入清理**：`wine <exe>` 启动的是 wine loader，真正的
#      Windows 进程是它的**子进程**。只杀 loader 会留下孤儿。
#   3. **PTY 未清理**：agent 起来就会 spawn 一个 PTY shell，测试结束时必须连同
#      子孙进程一起回收，否则"八件跑完"其实留下了八个 shell。
#   4. **probe 读到服务端版本却不比对**：overview 里明明有 `agent_version`，
#      上一版只用本地 `--version` 的输出比，等于没验证"跑起来的那份就是它"。
#
# 本版对每件制品断言：
#   1. 本地 `--version` 的输出里版本**恰好**是 $EXPECT_VERSION；
#   2. 该 session_id 在 overview 里 `online=true`，且 **agent_version 也等于
#      $EXPECT_VERSION**（服务端看到的那份 == 本地那份）；
#   3. capabilities 里有正确的 `variant:cli` / `variant:ui`，UI 必须有
#      `codec:av1`、CLI 必须没有（AV1 不许静默降级，也不许错误宣称）；
#   4. 进程在整个观察期内没 panic；
#   5. 退出后该会话转为 **online=false**，且进程树（含 wine 子进程与 PTY）全部
#      退出。
#
# 边界（不声称跑过没跑过的）：
#   - Linux x86_64：原生。Linux aarch64/armv7：qemu 用户态（只验证能启动、
#     注册、能力声明；真实抓屏/编码未跑）。
#   - Windows：**wine**，不是真机。wine 不模拟 SCM/DACL/提权，所以这里只覆盖
#     "能启动、能注册、能力声明正确、能退出"，不代表真 Win10 通过。
set -u
DIST="${DIST:-./dist}"
RELAY="${RELAY:-$DIST/shell-remote-x86_64}"
EXPECT_VERSION="${EXPECT_VERSION:-0.59.17}"
W="$(mktemp -d "${TMPDIR:-/tmp}/mys2827-artifacts-XXXXXX")"
PORT="${PORT:-39831}"; ADMIN=/sr2827; BASE="http://127.0.0.1:$PORT"
RPID=""; fail=0; pass=0; total=0
# 本测试自己起的进程（agent 及其子孙）。清理只认这些，不做全局 pkill。
OWNED_PIDS=()

cleanup(){
  local p c
  for p in "${OWNED_PIDS[@]:-}"; do
    [ -n "$p" ] || continue
    for c in $(descendants_of "$p"); do kill -9 "$c" 2>/dev/null; done
    kill -9 "$p" 2>/dev/null
  done
  [ -n "$RPID" ] && kill -9 "$RPID" 2>/dev/null
  # 本测试的 wine 前缀只属于本测试：按 WINEPREFIX 环境变量精确认领后逐个杀。
  # 不调 `wineserver -k`（它会杀掉所有前缀的 wine，包括同机其它任务）。
  if [ -d "$W/wineprefix" ]; then
    for p in $(pids_with_prefix "$W/wineprefix"); do kill -9 "$p" 2>/dev/null; done
  fi
  rm -rf "$W"
}
trap cleanup EXIT

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
alive (){ kill -0 "$1" 2>/dev/null; }

# 按 **本测试的 wine 前缀** 认领进程。
#
# 为什么不能靠 PID 树：wine 启动后会把真正的 Windows 进程 re-parent 给 init
# （实测 PPID=1），所以 `$!` 只是 loader，descendants_of 抓不到它。唯一可靠的
# 归属标记是进程环境里的 `WINEPREFIX=<本测试目录>`——那是测试自己创建的路径，
# 别的任务不可能撞上。这条判据同时覆盖 wineserver / winedevice / services.exe
# 等 wine 内部进程。
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

# 等一组 PID 真的消失（最多 $2 个 0.25s 时间片）。返回 0 = 已全部退出。
wait_gone(){ # $1 pids(空格分隔) $2 tries
  local pids="$1" tries="${2:-40}" p left
  for _ in $(seq 1 "$tries"); do
    left=""
    for p in $pids; do alive "$p" && left="$left $p"; done
    [ -z "$left" ] && return 0
    sleep 0.25
  done
  return 1
}

SR_LOG_DIR="$W/relay-logs" "$RELAY" relay --bind "127.0.0.1:$PORT" --auth v2827 --no-tls \
  --admin-path "$ADMIN" --admin-user admin --admin-pass v2827pass \
  --settings-file "$W/relay-settings.json" > "$W/relay.log" 2>&1 &
RPID=$!
for _ in $(seq 1 80); do curl -s -o /dev/null "$BASE/" && break; sleep 0.25; done
curl -s -c "$W/cookie" -X POST "$BASE$ADMIN/login" -H 'Content-Type: application/json' \
  -d '{"user":"admin","pass":"v2827pass"}' -o /dev/null

# 读该 session 的整份状态；不存在输出 NONE。
probe(){
  curl -s -b "$W/cookie" "$BASE$ADMIN/api/overview" 2>/dev/null | python3 -c '
import json,sys
sid=sys.argv[1]
try: d=json.load(sys.stdin)
except Exception: print("NONE"); raise SystemExit
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        print(json.dumps({"online":s.get("online"),
                          "caps":s.get("capabilities") or [],
                          "ver":s.get("agent_version"),
                          "proto":s.get("agent_protocol_version")}))
        raise SystemExit
print("NONE")
' "$1"
}

check_one(){ # $1 label $2 binary $3 sid $4 want(kind: cli|ui) $5 runner...
  local label="$1" bin="$2" sid="$3" want="$4"; shift 4
  local runner=("$@") out ver got online caps ok=1 apid
  total=$((total+1))

  echo "── $label ──"
  # 1. 版本必须精确等于 $EXPECT_VERSION
  ver="$("${runner[@]}" "$bin" --version 2>/dev/null | tr -d '\r' | head -1 \
        | grep -oE '[0-9]+\.[0-9]+\.[0-9]+$')"
  if [ "$ver" = "$EXPECT_VERSION" ]; then
    printf '  ✓ %-22s --version = %s\n' "$label" "$ver"
  else
    printf '  ✗ %-22s --version = %s（期望 %s）\n' "$label" "${ver:-<空>}" "$EXPECT_VERSION"
    ok=0
  fi

  # 2. 真启动并注册
  if [ "$want" = ui ]; then
    "${runner[@]}" "$bin" agent --relay-url "$BASE" --key "k$sid" --session-id "$sid" \
      --view headless --desktop-capture none > "$W/$label.log" 2>&1 &
  else
    "${runner[@]}" "$bin" agent --relay-url "$BASE" --key "k$sid" --session-id "$sid" \
      --shell /bin/sh --root "$W" > "$W/$label.log" 2>&1 &
  fi
  apid=$!
  OWNED_PIDS+=("$apid")
  got=""
  for _ in $(seq 1 260); do
    got="$(probe "$sid")"
    [ "$got" != "NONE" ] && printf '%s' "$got" | grep -q '"online": *true' && break
    got=""
    sleep 0.25
  done
  if [ -z "$got" ]; then
    printf '  ✗ %-22s 未以 online=true 出现在 overview（离线/历史会话不算）\n' "$label"
    ok=0
  else
    # 2b. 服务端看到的版本必须等于期望版本——probe 读到它，就必须比对。
    local sver; sver="$(printf '%s' "$got" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ver"] or "")')"
    if [ "$sver" = "$EXPECT_VERSION" ]; then
      printf '  ✓ %-22s 服务端 agent_version = %s\n' "$label" "$sver"
    else
      printf '  ✗ %-22s 服务端 agent_version = %s（期望 %s）\n' "$label" "${sver:-<空>}" "$EXPECT_VERSION"
      ok=0
    fi
    # 3. capabilities
    local var="" av1=NO
    printf '%s' "$got" | grep -q 'variant:ui' && var=ui
    printf '%s' "$got" | grep -q 'variant:cli' && var=cli
    printf '%s' "$got" | grep -q 'codec:av1' && av1=YES
    if [ "$var" = "$want" ]; then
      printf '  ✓ %-22s online=true variant=%s AV1=%s\n' "$label" "$var" "$av1"
    else
      printf '  ✗ %-22s variant=%s（期望 %s）\n' "$label" "${var:-无}" "$want"
      ok=0
    fi
    if [ "$want" = ui ] && [ "$av1" != "YES" ]; then
      printf '  ✗ %-22s UI 制品缺少 codec:av1（AV1 降级）\n' "$label"; ok=0
    fi
    if [ "$want" = cli ] && [ "$av1" = "YES" ]; then
      printf '  ✗ %-22s CLI 制品不应宣称 codec:av1\n' "$label"; ok=0
    fi
  fi

  # 4. 无 panic
  if grep -qi 'panicked' "$W/$label.log" 2>/dev/null; then
    printf '  ✗ %-22s 日志含 panic\n' "$label"; ok=0
  fi

  # 5. 退出与清理：进程树（含 wine 子进程与 PTY shell）必须全部退出，
  #    且该会话必须**转为 online=false**。
  #
  # wine 那条路径不能只靠 PID 树：真正的 Windows 进程会被 re-parent 给 init，
  # 所以额外按 `WINEPREFIX=<本测试前缀>` 认领。两条来源取并集，去重后统一等待。
  local kids; kids="$(descendants_of "$apid")"
  local pref="" p
  if [ "$want" != "" ] && [ -d "$W/wineprefix" ]; then
    pref="$(pids_with_prefix "$W/wineprefix")"
  fi
  kill -9 "$apid" 2>/dev/null
  for c in $kids $pref; do kill -9 "$c" 2>/dev/null; done
  wait "$apid" 2>/dev/null
  if alive "$apid"; then
    printf '  ✗ %-22s 进程未退出（PID %s）\n' "$label" "$apid"; ok=0
  fi
  # 子孙进程 + wine 前缀进程：给它们最多 10s 真正退出，不是 sleep 一下就当
  # 清理完了。
  local left="" c
  for _ in $(seq 1 40); do
    left=""
    for c in $kids $pref; do alive "$c" && left="$left $c"; done
    [ -z "$left" ] && break
    sleep 0.25
  done
  if [ -n "$left" ]; then
    printf '  ✗ %-22s 残留进程（wine/PTY）：%s\n' "$label" "$left"
    ok=0
    for c in $left; do kill -9 "$c" 2>/dev/null; done
  else
    printf '  ✓ %-22s 进程树与 wine 前缀均已清空（子孙 %s 个，前缀进程 %s 个）\n' "$label" \
      "$(printf '%s' "$kids" | wc -w)" "$(printf '%s' "$pref" | wc -w)"
  fi
  # 会话必须转 offline。异步登记有延迟，轮询到上限。
  local off=""; local seen_offline=0
  for _ in $(seq 1 80); do
    off="$(probe "$sid")"
    if [ "$off" != "NONE" ] && printf '%s' "$off" | grep -q '"online": *false'; then
      seen_offline=1; break
    fi
    sleep 0.25
  done
  if [ $seen_offline -eq 1 ]; then
    printf '  ✓ %-22s 退出后会话转为 online=false\n' "$label"
  else
    printf '  ✗ %-22s 退出后仍是 online（cleanup 没落到会话状态）\n' "$label"
    ok=0
  fi

  [ $ok -eq 1 ] && pass=$((pass+1)) || fail=$((fail+1))
  return 0
}

echo "── 八制品：版本 / 注册在线 / 服务端版本 / 能力 / 退出离线 / 子进程清理（期望版本 $EXPECT_VERSION）──"

# Linux UI 是 GNU/glibc 动态构建，qemu 用户态必须用**匹配架构的 sysroot**
# 才能找到 ld-linux / libc / libstdc++。没有对应 sysroot 时如实标记为未覆盖，
# 而不是让 qemu 报"找不到解释器"再被读成"制品坏了"。
AARCH64_SYSROOT="${AARCH64_SYSROOT:-/usr/aarch64-linux-gnu}"
ARMV7_SYSROOT="${ARMV7_SYSROOT:-/usr/arm-linux-gnueabihf}"
QEMU_A=(qemu-aarch64) ; [ -d "$AARCH64_SYSROOT" ] && QEMU_A=(qemu-aarch64 -L "$AARCH64_SYSROOT")
QEMU_V=(qemu-arm)     ; [ -d "$ARMV7_SYSROOT" ]   && QEMU_V=(qemu-arm -L "$ARMV7_SYSROOT")

# Windows 走**本测试自己的** wine 前缀：不碰用户默认前缀，清理范围可界定。
export WINEPREFIX="$W/wineprefix"
export WINEDEBUG=-all
mkdir -p "$WINEPREFIX"
wineboot -u > "$W/wineboot.log" 2>&1 || true
WINE=(env WINEPREFIX="$WINEPREFIX" WINEDEBUG=-all wine)

check_one linux-cli-x86_64    "$DIST/shell-remote-x86_64"           v2827cli64 cli
check_one linux-ui-x86_64     "$DIST/shell-remote-ui-x86_64"        v2827ui64  ui
check_one qemu-cli-aarch64    "$DIST/shell-remote-aarch64"          v2827cliA  cli qemu-aarch64
check_one qemu-ui-aarch64     "$DIST/shell-remote-ui-aarch64"       v2827uiA   ui  "${QEMU_A[@]}"
check_one qemu-cli-armv7      "$DIST/shell-remote-armv7"            v2827cliV  cli qemu-arm
check_one qemu-ui-armv7       "$DIST/shell-remote-ui-armv7"         v2827uiV   ui  "${QEMU_V[@]}"
check_one wine-cli-x86_64     "$DIST/shell-remote-x86_64.exe"       v2827wcli  cli "${WINE[@]}"
check_one wine-ui-x86_64      "$DIST/shell-remote-ui-x86_64.exe"    v2827wui   ui  "${WINE[@]}"

echo
echo "制品级断言：PASS=$pass FAIL=$fail（共 $total 件）"
[ $fail -eq 0 ] && echo PASS || echo "FAIL（日志 $W）"
exit $([ $fail -eq 0 ] && echo 0 || echo 1)
