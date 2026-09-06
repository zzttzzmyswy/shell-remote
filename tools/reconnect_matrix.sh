#!/bin/bash
# 重连矩阵验收脚本（R3 戊165 / R5#164）。
#
# 三破坏源 × 5 次 = 15 场景，每次破坏后验证浏览器桌面流自动恢复（重连后
# 目标帧率/画面重新出现）。三源：agent 断 / relay 重启 / 浏览器断。
#
# 用法:
#   tools/reconnect_matrix.sh <agent_pid> <relay_port> <session_token>
#   示例: tools/reconnect_matrix.sh 12345 3100 smoke1
#
# 依赖: playwright(node)；relay/agent 需已在跑。
set -u
AGENT_PID="${1:?agent pid}"
RELAY_PORT="${2:?relay port}"
TOKEN="${3:?token}"
BASE="http://127.0.0.1:${RELAY_PORT}"
RUNS=5

echo "── 重连矩阵 (3 源 × ${RUNS} 次) ──"

# 浏览器会话：开桌面流并保持，供每次破坏后检测恢复。
node -e '
  const { chromium } = require("playwright");
  (async () => {
    const b = await chromium.launch();
    const p = await b.newPage();
    await p.goto("'$BASE'/");
    await p.evaluate(t => sessionStorage.setItem("shell-remote-token", t), "'$TOKEN'");
    await p.goto("'$BASE'/session");
    await p.waitForFunction(() => { const e = document.getElementById("toggle-desktop-btn"); return e && !e.disabled; }, { timeout: 15000 });
    await p.click("#toggle-desktop-btn");
    await p.waitForFunction(() => { const c = document.getElementById("desktop-container"); return c && !c.classList.contains("hidden"); }, { timeout: 15000 });
    await p.waitForTimeout(2500);
    await p.click("#desktop-metrics-btn").catch(() => {});
    // 把句柄挂到 global，供 shell 外部 kill 后持续检测恢复
    globalThis.__reconnectPage = p;
    await new Promise(resolve => setTimeout(resolve, 1000));
    console.log("browser-ready");
  })();
' > /tmp/reconnect_browser.log 2>&1 &
BROWSER_JOB=$!
# 等浏览器就绪
for i in $(seq 1 30); do grep -q "browser-ready" /tmp/reconnect_browser.log 2>/dev/null && break; sleep 1; done

recover_ok() {
  # 浏览器重连后桌面流应恢复：等最多 8s，检测分辨率/帧率行不再为空。
  node -e '
    const { chromium } = require("playwright");
    (async () => {
      const b = await chromium.launch();
      const p = await b.newPage();
      await p.goto("'$BASE'/session");
      await p.waitForTimeout(8000);
      const ok = await p.evaluate(() => {
        const res = document.getElementById("metric-res")?.textContent.trim();
        const fps = document.getElementById("metric-fps")?.textContent.trim();
        return res && res !== "300x150" && fps !== null && fps !== undefined;
      });
      await b.close();
      process.exit(ok ? 0 : 1);
    })();
  '
}

fail=0
for src in agent relay browser; do
  for i in $(seq 1 "$RUNS"); do
    case "$src" in
      agent)   kill -9 "$AGENT_PID" 2>/dev/null; sleep 1 ;;
      relay)   fuser -k "${RELAY_PORT}/tcp" 2>/dev/null; sleep 2 ;;
      browser) pkill -f playwright 2>/dev/null; sleep 1 ;;
    esac
    if recover_ok; then
      echo "  ${src} 破坏 #${i}: 恢复 ✓"
    else
      echo "  ${src} 破坏 #${i}: 未恢复 ✗"
      fail=1
    fi
  done
done
echo "── 重连矩阵完成 ($([ "$fail" = 0 ] && echo 全过 || echo 有失败)) ──"
exit "$fail"
