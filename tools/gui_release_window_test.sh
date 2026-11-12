#!/bin/bash
# 发布制品**真实窗口**门禁（MYS-2821 重写版）。
#
# 上一版门禁用"进程 exit 0"当作开窗成功，被负责人用一个完全不创建窗口的假程序
# 直接骗过（review-window-false-positive.log）。本版把成功判据改成**只能由真实
# 窗口满足**的事实，并且对每个易误报的点都配一条负例：
#
#   成功 = 本次进程（按 PID 归属）在 X 服务器上映射出一个可见窗口
#          + 该窗口确实有可截取的内容
#          + 进程在整个观察期内一直存活
#          + 窗口能被正常关闭（WM_DELETE_WINDOW），关闭后进程自行退出、无残留
#
#   以下一律失败：
#     · 进程 exit 0 但没有窗口（假程序）
#     · 启动失败 / 崩溃
#     · 超时（timeout 不是 fail-closed 的成功路径）
#     · 窗口不属于本次进程（不能按名字匹配别人的同名窗口）
#     · 关闭后进程不退出，或残留子进程
#
# 用法:
#   tools/gui_release_window_test.sh --linux-ui dist-gnu/shell-remote-ui-x86_64
#   tools/gui_release_window_test.sh --linux-ui <bin> --expect-fail-no-window <fake>
#   tools/gui_release_window_test.sh --linux-ui <bin> --expect-fail-timeout <fake>
#
# 退出码：全部断言通过 0，否则 1。
set -u

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ROOT_DIR

LINUX_UI=""
NEG_NO_WINDOW=""
NEG_TIMEOUT=""
CLOSE_WAIT="${CLOSE_WAIT:-25}"

while [ $# -gt 0 ]; do
  case "$1" in
    --linux-ui)              LINUX_UI="${2:?}"; shift 2 ;;
    --expect-fail-no-window) NEG_NO_WINDOW="${2:?}"; shift 2 ;;
    --expect-fail-timeout)   NEG_TIMEOUT="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

fail=0
ok(){  echo "  ✓ $1"; }
bad(){ echo "  ✗ $1"; fail=1; }

free_display(){ local n; for n in $(seq 60 120); do [ -e "/tmp/.X11-unix/X$n" ] || { echo ":$n"; return 0; }; done; return 1; }
workdir(){ mktemp -d; }

# 找**属于指定 PID** 的可见窗口 id（按 _NET_WM_PID，绝不按窗口标题匹配）。
windows_for_pid(){ # $1 pid → 每行一个 window id
  xdotool search --onlyvisible --pid "$1" 2>/dev/null
}

# 等本次进程映射出窗口。打印窗口 id；超时返回 1。
wait_window(){ # $1 pid $2 timeout_s
  local pid=$1 t0=$SECONDS w
  while [ $((SECONDS - t0)) -lt "$2" ]; do
    # kill -0 先确认进程还活着，避免在已退出的进程上白等
    kill -0 "$pid" 2>/dev/null || return 1
    w="$(windows_for_pid "$pid" | tail -1)"
    [ -n "$w" ] && { echo "$w"; return 0; }
    sleep 0.5
  done
  return 1
}

# 通用：跑一个 UI 进程并断言"真实开窗 → 存活 → 正常关闭 → 退出"。
run_window_case(){ # $1 标签 $2 二进制
  local label="$1" bin="$2"
  local W D pid wid shot px closed rc
  W="$(workdir)"; D="$(free_display)" || { bad "$label: 没有可用的 X display"; return; }
  Xvfb "$D" -screen 0 1280x900x24 -nolisten tcp > "$W/xvfb.log" 2>&1 &
  local xpid=$!
  sleep 1.5
  export DISPLAY="$D"
  # 隔离 HOME / 配置：绝不碰用户真实 ~/.shell-remote
  mkdir -p "$W/home/.shell-remote"
  HOME="$W/home" SR_LOG_DIR="$W/logs" nohup "$bin" > "$W/gui.log" 2>&1 &
  pid=$!

  wid="$(wait_window "$pid" 60)" || {
    if ! kill -0 "$pid" 2>/dev/null; then
      bad "$label: 进程在映射窗口前退出（rc 见日志）"
      sed -n '1,3p' "$W/gui.log" | sed 's/^/       /'
    else
      bad "$label: 60s 内没有由本进程(PID $pid)映射的可见窗口"
    fi
    kill -9 "$pid" 2>/dev/null; kill -9 "$xpid" 2>/dev/null; wait "$pid" 2>/dev/null
    return
  }
  ok "$label: 本进程(PID $pid)映射出可见窗口 $wid"

  # 窗口必须真有内容：截图非空且能读出尺寸。
  shot="$W/window.png"
  if import -window "$wid" "$shot" 2>/dev/null && [ -s "$shot" ]; then
    px="$(identify -format '%wx%h' "$shot" 2>/dev/null)"
    if [ -n "$px" ] && [ "${px%x*}" -ge 200 ] 2>/dev/null; then
      ok "$label: 窗口有可截取内容（$px）"
      cp "$shot" "$PWD/gui-window-${label}.png" 2>/dev/null || true
    else
      bad "$label: 截图尺寸异常（${px:-读不出}）"
    fi
  else
    bad "$label: 无法截取窗口内容"
  fi

  # 窗口出现之后进程必须仍然存活（不是画完就崩）。
  sleep 2
  kill -0 "$pid" 2>/dev/null && ok "$label: 窗口出现后进程仍存活" || bad "$label: 窗口出现后进程已退出"

  # 正常关闭：向窗口发 WM_DELETE_WINDOW（等价点关闭按钮）。
  #
  # **不能**用 `xdotool windowclose`：它发的是 XDestroyWindow，服务端直接销毁
  # 窗口，winit 随后访问失效窗口 ID 会 panic（实测 TranslateCoordinates
  # BadWindow → rc=101）。真实用户点关闭走的是 WM_DELETE_WINDOW 协议消息。
  local closer="$ROOT_DIR/tools/wm_close_window.py"
  if [ -x "$closer" ]; then
    python3 "$closer" "$wid" >/dev/null 2>&1
  elif command -v wmctrl >/dev/null 2>&1; then
    wmctrl -i -c "$wid" 2>/dev/null
  else
    bad "$label: 没有可用的 WM_DELETE_WINDOW 发送工具（缺 tools/wm_close_window.py 且无 wmctrl）"
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
      ok "$label: 关闭窗口后进程自行退出（rc=0）"
    else
      bad "$label: 关闭窗口后进程以 rc=$rc 退出（异常，101 通常意味着 panic）"
    fi
  else
    bad "$label: 关闭窗口 ${CLOSE_WAIT}s 后进程仍在（强杀不算通过）"
    kill -9 "$pid" 2>/dev/null
  fi
  # 残留检查：只看**这个二进制路径**的真实进程，按 /proc/<pid>/exe 比对，
  # 不用 pgrep -f（那样会匹配到跑本脚本的 shell 命令行自身，必然误报）。
  local left
  left="$(python3 - "$bin" "$pid" <<'PY'
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
)"
  [ "$left" = "0" ] && ok "$label: 无残留进程" || bad "$label: 残留 $left 个进程"

  kill -9 "$xpid" 2>/dev/null; wait "$xpid" 2>/dev/null
  unset DISPLAY
}

# ── 正例：真实窗口 ─────────────────────────────────────────────────
if [ -n "$LINUX_UI" ]; then
  echo "── 正例：$LINUX_UI 必须真实开窗并正常关闭 ──"
  run_window_case "linux-ui" "$LINUX_UI"
fi

# ── 负例 1：完全不创建窗口、exit 0 的假程序必须被判失败 ─────────────
if [ -n "$NEG_NO_WINDOW" ]; then
  echo "── 负例：不创建窗口的假程序必须失败 ──"
  before=$fail
  # 复用同一套判定，但期望它失败：跑完把 fail 恢复回去再单独判定。
  out="$(run_window_case "fake-no-window" "$NEG_NO_WINDOW" 2>&1)"; echo "$out" | sed 's/^/    /'
  if printf '%s' "$out" | grep -q '✓ fake-no-window: 本进程'; then
    bad "负例未拦住：假程序被判定为开窗成功"
  else
    ok "负例已拦住：无窗口的 exit 0 被判失败"
  fi
  fail=$before
fi

# ── 负例 2：挂着不退出、也永远不建窗口的假程序 → 超时必须判失败 ─────
if [ -n "$NEG_TIMEOUT" ]; then
  echo "── 负例：只 sleep 不建窗口的假程序必须失败（且超时不算通过）──"
  before=$fail
  out="$(run_window_case "fake-timeout" "$NEG_TIMEOUT" 2>&1)"; echo "$out" | sed 's/^/    /'
  if printf '%s' "$out" | grep -q '✓ fake-timeout: 本进程'; then
    bad "负例未拦住：sleep 假程序被判定为开窗成功"
  else
    ok "负例已拦住：超时/无窗口被判失败"
  fi
  fail=$before
fi

echo
[ $fail -eq 0 ] && echo PASS || echo FAIL
exit $fail
