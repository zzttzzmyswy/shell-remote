'use strict';
// MYS-886 第12轮验证：#111 RTT 分带 + #113 中值滤波 后 QoS 决策不破坏
// 桌面流（agent 内部算法改动回归：桌面正常出画、QoS 决策日志正常）。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke12r';
const RUN_MS = 7000;
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
  while (Date.now() - start < RUN_MS) {
    last = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return { lag: t('metric-lag'), fps: t('metric-fps'), res: t('metric-res'), gofps: t('metric-gofps') };
    });
    await page.waitForTimeout(1000);
  }
  console.log('── MYS-886 第12轮 QoS 深化回归验证 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  const resOk = last.res && last.res !== '300x150';
  // gofps 应显示"目标 X"（qos-ack 链路，QoS 决策后回传正常）
  const qosOk = last.gofps && last.gofps.indexOf('目标') >= 0;
  console.log(`桌面出画: ${resOk}, QoS 回传: ${qosOk} (${last.gofps})`);
  const ok = resOk && qosOk;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();