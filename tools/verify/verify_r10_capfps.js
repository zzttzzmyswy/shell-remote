'use strict';
// MYS-886 第10轮验证：#130 首帧失败快速终止 + #135 --desktop-capture-fps=20
// 不破坏桌面流（agent 带 capture-fps 参数启动，浏览器正常出画）。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke10r';
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
  while (Date.now() - start < RUN_MS) {
    last = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return { lag: t('metric-lag'), fps: t('metric-fps'), res: t('metric-res') };
    });
    await page.waitForTimeout(1000);
  }
  console.log('── MYS-886 第10轮 capture-fps 验证 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  const resOk = last.res && last.res !== '300x150';
  console.log(`分辨率正常(capture-fps=20 下仍出画): ${resOk}`);
  const ok = resOk;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();