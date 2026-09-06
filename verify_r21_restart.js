'use strict';
// MYS-886 第21轮验证：relay 重启后桌面流自动恢复（R5#11）。
// 开桌面出画 → kill relay → agent 断线 → 重启 relay → agent 重连并自动
// 恢复桌面流（新 init）→ 浏览器 SSE 重连后画面恢复。
const { chromium } = require('playwright');
const { execSync, spawn } = require('child_process');
const BASE = 'http://127.0.0.1:3100';
const TOKEN = 'smoke21key';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function shell(cmd) {
  try { return execSync(cmd, { shell: '/bin/bash', stdio: 'pipe' }).toString(); }
  catch (e) { return ''; }
}

(async () => {
  const browser = await chromium.launch();
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
  await page.waitForTimeout(3000);
  const res1 = await page.evaluate(() => {
    const e = document.getElementById('metric-res');
    return e ? e.textContent.trim() : null;
  });
  console.log('初始桌面出画:', res1);

  // 1) kill relay（模拟重启）
  shell("pkill -f 'debug/shell-remot[e] relay' 2>/dev/null; true");
  console.log('relay 已 kill，等待 agent 断线…');
  await sleep(3500);

  // 2) 重启 relay
  shell('cd /home/zzt/workspace/shell-remote-desktop-video && nohup ./target/debug/shell-remote relay --no-tls --bind 127.0.0.1:3100 --auth sr-smoke-pass > /tmp/sr21b_relay.log 2>&1 &');
  console.log('relay 已重启，等待 agent 重连 + 自动恢复…');
  await sleep(12000);

  // 3) 等浏览器 SSE 重连（sse.js 自动退避重连）→ 画面恢复
  let res2 = null;
  for (let i = 0; i < 15; i++) {
    res2 = await page.evaluate(() => {
      const e = document.getElementById('metric-res');
      return e ? e.textContent.trim() : null;
    });
    if (res2 && res2 !== '300x150') break;
    await sleep(2000);
  }
  console.log('重启后桌面画面:', res2);
  const ok = res1 && res1 !== '300x150' && res2 && res2 !== '300x150';
  console.log(ok ? 'PASS' : 'FAIL');
  await browser.close();
  process.exit(ok ? 0 : 1);
})();
