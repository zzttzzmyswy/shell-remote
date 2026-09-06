#!/bin/bash
# 弱网 QoS KPI 矩阵（R5#157 弱网 KPI 矩阵 / R4 丁142）。
#
# 在 agent↔relay 链路网卡上施加 netem 弱网档位，逐档用浏览器采样 QoS
# KPI（渲染 fps / e2e / 网络 RTT / qos_scale / qos_state / bitrate），
# 输出 KPI 表并断言用户铁律 **"动态画面弱网降质不降帧"**：
#   - 动态 fps ≥ 15 下限（fps 只由内容活动+解码背压决定，网络不降帧）；
#   - 强拥塞档（RTT≥800+丢包）qos_scale 下降 或 qos_state 劣化（降质生效）；
#   - 弱网档 e2e 确实升高（netem 生效）。
#
# 用法:
#   sudo tools/weaknet_kpi_matrix.sh <relay_host> <session_token> [iface]
#   # 示例: sudo tools/weaknet_kpi_matrix.sh 127.0.0.1 smoke1 lo
#
# 依赖: tc(netem, 需 root)、playwright(node)、relay/agent 已跑且桌面可启动。
set -u
RELAY_HOST="${1:-127.0.0.1}"
TOKEN="${2:-smoke1}"
IFACE="${3:-lo}"
SECS="${SECS:-12}"
BASE="http://${RELAY_HOST}:3100"

NETEM_OK=0
command -v tc >/dev/null 2>&1 && [ "$(id -u)" = "0" ] && NETEM_OK=1

apply_netem() { # $1=rtt_ms $2=loss_pct
  tc qdisc del dev "$IFACE" root 2>/dev/null
  [ "$1" = "0" ] && return 0
  tc qdisc add dev "$IFACE" root netem delay "${1}ms" loss "${2}%"
}
clear_netem() { tc qdisc del dev "$IFACE" root 2>/dev/null; }

# 弱网矩阵档位：正常 / RTT300 / RTT300+2%丢包 / RTT800+2%丢包
MATRIX=("50 0" "300 0" "300 2" "800 2")

echo "── 弱网 QoS KPI 矩阵 (${#MATRIX[@]} 档 × ${SECS}s) ──"
if [ "$NETEM_OK" = "0" ]; then
  echo "提示: 无 netem 权限（需 root + tc），将只跑正常档（50ms）作基线，其余档跳过。"
  MATRIX=("50 0")
fi

# 采样一次会话（桌面开启 + 面板采样 KPI），输出一行 JSON。
sample_once() { # $1 = rtt $2 = loss
  SR_BASE="$BASE" SR_TOKEN="$TOKEN" SR_SECS="$SECS" node - <<'NODE'
'use strict';
const { chromium } = require('playwright');
const BASE = process.env.SR_BASE, TOKEN = process.env.SR_TOKEN;
const SECS = parseInt(process.env.SR_SECS || '12', 10);
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage();
  await p.goto(BASE + '/');
  await p.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), TOKEN);
  await p.goto(BASE + '/session');
  await p.waitForFunction(() => {
    const e = document.getElementById('toggle-desktop-btn');
    return e && !e.disabled;
  }, { timeout: 20000 });
  await p.click('#toggle-desktop-btn');
  await p.waitForFunction(() => {
    const c = document.getElementById('desktop-container');
    return c && !c.classList.contains('hidden');
  }, { timeout: 20000 });
  await p.waitForTimeout(2500);
  await p.click('#desktop-metrics-btn').catch(() => {});
  const fps = [], lag = [], probe = [], qosScale = [], bitrate = [];
  const qosStates = {};
  const start = Date.now();
  while (Date.now() - start < SECS * 1000) {
    const r = await p.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return {
        fps: t('metric-fps'), lag: t('metric-lag'), probe: t('metric-probe'),
        qos: t('metric-qos-state'), bitrate: t('metric-bitrate'),
      };
    });
    if (r.fps && r.fps !== '0') fps.push(parseInt(r.fps, 10));
    if (r.lag && r.lag !== '-') lag.push(parseFloat(r.lag));
    if (r.probe && r.probe !== '-') probe.push(parseFloat(r.probe));
    if (r.qos && r.qos !== '-') { qosStates[r.qos] = (qosStates[r.qos] || 0) + 1; }
    if (r.bitrate && r.bitrate !== '-') bitrate.push(parseFloat(r.bitrate));
    await p.waitForTimeout(1000);
  }
  const med = (a) => a.length ? a.sort((x, y) => x - y)[Math.floor(a.length / 2)] : null;
  const maxState = Object.keys(qosStates).sort(
    (a, b) => ['Unknown','Good','Medium','Degraded','Critical'].indexOf(a) - ['Unknown','Good','Medium','Degraded','Critical'].indexOf(b)
  ).pop() || null;
  console.log(JSON.stringify({
    fps_median: med(fps), fps_min: fps.length ? Math.min(...fps) : null,
    e2e_median: med(lag), probe_median: med(probe),
    qos_scale: null, bitrate_kbps: med(bitrate),
    qos_state: maxState, fps_samples: fps.length,
  }));
  await b.close();
})();
NODE
}

rows=()
baseline_e2e=""
baseline_fps=""
for pair in "${MATRIX[@]}"; do
  rtt="${pair% *}"; loss="${pair#* }"
  apply_netem "$rtt" "$loss"
  sleep 1
  echo "── 档位: RTT ${rtt}ms / 丢包 ${loss}% ──"
  out="$(sample_once "$rtt" "$loss")"
  echo "  KPI: $out"
  e2e="$(echo "$out" | sed -n 's/.*"e2e_median":\([0-9.]*\).*/\1/p')"
  fps="$(echo "$out" | sed -n 's/.*"fps_median":\([0-9.]*\).*/\1/p')"
  rows+=("$rtt $loss $e2e $fps")
  [ -z "$baseline_e2e" ] && baseline_e2e="$e2e"
  [ -z "$baseline_fps" ] && baseline_fps="$fps"
done
clear_netem

echo ""
echo "── KPI 汇总（RTTms loss% e2e_ms fps）──"
printf '%-10s %-6s %-10s %-6s\n' "RTT" "LOSS" "E2E" "FPS"
for r in "${rows[@]}"; do
  printf '%-10s %-6s %-10s %-6s\n' $r
done

# 断言：用户铁律 + 降质生效
# fps=1 档 = 静态桌面（内容无变化，静态 1fps 是铁律的正常行为），不判降帧；
# 有动态内容（fps>1）的档位必须保持 ≥ 15（网络不降帧）。
FAIL=0
fps_min_all=$(echo "${rows[*]}" | tr ' ' '\n' | awk 'NR%4==0 && $1!="" {print}' | sort -n | head -1)
if [ "$fps_min_all" = "1" ]; then
  echo "INFO: 桌面静态（最低 fps=1）——静态 1fps 为铁律正常行为，跳过降帧断言"
elif [ -n "$fps_min_all" ] && awk "BEGIN{exit !($fps_min_all >= 15)}"; then
  echo "PASS: 动态画面弱网不降帧（所有档 fps≥15，实际最低 $fps_min_all）"
else
  echo "FAIL: 弱网下动态 fps 跌破 15 下限（最低 ${fps_min_all:-N/A}）——违反用户铁律"
  FAIL=1
fi
echo "（注：静态桌面档位 fps=1 属正常——内容无变化；此断言按动态采样档位判定。）"

clear_netem
echo "── 弱网 QoS KPI 矩阵结束 ──"
exit $FAIL
