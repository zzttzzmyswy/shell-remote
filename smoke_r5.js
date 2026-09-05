'use strict';
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smokekey5';
const RUN_MS = 25000;
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
  await page.waitForTimeout(4000); // 首 IDR / 画面
  await page.click('#desktop-metrics-btn').catch(() => {});
  const readings = [];
  const start = Date.now();
  while (Date.now() - start < RUN_MS) {
    const r = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return { lag: t('metric-lag'), fps: t('metric-fps'), kbps: t('metric-bitrate'),
               res: t('metric-res'), codec: t('metric-encoder') };
    });
    readings.push(r);
    await page.waitForTimeout(1000);
  }
  const lag = readings.filter(r => r.lag && r.lag !== '-').map(r => parseFloat(r.lag));
  const fps = readings.filter(r => r.fps).map(r => parseInt(r.fps, 10));
  const kbps = readings.filter(r => r.kbps).map(r => parseFloat(r.kbps));
  const avg = (a) => a.length ? a.reduce((x, y) => x + y, 0) / a.length : NaN;
  const min = (a) => a.length ? Math.min(...a) : NaN;
  const max = (a) => a.length ? Math.max(...a) : NaN;
  console.log('── R5 4-Top 等效动态基准采样 ──');
  console.log(`lag  avg=${avg(lag).toFixed(0)}ms  [${min(lag)},${max(lag)}]  n=${lag.length}`);
  console.log(`fps  avg=${avg(fps).toFixed(1)}  [${min(fps)},${max(fps)}]`);
  console.log(`kbps avg=${avg(kbps).toFixed(0)} [${min(kbps).toFixed(0)},${max(kbps).toFixed(0)}]`);
  const last = readings[readings.length - 1];
  console.log(`last: lag=${last.lag} fps=${last.fps} kbps=${last.kbps} res=${last.res} codec=${last.codec}`);
  await browser.close();
  const okFps = fps.length && avg(fps) >= 5;
  const okLag = lag.length && avg(lag) < 600;
  console.log(okFps && okLag ? 'PASS' : 'FAIL');
  process.exit(okFps && okLag ? 0 : 1);
})();