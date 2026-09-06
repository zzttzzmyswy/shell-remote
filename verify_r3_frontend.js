'use strict';
// MYS-886 第3轮验证：#54 demux损坏重发、#65 能力缓存、#77 离页停抓、#56 面板分组
// 不破坏正常播放；面板分组标题存在；离页钩子触发 desktop:stop。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke3r';
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
  const start = Date.now();
  let last = null;
  while (Date.now() - start < RUN_MS) {
    last = await page.evaluate(() => {
      const t = (id) => { const e = document.getElementById(id); return e ? e.textContent.trim() : null; };
      const groups = document.querySelectorAll('#desktop-metrics .metrics-group-title');
      const groupTexts = Array.from(groups).map(g => g.textContent.trim());
      return {
        lag: t('metric-lag'), fps: t('metric-fps'), res: t('metric-res'),
        groups: groupTexts, cap: sessionStorage.getItem('sr-capability-v1'),
        consoleErr: null
      };
    });
    await page.waitForTimeout(1000);
  }
  console.log('── MYS-886 第3轮 前端收口验证 ──');
  console.log('last:', JSON.stringify(last, null, 1));
  const groupOk = last.groups && last.groups.length >= 3 &&
    last.groups.join('|').indexOf('流畅度') >= 0 &&
    last.groups.join('|').indexOf('质量') >= 0 &&
    last.groups.join('|').indexOf('传输') >= 0;
  const capOk = last.cap && last.cap.indexOf('webcodecs') >= 0;
  const resOk = last.res && last.res !== '300x150';
  console.log(`面板分组(流畅度/质量/传输): ${groupOk}`);
  console.log(`能力缓存 sr-capability-v1: ${capOk}`);
  console.log(`分辨率正常: ${resOk}`);
  // 验证离页钩子：触发 pagehide 后 agent 应收到 desktop:stop（在 agent 日志确认）。
  await page.evaluate(() => window.dispatchEvent(new Event('pagehide')));
  await page.waitForTimeout(500);
  const ok = groupOk && capOk && resOk;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();