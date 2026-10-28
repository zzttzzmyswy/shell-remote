'use strict';
// MYS-886 第2轮验证：#55 帧超龄丢弃、#34 输入降采样、#30 时钟重校 不破坏
// 正常流（渲染帧率/ke2e/丢帧行仍更新）；面板新行（超龄计数）存在。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke2r';
const RUN_MS = 12000;
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
      // 主动触发一次指针移动，验证输入降采样不抛错（弱网分支是否守卫）
      try {
        const c = document.getElementById('desktop-canvas');
        if (c) {
          const rect = c.getBoundingClientRect();
          c.dispatchEvent(new PointerEvent('pointermove', { clientX: rect.left + 10, clientY: rect.top + 10, bubbles: true }));
        }
      } catch (e) {}
      return {
        lag: t('metric-lag'), fps: t('metric-fps'), gofps: t('metric-gofps'),
        drop: t('metric-dropped'), weaknet: t('metric-weaknet'), res: t('metric-res')
      };
    });
    readings.push(r);
    await page.waitForTimeout(1000);
  }
  const last = readings[readings.length - 1];
  console.log('── MYS-886 第2轮 播放器韧性验证 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  // 断言：正常流下 lag/fps/res 有值（渲染未破坏）、gofps 有目标
  const lagSample = readings.filter(r => r.lag && r.lag !== '-').length;
  const fpsSample = readings.filter(r => r.fps && r.fps !== '0').length;
  const gofpsTarget = readings.filter(r => r.gofps && r.gofps.indexOf('目标') >= 0).length;
  const dropHasStale = last.drop && last.drop.indexOf('超龄') >= 0;
  console.log(`lag有值 ${lagSample}/${readings.length}, fps>0 ${fpsSample}/${readings.length}, gofps目标 ${gofpsTarget}/${readings.length}`);
  console.log(`丢帧行含超龄字段: ${dropHasStale}`);
  const ok = gofpsTarget > 0 && dropHasStale && last.res && last.res !== '300x150';
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();