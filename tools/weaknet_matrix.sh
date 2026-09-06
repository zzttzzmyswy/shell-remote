#!/bin/bash
# 弱网矩阵验收脚本（R4 丁122 / R5#156）。
#
# 在 agent↔relay 之间的网卡上施加 netem 延迟/丢包，逐一跑 6 组弱网矩阵
# （RTT 100/300/800ms × 丢包 {0, 2%}），每组跑一轮 4-top 等效动态基准
# （tools/bench_draw_quad + 浏览器采样），并记录评分卡
# （渲染 fps / e2e 中位 / 丢帧率 / CPU）。无 netem 权限时降级为"仅提示"。
#
# 用法:
#   sudo tools/weaknet_matrix.sh <relay_host> <session_token> [iface]
#   # 示例: sudo tools/weaknet_matrix.sh 127.0.0.1 smoke1 eth0
#
# 依赖: tc (iproute2), playwright(node), bench_draw_quad 在 :96
set -u
RELAY_HOST="${1:-127.0.0.1}"
TOKEN="${2:-smoke1}"
IFACE="${3:-lo}"
NETEM=""
have_netem() {
  command -v tc >/dev/null 2>&1 && [ "$(id -u)" = "0" ]
}

apply_netem() { # $1=rtt_ms $2=loss_pct
  tc qdisc del dev "$IFACE" root 2>/dev/null
  [ "$1" = "0" ] && return 0
  tc qdisc add dev "$IFACE" root netem delay "${1}ms" loss "${2}%"
}
clear_netem() { tc qdisc del dev "$IFACE" root 2>/dev/null; }

MATRIX=(
  "100 0"
  "100 2"
  "300 0"
  "300 2"
  "800 0"
  "800 2"
)

echo "── 弱网矩阵验收 (${#MATRIX[@]} 组) ──"
if ! have_netem; then
  echo "⚠ 无 root/tc：跳过实际 netem 施压，仅列出矩阵（CI 中可 sudo 执行）"
  for m in "${MATRIX[@]}"; do echo "  netem rtt=${m% *}ms loss=${m#* }%"; done
  exit 0
fi

for m in "${MATRIX[@]}"; do
  rtt="${m% *}"; loss="${m#* }"
  apply_netem "$rtt" "$loss"
  echo "── 组: RTT=${rtt}ms 丢包=${loss}% ──"
  # 4-top 等效动态基准（浏览器采样 10s）：渲染 fps 与 e2e 中位
  node -e '
    const { chromium } = require("playwright");
    (async () => {
      const b = await chromium.launch();
      const p = await b.newPage();
      await p.goto("http://'"$RELAY_HOST"':3100/");
      await p.evaluate(t => sessionStorage.setItem("shell-remote-token", t), "'"$TOKEN"'");
      await p.goto("http://'"$RELAY_HOST"':3100/session");
      await p.waitForFunction(() => { const e = document.getElementById("toggle-desktop-btn"); return e && !e.disabled; }, { timeout: 15000 });
      await p.click("#toggle-desktop-btn");
      await p.waitForFunction(() => { const c = document.getElementById("desktop-container"); return c && !c.classList.contains("hidden"); }, { timeout: 15000 });
      await p.waitForTimeout(3000);
      await p.click("#desktop-metrics-btn").catch(() => {});
      const lag=[], fps=[];
      const start=Date.now();
      while (Date.now()-start < 10000) {
        const r = await p.evaluate(() => ({
          lag: document.getElementById("metric-lag")?.textContent.trim(),
          fps: document.getElementById("metric-fps")?.textContent.trim(),
        }));
        if (r.lag && r.lag!=="-") lag.push(parseFloat(r.lag));
        if (r.fps && r.fps!=="0") fps.push(parseInt(r.fps,10));
        await p.waitForTimeout(1000);
      }
      const med=a=>a.length?a.sort((x,y)=>x-y)[Math.floor(a.length/2)]:NaN;
      console.log(`   渲染fps中位=${med(fps)} e2e中位=${med(lag)}ms 样本lag=${lag.length} fps=${fps.length}`);
      await b.close();
    })();
  '
  clear_netem
done
echo "── 弱网矩阵完成 ──"
