'use strict';
// MYS-886 第18轮验证：光标独立通道（R5#64）。
// X11 GetImage 不含光标层——agent 独立查询指针位置（100ms 节流）→
// desktop:cursor 轻量消息 → 浏览器 video 上方 .sr-cursor-overlay 渲染。
// 验证：1) overlay 出现；2) 注入鼠标移动后 overlay 位置跟随。
const { chromium } = require('playwright');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke18key';
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

  // 1) overlay 元素已创建并显示
  const ov = await page.evaluate(() => {
    const el = document.querySelector('.sr-cursor-overlay');
    return el ? { display: el.style.display, left: el.style.left, top: el.style.top } : null;
  });
  console.log('overlay:', JSON.stringify(ov));

  const pos = (label) => page.evaluate(() => {
    const el = document.querySelector('.sr-cursor-overlay');
    return el ? { left: el.style.left, top: el.style.top } : null;
  }).then((p) => { console.log(`  ${label}: ${JSON.stringify(p)}`); return p; });

  const p1 = await pos('初始');
  await page.evaluate(() => {
    window.shellRemote.send('desktop:mouse', { type: 'move', x: 300, y: 200 });
  });
  await page.waitForTimeout(900);
  const p2 = await pos('移到(300,200)');

  await page.evaluate(() => {
    window.shellRemote.send('desktop:mouse', { type: 'move', x: 600, y: 450 });
  });
  await page.waitForTimeout(900);
  const p3 = await pos('移到(600,450)');

  const overlayOk = ov && ov.display === 'block';
  const movedOk = p1 && p2 && p3 && (p1.left !== p2.left || p1.top !== p2.top) && (p2.left !== p3.left || p2.top !== p3.top);
  console.log(`overlay创建=${overlayOk}, 位置跟随=${movedOk}`);
  const ok = overlayOk && movedOk;
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();
