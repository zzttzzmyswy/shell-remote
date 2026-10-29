'use strict';
// MYS-886 第9轮验证：#46 init 最小化后浏览器仍能解码渲染（关键回归——
// 去掉 stts/stsc/stsz/stco 空表后 MSE/WebCodecs 必须正常出画）。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke9r';
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
      return { lag: t('metric-lag'), fps: t('metric-fps'), res: t('metric-res'), codec: t('metric-encoder') };
    });
    await page.waitForTimeout(1000);
  }
  console.log('── MYS-886 第9轮 init 最小化验证 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  // init 最小化后：解码器能出画（res 非默认）、codec 解析正常。
  const resOk = last.res && last.res !== '300x150';
  const codecOk = last.codec && last.codec !== '-' && last.codec !== null;
  console.log(`分辨率正常(解码器出画): ${resOk}, codec 解析: ${codecOk} (${last.codec})`);
  const ok = resOk && codecOk;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();