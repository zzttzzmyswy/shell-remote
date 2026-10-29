'use strict';
// MYS-886 第21轮诊断：连续 playwright 会话后 toggle-desktop-btn disabled 问题。
// 会话1：开桌面 → close（pagehide → desktop:stop）。会话2：新连接 → 检查按钮。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke21key';

async function openDesktop(browser) {
  const page = await browser.newPage();
  await page.goto(BASE + '/');
  await page.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), TOKEN);
  await page.goto(BASE + '/session');
  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-desktop-btn');
    return b && !b.disabled;
  }, { timeout: 20000 });
  await page.click('#toggle-desktop-btn');
  await page.waitForFunction(() => {
    const c = document.getElementById('desktop-container');
    return c && !c.classList.contains('hidden');
  }, { timeout: 20000 });
  await page.waitForTimeout(2000);
  return page;
}

(async () => {
  const browser = await chromium.launch();
  // 会话1：开桌面
  console.log('── 会话1：开桌面 ──');
  const p1 = await openDesktop(browser);
  console.log('会话1 桌面已开');
  await p1.close(); // close 触发 pagehide → desktop:stop
  await new Promise((r) => setTimeout(r, 1500));

  // 会话2：新连接，检查按钮
  console.log('── 会话2：新连接 ──');
  const p2 = await browser.newPage();
  await p2.goto(BASE + '/');
  await p2.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), TOKEN);
  await p2.goto(BASE + '/session');
  await p2.waitForTimeout(3500);
  const st = await p2.evaluate(() => {
    const btn = document.getElementById('toggle-desktop-btn');
    return { disabled: btn ? btn.disabled : null, text: btn ? btn.textContent : null };
  });
  console.log('会话2 按钮:', JSON.stringify(st));
  // 收集会话2的 capabilities 事件
  await p2.evaluate(() => {
    window.__caps = [];
    window.shellRemote.on('desktop:capabilities', function(m) { window.__caps.push(m.payload || {}); });
  });
  await p2.waitForTimeout(1500);
  const caps = await p2.evaluate(() => window.__caps);
  console.log('会话2 capabilities 事件数:', caps.length, JSON.stringify(caps.slice(0, 2)));
  await browser.close();
  process.exit(st && st.disabled ? 1 : 0);
})();
