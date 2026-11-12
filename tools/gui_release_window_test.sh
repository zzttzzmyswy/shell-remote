#!/bin/bash
# 发布制品**真实窗口**门禁（MYS-2821 重写，MYS-2827 修正失败传播）。
#
# 上一版门禁用"进程 exit 0"当作开窗成功，被一个完全不创建窗口的假程序直接骗过。
# 再上一版（PR83@2be0dce）虽然换了判据，却在负例分支里用 `fail=$before` 把刚记录
# 的失败**恢复掉**：负责人把真实 GUI 当负例喂进去，脚本明确打印"负例未拦住"，
# 最后仍然 PASS / exit 0。判据再多，只要失败被吞掉就等于没有门禁。
#
# 本版的三条硬规矩：
#
#   1. **判定靠返回码与结构化原因码，不靠 grep "✓" 文字**。`run_window_case` 把
#      结果写进 `RESULT`（失败原因码）并返回 0/1；负例断言的是"返回非零 **且**
#      原因码符合预期"。
#   2. **失败一旦记录就必须传播**。全局 `fail` 只由外层断言写，用例内部的断言写
#      用例私有的 `CASE_BAD`——没有任何"把 fail 恢复回去"的分支，也不可能出现
#      "负例失败被清零"。
#   3. **不许空跑 PASS**：没给任何用例直接 exit 2。
#
# 成功 = 本次进程（按 PID 归属）在 X 服务器上映射出一个可见窗口
#        + 该窗口确实有可截取的内容
#        + 进程在整个观察期内一直存活
#        + 窗口能被正常关闭（WM_DELETE_WINDOW），关闭后进程自行退出、无残留
#
# 用法:
#   tools/gui_release_window_test.sh --linux-ui dist-gnu/shell-remote-ui-x86_64
#   tools/gui_release_window_test.sh --linux-ui <bin> \
#       --expect-fail-no-window       tools/fake_no_window_app.sh \
#       --expect-fail-timeout         tools/fake_timeout_app.sh \
#       --expect-fail-wrong-pid       tools/fake_foreign_window_app.sh \
#       --expect-fail-missing-restore <ui-binary>
#
# 退出码：全部断言通过 0，否则 1；没给用例 2。
set -u

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ROOT_DIR

LINUX_UI=""
NEG_NO_WINDOW=""
NEG_TIMEOUT=""
NEG_WRONG_PID=""
NEG_MISSING_RESTORE=""
CLOSE_WAIT="${CLOSE_WAIT:-25}"
WINDOW_TIMEOUT="${WINDOW_TIMEOUT:-60}"
NEG_WINDOW_TIMEOUT="${NEG_WINDOW_TIMEOUT:-20}"

while [ $# -gt 0 ]; do
  case "$1" in
    --linux-ui)                    LINUX_UI="${2:?}"; shift 2 ;;
    --expect-fail-no-window)       NEG_NO_WINDOW="${2:?}"; shift 2 ;;
    --expect-fail-timeout)         NEG_TIMEOUT="${2:?}"; shift 2 ;;
    --expect-fail-wrong-pid)       NEG_WRONG_PID="${2:?}"; shift 2 ;;
    --expect-fail-missing-restore) NEG_MISSING_RESTORE="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,33p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$LINUX_UI$NEG_NO_WINDOW$NEG_TIMEOUT$NEG_WRONG_PID$NEG_MISSING_RESTORE" ]; then
  echo "没有给任何用例：至少要 --linux-ui 或一个 --expect-fail-*（拒绝空跑 PASS）" >&2
  exit 2
fi

# ── 计数器 ──────────────────────────────────────────────────────────
# fail     = 全局失败（只由外层断言写），最终决定退出码
# npass    = 通过的断言数（交付里要报的分子）
# nassert  = 断言总数
# SCORE    = 1 时用例内部断言计入全局统计。负例要"故意失败"，所以它的内部
#            断言不计分（否则 PASS 比分会因为"负例被正确拦住"而变难看），
#            每个负例只作为**一条**断言计入。
fail=0; npass=0; nassert=0; SCORE=1
ok(){  echo "  ✓ $1"; npass=$((npass+1)); nassert=$((nassert+1)); }
bad(){ echo "  ✗ $1"; fail=1; nassert=$((nassert+1)); }

# 用例内部断言：只影响本用例的 CASE_BAD，**不碰全局 fail**。
CASE_BAD=0
cok(){ echo "  ✓ $1"; [ "$SCORE" = 1 ] && { npass=$((npass+1)); nassert=$((nassert+1)); }; }
cbad(){ echo "  ✗ $1"; CASE_BAD=1; [ "$SCORE" = 1 ] && nassert=$((nassert+1)); }

TMPROOT="$(mktemp -d)"
# 只认本测试自己起的 PID 与本测试自己的临时目录；绝不做 basename 全局 pkill，
# 也绝不 `wineserver -k`（那会杀掉同机其它任务的前缀）。
CASE_PIDS=()
CASE_XPID=""
cleanup_all(){
  local p
  for p in "${CASE_PIDS[@]:-}"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null; done
  [ -n "$CASE_XPID" ] && kill -9 "$CASE_XPID" 2>/dev/null
  rm -rf "$TMPROOT"
}
trap cleanup_all EXIT

free_display(){ local n; for n in $(seq 60 120); do [ -e "/tmp/.X11-unix/X$n" ] || { echo ":$n"; return 0; }; done; return 1; }

# 找**属于指定 PID** 的可见窗口 id（按 _NET_WM_PID，绝不按窗口标题匹配）。
# 这正是"错误窗口 PID"负例的判据所在：同屏有窗口但属于别的进程时，这里必须为空。
windows_for_pid(){ xdotool search --onlyvisible --pid "$1" 2>/dev/null; }

wait_window(){ # $1 pid $2 timeout_s → 打印窗口 id；超时返回 1
  local pid=$1 t0=$SECONDS w
  while [ $((SECONDS - t0)) -lt "$2" ]; do
    kill -0 "$pid" 2>/dev/null || return 1
    w="$(windows_for_pid "$pid" | tail -1)"
    [ -n "$w" ] && { echo "$w"; return 0; }
    sleep 0.5
  done
  return 1
}

# 残留检查：只看**这个二进制路径**的真实进程，按 /proc/<pid>/exe 比对。
# 不用 pgrep -f（会匹配到跑本脚本的 shell 命令行自身，必然误报）。
leftover_for(){ # $1 bin $2 skip-pid
  python3 - "$1" "$2" <<'PY'
import os, sys
target = os.path.realpath(sys.argv[1])
skip = int(sys.argv[2])
n = 0
for pid in filter(str.isdigit, os.listdir('/proc')):
    if int(pid) == skip:
        continue
    try:
        exe = os.path.realpath('/proc/%s/exe' % pid)
    except OSError:
        continue
    if exe == target:
        n += 1
print(n)
PY
}

# 跑一个 UI 进程并断言"真实开窗 → 存活 → 正常关闭 → 退出"。
#
# 返回：0 = 该用例全部断言成立；1 = 有断言失败。
# 副作用：RESULT = 原因码（成功 "ok"），CASE_BAD = 是否有失败。
#   no_display / exited_early / no_window / no_content / died_after_map
#   close_timeout / bad_exit / leftover / no_closer
RESULT=""
LAST_W=""
run_window_case(){ # $1 标签 $2 二进制 [额外 argv...]；WINDOW_TIMEOUT 可用环境变量覆盖
  local label="$1" bin="$2"; shift 2
  local W D pid wid shot px closed rc left closer to="${CASE_WINDOW_TIMEOUT:-$WINDOW_TIMEOUT}"
  RESULT=""; CASE_BAD=0
  W="$(mktemp -d -p "$TMPROOT")"; LAST_W="$W"
  D="$(free_display)" || { RESULT="no_display"; cbad "$label: 没有可用的 X display"; return 1; }
  Xvfb "$D" -screen 0 1280x900x24 -nolisten tcp > "$W/xvfb.log" 2>&1 &
  local xpid=$!; CASE_XPID="$xpid"; CASE_PIDS+=("$xpid")
  sleep 1.5
  export DISPLAY="$D"
  # 隔离 HOME / 配置：绝不碰用户真实 ~/.shell-remote
  mkdir -p "$W/home/.shell-remote"
  HOME="$W/home" SR_LOG_DIR="$W/logs" nohup "$bin" "$@" > "$W/gui.log" 2>&1 &
  pid=$!; CASE_PIDS+=("$pid")

  wid="$(wait_window "$pid" "$to")" || {
    if ! kill -0 "$pid" 2>/dev/null; then
      RESULT="exited_early"
      wait "$pid" 2>/dev/null; rc=$?
      cbad "$label: 进程在映射窗口前退出（rc=$rc）"
    else
      RESULT="no_window"
      cbad "$label: ${to}s 内没有由本进程(PID $pid)映射的可见窗口"
      # 诊断：同屏有没有别人的窗口？有就证明这里判的是"归属"而不是"有没有"。
      echo "      同屏可见窗口数（含其它进程）：$(xdotool search --onlyvisible --name '.*' 2>/dev/null | wc -l)"
    fi
    kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    kill -9 "$xpid" 2>/dev/null; wait "$xpid" 2>/dev/null; CASE_XPID=""
    unset DISPLAY
    return 1
  }
  cok "$label: 本进程(PID $pid)映射出可见窗口 $wid"

  # 窗口必须真有内容：截图非空且能读出尺寸。
  shot="$W/window.png"
  if import -window "$wid" "$shot" 2>/dev/null && [ -s "$shot" ]; then
    px="$(identify -format '%wx%h' "$shot" 2>/dev/null)"
    if [ -n "$px" ] && [ "${px%x*}" -ge 200 ] 2>/dev/null; then
      cok "$label: 窗口有可截取内容（$px）"
      cp "$shot" "$W/case-${label}.png" 2>/dev/null || true
    else
      RESULT="no_content"; cbad "$label: 截图尺寸异常（${px:-读不出}）"
    fi
  else
    RESULT="no_content"; cbad "$label: 无法截取窗口内容"
  fi

  # 窗口出现之后进程必须仍然存活（不是画完就崩）。
  sleep 2
  if kill -0 "$pid" 2>/dev/null; then
    cok "$label: 窗口出现后进程仍存活"
  else
    RESULT="died_after_map"; cbad "$label: 窗口出现后进程已退出"
  fi

  # 正常关闭：向窗口发 WM_DELETE_WINDOW（等价点关闭按钮）。
  #
  # **不能**用 `xdotool windowclose`：它发的是 XDestroyWindow，服务端直接销毁
  # 窗口，winit 随后访问失效窗口 ID 会 panic（实测 TranslateCoordinates
  # BadWindow → rc=101）。真实用户点关闭走的是 WM_DELETE_WINDOW 协议消息。
  closer="$ROOT_DIR/tools/wm_close_window.py"
  if [ -f "$closer" ]; then
    python3 "$closer" "$wid" >/dev/null 2>&1
  elif command -v wmctrl >/dev/null 2>&1; then
    wmctrl -i -c "$wid" 2>/dev/null
  else
    RESULT="no_closer"
    cbad "$label: 没有可用的 WM_DELETE_WINDOW 发送工具（缺 tools/wm_close_window.py 且无 wmctrl）"
  fi
  closed=0
  for _ in $(seq 1 $((CLOSE_WAIT * 2))); do
    kill -0 "$pid" 2>/dev/null || { closed=1; break; }
    sleep 0.5
  done
  if [ $closed -eq 1 ]; then
    wait "$pid" 2>/dev/null; rc=$?
    # 正常关闭 → 退出码 0。非 0（尤其 101=panic）必须暴露出来，不能算"正常关闭"。
    if [ "$rc" = "0" ]; then
      cok "$label: 关闭窗口后进程自行退出（rc=0）"
    else
      RESULT="bad_exit"
      cbad "$label: 关闭窗口后进程以 rc=$rc 退出（异常，101 通常意味着 panic）"
    fi
  else
    RESULT="close_timeout"
    cbad "$label: 关闭窗口 ${CLOSE_WAIT}s 后进程仍在（强杀不算通过）"
    kill -9 "$pid" 2>/dev/null
  fi
  left="$(leftover_for "$bin" "$pid")"
  if [ "$left" = "0" ]; then
    cok "$label: 无残留进程"
  else
    RESULT="${RESULT:-leftover}"
    cbad "$label: 残留 $left 个进程"
  fi

  kill -9 "$xpid" 2>/dev/null; wait "$xpid" 2>/dev/null; CASE_XPID=""
  unset DISPLAY
  [ -z "$RESULT" ] && RESULT="ok"
  [ "$RESULT" = "ok" ]
}

# 负例统一断言：必须**非零**返回，且原因码符合预期。
# 关键：用例内部的 cbad 只写 CASE_BAD，这里既不读它也不清它——所以"负例该失败"
# 与"负例意外成功"是两条互斥的判定，绝不会出现"失败被清零后仍 PASS"。
expect_blocked(){ # $1 标签 $2 期望原因码 $3 二进制 [额外 argv...]
  local label="$1" want="$2" bin="$3"; shift 3
  echo
  echo "── 负例：$label 必须被拦住（期望原因码 $want）──"
  SCORE=0   # 负例的内部断言是"故意失败"，不计入最终比分
  if run_window_case "$label" "$bin" "$@"; then
    SCORE=1
    bad "负例未拦住：$label 被判定为开窗成功（RESULT=$RESULT）"
    sed -n '1,5p' "$LAST_W/gui.log" 2>/dev/null | sed 's/^/       /'
    return
  fi
  SCORE=1
  if [ "$RESULT" = "$want" ]; then
    ok "负例已拦住：$label（原因码 $RESULT）"
  else
    bad "负例拦住了但原因不符：$label 期望原因码 $want，实际 $RESULT"
  fi
}

# ── 正例：真实窗口 ─────────────────────────────────────────────────
if [ -n "$LINUX_UI" ]; then
  echo "── 正例：$LINUX_UI 必须真实开窗并正常关闭 ──"
  if ! run_window_case "linux-ui" "$LINUX_UI"; then
    bad "正例失败：$LINUX_UI 没能通过窗口断言（原因码 $RESULT）"
    [ -f "$LAST_W/gui.log" ] && sed -n '1,8p' "$LAST_W/gui.log" | sed 's/^/       /'
  fi
fi

# ── 负例 1：完全不创建窗口、exit 0 的假程序 ────────────────────────
if [ -n "$NEG_NO_WINDOW" ]; then
  expect_blocked "fake-no-window" exited_early "$NEG_NO_WINDOW"
fi

# ── 负例 2：挂着不退出、也永远不建窗口 → 超时必须判失败 ────────────
if [ -n "$NEG_TIMEOUT" ]; then
  CASE_WINDOW_TIMEOUT="$NEG_WINDOW_TIMEOUT" \
    expect_blocked "fake-timeout" no_window "$NEG_TIMEOUT"
fi

# ── 负例 3：窗口存在但不属于本进程（按标题/按"有窗口"匹配会误判成功）──
if [ -n "$NEG_WRONG_PID" ]; then
  CASE_WINDOW_TIMEOUT="$NEG_WINDOW_TIMEOUT" \
    expect_blocked "fake-wrong-pid" no_window "$NEG_WRONG_PID"
fi

# ── 负例 4：要求恢复但交接文件不存在 → fail-closed，必须非零退出 ────
#
# 这一条不是窗口断言，是入口断言：缺恢复文件不许当成"普通启动"。
# 用独立进程直接跑，只看退出码与 stderr。
if [ -n "$NEG_MISSING_RESTORE" ]; then
  echo
  echo "── 负例：交接文件缺失时必须 fail-closed（期望非零退出）──"
  W="$(mktemp -d -p "$TMPROOT")"
  mkdir -p "$W/home/.shell-remote"
  missing="$W/definitely-not-there.json"
  HOME="$W/home" SR_LOG_DIR="$W/logs" timeout 90 "$NEG_MISSING_RESTORE" \
    --upgrade-restore "$missing" > "$W/out.log" 2>&1
  rc=$?
  if [ "$rc" -eq 0 ]; then
    bad "负例未拦住：交接文件缺失时进程仍以 0 退出"
  elif [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
    bad "负例未拦住：交接文件缺失时进程挂住（rc=$rc），不是 fail-closed"
  else
    ok "负例已拦住：交接文件缺失时非零退出（rc=$rc）"
    sed -n '1,3p' "$W/out.log" | sed 's/^/       /'
  fi
  if grep -qi 'restore failed\|handoff' "$W/out.log"; then
    ok "负例已拦住：stderr 明确说明恢复失败原因"
  else
    bad "非零退出但没说清原因（stderr 未见 handoff/restore failed 字样）"
    sed -n '1,5p' "$W/out.log" | sed 's/^/       /'
  fi
fi

echo
echo "断言：PASS=$npass / 共 $nassert，FAIL=$([ $fail -eq 0 ] && echo 0 || echo 1)"
[ $fail -eq 0 ] && echo PASS || echo FAIL
exit $fail
