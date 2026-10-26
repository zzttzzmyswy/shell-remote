#!/usr/bin/env node
'use strict';
// MYS-2383-C 真实验证：会话页「网页」子页面 + 旧版 agent 兼容（playwright 无头 Chromium）。
//
// 一条命令跑完整链路：真 relay + 真 agent + 只监听 127.0.0.1 的静态测试站，
// 浏览器里点真实按钮、看真实 iframe。
//
// 覆盖：
//   1. 能力协商：新 agent → 「网页」按钮出现；旧 agent(v0.57.0) → 完全不出现。
//   2. 远端首页 HTML 正常显示（含 CSS 生效的断言、图片真的加载出来）。
//   3. CSS / 图片都经代理取到（relay 访问日志以外的证据：图片 naturalWidth>0、
//      iframe 里所有资源 URL 都是代理 URL）。
//   4. 站内链接点击后继续经代理打开第二页。
//   5. 刷新 / 返回终端可用；终端仍能输入回显（不回归）。
//   6. ro token 也能浏览（页面 GET 只读）。
//   7. 不带 ticket/服务器口令直接请求代理 → 401；带错 ticket → 403。
//   8. agent 断线 → 入口隐藏、网页视图退出。
//   9. 桌面与网页互不覆盖：桌面进行中进入网页只隐藏画面（不 stop），返回桌面仍在。
//
// 用法：
//   cargo build --bin shell-remote
//   BIN=./target/debug/shell-remote OLD_BIN=./dist/shell-remote-x86_64 node tools/verify_webview_ui.js
//
// 环境变量：BIN / OLD_BIN / PORT / OLD_PORT / SITE_PORT / OUT

let chromium;
try {
  ({ chromium } = require('playwright'));
} catch (e) {
  ({ chromium } = require('playwright-core'));
}
const { spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const os = require('os');
const path = require('path');

const ROOT = path.dirname(__dirname);
const BIN = process.env.BIN || path.join(ROOT, 'target', 'debug', 'shell-remote');
// 旧 agent：publish 的 v0.57.0 二进制（基线，未含 web:proxy-v1）。
const OLD_BIN = process.env.OLD_BIN || path.join(ROOT, 'dist', 'shell-remote-x86_64');
// 带桌面 feature 的 agent（shell-remote-ui）。存在时用它验证「桌面 ↔ 网页互不
// 覆盖」；不存在则该段如实跳过，不伪装通过。
const UI_BIN = process.env.UI_BIN || path.join(ROOT, 'target', 'debug', 'shell-remote-ui');
const PORT = Number(process.env.PORT || 18860);
const OLD_PORT = Number(process.env.OLD_PORT || 18861);
const SITE_PORT = Number(process.env.SITE_PORT || 18862);
const BASE = `http://127.0.0.1:${PORT}`;
const OLD_BASE = `http://127.0.0.1:${OLD_PORT}`;
const SITE = `http://127.0.0.1:${SITE_PORT}`;
const AUTH = 'wv-e2e-pass';
const SESSION = 'wvnewagent';
const OLD_SESSION = 'wvoldagent';
const OUT = process.env.OUT || path.join(ROOT, 'artifacts', 'mys2383-c');

const fails = [];
let checks = 0;
function check(name, cond, detail) {
  checks += 1;
  if (cond) console.log('  PASS ' + name);
  else {
    console.log('  FAIL ' + name + (detail === undefined ? '' : '   <- ' + detail));
    fails.push(name);
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── 只监听 127.0.0.1 的测试站（= 设备自身的页面服务）──────────────────
// 8x8 RGB PNG（真实可解码：naturalWidth 才能读到 8，坏字节会让断言假阴性）。
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAFElEQVR4nGOoOIEdMuAQr2AYlDoA43p4Adr0fjIAAAAASUVORK5CYII=',
  'base64');
const SITE_FILES = {
  '/': ['text/html; charset=utf-8', `<!DOCTYPE html>
<html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'">
<span style="display:none">raw-source-marker</span>
<title>设备首页</title>
<link rel="stylesheet" href="/assets/site.css">
<style>.inline-rule{color:rgb(1, 2, 3)}</style>
<script>document.title = 'SHOULD-NOT-RUN';</script>
</head>
<body>
<h1 id="headline">device-local-home</h1>
<p class="dir-styled">styled-by-external-css</p>
<p class="inline-rule">styled-by-inline-css</p>
<p class="bg-probe">bg-from-css-url</p>
<img id="logo" src="/assets/logo.png" alt="logo">
<a id="to-page2" href="/sub/page2.html">go page2</a>
</body></html>`],
  '/assets/site.css': ['text/css; charset=utf-8',
    `body{background:#ffffff;color:#202020;font-family:sans-serif}
.dir-styled{color:rgb(11, 22, 33)}
.bg-probe{background-image:url(../img/bg.png);width:40px;height:12px}`],
  '/assets/logo.png': ['image/png', PNG],
  '/img/bg.png': ['image/png', PNG],
  '/sub/page2.html': ['text/html; charset=utf-8', `<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>第二页</title>
<link rel="stylesheet" href="../assets/site.css"></head>
<body><h1 id="headline">page-two-reached</h1>
<img id="logo2" src="../assets/logo.png" alt="logo2"></body></html>`],
};
const siteHits = [];

function startSite() {
  const srv = http.createServer((req, res) => {
    const url = req.url.split('?')[0];
    siteHits.push(url);
    const f = SITE_FILES[url];
    if (!f) {
      res.writeHead(404, { 'content-type': 'text/html' });
      res.end('<html><body>site-404</body></html>');
      return;
    }
    res.writeHead(200, {
      'content-type': f[0],
      'content-length': Buffer.byteLength(f[1]),
      // 这些头必须被 agent 剔除：
      'set-cookie': 'secret=leak',
      'access-control-allow-origin': 'https://evil.example',
    });
    res.end(f[1]);
  });
  return new Promise((resolve) => srv.listen(SITE_PORT, '127.0.0.1', () => resolve(srv)));
}

// ── 进程与浏览器 ────────────────────────────────────────────────────
function startProc(args, label, bin) {
  const p = spawn(bin || BIN, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  p.stderr.on('data', () => {});
  p.stdout.on('data', () => {});
  p.on('exit', (code) => { if (code && code !== 0) console.log(`  · ${label} exited ${code}`); });
  return p;
}

function resolveChromium() {
  const base = process.env.PLAYWRIGHT_BROWSERS_PATH ||
    path.join(os.homedir(), '.cache', 'ms-playwright');
  try {
    const dirs = fs.readdirSync(base).filter((d) => /^chromium-\d+$/.test(d)).sort();
    for (const d of dirs.reverse()) {
      const exe = path.join(base, d, 'chrome-linux', 'chrome');
      if (fs.existsSync(exe)) return exe;
      const exe2 = path.join(base, d, 'chrome-linux64', 'chrome');
      if (fs.existsSync(exe2)) return exe2;
    }
  } catch (e) {}
  return null;
}

function httpReq(url, opts) {
  opts = opts || {};
  return new Promise((resolve) => {
    const u = new URL(url);
    const req = http.request({
      hostname: u.hostname, port: u.port, path: u.pathname + u.search,
      method: opts.method || 'GET', headers: opts.headers || {},
    }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({
        status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks),
      }));
    });
    req.on('error', () => resolve({ status: 0, headers: {}, body: Buffer.alloc(0) }));
    req.end();
  });
}

async function waitPort(port, timeout) {
  const end = Date.now() + (timeout || 30000);
  while (Date.now() < end) {
    const r = await httpReq(`http://127.0.0.1:${port}/`);
    if (r.status > 0) return true;
    await sleep(200);
  }
  return false;
}

(async () => {
  fs.mkdirSync(OUT, { recursive: true });
  const site = await startSite();
  const roots = fs.mkdtempSync(path.join(os.tmpdir(), 'sr2383c-'));
  const relayA = startProc(
    ['relay', '--bind', `127.0.0.1:${PORT}`, '--auth', AUTH, '--no-tls'], 'relay-new');
  const relayB = startProc(
    ['relay', '--bind', `127.0.0.1:${OLD_PORT}`, '--auth', AUTH, '--no-tls'], 'relay-old');
  let agentA = null;
  let agentB = null;
  let browser = null;
  try {
    await waitPort(PORT);
    await waitPort(OLD_PORT);
    // 新 agent 优先用带 desktop feature 的 shell-remote-ui（存在时）：同一进程
    // 同时声明 desktop + web:proxy-v1，才能验证两个视图互不覆盖。
    const newAgentBin = fs.existsSync(UI_BIN) ? UI_BIN : BIN;
    console.log(`（新 agent: ${newAgentBin}）`);
    agentA = startProc(['agent', '--relay-url', BASE, '--key', AUTH,
      '--session-id', SESSION, '--root', roots, '--shell', '/bin/sh'], 'agent-new', newAgentBin);
    agentB = spawn(OLD_BIN, ['agent', '--relay-url', OLD_BASE, '--key', AUTH,
      '--session-id', OLD_SESSION, '--root', roots, '--shell', '/bin/sh'],
      { stdio: 'ignore' });
    await sleep(3000);

    const exe = resolveChromium();
    // --disable-gpu：无头 Chromium 里 xterm 的 WebglAddon 会把字符画进 canvas，
    // DOM 里读不到文本。关掉 GPU 后回退到 DOM/canvas 渲染器，innerText 可读。
    // 这不影响被验证的网页视图逻辑（本断言只用来确认终端没被网页视图弄坏）。
    browser = await chromium.launch(exe
      ? { executablePath: exe, args: ['--disable-gpu', '--disable-software-rasterizer'] }
      : { args: ['--disable-gpu', '--disable-software-rasterizer'] });
    console.log(`（chromium: ${exe || 'playwright 默认'}）`);

    const makePage = async (base, token, perm) => {
      const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 } });
      await ctx.addInitScript(([t, a, p]) => {
        try {
          sessionStorage.setItem('shell-remote-token', t);
          sessionStorage.setItem('shell-remote-auth', a);
          sessionStorage.setItem('shell-remote-permission', p);
          localStorage.setItem('sr-theme', 'dark');
        } catch (e) {}
      }, [token, AUTH, perm]);
      const page = await ctx.newPage();
      page.on('pageerror', (e) => console.log('  · pageerror: ' + e.message));
      await page.goto(`${base}/session`);
      return page;
    };

    // ── 旧 agent：入口完全不出现 ────────────────────────────────────
    console.log('\n[1] 旧 agent（v0.57.0，无 web:proxy-v1）');
    const oldPage = await makePage(OLD_BASE, AUTH, 'rw');
    await oldPage.waitForTimeout(4000);
    check('会话页已连上（终端初始化）',
      await oldPage.$('#terminal-container .xterm') !== null);
    const oldBtn = await oldPage.evaluate(() => {
      const b = document.getElementById('toggle-web-btn');
      if (!b) return { exists: false };
      const r = b.getBoundingClientRect();
      return {
        exists: true, hidden: b.classList.contains('hidden'), disabled: b.disabled,
        width: r.width, height: r.height,
        display: getComputedStyle(b).display,
      };
    });
    check('旧 agent：「网页」按钮不可见（hidden + disabled + 零尺寸）',
      oldBtn.exists && oldBtn.hidden && oldBtn.disabled && oldBtn.width === 0 &&
      oldBtn.display === 'none', JSON.stringify(oldBtn));
    // 旧 agent 直接调代理必须失败
    const tOld = await httpReq(`${OLD_BASE}/agent/web/ticket`,
      { method: 'POST', headers: { 'x-sr-token': AUTH, 'x-auth': AUTH } });
    check('旧 agent：POST /agent/web/ticket → 404（能力缺失）', tOld.status === 404,
      String(tOld.status));
    const pOld = await httpReq(`${OLD_BASE}/agent/web/proxy?ticket=deadbeef&url=${
      encodeURIComponent(SITE + '/')}`);
    check('旧 agent：GET /agent/web/proxy → 403/404（无可用 ticket + 无能力）',
      pOld.status === 403 || pOld.status === 404, String(pOld.status));
    await oldPage.screenshot({ path: path.join(OUT, 'old-agent-no-entry.png') });

    // ── 新 agent：入口出现 ─────────────────────────────────────────
    console.log('\n[2] 新 agent：能力协商与入口');
    const page = await makePage(BASE, AUTH, 'rw');
    await page.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
    await page.waitForFunction(() => {
      const b = document.getElementById('toggle-web-btn');
      return b && !b.classList.contains('hidden') && !b.disabled;
    }, { timeout: 20000 }).catch(() => {});
    const newBtn = await page.evaluate(() => {
      const b = document.getElementById('toggle-web-btn');
      const r = b.getBoundingClientRect();
      return { hidden: b.classList.contains('hidden'), disabled: b.disabled,
               width: r.width, height: r.height, text: b.textContent.trim() };
    });
    check('新 agent：「网页」按钮可见且可点',
      !newBtn.hidden && !newBtn.disabled && newBtn.width > 0 && newBtn.text === '网页',
      JSON.stringify(newBtn));

    // ── 打开网页视图 + 远端页面 ────────────────────────────────────
    console.log('\n[3] 打开远端首页（经 relay/agent 隧道）');
    await page.click('#toggle-web-btn');
    await page.waitForSelector('#webview-url-input', { state: 'visible', timeout: 10000 });
    await page.fill('#webview-url-input', SITE + '/');
    await page.click('#webview-open-btn');
    await page.waitForFunction(() => {
      const f = document.getElementById('webview-frame');
      return f && f.contentDocument &&
        f.contentDocument.getElementById('headline') !== null;
    }, { timeout: 40000 }).catch(() => {});
    // 等首页图片真正解码完成（异步）再统一断言，避免竞态。
    await page.waitForFunction(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      const img = d && d.getElementById('logo');
      return !!(img && img.complete && img.naturalWidth > 0);
    }, { timeout: 15000 }).catch(() => {});
    const doc1 = await page.evaluate(() => {
      const f = document.getElementById('webview-frame');
      const d = f.contentDocument;
      return {
        visible: !document.getElementById('webview-container').classList.contains('hidden'),
        terminalHidden: document.getElementById('terminal-area').classList.contains('hidden'),
        headline: d && d.getElementById('headline') ? d.getElementById('headline').textContent : null,
        title: d ? d.title : null,
        hasScript: d ? d.querySelectorAll('script').length : -1,
        srcAll: d ? Array.from(d.querySelectorAll('img,link,source')).map((e) =>
          e.getAttribute('src') || e.getAttribute('href')) : [],
        logoW: d && d.getElementById('logo') ? d.getElementById('logo').naturalWidth : -1,
        extColor: d && d.querySelector('.dir-styled')
          ? getComputedStyle(d.querySelector('.dir-styled')).color : null,
        inlineColor: d && d.querySelector('.inline-rule')
          ? getComputedStyle(d.querySelector('.inline-rule')).color : null,
        bgImage: d && d.querySelector('.bg-probe')
          ? getComputedStyle(d.querySelector('.bg-probe')).backgroundImage : null,
      };
    });
    check('网页视图容器可见、终端视图隐藏',
      doc1.visible && doc1.terminalHidden, JSON.stringify([doc1.visible, doc1.terminalHidden]));
    check('远端 HTML 正文渲染（device-local-home）', doc1.headline === 'device-local-home',
      String(doc1.headline));
    check('远端 <script> 未执行也未保留（title 未被改写、DOM 无 script）',
      doc1.title !== 'SHOULD-NOT-RUN' && doc1.hasScript === 0,
      JSON.stringify([doc1.title, doc1.hasScript]));
    check('所有资源 URL 都是同源代理 URL',
      doc1.srcAll.length >= 1 &&
      doc1.srcAll.every((u) => u && u.indexOf('/agent/web/proxy?ticket=') === 0),
      JSON.stringify(doc1.srcAll));
    check('图片经代理真实加载（naturalWidth>0）', doc1.logoW > 0, String(doc1.logoW));
    check('外部 CSS 经代理取回并生效（颜色来自 site.css）',
      doc1.extColor === 'rgb(11, 22, 33)', String(doc1.extColor));
    check('文档内联 <style> 生效', doc1.inlineColor === 'rgb(1, 2, 3)',
      String(doc1.inlineColor));
    check('CSS url() 背景图指向代理',
      typeof doc1.bgImage === 'string' && doc1.bgImage.indexOf('/agent/web/proxy?ticket=') >= 0,
      String(doc1.bgImage));
    check('远端页面无会话 token 泄漏',
      JSON.stringify(doc1.srcAll).indexOf(AUTH) === -1 && String(doc1.title).indexOf(AUTH) === -1);
    check('测试站确实被访问（含 CSS/图片）',
      siteHits.indexOf('/') >= 0 && siteHits.indexOf('/assets/site.css') >= 0 &&
      siteHits.indexOf('/assets/logo.png') >= 0, JSON.stringify(siteHits));
    await page.screenshot({ path: path.join(OUT, 'webview-home.png') });

    // ── 站内链接继续代理 ──────────────────────────────────────────
    console.log('\n[4] 站内链接继续经代理');
    await page.evaluate(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      d.getElementById('to-page2').click();
    });
    await page.waitForFunction(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      return d && d.getElementById('headline') &&
        d.getElementById('headline').textContent === 'page-two-reached';
    }, { timeout: 30000 }).catch(() => {});
    // 图片是异步解码的：先等它真的加载完再断言 naturalWidth，否则是竞态假阴性。
    await page.waitForFunction(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      const img = d && d.getElementById('logo2');
      return !!(img && img.complete && img.naturalWidth > 0);
    }, { timeout: 15000 }).catch(() => {});
    const doc2 = await page.evaluate(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      return {
        headline: d.getElementById('headline') ? d.getElementById('headline').textContent : null,
        logo2: d.getElementById('logo2') ? d.getElementById('logo2').naturalWidth : -1,
        color: d.querySelector('.dir-styled') ? null : null,
        status: document.getElementById('webview-status').textContent,
        input: document.getElementById('webview-url-input').value,
      };
    });
    check('点击站内链接打开第二页', doc2.headline === 'page-two-reached', String(doc2.headline));
    check('第二页的图片同样经代理加载', doc2.logo2 > 0, String(doc2.logo2));
    check('地址栏同步为第二页地址', doc2.input === SITE + '/sub/page2.html', doc2.input);
    check('状态区显示已加载', /已加载/.test(doc2.status || ''), doc2.status);
    await page.screenshot({ path: path.join(OUT, 'webview-page2.png') });

    // ── 刷新 / 返回终端 ───────────────────────────────────────────
    console.log('\n[5] 刷新与返回终端');
    const hitsBefore = siteHits.length;
    await page.click('#webview-refresh-btn');
    await page.waitForFunction(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      return d && d.getElementById('headline') &&
        d.getElementById('headline').textContent === 'page-two-reached';
    }, { timeout: 30000 }).catch(() => {});
    check('刷新重新请求远端页面', siteHits.length > hitsBefore,
      `${hitsBefore} -> ${siteHits.length}`);
    await page.click('#webview-back-btn');
    await page.waitForTimeout(400);
    const backState = await page.evaluate(() => ({
      termVisible: !document.getElementById('terminal-area').classList.contains('hidden'),
      tabBarVisible: !document.getElementById('tab-bar').classList.contains('hidden'),
      webHidden: document.getElementById('webview-container').classList.contains('hidden'),
      btnText: document.getElementById('toggle-web-btn').textContent.trim(),
    }));
    check('返回终端：终端与 tab bar 恢复、网页视图隐藏',
      backState.termVisible && backState.tabBarVisible && backState.webHidden,
      JSON.stringify(backState));
    check('返回终端后按钮文案恢复为「网页」', backState.btnText === '网页', backState.btnText);

    // ── 终端不回归：输入有回显 ────────────────────────────────────
    console.log('\n[6] 终端功能不回归');
    const marker = 'wvterm' + Date.now();
    await page.click('#terminal-container .xterm');
    await page.waitForTimeout(300);
    await page.keyboard.type(`echo ${marker}\n`);
    const echoed = await page.waitForFunction(
      (m) => document.getElementById('terminal-container').innerText.indexOf(m) !== -1,
      marker, { timeout: 15000 }
    ).then(() => true).catch(() => false);
    check('返回终端后键盘输入正常回显', echoed, marker);

    // ── 桌面 ↔ 网页互不干扰 ───────────────────────────────────────
    console.log('\n[7] 桌面与网页视图互不覆盖');
    const hasDesktop = await page.evaluate(() => {
      const b = document.getElementById('toggle-desktop-btn');
      return b && !b.classList.contains('hidden') && !b.disabled;
    });
    if (hasDesktop) {
      await page.click('#toggle-desktop-btn');
      await page.waitForFunction(() =>
        !document.getElementById('desktop-container').classList.contains('hidden'),
        { timeout: 15000 }).catch(() => {});
      const desktopOn = await page.evaluate(() =>
        !document.getElementById('desktop-container').classList.contains('hidden'));
      check('桌面视图可打开', desktopOn);
      // 桌面进行中 → 进入网页：只隐藏桌面容器，不发 desktop:stop
      await page.click('#toggle-web-btn');
      await page.waitForTimeout(800);
      const midState = await page.evaluate(() => ({
        webVisible: !document.getElementById('webview-container').classList.contains('hidden'),
        desktopHidden: document.getElementById('desktop-container').classList.contains('hidden'),
        desktopHasVideo: !!document.querySelector('#desktop-container video'),
      }));
      check('桌面进行中进入网页：网页可见、桌面仅隐藏（DOM 仍在）',
        midState.webVisible && midState.desktopHidden && midState.desktopHasVideo,
        JSON.stringify(midState));
      // 返回桌面：桌面容器恢复
      await page.click('#toggle-desktop-btn');
      await page.waitForTimeout(600);
      const backDesktop = await page.evaluate(() => ({
        desktopVisible: !document.getElementById('desktop-container').classList.contains('hidden'),
        webHidden: document.getElementById('webview-container').classList.contains('hidden'),
      }));
      check('从网页返回桌面：桌面视图恢复、网页隐藏（不重发 desktop:start）',
        backDesktop.desktopVisible && backDesktop.webHidden, JSON.stringify(backDesktop));
      // 桌面仍在跑：点按钮=切回终端并停流（currentView==='desktop' 分支）
      await page.click('#toggle-desktop-btn');
      await page.waitForTimeout(800);
      const afterStop = await page.evaluate(() => ({
        termVisible: !document.getElementById('terminal-area').classList.contains('hidden'),
        desktopHidden: document.getElementById('desktop-container').classList.contains('hidden'),
      }));
      check('再点「终端」停桌面并回到终端视图',
        afterStop.termVisible && afterStop.desktopHidden, JSON.stringify(afterStop));
    } else {
      // 该构建未编译 desktop feature（lean）→ 没有桌面按钮可点。如实记录，
      // 不把「跳过」当成通过：桌面互不覆盖的用例由下面独立的 relay/agent 断言
      // 之外的构建覆盖（UI 构建：target/debug/shell-remote-ui）。
      console.log('  · 跳过（本次 BIN 为 lean 构建，无桌面能力；见交付说明）');
    }

    // ── ro token 也能浏览 ────────────────────────────────────────
    console.log('\n[8] 只读 token 浏览');
    const roPage = await makePage(BASE, AUTH, 'ro');
    await roPage.waitForFunction(() => {
      const b = document.getElementById('toggle-web-btn');
      return b && !b.classList.contains('hidden') && !b.disabled;
    }, { timeout: 20000 }).catch(() => {});
    const roBtnVisible = await roPage.evaluate(() => {
      const b = document.getElementById('toggle-web-btn');
      return b && !b.classList.contains('hidden') && !b.disabled;
    });
    check('ro 会话也显示「网页」入口', roBtnVisible);
    await roPage.click('#toggle-web-btn');
    await roPage.waitForSelector('#webview-url-input', { state: 'visible', timeout: 10000 });
    await roPage.fill('#webview-url-input', SITE + '/');
    await roPage.click('#webview-open-btn');
    await roPage.waitForFunction(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      return d && d.getElementById('headline');
    }, { timeout: 40000 }).catch(() => {});
    const roHeadline = await roPage.evaluate(() => {
      const d = document.getElementById('webview-frame').contentDocument;
      return d && d.getElementById('headline') ? d.getElementById('headline').textContent : null;
    });
    check('ro token 能正常浏览远端页面', roHeadline === 'device-local-home', String(roHeadline));

    // ── 鉴权边界：不带凭据不能取资源 ────────────────────────────
    console.log('\n[9] 鉴权边界（无凭据 / 坏 ticket 不能取资源）');
    const noCred = await httpReq(
      `${BASE}/agent/web/proxy?url=${encodeURIComponent(SITE + '/')}`);
    check('无 ticket 且无会话 token → 401', noCred.status === 401, String(noCred.status));
    const badTicket = await httpReq(
      `${BASE}/agent/web/proxy?ticket=${'0'.repeat(64)}&url=${encodeURIComponent(SITE + '/')}`);
    check('伪造 ticket → 403', badTicket.status === 403, String(badTicket.status));
    const wrongPw = await httpReq(`${BASE}/agent/web/ticket`,
      { method: 'POST', headers: { 'x-sr-token': AUTH, 'x-auth': 'wrong-password' } });
    check('服务器口令错 → 401', wrongPw.status === 401, String(wrongPw.status));
    const rightTicket = await httpReq(`${BASE}/agent/web/ticket`,
      { method: 'POST', headers: { 'x-sr-token': AUTH, 'x-auth': AUTH } });
    const t = JSON.parse(rightTicket.body.toString() || '{}');
    check('正确凭据签发 ticket', rightTicket.status === 200 && !!t.ticket,
      JSON.stringify(t).slice(0, 80));
    check('ticket 不等于会话 token / 不含口令', t.ticket && t.ticket !== AUTH &&
      String(t.ticket).indexOf(AUTH) === -1);

    // ── agent 断线：入口隐藏 + 视图退出 ──────────────────────────
    console.log('\n[10] agent 断线：入口隐藏、网页视图退出');
    // 先在网页视图里
    await page.click('#toggle-web-btn');
    await page.waitForTimeout(500);
    const beforeDrop = await page.evaluate(() => ({
      webVisible: !document.getElementById('webview-container').classList.contains('hidden'),
    }));
    check('断线前处于网页视图', beforeDrop.webVisible);
    agentA.kill('SIGKILL');
    await page.waitForFunction(() => {
      const b = document.getElementById('toggle-web-btn');
      return b && b.classList.contains('hidden');
    }, { timeout: 20000 }).catch(() => {});
    const afterDrop = await page.evaluate(() => {
      const b = document.getElementById('toggle-web-btn');
      return {
        hidden: b.classList.contains('hidden'),
        webHidden: document.getElementById('webview-container').classList.contains('hidden'),
        srcdom: document.getElementById('webview-frame').getAttribute('srcdoc'),
        input: document.getElementById('webview-url-input').value,
      };
    });
    check('断线后「网页」入口隐藏', afterDrop.hidden, JSON.stringify(afterDrop));
    check('断线后网页视图退出到终端', afterDrop.webHidden);
    check('断线后 iframe 内容被清空', !afterDrop.srcdom, String(afterDrop.srcdom).slice(0, 40));
    await page.screenshot({ path: path.join(OUT, 'after-agent-disconnect.png') });

    console.log('\n' + (fails.length === 0
      ? `全部通过：${checks} 项`
      : `失败 ${fails.length}/${checks} 项：\n  - ` + fails.join('\n  - ')));
  } catch (e) {
    console.error('E2E 脚本异常：', e);
    fails.push('script-error');
  } finally {
    try { if (browser) await browser.close(); } catch (e) {}
    for (const p of [agentA, agentB, relayA, relayB]) {
      try { if (p) p.kill('SIGKILL'); } catch (e) {}
    }
    try { site.close(); } catch (e) {}
  }
  process.exit(fails.length === 0 ? 0 : 1);
})();
