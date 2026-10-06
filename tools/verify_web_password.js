'use strict';
// 服务器密码（relay --auth）守卫验证：真实 relay + agent + 无头 Chromium。
// 校验：登录页必须填服务器密码；密码错 → 回登录页并提示；密码对 → 进入会话；
// 管理后台密码 / 会话密钥不能当服务器密码用；MCP 仍需服务器密码。
// 用法: BIN=./target/debug/shell-remote node tools/verify_web_password.js
const { chromium } = require('playwright');
const { spawn } = require('child_process');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const BIN = process.env.BIN || './target/debug/shell-remote';
const PORT = +(process.env.PORT || 3187);
const BASE = `http://127.0.0.1:${PORT}`;
const SERVER_PW = 'server-pw-1', ADMIN_PW = 'admin-pw-2', KEY = 'wpkey1766';
let fails = 0;
const check = (name, ok, extra = '') => { console.log(`${ok ? '✓' : '✗'} ${name} ${extra}`); if (!ok) fails++; };

(async () => {
  const relay = spawn(BIN, ['relay', '--bind', `127.0.0.1:${PORT}`, '--auth', SERVER_PW, '--no-tls',
    '--admin-path', '/adm', '--admin-user', 'admin', '--admin-pass', ADMIN_PW], { stdio: 'ignore' });
  let agent, browser;
  try {
    await sleep(1000);
    agent = spawn(BIN, ['agent', '--relay-url', BASE, '--key', KEY, '--session-id', 'wp1766', '--shell', '/bin/sh'], { stdio: 'ignore' });
    await sleep(1500);
    browser = await chromium.launch();
    const login = async (token, pw) => {
      const ctx = await browser.newContext(); const p = await ctx.newPage();
      await p.goto(`${BASE}/`);
      check('登录页有服务器密码输入框', await p.locator('#auth-input').count() === 1);
      await p.fill('#token-input', token);
      if (pw !== null) await p.fill('#auth-input', pw);
      await p.click('#connect-btn');
      return p;
    };
    // 1. 不填密码：前端拦截
    let p = await login(KEY, null);
    await sleep(500);
    check('不填服务器密码不能进入', p.url().endsWith('/') && /服务器密码/.test(await p.locator('#connect-error').innerText()));
    // 2. 错密码（含：把管理后台密码、会话密钥当服务器密码）
    for (const [label, pw] of [['错误密码', 'nope'], ['管理后台密码', ADMIN_PW], ['会话密钥', KEY]]) {
      p = await login(KEY, pw);
      await p.waitForURL(/\/\?err=password/, { timeout: 15000 }).catch(() => {});
      check(`${label}被拒并回登录页提示`, /err=password/.test(p.url()) && /服务器密码错误/.test(await p.locator('#connect-error').innerText()), p.url());
    }
    // 3. 对的服务器密码
    p = await login(KEY, SERVER_PW);
    const ok = await p.waitForSelector('.tab-item', { timeout: 20000 }).then(() => true).catch(() => false);
    check('正确服务器密码 + 会话密钥可进入并连上终端', ok);
    // 4. 正确密码但会话密钥错 → 提示密钥错误（不是密码错误）
    p = await login('bad-token-xyz', SERVER_PW);
    await p.waitForURL(/\/\?err=token/, { timeout: 15000 }).catch(() => {});
    check('会话密钥错误提示区分于密码错误', /err=token/.test(p.url()), p.url());
    // 5. HTTP 层：无密码/错密码 401，MCP 仍需密码
    const r = async (url, init) => (await fetch(url, init)).status;
    check('SSE 无密码 401', await r(`${BASE}/agent/session/sse`, { headers: { Authorization: `Bearer ${KEY}` } }) === 401);
    check('桌面流无密码 401', await r(`${BASE}/agent/desktop/stream`, { headers: { Authorization: `Bearer ${KEY}` } }) === 401);
    const mcp = await fetch(`${BASE}/agent/mcp/sse`, { headers: { 'X-Auth': 'wrong' } });
    check('MCP 错密码被拒', /AUTH_INVALID_PASSWORD/.test((await mcp.text()).slice(0, 400)));
  } catch (e) { console.log('ERR', e.message); fails++; }
  finally { if (browser) await browser.close(); if (agent) agent.kill(); relay.kill(); }
  console.log(fails === 0 ? 'PASS' : `FAIL(${fails})`);
  process.exit(fails ? 1 : 0);
})();
