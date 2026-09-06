'use strict';
// MYS-886 第14轮验证：TestDelay 探针全链路（R4 甲 A1 / R5#148）。
// 浏览器每 1s 发 desktop:test-delay → agent 即时 echo → 浏览器本地单调时钟
// 算纯网络 RTT → 面板"网络 RTT"行显示 + 随 qos 上报 probe_ms（agent 日志）。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke14key';
const RUN_MS = 8000;
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
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
  await page.click('#desktop-metrics-btn').catch(() => {});
  const start = Date.now();
  let last = null;
  let probeSeen = 0;
  while (Date.now() - start < RUN_MS) {
    last = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return { lag: t('metric-lag'), fps: t('metric-fps'), res: t('metric-res'), probe: t('metric-probe') };
    });
    if (last.probe && last.probe !== '-') probeSeen += 1;
    await page.waitForTimeout(1000);
  }
  console.log('── MYS-886 第14轮 TestDelay 探针验证 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  const resOk = last.res && last.res !== '300x150';
  // 探针行出现数字（网络 RTT 已回传）且多次采样稳定
  const probeOk = last.probe && /^\d+ ms$/.test(last.probe) && probeSeen >= 3;
  console.log(`桌面出画: ${resOk}, 网络RTT行: ${last.probe} (采样${probeSeen}次), 面板: ${probeOk}`);
  const ok = resOk && probeOk;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();
