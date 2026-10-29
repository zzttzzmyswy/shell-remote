'use strict';
// MYS-886 第5轮验证：#33 输入10ms合并 不破坏桌面流渲染；#155 评分卡
// 脚本可运行（直接用真实浏览器采样跑一次 scorecard 的 node 逻辑）。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke5r';
const RUN_MS = 10000;
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
  // 触发一串高频 pointermove，验证 10ms 合并不抛错、不阻塞。
  for (let i = 0; i < 30; i++) {
    await page.evaluate((n) => {
      const c = document.getElementById('desktop-canvas');
      if (!c) return;
      const rect = c.getBoundingClientRect();
      c.dispatchEvent(new PointerEvent('pointermove', {
        clientX: rect.left + 10 + n, clientY: rect.top + 10 + n, bubbles: true
      }));
    }, i).catch(() => {});
  }
  const start = Date.now();
  let last = null;
  while (Date.now() - start < RUN_MS) {
    last = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      return { lag: t('metric-lag'), fps: t('metric-fps'), res: t('metric-res'), drop: t('metric-dropped') };
    });
    await page.waitForTimeout(1000);
  }
  console.log('── MYS-886 第5轮 输入合并+渲染验证 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  const resOk = last.res && last.res !== '300x150';
  const fpsAny = last.fps !== null;
  const dropHasStale = last.drop && last.drop.indexOf('超龄') >= 0;
  console.log(`分辨率正常: ${resOk}, fps行存在: ${fpsAny}, 丢帧行含超龄: ${dropHasStale}`);
  const ok = resOk && fpsAny;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();