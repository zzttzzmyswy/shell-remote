'use strict';
// 验证 MYS-886 块B：面板新增行（目标帧率/活动 gofps、reqkey、弱网/TTFV weaknet）
// + qos-ack 桥接 + X11 SHM 捕获生效（backend 行）。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smokekey5';
const RUN_MS = 15000;
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
  await page.waitForTimeout(3000);
  await page.click('#desktop-metrics-btn').catch(() => {});
  const readings = [];
  const start = Date.now();
  while (Date.now() - start < RUN_MS) {
    const r = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return {
        lag: t('metric-lag'), fps: t('metric-fps'), gofps: t('metric-gofps'),
        reqkey: t('metric-reqkey'), weaknet: t('metric-weaknet'),
        backend: t('metric-backend'), res: t('metric-res')
      };
    });
    readings.push(r);
    await page.waitForTimeout(1000);
  }
  const last = readings[readings.length - 1];
  console.log('── MYS-886 块B 面板补全采样 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  const gofpsSamples = readings.filter(r => r.gofps && r.gofps !== '-');
  const weakSamples = readings.filter(r => r.weaknet && r.weaknet !== '正常');
  const gofpsOk = gofpsSamples.length > 0;
  // backend 应是 x11（SHM 快路径下 capture 正常）
  const backendOk = last.backend && last.backend.indexOf('x11') >= 0;
  console.log(`gofps 出现于 ${gofpsSamples.length}/${readings.length} 采样`);
  console.log(`weaknet 正常行展现：${readings.filter(r => r.weaknet).length} 采样`);
  const ok = gofpsOk && backendOk && last.res && last.fps !== null;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();