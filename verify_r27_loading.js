'use strict';
// MYS-886 第27轮验证：白闪修复（R5#74-78）。loading 覆盖在连接时显示、
// 首帧解码后隐藏，避免切流/重连白屏闪烁。检查：出画后 loading 已隐藏 + 画面正常。
const { chromium } = require('playwright');
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage();
  await p.goto('http://127.0.0.1:3100/');
  await p.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), 'smoke27key');
  await p.goto('http://127.0.0.1:3100/session');
  await p.waitForFunction(() => {
    const x = document.getElementById('toggle-desktop-btn');
    return x && !x.disabled;
  }, { timeout: 15000 });
  await p.click('#toggle-desktop-btn');
  await p.waitForFunction(() => {
    const c = document.getElementById('desktop-container');
    return c && !c.classList.contains('hidden');
  }, { timeout: 15000 });
  await p.waitForTimeout(3000);
  const st = await p.evaluate(() => {
    const el = document.getElementById('desktop-loading');
    const res = document.getElementById('metric-res');
    return { loadingHidden: el ? el.classList.contains('hidden') : null, res: res ? res.textContent.trim() : null };
  });
  console.log('出画后:', JSON.stringify(st));
  const ok = st.loadingHidden === true && st.res && st.res !== '300x150';
  console.log(`loading 已隐藏=${st.loadingHidden}, 画面=${st.res} ${ok ? 'PASS' : 'FAIL'}`);
  await b.close();
  process.exit(ok ? 0 : 1);
})();
