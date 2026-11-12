#!/bin/bash
# 发布候选制品的**真实窗口**验证 + 无参数入口的 fail-closed 断言（MYS-2821）。
#
# 为什么单独一个脚本：`cargo test` 里的 GUI 用例用的是本机构建的 glibc 动态
# 二进制，**不能**代表发布制品。发布制品是 musl 静态链接，而 winit 的 X11 后端
# 走 `x11-dl`（运行时 `dlopen` libX11/libxkbcommon/libGL）。musl 静态二进制
# 不支持 `dlopen`，所以在 Linux 上窗口一定起不来——这是制品的静态链接形态
# 决定的，与 GUI 代码无关（同一份代码的 glibc 动态构建能正常开窗）。
#
# 本脚本因此分成两段，各自断言**它真正能证明的事**：
#
#   A. Linux 发布 UI 制品：无参数启动**必须**给出可读失败，且带恢复意图但交接
#      文件缺失时**必须** fail-closed（非零退出 + 可读原因），绝不能静默注册成
#      一台新设备。这两条是安全语义，与窗口能不能开无关。
#
#   B. Windows 发布 UI 制品（wine）：无参数启动**必须**出现配置窗口，且
#      **不自动连接**；关闭窗口后进程退出。
#
# 用法:
#   tools/gui_release_window_test.sh \
#     --linux-ui dist/shell-remote-ui-x86_64 \
#     --windows-ui dist/shell-remote-ui-x86_64.exe
#
# 环境: 需要 Xvfb / xdotool / import（ImageMagick）/ wine（B 段）。
# 退出码：全部断言通过 0，否则 1。
set -u

LINUX_UI=""
WINDOWS_UI=""
while [ $# -gt 0 ]; do
  case "$1" in
    --linux-ui)   LINUX_UI="${2:?}"; shift 2 ;;
    --windows-ui) WINDOWS_UI="${2:?}"; shift 2 ;;
    -h|--help)    sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 2 ;;
  esac
done

fail=0
ok(){   echo "  ✓ $1"; }
bad(){  echo "  ✗ $1"; fail=1; }

free_display(){ # 找一个没被占用的 :N
  local n
  for n in $(seq 40 90); do
    [ -e "/tmp/.X11-unix/X$n" ] || { printf ':%s' "$n"; return 0; }
  done
  return 1
}

# ── A. Linux 发布 UI 制品 ───────────────────────────────────────────
if [ -n "$LINUX_UI" ]; then
  echo "── A. Linux 发布 UI 制品：窗口 + fail-closed（$LINUX_UI）──"
  D="$(free_display)"; W="$(mktemp -d)"
  Xvfb "$D" -screen 0 1280x900x24 -nolisten tcp > "$W/xvfb.log" 2>&1 & XP=$!
  sleep 1.5
  mkdir -p "$W/home/.shell-remote"

  # A1. 无参数启动：要么开出窗口，要么显式、可读地失败（并说明是 dlopen 那条）。
  DISPLAY="$D" HOME="$W/home" SR_LOG_DIR="$W/logs" timeout 30 "$LINUX_UI" \
    > "$W/a1.out" 2> "$W/a1.err"
  rc=$?
  if [ $rc -eq 0 ]; then
    ok "A1. 无参数启动正常退出"
  elif grep -q '窗口初始化失败\|Failed to load one of xlib' "$W/a1.err"; then
    # 这是**已知且已定位**的制品形态缺陷：musl 静态 + winit 的 x11-dl dlopen。
    # 不记为 PASS，而是明确报出来，让发布决策看到它。
    echo "  ! A1. 无参数启动未能开窗（musl 静态 dlopen，见 docs/release-verification.md）"
    echo "       stderr: $(head -1 "$W/a1.err")"
    fail=1
  else
    bad "A1. 非零退出但没有可读原因（rc=$rc）"
  fi

  # A2. 有恢复意图但交接文件缺失 → fail-closed。
  DISPLAY="$D" HOME="$W/home" SR_LOG_DIR="$W/logs" \
    SR_UPGRADE_RESTORE="$W/missing-handoff.json" timeout 30 "$LINUX_UI" \
    > "$W/a2.out" 2> "$W/a2.err"
  rc=$?
  if [ $rc -ne 0 ] && grep -q 'cannot read the upgrade handoff\|upgrade restore failed' "$W/a2.err"; then
    ok "A2. 交接文件缺失时 fail-closed（rc=$rc，原因可读）"
  else
    bad "A2. 未按 fail-closed 处理（rc=$rc）: $(head -1 "$W/a2.err")"
  fi

  kill $XP 2>/dev/null; wait $XP 2>/dev/null
fi

# ── B. Windows 发布 UI 制品（wine）──────────────────────────────────
if [ -n "$WINDOWS_UI" ]; then
  echo "── B. Windows 发布 UI 制品：无参数窗口（$WINDOWS_UI）──"
  if ! command -v wine >/dev/null 2>&1; then
    echo "  ! 未安装 wine，B 段跳过（不算 PASS）"; fail=1
  else
    D="$(free_display)"; W="$(mktemp -d)"
    Xvfb "$D" -screen 0 1280x900x24 -nolisten tcp > "$W/xvfb.log" 2>&1 & XP=$!
    sleep 1.5
    export DISPLAY="$D"
    WINEDEBUG=-all nohup wine "$WINDOWS_UI" > "$W/gui.log" 2>&1 & GP=$!
    WID=""
    for _ in $(seq 1 120); do
      WID="$(xdotool search --onlyvisible --name 'shell-remote · 连接设置' 2>/dev/null | tail -1)"
      [ -n "$WID" ] && break; sleep 0.5
    done
    if [ -n "$WID" ]; then
      ok "B1. 无参数启动出现配置窗口（window=$WID）"
      # 窗口必须**真的有内容**：截一张图，非空且能读出尺寸。
      shot="$PWD/gui-window.png"
      if import -window "$WID" "$shot" 2>/dev/null && [ -s "$shot" ]; then
        px="$(identify -format '%wx%h' "$shot" 2>/dev/null)"
        [ -n "$px" ] && ok "B2. 窗口已绘制（截图 $shot，$px）" \
          || bad "B2. 截图无法识别尺寸"
      else
        bad "B2. 无法截取窗口内容"
      fi
      xdotool windowactivate --sync "$WID" 2>/dev/null
      xdotool key --window "$WID" alt+F4 2>/dev/null || xdotool windowclose "$WID"
      exited=0
      for _ in $(seq 1 60); do kill -0 "$GP" 2>/dev/null || { exited=1; break; }; sleep 0.5; done
      [ $exited -eq 1 ] && ok "B3. 关闭窗口后进程退出" || bad "B3. 关闭后进程仍在"
    else
      bad "B1. 未出现配置窗口（$(tail -1 "$W/gui.log" 2>/dev/null)）"
      kill $GP 2>/dev/null
    fi
    kill $XP 2>/dev/null; wait $XP 2>/dev/null
  fi
fi

[ $fail -eq 0 ] && echo PASS || echo FAIL
exit $fail
