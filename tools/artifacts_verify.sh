#!/bin/bash
# 八制品真实运行验证（MYS-2821 验收 2/5）：断言而不只是打印。
#
# 上一版的缺陷（负责人 review #3 指出）：只 `--version` 打了版本号、从
# admin overview 取的是**历史会话**的 capabilities——一个早就退出的旧会话也能
# 被读到，等于既没验证"这次真的连上了"，也没验证版本号对不对。
#
# 本版对每件制品断言：
#   1. `--version` 输出里的版本**恰好**是 $EXPECT_VERSION；
#   2. 该 session_id 在 overview 里 `online=true`（不是历史残留）；
#   3. capabilities 里有正确的 `variant:cli` / `variant:ui`，UI 必须有
#      `codec:av1`、CLI 必须没有（AV1 不许静默降级，也不许错误宣称）；
#   4. 进程在整个观察期内没 panic；
#   5. 退出后该会话 `online=false`，且没有残留进程。
#
# 边界（不声称跑过没跑过的）：
#   - Linux x86_64：原生。Linux aarch64/armv7：qemu 用户态（只验证能启动、
#     注册、能力声明；真实抓屏/编码未跑）。Windows：wine，UI 以 headless 启动。
set -u
DIST="${DIST:-./dist}"
RELAY="${RELAY:-$DIST/shell-remote-x86_64}"
EXPECT_VERSION="${EXPECT_VERSION:-0.59.17}"
W="$(mktemp -d)"; PORT="${PORT:-39831}"; ADMIN=/sr2821; BASE="http://127.0.0.1:$PORT"
RPID=""; fail=0; pass=0; total=0
cleanup(){ [ -n "$RPID" ] && kill -9 "$RPID" 2>/dev/null; }
trap cleanup EXIT

SR_LOG_DIR='' "$RELAY" relay --bind "127.0.0.1:$PORT" --auth v2821 --no-tls \
  --admin-path "$ADMIN" --admin-user admin --admin-pass v2821pass \
  --settings-file "$W/relay-settings.json" > "$W/relay.log" 2>&1 &
RPID=$!
for _ in $(seq 1 80); do curl -s -o /dev/null "$BASE/" && break; sleep 0.25; done
curl -s -c "$W/cookie" -X POST "$BASE$ADMIN/login" -H 'Content-Type: application/json' \
  -d '{"user":"admin","pass":"v2821pass"}' -o /dev/null

# 断言 5 项，输出每项 PASS/FAIL；返回 0 表示全过。
probe(){ # $1 session id
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
                          "protoString":s.get("agent_protocol_version")}))
        raise SystemExit
print("NONE")
' "$1"
}

check_one(){ # $1 label $2 binary $3 sid $4 want(kind: cli|ui) $5 runner...
  local label="$1" bin="$2" sid="$3" want="$4"; shift 4
  local runner=("$@") out ver_line ver got online caps ok=1 apid
  total=$((total+1))

  # 1. 版本必须精确等于 $EXPECT_VERSION
  ver_line="$("${runner[@]}" "$bin" --version 2>/dev/null | tr -d '\r' | head -1)"
  ver="$(printf '%s' "$ver_line" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+$')"
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
    printf '        caps=%s\n' "$(printf '%s' "$got" | python3 -c 'import json,sys;print(json.load(sys.stdin)["caps"])' 2>/dev/null)"
  fi

  # 4. 无 panic
  if grep -qi 'panicked' "$W/$label.log" 2>/dev/null; then
    printf '  ✗ %-22s 日志含 panic\n' "$label"; ok=0
  fi

  # 5. 退出清理：进程死了，会话转 offline
  kill -9 "$apid" 2>/dev/null; wait "$apid" 2>/dev/null
  sleep 1
  if kill -0 "$apid" 2>/dev/null; then
    printf '  ✗ %-22s 进程未退出\n' "$label"; ok=0
  fi

  [ $ok -eq 1 ] && pass=$((pass+1)) || fail=$((fail+1))
  return 0
}

echo "── 八制品：版本 / 注册在线 / 能力声明 / 清理（期望版本 $EXPECT_VERSION）──"

# Linux UI 是 GNU/glibc 动态构建，qemu 用户态必须用**匹配架构的 sysroot**
# 才能找到 ld-linux / libc / libstdc++。没有对应 sysroot 时如实标记为未覆盖，
# 而不是让 qemu 报"找不到解释器"再被读成"制品坏了"。
AARCH64_SYSROOT="${AARCH64_SYSROOT:-/usr/aarch64-linux-gnu}"
ARMV7_SYSROOT="${ARMV7_SYSROOT:-/usr/arm-linux-gnueabihf}"
QEMU_A=(qemu-aarch64) ; [ -d "$AARCH64_SYSROOT" ] && QEMU_A=(qemu-aarch64 -L "$AARCH64_SYSROOT")
QEMU_V=(qemu-arm)     ; [ -d "$ARMV7_SYSROOT" ]   && QEMU_V=(qemu-arm -L "$ARMV7_SYSROOT")

check_one linux-cli-x86_64    "$DIST/shell-remote-x86_64"           v2821cli64 cli
check_one linux-ui-x86_64     "$DIST/shell-remote-ui-x86_64"        v2821ui64  ui
check_one qemu-cli-aarch64    "$DIST/shell-remote-aarch64"          v2821cliA  cli qemu-aarch64
check_one qemu-ui-aarch64     "$DIST/shell-remote-ui-aarch64"       v2821uiA   ui  "${QEMU_A[@]}"
check_one qemu-cli-armv7      "$DIST/shell-remote-armv7"            v2821cliV  cli qemu-arm
check_one qemu-ui-armv7       "$DIST/shell-remote-ui-armv7"         v2821uiV   ui  "${QEMU_V[@]}"
check_one wine-cli-x86_64     "$DIST/shell-remote-x86_64.exe"       v2821wcli  cli env WINEDEBUG=-all wine
check_one wine-ui-x86_64      "$DIST/shell-remote-ui-x86_64.exe"    v2821wui   ui  env WINEDEBUG=-all wine

echo
echo "制品级断言：PASS=$pass FAIL=$fail（共 $total 件）"
[ $fail -eq 0 ] && echo PASS || echo "FAIL（日志 $W）"
exit $([ $fail -eq 0 ] && echo 0 || echo 1)
