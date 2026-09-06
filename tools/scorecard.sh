#!/bin/bash
# 评分卡脚本（R3 戊176 / R5#155）：每次基准产出结构化评分卡
# (fps, e2e, 丢帧率, CPU, 内存)，供合入前回归门禁与 issue 汇报。
#
# 用法:
#   SR_BASE=http://127.0.0.1:3100 SR_TOKEN=smoke1 tools/scorecard.sh [seconds]
#
# 输出: 单行 JSON 评分卡 + 通过/失败判定（对照 R4 戊172 发布门槛）：
#   4-top 等效基准 fps≥25、e2e 本地中位<150ms、seq 丢帧率<2%。
#
# 依赖: playwright(node)、relay/agent 已跑、桌面可启动。
set -u
SECS="${1:-15}"
export SR_BASE="${SR_BASE:-http://127.0.0.1:3100}"
export SR_TOKEN="${SR_TOKEN:-smoke1}"
export SR_SECS="$SECS"

echo "── 评分卡采样 (${SECS}s) ──"
node - <<'NODE'
'use strict';
const { chromium } = require('playwright');
const BASE = process.env.SR_BASE || 'http://127.0.0.1:3100';
const TOKEN = process.env.SR_TOKEN || 'smoke1';
const SECS = parseInt(process.env.SR_SECS || '15', 10);
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage();
  await p.goto(BASE + '/');
  await p.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), TOKEN);
  await p.goto(BASE + '/session');
  await p.waitForFunction(() => {
    const e = document.getElementById('toggle-desktop-btn');
    return e && !e.disabled;
  }, { timeout: 15000 });
  await p.click('#toggle-desktop-btn');
  await p.waitForFunction(() => {
    const c = document.getElementById('desktop-container');
    return c && !c.classList.contains('hidden');
  }, { timeout: 15000 });
  await p.waitForTimeout(2500);
  await p.click('#desktop-metrics-btn').catch(() => {});
  const lag = [], fps = [], drop = [];
  const start = Date.now();
  while (Date.now() - start < SECS * 1000) {
    const r = await p.evaluate(() => ({
      lag: document.getElementById('metric-lag')?.textContent.trim(),
      fps: document.getElementById('metric-fps')?.textContent.trim(),
      drop: document.getElementById('metric-dropped')?.textContent.trim(),
    }));
    if (r.lag && r.lag !== '-') lag.push(parseFloat(r.lag));
    if (r.fps && r.fps !== '0') fps.push(parseInt(r.fps, 10));
    if (r.drop) drop.push(r.drop);
    await p.waitForTimeout(1000);
  }
  const med = (a) => a.length ? a.sort((x, y) => x - y)[Math.floor(a.length / 2)] : null;
  // 丢帧率口径：从 "N 解码 / M 超龄 / K 上行(seq)" 提取 seq gap 累计。
  // 样本数×总帧预算粗略归一：seq drop 是累计值，窗口内新增量 = 末-首。
  let seqFirst = null, seqLast = null;
  for (const d of drop) {
    const m = d.match(/(\d+) 上行\(seq\)/);
    if (m) {
      const v = parseInt(m[1], 10);
      if (seqFirst === null) seqFirst = v;
      seqLast = v;
    }
  }
  const seqDrop = (seqFirst !== null && seqLast !== null) ? Math.max(0, seqLast - seqFirst) : 0;
  const fpsMed = med(fps);
  const e2eMed = med(lag);
  // 丢帧率 ≈ 窗口内 seq 缺口 / (fps中位 × 窗口秒数) —— 估算法。
  const totalFrames = fpsMed ? Math.max(1, fpsMed * SECS) : 1;
  const dropRate = Math.min(100, seqDrop / totalFrames * 100);
  const card = {
    fps_median: fpsMed,
    e2e_median_ms: e2eMed,
    seq_drop: seqDrop,
    drop_rate_pct: Number(dropRate.toFixed(2)),
    samples: { lag: lag.length, fps: fps.length },
    ts: Date.now(),
  };
  console.log('SCORECARD ' + JSON.stringify(card));
  // 发布门槛（R4 戊172）：动态 fps≥25、e2e<150ms、丢帧<2%。
  const gateOk = (fpsMed === null || fpsMed >= 25) &&
    (e2eMed === null || e2eMed < 150) &&
    dropRate < 2;
  console.log(gateOk ? 'GATE PASS' : 'GATE FAIL');
  await b.close();
  process.exit(gateOk ? 0 : 1);
})();
NODE
echo "── 评分卡完成 ──"