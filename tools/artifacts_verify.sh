#!/bin/bash
# 八制品真实运行验证（MYS-2821 验收 2）：启动 / 注册 / 能力声明。
#
# 边界（不声称跑过没跑过的）：
#   - Linux x86_64：原生执行。
#   - Linux aarch64 / armv7：qemu 用户态模拟；只验证进程可启动、能真实注册到
#     本地 relay、能力声明正确。**不验证**真实桌面抓屏/编码（模拟器无对应显示后端）。
#   - Windows x86_64：wine；UI 无窗口驱动，以 `--view headless --desktop-capture none`
#     启动。**不验证**真实 Windows 桌面抓屏。
set -u
DIST="${DIST:-./dist}"
RELAY="${RELAY:-$DIST/shell-remote-x86_64}"
W="$(mktemp -d)"; PORT="${PORT:-39831}"; ADMIN=/sr2821; BASE="http://127.0.0.1:$PORT"
RPID=""; fail=0
cleanup(){ [ -n "$RPID" ] && kill -9 "$RPID" 2>/dev/null; }
trap cleanup EXIT

SR_LOG_DIR='' "$RELAY" relay --bind "127.0.0.1:$PORT" --auth v2821 --no-tls \
  --admin-path "$ADMIN" --admin-user admin --admin-pass v2821pass \
  --settings-file "$W/relay-settings.json" > "$W/relay.log" 2>&1 &
RPID=$!
for _ in $(seq 1 80); do curl -s -o /dev/null "$BASE/" && break; sleep 0.25; done
curl -s -c "$W/cookie" -X POST "$BASE$ADMIN/login" -H 'Content-Type: application/json' \
  -d '{"user":"admin","pass":"v2821pass"}' -o /dev/null

caps_for(){ curl -s -b "$W/cookie" "$BASE$ADMIN/api/overview" 2>/dev/null | python3 -c '
import json,sys
sid=sys.argv[1]
try: d=json.load(sys.stdin)
except Exception: raise SystemExit
for s in d.get("sessions",[]):
    if s.get("session_id")==sid:
        print(json.dumps({"caps":s.get("capabilities"),"ver":s.get("agent_version")}))
        raise SystemExit
print("")
' "$1"; }

# verify <label> <binary> <sid> <want-variant> <ui|cli> <runner...>
verify(){
  local label="$1" bin="$2" sid="$3" want="$4" kind="$5"; shift 5
  local runner=("$@")
  local caps var av1 ok=1 ver
  ver="$("${runner[@]}" "$bin" --version 2>/dev/null | tr -d '\r' | head -1)"
  if [ "$kind" = ui ]; then
    "${runner[@]}" "$bin" agent --relay-url "$BASE" --key "k$sid" --session-id "$sid" \
      --view headless --desktop-capture none > "$W/$label.log" 2>&1 &
  else
    "${runner[@]}" "$bin" agent --relay-url "$BASE" --key "k$sid" --session-id "$sid" \
      --shell /bin/sh --root "$W" > "$W/$label.log" 2>&1 &
  fi
  local apid=$!
  caps=""
  for _ in $(seq 1 240); do caps="$(caps_for "$sid")"; [ -n "$caps" ] && break; sleep 0.25; done
  kill -9 $apid 2>/dev/null; wait $apid 2>/dev/null
  var="-"; printf '%s' "$caps" | grep -q 'variant:ui' && var=ui
  printf '%s' "$caps" | grep -q 'variant:cli' && var=cli
  av1=NO; printf '%s' "$caps" | grep -q 'codec:av1' && av1=YES
  [ "$var" = "$want" ] || ok=0
  if [ "$want" = ui ]; then [ "$av1" = YES ] || ok=0; else [ "$av1" = NO ] || ok=0; fi
  grep -q panicked "$W/$label.log" 2>/dev/null && ok=0
  printf '  %s %-24s %-22s variant=%-4s AV1=%-3s\n' \
    "$([ $ok -eq 1 ] && echo ✓ || echo ✗)" "$label" "$ver" "$var" "$av1"
  [ -n "$caps" ] && printf '      %s\n' "$caps"
  grep -qi 'panicked' "$W/$label.log" 2>/dev/null && echo "      ✗ 日志含 panic"
  [ $ok -eq 1 ] || fail=1
}

echo "── 八制品：启动 / 注册 / 能力声明（relay=$RELAY）──"
verify linux-cli-x86_64      "$DIST/shell-remote-x86_64"          v2821cli64 cli cli
verify linux-ui-x86_64       "$DIST/shell-remote-ui-x86_64"       v2821ui64  ui  ui
verify qemu-cli-aarch64      "$DIST/shell-remote-aarch64"         v2821cliA  cli cli qemu-aarch64
verify qemu-ui-aarch64       "$DIST/shell-remote-ui-aarch64"      v2821uiA   ui  ui  qemu-aarch64
verify qemu-cli-armv7        "$DIST/shell-remote-armv7"           v2821cliV  cli cli qemu-arm
verify qemu-ui-armv7         "$DIST/shell-remote-ui-armv7"        v2821uiV   ui  ui  qemu-arm
verify wine-cli-x86_64       "$DIST/shell-remote-x86_64.exe"      v2821wcli  cli cli env WINEDEBUG=-all wine
verify wine-ui-x86_64        "$DIST/shell-remote-ui-x86_64.exe"   v2821wui   ui  ui  env WINEDEBUG=-all wine

[ $fail -eq 0 ] && echo "PASS" || echo "FAIL（日志 $W）"
exit $fail
