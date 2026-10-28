'use strict';
// MYS-886 第16轮验证：admin KPI 曲线（R5 丙111/140）。
// 桌面开启后 agent 心跳带真实 KPI → relay 采样 → admin /api/session/kpi
// 返回时间序列 → admin 面板 📈 按钮展开 canvas 曲线（fps/bitrate）。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke16key';
(async () => {
  const browser = await chromium.launch();
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  // 开桌面（触发 agent 桌面流 → 心跳 KPI running=true + 真实码率）
  await page.goto(BASE + '/');
  await page.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), TOKEN);
  await page.goto(BASE + '/session');
  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-desktop-btn');
    return b && !b.disabled;
  }, { timeout: 15000 });
  await page.click('#toggle-desktop-btn');
  await page.waitForFunction(() => {
    const c = document.getElementById('desktop-container');
    return c && !c.classList.contains('hidden');
  }, { timeout: 15000 });
  await page.waitForTimeout(2500);
  console.log('桌面已开启，等待心跳采样（15s 粒度）…');
  // admin 登录（同 context 存 cookie）
  await page.evaluate(async () => {
    await fetch('/sr-admin-test/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ user: 'admin', pass: 'srpass' })
    });
  });
  await page.waitForTimeout(40000);
  // 验证 KPI API：应有 running=true 且 bitrate>0 的样本
  const kpi = await page.evaluate(async () => {
    const r = await fetch('/sr-admin-test/api/session/kpi/smoke16', { credentials: 'same-origin' });
    return await r.json();
  });
  const samples = (kpi && kpi.samples) || [];
  const runningCount = samples.filter((s) => s.running).length;
  const bitrateOk = samples.some((s) => s.bitrate_kbps > 0);
  console.log(`KPI API: samples=${samples.length}, running样本=${runningCount}, bitrate>0=${bitrateOk}`);

  // admin UI：打开面板 → 切到 Sessions tab → 点 📈 → 检查 canvas 绘制
  await page.goto(BASE + '/sr-admin-test');
  await page.waitForTimeout(1200);
  const sessTab = page.locator('.tab-btn[data-tab="sessions"]').first();
  if (await sessTab.count()) { await sessTab.click({ timeout: 8000 }); }
  await page.waitForTimeout(800);
  const kpiBtn = page.locator('button:text("📈")').first();
  await kpiBtn.click({ timeout: 8000 });
  await page.waitForTimeout(800);
  const canvas = await page.$('#kpi-row-smoke16 canvas');
  const canvasDrawn = canvas !== null;
  // canvas 非空白（有绘制内容）
  let hasPixels = false;
  if (canvas) {
    hasPixels = await page.evaluate((cv) => {
      const ctx = cv.getContext('2d');
      const d = ctx.getImageData(0, 0, cv.width, cv.height).data;
      for (let i = 3; i < d.length; i += 4) { if (d[i] > 0) return true; }
      return false;
    }, canvas);
  }
  console.log(`admin UI: 📈按钮点击=${kpiBtn ? 'ok' : 'fail'}, canvas展开=${canvasDrawn}, 曲线已绘制=${hasPixels}`);
  const ok = bitrateOk && runningCount >= 1 && canvasDrawn && hasPixels;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();
