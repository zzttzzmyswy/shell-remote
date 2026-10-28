'use strict';
// MYS-2286-C 前端逻辑修复验证（playwright，无头 Chromium）。
//
// 覆盖任务卡验收标准 3 + 4：
//   1. 页面加载不访问 cdn.jsdelivr.net（拦截该域名让请求失败，终端仍能初始化）；
//   2. desktop:error 后桌面按钮恢复可点击；
//   3. ?token=xxx 进入后地址栏不再含 token；
//   4. Windows 路径纯函数断言（page.evaluate）。
//
// 用法:
//   BIN=./target/debug/shell-remote node tools/verify/verify_mys2283_c.js
//   （relay/agent 由本脚本自行拉起，端口用 PORT 或默认 3189）

const { chromium } = require('playwright');
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const BIN = process.env.BIN || './target/debug/shell-remote';
const PORT = +(process.env.PORT || 3189);
const BASE = `http://127.0.0.1:${PORT}`;
const KEY = 'mys2283ckey';
const SESSION = 'mys2283c';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let fails = 0;
const check = (name, ok, extra = '') => {
  console.log(`${ok ? '✓' : '✗'} ${name}${extra ? '  ' + extra : ''}`);
  if (!ok) fails++;
};

// 本机可能装的是另一版 playwright-core 对应的 chromium（默认路径缺失时
// launch 直接报 "Executable doesn't exist"）。这里在默认路径不可用时回退到
// ms-playwright 缓存里已安装的最新 chromium，保证脚本可直接跑。
function resolveChromium() {
  const def = chromium.executablePath();
  if (fs.existsSync(def)) return undefined; // 用默认的
  const cache = path.join(os.homedir(), '.cache', 'ms-playwright');
  if (!fs.existsSync(cache)) return undefined;
  const candidates = fs
    .readdirSync(cache)
    .filter((d) => /^chromium-\d+$/.test(d))
    .sort((a, b) => Number(b.split('-')[1]) - Number(a.split('-')[1]));
  for (const c of candidates) {
    for (const rel of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
      const p = path.join(cache, c, rel);
      if (fs.existsSync(p)) return p;
    }
  }
  return undefined;
}

async function main() {
  if (!fs.existsSync(BIN)) {
    console.error(`binary not found: ${BIN} (先 cargo build --bin shell-remote)`);
    process.exit(2);
  }
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sr2283c-root-'));
  fs.writeFileSync(path.join(root, 'probe.txt'), 'hello\n');

  // relay 的服务器密码（--auth）必须等于 agent 的 --key：agent 注册时用 key
  // 作为 server_auth 提交。浏览器侧对应 sessionStorage 的 shell-remote-auth。
  const relay = spawn(BIN, ['relay', '--bind', `127.0.0.1:${PORT}`, '--auth', KEY, '--no-tls'], {
    stdio: 'ignore'
  });
  let agent = null;
  let browser = null;
  try {
    await sleep(1200);
    agent = spawn(
      BIN,
      ['agent', '--relay-url', BASE, '--key', KEY, '--session-id', SESSION, '--root', root, '--shell', '/bin/sh'],
      { stdio: 'ignore' }
    );
    await sleep(2000);

    const exe = resolveChromium();
    browser = await chromium.launch(exe ? { executablePath: exe } : {});
    if (exe) console.log(`（使用 chromium: ${exe}）`);
    const ctx = await browser.newContext();
    const page = await ctx.newPage();

    // 记录事件 handler，以便在测试里按真实 handler 路径投递一条 desktop:error。
    // 只包一层 shellRemote.on，不改生产代码。
    await ctx.addInitScript(([token, auth]) => {
      try {
        sessionStorage.setItem('shell-remote-token', token);
        sessionStorage.setItem('shell-remote-auth', auth);
      } catch (e) {}
      let real;
      Object.defineProperty(window, 'shellRemote', {
        configurable: true,
        get() { return real; },
        set(v) {
          real = v;
          if (v && typeof v.on === 'function') {
            const origOn = v.on.bind(v);
            v.on = function (type, fn) {
              window.__srHandlers = window.__srHandlers || {};
              (window.__srHandlers[type] = window.__srHandlers[type] || []).push(fn);
              return origOn(type, fn);
            };
          }
        }
      });
    }, [KEY, KEY]);

    // ── 1. 不依赖 CDN ────────────────────────────────────────────────
    // 任何 cdn.jsdelivr.net 请求直接 abort。若 xterm 仍从 CDN 加载，
    // window.Terminal 会是 undefined，TerminalManager 构造即抛错，终端不会初始化。
    const cdnHits = [];
    const vendorHits = [];
    await page.route('**://cdn.jsdelivr.net/**', (route) => {
      cdnHits.push(route.request().url());
      route.abort();
    });
    page.on('response', (r) => {
      const u = r.url();
      if (u.indexOf('/vendor/') !== -1) vendorHits.push(`${r.status()} ${u.replace(BASE, '')}`);
    });

    await page.goto(`${BASE}/`);
    await page.goto(`${BASE}/session`);

    const termReady = await page
      .waitForFunction(() => !!document.querySelector('#terminal-container .xterm'), { timeout: 25000 })
      .then(() => true)
      .catch(() => false);
    check('拦截 cdn.jsdelivr.net 后终端仍正常初始化', termReady);
    check('页面未发起任何 jsdelivr 请求', cdnHits.length === 0, cdnHits.join(','));

    const xtermLoaded = await page.evaluate(
      () => typeof window.Terminal === 'function' && !!window.FitAddon && !!window.WebglAddon
    );
    check('xterm / fit / webgl 三个脚本均已加载', xtermLoaded);
    check(
      'xterm 资源来自本地 /vendor（HTTP 200）',
      vendorHits.length >= 4 && vendorHits.every((h) => h.startsWith('200 ')),
      vendorHits.join(' | ')
    );

    const tabOk = await page
      .waitForSelector('.tab-item', { timeout: 25000 })
      .then(() => true)
      .catch(() => false);
    check('agent 会话已加入并渲染 tab', tabOk);

    // ── 2. desktop:error 后按钮恢复 ──────────────────────────────────
    // 先投一条 desktop:capabilities 声明设备支持桌面（纯终端 agent 会报
    // available=false，此时按钮本就该 disabled，属另一种正确状态）。
    // 然后把按钮置成"连接中…"（用户点了桌面、agent 还没回执的状态），
    // 再按 session.js 真实的 desktop:error handler 投递一条错误。
    await page.waitForFunction(
      () => !!window.__srHandlers && !!window.__srHandlers['desktop:capabilities'] && !!window.__srHandlers['desktop:error'],
      { timeout: 10000 }
    );
    await page.evaluate(() => {
      window.__srHandlers['desktop:capabilities'].forEach((fn) =>
        fn({ type: 'desktop:capabilities', payload: { available: true, codecs: ['av1', 'h264'] } })
      );
    });
    const capState = await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      return { disabled: btn.disabled, hidden: btn.classList.contains('hidden') };
    });
    check('desktop:capabilities(available) 后按钮可用', capState.disabled === false && !capState.hidden, JSON.stringify(capState));

    await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      btn.disabled = true;
      btn.textContent = '连接中…';
    });
    await page.evaluate(() => {
      window.__srHandlers['desktop:error'].forEach((fn) =>
        fn({ type: 'desktop:error', payload: { error: 'synthetic capture failure' } })
      );
    });
    await sleep(200);
    const afterError = await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      return { disabled: btn.disabled, text: btn.textContent.trim() };
    });
    check(
      'desktop:error 后桌面按钮恢复可点击（文字回到「桌面」）',
      afterError.disabled === false && afterError.text === '桌面',
      JSON.stringify(afterError)
    );

    // session:agent_disconnect 同样要复位
    await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      btn.disabled = true;
      btn.textContent = '连接中…';
      window.__srHandlers['session:agent_disconnect'].forEach((fn) =>
        fn({ type: 'session:agent_disconnect', payload: {} })
      );
    });
    await sleep(200);
    const afterDisc = await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      return { disabled: btn.disabled, text: btn.textContent.trim() };
    });
    check(
      'session:agent_disconnect 后桌面按钮也恢复',
      afterDisc.disabled === false && afterDisc.text === '桌面',
      JSON.stringify(afterDisc)
    );

    // 点击后 15s 等不到 desktop:started → 复位按钮 + toast（C6）。
    // 真实路径：点按钮（会发出 desktop:start；本用例的 agent 未启用桌面捕获，
    // 会立刻回一条带 error 的 desktop:started —— 这属于"有回执"分支）。
    await page.evaluate(() => {
      document.getElementById('toggle-desktop-btn').click();
    });
    const clicking = await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      return { disabled: btn.disabled, text: btn.textContent.trim() };
    });
    check(
      '点击桌面后按钮进入「连接中…」并禁用',
      clicking.disabled === true && clicking.text === '连接中…',
      JSON.stringify(clicking)
    );
    await sleep(1500);
    const afterReply = await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      return { disabled: btn.disabled, text: btn.textContent.trim() };
    });
    check(
      'agent 回执（成功或失败）后按钮不再卡在「连接中…」',
      afterReply.disabled === false && afterReply.text === '桌面',
      JSON.stringify(afterReply)
    );

    // 真正的超时分支：把 desktop:start 的发送通道置空，agent 就不会有回执。
    await page.evaluate(() => {
      window.__sendSaved = window.shellRemote.send;
      window.shellRemote.send = function () {};
      document.getElementById('toggle-desktop-btn').click();
    });
    // 15s 超时（代码里的 DESKTOP_START_TIMEOUT）+ 余量。
    await sleep(16000);
    const afterTimeout = await page.evaluate(() => {
      const btn = document.getElementById('toggle-desktop-btn');
      const toast = document.getElementById('toast');
      const r = {
        disabled: btn.disabled,
        text: btn.textContent.trim(),
        toast: toast ? toast.textContent.trim() : ''
      };
      window.shellRemote.send = window.__sendSaved;
      if (typeof window.__srHandlers !== 'undefined') { /* keep */ }
      return r;
    });
    check(
      '15s 无 desktop:started 后按钮复位并提示「桌面启动超时」',
      afterTimeout.disabled === false &&
        afterTimeout.text === '桌面' &&
        afterTimeout.toast.indexOf('桌面启动超时') !== -1,
      JSON.stringify(afterTimeout)
    );

    // ── 3. ?token= 从地址栏清掉 ─────────────────────────────────────
    const page2 = await ctx.newPage();
    await page2.route('**://cdn.jsdelivr.net/**', (route) => route.abort());
    await page2.goto(`${BASE}/?token=${encodeURIComponent(KEY)}`);
    await sleep(300);
    const url2 = page2.url();
    const filled = await page2.inputValue('#token-input');
    check('?token= 进入后地址栏不含 token', !/token=/.test(url2), url2);
    check('?token= 的值已填入会话密钥输入框', filled === KEY, filled);
    await page2.close();

    // ── 4. Windows 路径纯函数 ────────────────────────────────────────
    const pathChecks = await page.evaluate(() => {
      const U = window.SRPathUtils;
      if (!U) return { missing: true };
      return {
        sepWin: U.separatorFor('C:\\Users\\u'),
        sepPosix: U.separatorFor('/home/u'),
        sepDot: U.separatorFor('.'),
        splitWin: U.splitPath('C:\\Users\\u\\a.txt'),
        splitPosix: U.splitPath('/home/u/a.txt'),
        joinWin: U.joinPath('C:\\Users\\u', 'a.txt'),
        joinPosix: U.joinPath('/home/u', 'a.txt'),
        joinAtWinRoot: U.joinPath('C:\\', 'a.txt'),
        parentWin: U.parentPath('C:\\Users\\u\\a'),
        parentWinDriveRoot: U.parentPath('C:\\'),
        parentPosix: U.parentPath('/home/u/a'),
        parentPosixRoot: U.parentPath('/'),
        replaceWin: U.replaceLeaf('C:\\Users\\u\\a.txt', 'b.txt'),
        replacePosix: U.replaceLeaf('/home/u/a.txt', 'b.txt'),
        hasSepSlash: U.hasSeparator('a/b'),
        hasSepBackslash: U.hasSeparator('a\\b'),
        hasSepNone: U.hasSeparator('ab'),
        isWin: U.isWindowsPath('C:\\x'),
        isPosix: U.isWindowsPath('/x')
      };
    });
    const eq = (label, actual, expected) => {
      check(`路径函数 ${label}`, JSON.stringify(actual) === JSON.stringify(expected), `got ${JSON.stringify(actual)}`);
    };
    if (pathChecks.missing) {
      check('window.SRPathUtils 存在', false);
    } else {
      eq("separatorFor('C:\\Users\\u')", pathChecks.sepWin, '\\');
      eq("separatorFor('/home/u')", pathChecks.sepPosix, '/');
      eq("separatorFor('.')", pathChecks.sepDot, '/');
      eq("splitPath('C:\\Users\\u\\a.txt')", pathChecks.splitWin, ['C:', 'Users', 'u', 'a.txt']);
      eq("splitPath('/home/u/a.txt')", pathChecks.splitPosix, ['home', 'u', 'a.txt']);
      eq("joinPath('C:\\Users\\u','a.txt')", pathChecks.joinWin, 'C:\\Users\\u\\a.txt');
      eq("joinPath('/home/u','a.txt')", pathChecks.joinPosix, '/home/u/a.txt');
      eq("joinPath('C:\\','a.txt')", pathChecks.joinAtWinRoot, 'C:\\a.txt');
      eq("parentPath('C:\\Users\\u\\a')", pathChecks.parentWin, 'C:\\Users\\u\\');
      eq("parentPath('C:\\')", pathChecks.parentWinDriveRoot, 'C:\\');
      eq("parentPath('/home/u/a')", pathChecks.parentPosix, '/home/u');
      eq("parentPath('/')", pathChecks.parentPosixRoot, '/');
      eq('replaceLeaf win', pathChecks.replaceWin, 'C:\\Users\\u\\b.txt');
      eq('replaceLeaf posix', pathChecks.replacePosix, '/home/u/b.txt');
      eq("hasSeparator('a/b')", pathChecks.hasSepSlash, true);
      eq("hasSeparator('a\\b')", pathChecks.hasSepBackslash, true);
      eq("hasSeparator('ab')", pathChecks.hasSepNone, false);
      eq("isWindowsPath('C:\\x')", pathChecks.isWin, true);
      eq("isWindowsPath('/x')", pathChecks.isPosix, false);
    }
  } catch (e) {
    console.log('ERR', e && e.stack ? e.stack : e);
    fails++;
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (agent) agent.kill();
    relay.kill();
    try { fs.rmSync(root, { recursive: true, force: true }); } catch (e) {}
  }
  console.log(fails === 0 ? 'PASS' : `FAIL(${fails})`);
  process.exit(fails === 0 ? 0 : 1);
}

main();
