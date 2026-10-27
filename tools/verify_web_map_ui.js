#!/usr/bin/env node
'use strict';
// MYS-2383-G 真实验证：网页映射 v2（Service Worker + 注入脚本 + 会话页接入）。
//
// 一条命令跑完整链路：**真 relay 二进制 + 真 agent 二进制 + 只监听 127.0.0.1 的
// 测试 SPA**，在真实 Chromium 里点会话页的「网页」按钮，断言 iframe 里的页面
// 行为（脚本执行、history 路由、Cookie 登录、整页跳转、302、WebSocket、绝对地址
// fetch、GBK 中文、页面尝试注销我们的 SW）。
//
// 全都是真进程、真 HTTP、真 WebSocket：不 mock relay 内部状态，测试 SPA 用
// Node 标准库手写（零构建，RFC6455 帧自己实现）。
//
// 用法（先构建）：
//   cargo build --bin shell-remote
//   xvfb-run -a node tools/verify_web_map_ui.js
//
// 环境变量：BIN / V057_BIN / V058_BIN / RW_PORT / RO_PORT / OLD_PORT /
// ANCIENT_PORT / SITE_PORT / OUT。
// 两个旧二进制默认从 `dist/shell-remote-x86_64` 按**能力字符串**识别（v0.57.0
// 无网页能力、v0.58.0 只有 web:proxy-v1）；识别不出来就跳过对应回归段，如实
// 打印"SKIP"，不算通过。

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

let chromium;
try {
  ({ chromium } = require('playwright'));
} catch (e) {
  ({ chromium } = require('playwright-core'));
}

/// 二进制里是否含某个能力字符串。能力声明是编译进去的静态字符串，直接
/// 在文件字节里找即可——比"按文件名猜版本"可靠。
function hasCap(bin, cap) {
  if (!bin || !fs.existsSync(bin)) return false;
  try {
    return fs.readFileSync(bin).includes(Buffer.from(cap));
  } catch (e) {
    return false;
  }
}

const ROOT = path.dirname(path.dirname(fs.realpathSync(__filename)));
const BIN = process.env.BIN || path.join(ROOT, 'target', 'debug', 'shell-remote');
// 旧二进制按能力字符串分类（不是按文件名）：v0.58.0 只声明 web:proxy-v1，
// v0.57.0 什么网页能力都不声明，本仓库新构建声明 net:tcp-v1。
// `dist/` 里那份是发布基线；v0.58.0 的二进制可能被别的任务放在同级目录，
// 这里按能力字符串在候选里找，找不到就跳过（绝不把"跳过"当通过）。
const DIST_BIN = path.join(ROOT, 'dist', 'shell-remote-x86_64');
const CANDIDATE_BINS = [
  DIST_BIN,
  path.join(ROOT, 'dist', 'shell-remote-x86_64.v0.58.0'),
  path.join(path.dirname(ROOT), 'sr2383-release-dist-v0.58.0', 'shell-remote-x86_64'),
];
function findBin(cap, deny) {
  for (const b of CANDIDATE_BINS) {
    if (!fs.existsSync(b)) continue;
    if (cap && !hasCap(b, cap)) continue;
    if (deny && hasCap(b, deny)) continue;
    return b;
  }
  return '';
}
// v0.58.0：有 web:proxy-v1、没有 net:tcp-v1。v0.57.0：两个都没有。
const V058_BIN = process.env.V058_BIN || findBin('web:proxy-v1', 'net:tcp-v1');
const V057_BIN = process.env.V057_BIN ||
  CANDIDATE_BINS.find((b) => fs.existsSync(b) && !hasCap(b, 'web:proxy-v1') &&
    !hasCap(b, 'net:tcp-v1')) || '';
const RW_PORT = Number(process.env.RW_PORT || 18940);
const RO_PORT = Number(process.env.RO_PORT || 18941);
const OLD_PORT = Number(process.env.OLD_PORT || 18942);
const ANCIENT_PORT = Number(process.env.ANCIENT_PORT || 18943);
const SITE_PORT = Number(process.env.SITE_PORT || 18944);
const BASE = `http://127.0.0.1:${RW_PORT}`;
const RO_BASE = `http://127.0.0.1:${RO_PORT}`;
const OLD_BASE = `http://127.0.0.1:${OLD_PORT}`;
const ANCIENT_BASE = `http://127.0.0.1:${ANCIENT_PORT}`;
const SITE = `http://127.0.0.1:${SITE_PORT}`;
const AUTH = process.env.RELAY_PASSWORD || 'mapui-e2e-pass';
const OUT = process.env.OUT || path.join(ROOT, 'artifacts', 'mys2383-g');

const RW_KEY = 'mapuirw001';
// 第二个并发 agent：验证"两个会话同时映射"时 SW 的 clientId 归属互不串台。
const RW2_KEY = 'mapuirw002';
const RW2_SID = 'mapuirwdev2';
const RO_KEY = 'mapuiro001';
const RW_SID = 'mapuirwdev';
const RO_SID = 'mapuirodevi';
const OLD_KEY = 'mapuiold01';
const OLD_SID = 'mapuiolddev';
const ANCIENT_KEY = 'mapuianc001';
const ANCIENT_SID = 'mapuiancdev';

let checks = 0;
const fails = [];
const skips = [];
function check(name, cond, detail) {
  checks += 1;
  if (cond) {
    console.log(`  PASS ${name}`);
  } else {
    const d = detail === undefined ? '' : (typeof detail === 'string' ? detail : JSON.stringify(detail));
    console.log(`  FAIL ${name}${d ? `   <- ${d}` : ''}`);
    fails.push(name);
  }
}
function skip(name, why) {
  skips.push(name);
  console.log(`  SKIP ${name}（${why}）`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ══════════════════════ 测试 SPA（只监听 127.0.0.1） ══════════════════════

// GBK 编码的「中文页面」：用 iconv 不可靠（不保证装了），这里直接写 GBK 字节。
const GBK_TEXT = Buffer.from([
  0xd6, 0xd0, 0xce, 0xc4, 0xd2, 0xb3, 0xc3, 0xe6,   // 中文页面
]);

const SPA_INDEX = `<!DOCTYPE html>
<html lang="zh"><head>
<meta charset="utf-8">
<title>SPA 首页</title>
<link rel="stylesheet" href="/static/style.css">
</head>
<body>
<h1 id="app-title">spa-root</h1>
<div id="view"></div>
<nav>
  <a id="link-dashboard" href="/dashboard">dashboard</a>
  <a id="link-page2" href="/page2">page2</a>
  <a id="link-old" href="/old">old-redirect</a>
  <a id="link-gbk" href="/gbk">gbk</a>
</nav>
<div id="login-state">anon</div>
<div id="ws-log"></div>
<div id="fetch-result"></div>
<script src="/static/app.js"></script>
</body></html>`;

const APP_JS = `
// 路由表（history 模式）：/ 与 /dashboard 都渲染在同一份 DOM 里。
function render() {
  var p = location.pathname;
  var view = document.getElementById('view');
  if (p === '/dashboard') {
    view.innerHTML = '<h2 id="view-name">dashboard-view</h2>';
  } else if (p === '/page2') {
    // 整页跳转进来的：这份文档是服务端渲染的，不应走到这里。
    view.innerHTML = '<h2 id="view-name">page2-view</h2>';
  } else {
    view.innerHTML = '<h2 id="view-name">root-view</h2>';
  }
  document.body.setAttribute('data-path', p);
}

document.getElementById('link-dashboard').addEventListener('click', function (ev) {
  ev.preventDefault();
  history.pushState({}, '', '/dashboard');
  render();
});

window.addEventListener('popstate', render);
// 关键：脚本真的执行了才会设置这个标记（v1 静态模式下永远是 'no-script'）。
document.body.setAttribute('data-script', 'ran');
render();

// ── Cookie 登录（POST + Set-Cookie + 后续依赖 Cookie 的 GET）──
window.apiLogin = function () {
  return fetch('/api/login', { method: 'POST', body: 'u=alice' })
    .then(function (r) { return r.text(); })
    .then(function () {
      return fetch('/api/me');
    })
    .then(function (r) { return r.text(); })
    .then(function (t) {
      document.getElementById('login-state').textContent = t;
      return t;
    });
};

// ── 绝对地址 fetch（写死 targetOrigin 的写法，由 shim 改写）──
window.apiPing = function (origin) {
  return fetch(origin + '/api/ping')
    .then(function (r) { return r.text(); })
    .then(function (t) {
      document.getElementById('fetch-result').textContent = t;
      return t;
    });
};

// ── WebSocket（用 location.host，走同源；shim 改写到反代路径）──
var wsLog = document.getElementById('ws-log');
window.wsState = { echo: [], pushed: [], error: null, closed: false };
window.startWs = function () {
  return new Promise(function (resolve) {
    var ws = new WebSocket('ws://' + location.host + '/ws');
    window.__ws = ws;
    // shim 改写后的最终 URL：用于断言握手确实走了反代路径。
    window.wsState.url = ws.url;
    ws.onopen = function () {
      ws.send('hello-echo');
      setTimeout(function () { ws.send('push-request'); }, 120);
    };
    ws.onmessage = function (ev) {
      var t = String(ev.data);
      if (t.indexOf('server-push') === 0) window.wsState.pushed.push(t);
      else window.wsState.echo.push(t);
      wsLog.textContent = JSON.stringify(window.wsState);
      if (window.wsState.echo.length && window.wsState.pushed.length === 2) resolve(window.wsState);
    };
    ws.onerror = function () {
      window.wsState.error = 'ws-error';
      resolve(window.wsState);
    };
    ws.onclose = function () { window.wsState.closed = true; };
  });
};

// ── 尝试注销我们的 Service Worker（必须失败）──
window.killSw = function () {
  if (!navigator.serviceWorker || !navigator.serviceWorker.getRegistrations) {
    return Promise.resolve('no-sw-api');
  }
  return navigator.serviceWorker.getRegistrations().then(function (regs) {
    var results = [];
    regs.forEach(function (r) { results.push({ scope: r.scope }); });
    return Promise.all(regs.map(function (r) { return r.unregister(); })).then(function (outs) {
      return JSON.stringify({ found: results, unregistered: outs });
    });
  }).catch(function (e) { return 'error:' + (e && e.message); });
};
`;

const SPA_CSS = `
body { background: #fafafa; color: #123456; font-family: sans-serif; }
#app-title { color: rgb(9, 87, 65); }
`;

function siteHandler(req, res) {
  const u = new URL(req.url, SITE);
  const p = u.pathname;
  const send = (code, body, headers) => {
    const buf = Buffer.isBuffer(body) ? body : Buffer.from(body);
    res.writeHead(code, Object.assign({
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': buf.length,
    }, headers || {}));
    res.end(buf);
  };

  if (p === '/' || p === '/dashboard') return send(200, SPA_INDEX);
  if (p === '/static/app.js') {
    return send(200, APP_JS, { 'Content-Type': 'application/javascript; charset=utf-8' });
  }
  if (p === '/static/style.css') {
    return send(200, SPA_CSS, { 'Content-Type': 'text/css; charset=utf-8' });
  }
  if (p === '/api/login' && req.method === 'POST') {
    let body = '';
    req.on('data', (c) => { body += c.toString(); });
    req.on('end', () => {
      send(200, 'login-ok:' + body, {
        'Content-Type': 'text/plain; charset=utf-8',
        // 带 Domain：relay 必须去掉（否则浏览器整条丢弃）。Path=/ 保持。
        'Set-Cookie': `sid=alice-session; Domain=127.0.0.1; Path=/; HttpOnly; SameSite=Lax`,
      });
    });
    return undefined;
  }
  if (p === '/api/me') {
    const cookie = req.headers.cookie || '';
    return send(200, cookie.includes('sid=alice-session') ? 'user=alice' : 'user=anonymous', {
      'Content-Type': 'text/plain; charset=utf-8',
    });
  }
  if (p === '/api/ping') {
    return send(200, 'pong', { 'Content-Type': 'text/plain; charset=utf-8' });
  }
  if (p === '/page2') {
    // 整页跳转目标：服务端渲染的独立页面，同样带脚本与样式。
    return send(200, `<!DOCTYPE html><html><head><meta charset="utf-8">
<link rel="stylesheet" href="/static/style.css">
<title>page2</title></head>
<body data-page="page2"><h1 id="page2-title">page2-full-load</h1>
<img id="logo" src="/static/pixel.png" alt="logo">
<script>document.body.setAttribute('data-script','ran');</script>
</body></html>`);
  }
  if (p === '/old') {
    res.writeHead(302, { Location: '/page2', 'Content-Length': 0 });
    return res.end();
  }
  if (p === '/gbk') {
    return send(200, Buffer.concat([
      Buffer.from('<!DOCTYPE html><html><head><meta http-equiv="Content-Type" ' +
        'content="text/html; charset=gbk"><title>gbk</title></head><body><h1 id="gbk-title">'),
      GBK_TEXT,
      Buffer.from('</h1></body></html>'),
    ]), { 'Content-Type': 'text/html; charset=gbk' });
  }
  if (p === '/static/pixel.png') {
    // 1x1 透明 PNG
    return send(200, Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
      'base64'), { 'Content-Type': 'image/png' });
  }
  return send(404, '<html><body>site-404</body></html>');
}

// WebSocket echo + 服务端推送（RFC6455 手写，不引依赖）。
const wsPeers = new Set();
let wsUpgrades = 0;
let wsPaths = [];

function wsUpgrade(req, socket) {
  const key = req.headers['sec-websocket-key'];
  if (!key) { socket.destroy(); return; }
  wsUpgrades += 1;
  wsPaths.push(req.url);
  const accept = crypto.createHash('sha1')
    .update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  socket.write('HTTP/1.1 101 Switching Protocols\r\n' +
    'Upgrade: websocket\r\nConnection: Upgrade\r\n' +
    `Sec-WebSocket-Accept: ${accept}\r\n\r\n`);
  socket.setNoDelay(true);
  wsPeers.add(socket);

  let buf = Buffer.alloc(0);
  const sendFrame = (opcode, payload) => {
    const len = payload.length;
    let head;
    if (len < 126) head = Buffer.from([0x80 | opcode, len]);
    else if (len < 65536) {
      head = Buffer.alloc(4);
      head[0] = 0x80 | opcode; head[1] = 126; head.writeUInt16BE(len, 2);
    } else {
      head = Buffer.alloc(10);
      head[0] = 0x80 | opcode; head[1] = 127; head.writeBigUInt64BE(BigInt(len), 2);
    }
    socket.write(Buffer.concat([head, payload]));
  };
  socket.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const opcode = buf[0] & 0x0f;
      const masked = (buf[1] & 0x80) !== 0;
      let len = buf[1] & 0x7f;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2); off = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2)); off = 10;
      }
      const maskLen = masked ? 4 : 0;
      if (buf.length < off + maskLen + len) return;
      const mask = masked ? buf.subarray(off, off + 4) : null;
      const payload = Buffer.from(buf.subarray(off + maskLen, off + maskLen + len));
      buf = buf.subarray(off + maskLen + len);
      if (masked) for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
      if (opcode === 0x8) { sendFrame(0x8, Buffer.alloc(0)); socket.end(); wsPeers.delete(socket); return; }
      if (opcode === 0x9) { sendFrame(0xa, payload); continue; }
      if (opcode === 0x1 || opcode === 0x2) {
        const text = payload.toString('utf8');
        if (text === 'push-request') {
          sendFrame(0x1, Buffer.from('server-push-1'));
          sendFrame(0x1, Buffer.from('server-push-2'));
          continue;
        }
        sendFrame(opcode, payload);
        continue;
      }
    }
  });
  socket.on('error', () => wsPeers.delete(socket));
  socket.on('close', () => wsPeers.delete(socket));
}

function startSite() {
  const srv = http.createServer(siteHandler);
  srv.on('upgrade', wsUpgrade);
  return new Promise((resolve) => srv.listen(SITE_PORT, '127.0.0.1', () => resolve(srv)));
}

// ══════════════════════ 进程 / 浏览器工具 ══════════════════════

function startProc(args, label, bin) {
  const p = spawn(bin || BIN, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  const log = [];
  const push = (b) => { log.push(b.toString()); if (log.length > 200) log.shift(); };
  p.stdout.on('data', push);
  p.stderr.on('data', push);
  p.on('exit', (code) => { if (code && code !== 0) console.log(`  · ${label} exited ${code}`); });
  p.__log = log;
  return p;
}

function resolveChromium() {
  const base = process.env.PLAYWRIGHT_BROWSERS_PATH ||
    path.join(os.homedir(), '.cache', 'ms-playwright');
  try {
    const dirs = fs.readdirSync(base).filter((d) => /^chromium-\d+$/.test(d)).sort();
    for (const d of dirs.reverse()) {
      for (const sub of ['chrome-linux', 'chrome-linux64']) {
        const exe = path.join(base, d, sub, 'chrome');
        if (fs.existsSync(exe)) return exe;
      }
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
    req.end(opts.body);
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

/// 新会话页：把 token/权限/服务器口令写进 sessionStorage（与登录流程一致）。
async function makePage(browser, base, token, perm, opts) {
  opts = opts || {};
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  await ctx.addInitScript(([t, a, p, noSw]) => {
    try {
      sessionStorage.setItem('shell-remote-token', t);
      sessionStorage.setItem('shell-remote-auth', a);
    } catch (e) {}
    // 模拟"浏览器没有 Service Worker"：v2 条件之一不满足 → 应回落到 v1 并
    // 说明原因（真实场景：旧 WebView、隐私模式禁用 SW）。
    if (noSw) {
      try {
        Object.defineProperty(navigator, 'serviceWorker', {
          get: function () { return undefined; },
          configurable: false
        });
      } catch (e) {}
    }
  }, [token, AUTH, perm, !!opts.noServiceWorker]);
  const page = await ctx.newPage();
  page.on('pageerror', (e) => console.log('  · pageerror: ' + e.message));
  page.on('console', (m) => {
    if (m.type() === 'error') console.log('  · console.error: ' + m.text());
  });
  // 只在出错时打印响应：便于定位 502/404 是哪个请求（正常路径不刷屏）。
  page.on('response', (r) => {
    if (r.status() >= 400) {
      console.log(`  · http ${r.status()} ${r.request().method()} ${r.url()}`);
    }
  });
  await page.goto(`${base}/session`);
  return { ctx, page };
}

/// 打开网页视图。`initial` 给定时先把它填进地址栏：网页视图首次进入会自动
/// 打开地址栏里的值，不预设就会去请求默认的 `http://127.0.0.1/`（测试环境
/// 80 端口没有服务，只会产出一条无关的 502 日志）。
async function enterWebView(page, initial) {
  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-web-btn');
    return b && !b.classList.contains('hidden') && !b.disabled;
  }, { timeout: 30000 });
  if (initial) {
    await page.evaluate((u) => {
      document.getElementById('webview-url-input').value = u;
    }, initial);
  }
  await page.click('#toggle-web-btn');
  await page.waitForSelector('#webview-url-input', { state: 'visible', timeout: 15000 });
}

async function openUrl(page, url) {
  await page.fill('#webview-url-input', url);
  await page.click('#webview-open-btn');
}

/// 等 iframe 里出现某个元素且满足条件（`expr` 是在 iframe 上下文里求值的
/// JS 表达式字符串）。
async function waitFrame(page, expr, timeout) {
  // iframe 可能还停在 about:blank（刚切进网页视图）：先轮询等它出现。
  const deadline = Date.now() + (timeout || 40000);
  let f = await findFrame(page);
  while (!f && Date.now() < deadline) {
    await sleep(200);
    f = await findFrame(page);
  }
  if (!f) return false;
  return f.waitForFunction(expr, null, { timeout: Math.max(1000, deadline - Date.now()) })
    .then(() => true).catch(() => false);
}

/// 等映射首页**完整**加载：状态区报「已加载」且脚本已执行、视图已渲染。
/// 只等 `#view-name` 会被上一次加载残留的旧文档提前满足（重复打开时的竞态）。
const SPA_READY = `document.body && document.body.getAttribute('data-script') === 'ran' &&
  (document.getElementById('view-name') || {}).textContent === 'root-view'`;
async function waitSpaReady(page) {
  await page.waitForFunction(() => /已加载/.test(
    (document.getElementById('webview-status') || {}).textContent || ''),
    null, { timeout: 40000 }).catch(() => {});
  return waitFrame(page, SPA_READY);
}

/// 找映射页面的 frame：非主 frame，且不是 about:blank。
async function findFrame(page) {
  for (const fr of page.frames()) {
    if (fr === page.mainFrame()) continue;
    const u = fr.url();
    if (!u || u === 'about:blank') continue;
    return fr;
  }
  return null;
}

/// 在 iframe 上下文中求值。iframe 可能正在导航（执行上下文被销毁 → evaluate 抛
/// "Execution context was destroyed"），这里重试几次而不是把它当成断言失败。
async function frameEval(page, fn, arg) {
  let lastErr = null;
  for (let i = 0; i < 12; i += 1) {
    const f = await findFrame(page);
    if (f) {
      try {
        return await f.evaluate(fn, arg);
      } catch (e) {
        lastErr = e;
      }
    }
    await sleep(250);
  }
  if (lastErr) console.log('  · frameEval: ' + String(lastErr.message || lastErr).slice(0, 120));
  return null;
}

// ══════════════════════ 主流程 ══════════════════════

(async () => {
  fs.mkdirSync(OUT, { recursive: true });
  const site = await startSite();
  const roots = fs.mkdtempSync(path.join(os.tmpdir(), 'sr2383g-'));
  const procs = [];
  let browser = null;
  let rwPage = null;

  const haveOld = !!V058_BIN && fs.existsSync(V058_BIN) && V058_BIN !== BIN;
  const haveAncient = !!V057_BIN && fs.existsSync(V057_BIN) && V057_BIN !== BIN;

  try {
    // ── relay ×4（rw / ro / 旧 agent / 无能力 agent）──
    procs.push(startProc(['relay', '--bind', `127.0.0.1:${RW_PORT}`, '--auth', AUTH, '--no-tls'], 'relay-rw'));
    procs.push(startProc(['relay', '--bind', `127.0.0.1:${RO_PORT}`, '--auth', AUTH, '--no-tls'], 'relay-ro'));
    if (haveOld) {
      procs.push(startProc(['relay', '--bind', `127.0.0.1:${OLD_PORT}`, '--auth', AUTH, '--no-tls'], 'relay-old'));
    }
    if (haveAncient) {
      procs.push(startProc(['relay', '--bind', `127.0.0.1:${ANCIENT_PORT}`, '--auth', AUTH, '--no-tls'], 'relay-ancient'));
    }
    await waitPort(RW_PORT);
    await waitPort(RO_PORT);

    // ── agent：本仓库新构建（含 net:tcp-v1）+ ro token + 旧版 ──
    procs.push(startProc(['agent', '--relay-url', BASE, '--key', RW_KEY,
      '--session-id', RW_SID, '--root', roots, '--shell', '/bin/sh'], 'agent-rw'));
    procs.push(startProc(['agent', '--relay-url', BASE, '--key', RW2_KEY,
      '--session-id', RW2_SID, '--root', roots, '--shell', '/bin/sh'], 'agent-rw2'));
    procs.push(startProc(['agent', '--relay-url', RO_BASE, '--key', RO_KEY,
      '--token-type', 'ro', '--session-id', RO_SID, '--root', roots, '--shell', '/bin/sh'], 'agent-ro'));
    if (haveOld) {
      await waitPort(OLD_PORT);
      procs.push(startProc(['agent', '--relay-url', OLD_BASE, '--key', OLD_KEY,
        '--session-id', OLD_SID, '--root', roots, '--shell', '/bin/sh'], 'agent-old', V058_BIN));
    }
    if (haveAncient) {
      await waitPort(ANCIENT_PORT);
      procs.push(startProc(['agent', '--relay-url', ANCIENT_BASE, '--key', ANCIENT_KEY,
        '--session-id', ANCIENT_SID, '--root', roots, '--shell', '/bin/sh'], 'agent-ancient', V057_BIN));
    }
    await sleep(3500);

    const exe = resolveChromium();
    browser = await chromium.launch(exe
      ? { executablePath: exe, args: ['--disable-gpu', '--disable-software-rasterizer'] }
      : { args: ['--disable-gpu', '--disable-software-rasterizer'] });
    console.log(`（chromium: ${exe || 'playwright 默认'}）`);

    // ══════════════ [1] 模式判定：v2 完整模式 ══════════════
    console.log('\n[1] 模式判定与入口（rw token + 新 agent）');
    const rwCtx = await makePage(browser, BASE, RW_KEY, 'rw');
    rwPage = rwCtx.page;
    await rwPage.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
    // 模式判定要在打开页面前观察 iframe 属性，所以这一步不预设地址；打开
    // 页面在 [2] 里显式做（首次进入的默认 http://127.0.0.1/ 请求会 502，
    // 与断言无关）。
    await enterWebView(rwPage, SITE + '/');
    const modeInfo = await rwPage.evaluate(() => {
      const badge = document.getElementById('webview-mode');
      const frame = document.getElementById('webview-frame');
      return {
        badge: badge ? badge.textContent.trim() : null,
        badgeHidden: badge ? badge.classList.contains('hidden') : true,
        sandbox: frame.getAttribute('sandbox'),
        referrerpolicy: frame.getAttribute('referrerpolicy'),
        hint: (document.getElementById('webview-hint') || {}).textContent || '',
        secure: window.isSecureContext,
      };
    });
    check('状态区显示「完整模式」', modeInfo.badge === '完整模式', modeInfo.badge);
    check('v2 下 iframe 去掉 sandbox（需要执行脚本）', modeInfo.sandbox === null, modeInfo.sandbox);
    check('v2 下 iframe 去掉 no-referrer', modeInfo.referrerpolicy === null, modeInfo.referrerpolicy);
    check('v2 提示文案说明同源风险与 rw 限制',
      /同源运行/.test(modeInfo.hint) && /rw Token/.test(modeInfo.hint), modeInfo.hint.slice(0, 120));
    // ══════════════ [2] SPA 渲染、脚本执行、样式 ══════════════
    console.log('\n[2] SPA 首页：脚本执行、样式、根路径资源');
    await openUrl(rwPage, SITE + '/');
    const okIndex = await waitSpaReady(rwPage);
    check('SPA 首页渲染（#view-name 出现）', okIndex);
    // 等 Service Worker 接管并完成第二次导航（首次进入 → 302 → 原始路径）。
    const frameUrl = await rwPage.evaluate(() => {
      const f = document.getElementById('webview-frame');
      try { return f.contentWindow.location.href; } catch (e) { return 'cross-origin'; }
    }).catch(() => null);
    const doc = await frameEval(rwPage, () => ({
      path: location.pathname,
      scriptRan: document.body.getAttribute('data-script'),
      dataPath: document.body.getAttribute('data-path'),
      titleColor: (function () {
        const t = document.getElementById('app-title');
        return t ? getComputedStyle(t).color : null;
      })(),
      viewName: (document.getElementById('view-name') || {}).textContent || null,
      input: document.getElementById('webview-url-input')
        ? document.getElementById('webview-url-input').value : null,
      hasSw: !!navigator.serviceWorker,
    }));
    check('iframe 地址是目标站原始路径（无 /agent/web/m 前缀）',
      !!doc && doc.path === '/', doc && doc.path);
    check('远端脚本真实执行（data-script=ran）', !!doc && doc.scriptRan === 'ran',
      doc && doc.scriptRan);
    check('外部 CSS 生效（颜色来自 /static/style.css）',
      !!doc && doc.titleColor === 'rgb(9, 87, 65)', doc && doc.titleColor);
    check('SPA 渲染出 root 视图', !!doc && doc.viewName === 'root-view', doc && doc.viewName);
    const statusText = await rwPage.evaluate(() =>
      document.getElementById('webview-status').textContent);
    check('状态区显示已加载目标地址', /已加载/.test(statusText) && statusText.indexOf(SITE) >= 0,
      statusText);
    const inputVal = await rwPage.evaluate(() =>
      document.getElementById('webview-url-input').value);
    check('地址栏同步为目标站地址（原始路径，不带前缀）',
      inputVal.indexOf(SITE) === 0 && inputVal.indexOf('/agent/web/m') < 0, inputVal);
    await rwPage.screenshot({ path: path.join(OUT, 'spa-dashboard.png') });

    // ══════════════ [3] history 路由 + 刷新 ══════════════
    console.log('\n[3] history 路由（pushState）与刷新保持路由');
    await frameEval(rwPage, () => { document.getElementById('link-dashboard').click(); });
    await sleep(400);
    const afterPush = await frameEval(rwPage, () => ({
      path: location.pathname,
      view: (document.getElementById('view-name') || {}).textContent || null,
    }));
    check('pushState 后 location.pathname=/dashboard 且视图切换',
      !!afterPush && afterPush.path === '/dashboard' && afterPush.view === 'dashboard-view',
      afterPush);
    await rwPage.screenshot({ path: path.join(OUT, 'spa-dashboard-route.png') });
    // 刷新 iframe：目标站的 /dashboard（服务端也返回 SPA 首页）必须能重新加载。
    await rwPage.click('#webview-refresh-btn');
    const reloadOk = await waitFrame(rwPage,
      `document.getElementById('view-name') !== null && location.pathname === '/dashboard'`);
    check('刷新后仍停在 /dashboard（路由与文档都正确）', reloadOk);
    const afterReload = await frameEval(rwPage, () => ({
      path: location.pathname,
      script: document.body.getAttribute('data-script'),
      view: (document.getElementById('view-name') || {}).textContent || null,
    }));
    check('刷新后脚本仍执行、视图仍是 dashboard',
      !!afterReload && afterReload.script === 'ran' && afterReload.view === 'dashboard-view',
      afterReload);

    // ══════════════ [4] Cookie 登录（POST + Set-Cookie + 依赖 Cookie 的 GET）══
    console.log('\n[4] Cookie 登录：POST /api/login → /api/me');
    const meText = await frameEval(rwPage, () => window.apiLogin());
    check('POST /api/login 设置 Cookie 后 /api/me 返回已登录',
      meText === 'user=alice', meText);

    // ══════════════ [5] 绝对地址 fetch（shim 改写）══
    console.log('\n[5] 写死目标源绝对地址的 fetch');
    const ping = await frameEval(rwPage, (origin) => window.apiPing(origin), SITE);
    check('绝对地址 fetch 经映射成功返回 pong', ping === 'pong', ping);

    // ══════════════ [6] WebSocket echo + 服务端推送 ══════════════
    console.log('\n[6] WebSocket：echo 与服务端主动推送');
    const wsState = await frameEval(rwPage, () => window.startWs());
    check('WebSocket echo 收到 hello-echo',
      !!wsState && Array.isArray(wsState.echo) && wsState.echo.indexOf('hello-echo') >= 0,
      wsState);
    check('WebSocket 收到服务端推送两条',
      !!wsState && Array.isArray(wsState.pushed) && wsState.pushed.length === 2,
      wsState && wsState.pushed);
    // shim 改写后的 URL 必须带映射前缀（这是握手真的经过反代的最直接证据：
    // 直连 relay 源的 /ws 会 404，直连目标源则完全绕过网络隧道）。
    const wsUrl = wsState && wsState.url ? String(wsState.url) : '';
    check('WebSocket URL 被 shim 改写为 /agent/web/m/<map_id>/ws',
      /^wss?:\/\/[^/]+\/agent\/web\/m\/[0-9a-f]{64}\/ws$/.test(wsUrl), wsUrl);
    check('目标站收到的 WS 路径是剥离前缀后的 /ws',
      wsUpgrades > 0 && wsPaths.indexOf('/ws') >= 0, JSON.stringify(wsPaths));
    await rwPage.screenshot({ path: path.join(OUT, 'websocket.png') });

    // ══════════════ [7] 整页跳转与 302 ══════════════
    console.log('\n[7] 整页跳转 <a href="/page2"> 与 302 /old → /page2');
    await frameEval(rwPage, () => { document.getElementById('link-page2').click(); });
    const page2Ok = await waitFrame(rwPage, `document.getElementById('page2-title') !== null`);
    check('整页跳转到 /page2 成功', page2Ok);
    const page2 = await frameEval(rwPage, () => ({
      path: location.pathname,
      title: (document.getElementById('page2-title') || {}).textContent || null,
      script: document.body.getAttribute('data-script'),
    }));
    const inputPage2 = await rwPage.evaluate(() =>
      document.getElementById('webview-url-input').value);
    check('整页跳转后地址栏显示目标原始路径',
      !!page2 && page2.path === '/page2' && inputPage2 === SITE + '/page2',
      [page2 && page2.path, inputPage2]);
    check('整页跳转后的页面脚本也执行（data-script=ran）',
      !!page2 && page2.script === 'ran', page2 && page2.script);
    // 回到首页再走 302
    await frameEval(rwPage, () => { location.href = '/'; });
    await waitFrame(rwPage, `document.getElementById('link-old') !== null`);
    await frameEval(rwPage, () => { document.getElementById('link-old').click(); });
    // 302 → /page2：等目标页渲染完再断言（SW 自己跟随一次跳转，会多一帧）。
    const ok302 = await waitFrame(rwPage, `document.getElementById('page2-title') !== null`);
    const after302 = await frameEval(rwPage, () => ({
      path: location.pathname,
      title: (document.getElementById('page2-title') || {}).textContent || null,
    }));
    const input302 = await rwPage.evaluate(() =>
      document.getElementById('webview-url-input').value);
    check('302 /old → /page2 页面正确、地址栏显示 /page2',
      ok302 && !!after302 && after302.title === 'page2-full-load' &&
      after302.path === '/page2' && input302 === SITE + '/page2',
      [ok302, after302, input302]);
    await rwPage.screenshot({ path: path.join(OUT, 'page2.png') });

    // ══════════════ [8] GBK 页面 ══════════════
    console.log('\n[8] GBK 编码页面：中文不乱码');
    await openUrl(rwPage, SITE + '/gbk');
    const gbkOk = await waitFrame(rwPage, `document.getElementById('gbk-title') !== null`);
    check('GBK 页面已渲染', gbkOk);
    const gbkText = await frameEval(rwPage, () => {
      const el = document.getElementById('gbk-title');
      return el ? el.textContent : null;
    });
    check('GBK 中文按 GBK 正确解码（注入脚本未破坏字节）', gbkText === '中文页面',
      JSON.stringify(gbkText));
    await rwPage.screenshot({ path: path.join(OUT, 'gbk-page.png') });

    // ══════════════ [9] 页面尝试注销我们的 SW ══════════════
    console.log('\n[9] 映射页面尝试注销 Service Worker（必须失败）');
    await openUrl(rwPage, SITE + '/');
    await waitFrame(rwPage, `document.getElementById('view-name') !== null`);
    const kill = await frameEval(rwPage, () => window.killSw());
    // shim 让它枚举为空：found 数组必须为空（拿不到我们的 registration）。
    const killObj = (() => { try { return JSON.parse(kill); } catch (e) { return null; } })();
    check('页面枚举不到任何 registration（无法注销我们的 SW）',
      !!killObj && Array.isArray(killObj.found) && killObj.found.length === 0, kill);
    // 后续请求仍正常：再点一次 dashboard 路由 + 一次 fetch。
    await frameEval(rwPage, () => { document.getElementById('link-dashboard').click(); });
    await sleep(300);
    const stillWorks = await frameEval(rwPage, () => window.apiLogin());
    check('注销尝试之后后续请求仍正常（/api/me 仍返回已登录）',
      stillWorks === 'user=alice', stillWorks);

    // ══════════════ [10] 回归：SW 不影响 shell-remote 自身 + 刷新会话页 ══════════════
    console.log('\n[10] 回归：SW 安装后 shell-remote 自身功能与刷新');
    await rwPage.click('#webview-back-btn');
    await sleep(300);
    const marker = 'mapuiterm' + Date.now();
    await rwPage.click('#terminal-container .xterm');
    await rwPage.waitForTimeout(300);
    await rwPage.keyboard.type(`echo ${marker}\n`);
    const echoed = await rwPage.waitForFunction(
      (m) => document.getElementById('terminal-container').innerText.indexOf(m) !== -1,
      marker, { timeout: 20000 }).then(() => true).catch(() => false);
    check('SW 安装后终端输入仍有回显（SSE 上行未被拦截）', echoed, marker);

    // 文件面板 + 桌面按钮的可用性（desktop 能力取决于构建；UI 构建才有）。
    const sidePanels = await rwPage.evaluate(() => ({
      filesBtn: !!document.getElementById('toggle-files-btn') ||
        !!document.getElementById('file-refresh-btn'),
      desktopBtn: (function () {
        const b = document.getElementById('toggle-desktop-btn');
        return b ? !b.classList.contains('hidden') : null;
      })(),
    }));
    check('文件面板入口仍可用', sidePanels.filesBtn);
    // 刷新会话页：SW 必须在刷新后仍然生效（v2 仍能打开页面）。
    await rwPage.reload({ waitUntil: 'domcontentloaded' });
    await rwPage.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
    await enterWebView(rwPage, SITE + '/');
    await openUrl(rwPage, SITE + '/');
    const afterReloadSession = await waitSpaReady(rwPage);
    check('刷新会话页后 v2 仍能打开映射页面（SW 跨刷新有效）', afterReloadSession);
    const afterReloadSessionDoc = await frameEval(rwPage, () => ({
      path: location.pathname,
      script: document.body.getAttribute('data-script'),
    }));
    check('刷新会话页后地址仍是原始路径且脚本执行',
      !!afterReloadSessionDoc && afterReloadSessionDoc.path === '/' &&
      afterReloadSessionDoc.script === 'ran', afterReloadSessionDoc);

    // ══════════════ [10b] 并发：两个会话同时映射，子资源与 fetch 不串台 ══════════════
    // SW 的 scope 是 `/`，两个会话页共享同一个 SW。归属靠 clientId，因此必须
    // 证明两个会话页各自 iframe 里的 fetch / 子资源都只走自己的 map_id。
    console.log('\n[10b] 并发多会话：clientId 归属不串台');
    const s2Ctx = await makePage(browser, BASE, RW2_KEY, 'rw');
    await s2Ctx.page.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
    // 两个会话页同时打开同一个目标站（relay 侧各自一个映射）。
    await enterWebView(s2Ctx.page, SITE + '/');
    await openUrl(s2Ctx.page, SITE + '/');
    await waitFrame(s2Ctx.page, `document.getElementById('view-name') !== null`);
    await openUrl(rwPage, SITE + '/');
    await waitFrame(rwPage, `document.getElementById('view-name') !== null`);

    // 各自登录（Cookie 属于目标站，但每个 iframe 的请求必须走自己的映射）。
    const me1 = await frameEval(rwPage, () => window.apiLogin());
    const me2 = await frameEval(s2Ctx.page, () => window.apiLogin());
    check('两个会话页并发映射同一目标站都能登录', me1 === 'user=alice' && me2 === 'user=alice',
      [me1, me2]);
    // 两个 iframe 的 map_id 不同（relay 按 session 隔离映射）。
    const mapIds = await Promise.all([rwPage, s2Ctx.page].map((pg) => pg.evaluate(() => {
      const f = document.getElementById('webview-frame');
      return f ? f.getAttribute('src') : '';
    })));
    check('两个会话页各自持有独立的映射入口',
      mapIds[0] !== mapIds[1], mapIds);
    // 并发压一轮：各 5 次绝对地址 fetch，结果都必须是 pong（没串到对方/超时）。
    const pings = await Promise.all([
      frameEval(rwPage, async (origin) => {
        const out = [];
        for (let i = 0; i < 5; i += 1) out.push(await window.apiPing(origin));
        return out;
      }, SITE),
      frameEval(s2Ctx.page, async (origin) => {
        const out = [];
        for (let i = 0; i < 5; i += 1) out.push(await window.apiPing(origin));
        return out;
      }, SITE),
    ]);
    check('并发 fetch 全部成功（5+5）',
      Array.isArray(pings[0]) && Array.isArray(pings[1]) &&
      pings[0].length === 5 && pings[1].length === 5 &&
      pings[0].every((v) => v === 'pong') && pings[1].every((v) => v === 'pong'),
      pings);
    await rwPage.screenshot({ path: path.join(OUT, 'concurrent-a.png') });
    await s2Ctx.page.screenshot({ path: path.join(OUT, 'concurrent-b.png') });
    await s2Ctx.ctx.close();

    // ══════════════ [11] 降级：ro token → v1 静态模式 ══════════════
    console.log('\n[11] 降级：ro token → v1 静态模式');
    const roCtx = await makePage(browser, RO_BASE, RO_KEY, 'ro');
    await roCtx.page.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
    await enterWebView(roCtx.page, SITE + '/');
    const roMode = await roCtx.page.evaluate(() => ({
      badge: (document.getElementById('webview-mode') || {}).textContent,
      sandbox: document.getElementById('webview-frame').getAttribute('sandbox'),
      hint: (document.getElementById('webview-hint') || {}).textContent || '',
    }));
    check('ro token 显示「静态模式」', String(roMode.badge).trim() === '静态模式', roMode.badge);
    check('ro token 下 iframe 恢复 sandbox=allow-same-origin',
      roMode.sandbox === 'allow-same-origin', roMode.sandbox);
    check('ro token 提示只读原因', /只读/.test(roMode.hint), roMode.hint.slice(0, 100));
    await roCtx.page.screenshot({ path: path.join(OUT, 'ro-static-mode.png') });

    // ══════════════ [11b] 降级：无 Service Worker → v1 静态模式 ══════════════
    console.log('\n[11b] 降级：浏览器无 Service Worker → v1 静态模式');
    const noSwCtx = await makePage(browser, BASE, RW_KEY, 'rw', { noServiceWorker: true });
    await noSwCtx.page.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
    await enterWebView(noSwCtx.page, SITE + '/');
    const noSwMode = await noSwCtx.page.evaluate(() => ({
      badge: (document.getElementById('webview-mode') || {}).textContent,
      sandbox: document.getElementById('webview-frame').getAttribute('sandbox'),
      hint: (document.getElementById('webview-hint') || {}).textContent || '',
    }));
    check('无 SW 时显示「静态模式」（rw + 新 agent 也不强上 v2）',
      String(noSwMode.badge).trim() === '静态模式', noSwMode.badge);
    check('无 SW 时 iframe 恢复 sandbox=allow-same-origin',
      noSwMode.sandbox === 'allow-same-origin', noSwMode.sandbox);
    check('无 SW 时提示文案说明浏览器原因',
      /Service Worker/.test(noSwMode.hint), noSwMode.hint.slice(0, 120));
    await noSwCtx.page.screenshot({ path: path.join(OUT, 'no-serviceworker-static-mode.png') });
    await noSwCtx.ctx.close();

    // ══════════════ [12] 降级：v0.58（只有 web:proxy-v1）→ v1 ══════════════
    console.log('\n[12] 降级：v0.58.0 agent（只有 web:proxy-v1）→ v1 静态模式');
    if (haveOld) {
      const oldCtx = await makePage(browser, OLD_BASE, OLD_KEY, 'rw');
      await oldCtx.page.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
      await enterWebView(oldCtx.page, SITE + '/');
      const oldMode = await oldCtx.page.evaluate(() => ({
        badge: (document.getElementById('webview-mode') || {}).textContent,
        sandbox: document.getElementById('webview-frame').getAttribute('sandbox'),
        hint: (document.getElementById('webview-hint') || {}).textContent || '',
      }));
      check('v0.58 agent 显示「静态模式」', String(oldMode.badge).trim() === '静态模式', oldMode.badge);
      check('v0.58 agent 下 iframe 有 sandbox（不执行远端脚本）',
        oldMode.sandbox === 'allow-same-origin', oldMode.sandbox);
      check('v0.58 agent 提示 agent 版本原因', /agent/.test(oldMode.hint), oldMode.hint.slice(0, 100));
      await oldCtx.page.screenshot({ path: path.join(OUT, 'v058-static-mode.png') });
      await oldCtx.ctx.close();
    } else {
      skip('v0.58 agent → v1 静态模式', `V058_BIN 不可用: ${V058_BIN || 'dist 下没有 web:proxy-v1 二进制'}`);
    }

    // ══════════════ [13] 降级：v0.57（无任何网页能力）→ 无入口 ══════════════
    console.log('\n[13] 降级：v0.57.0 agent（无网页能力）→ 无入口');
    if (haveAncient) {
      const ancCtx = await makePage(browser, ANCIENT_BASE, ANCIENT_KEY, 'rw');
      await ancCtx.page.waitForSelector('#terminal-container .xterm', { timeout: 30000 });
      await ancCtx.page.waitForTimeout(3000);
      const ancBtn = await ancCtx.page.evaluate(() => {
        const b = document.getElementById('toggle-web-btn');
        return b ? { hidden: b.classList.contains('hidden'), disabled: b.disabled } : null;
      });
      check('v0.57 agent：「网页」入口不可见', !!ancBtn && ancBtn.hidden && ancBtn.disabled, ancBtn);
      await ancCtx.page.screenshot({ path: path.join(OUT, 'v057-no-entry.png') });
      await ancCtx.ctx.close();
    } else {
      skip('v0.57 agent → 无入口', `V057_BIN 不可用: ${V057_BIN || 'dist 下没有无网页能力的二进制'}`);
    }

    // ══════════════ [14] agent 断线 → 入口隐藏、iframe 清空 ══════════════
    console.log('\n[14] agent 断线：入口隐藏、iframe 清空');
    const rwAgent = procs.find((p) => p.spawnargs && p.spawnargs.indexOf(RW_SID) >= 0);
    if (rwAgent) {
      // 上一步（刷新会话页后的映射）留下的就是网页视图：直接确认、不再点
      // 切换按钮（再点一次会退出网页视图）。
      await rwPage.waitForTimeout(400);
      const beforeDrop = await rwPage.evaluate(() => ({
        webVisible: !document.getElementById('webview-container').classList.contains('hidden'),
        btnText: document.getElementById('toggle-web-btn').textContent.trim(),
      }));
      check('断线前处于网页视图', beforeDrop.webVisible, beforeDrop);
      rwAgent.kill('SIGKILL');
      await rwPage.waitForFunction(() => {
        const b = document.getElementById('toggle-web-btn');
        return b && b.classList.contains('hidden');
      }, { timeout: 25000 }).catch(() => {});
      const afterDrop = await rwPage.evaluate(() => {
        const b = document.getElementById('toggle-web-btn');
        const f = document.getElementById('webview-frame');
        return {
          hidden: b.classList.contains('hidden'),
          webHidden: document.getElementById('webview-container').classList.contains('hidden'),
          src: f.getAttribute('src'),
          srcdoc: f.getAttribute('srcdoc'),
        };
      });
      check('断线后「网页」入口隐藏', afterDrop.hidden, afterDrop);
      check('断线后网页视图退出、iframe 清空',
        afterDrop.webHidden && (!afterDrop.src || afterDrop.src === 'about:blank') &&
        !afterDrop.srcdoc, afterDrop);
      await rwPage.screenshot({ path: path.join(OUT, 'after-disconnect.png') });
    } else {
      skip('agent 断线回归', '未找到 rw agent 进程');
    }

    console.log('\nE2E 截图目录：' + OUT);
  } catch (e) {
    console.error('E2E 脚本异常：', e);
    fails.push('script-error');
  } finally {
    try { if (browser) await browser.close(); } catch (e) {}
    for (const p of procs) { try { p.kill('SIGKILL'); } catch (e) {} }
    try { site.close(); } catch (e) {}
    try { for (const s of wsPeers) { try { s.destroy(); } catch (e) {} } } catch (e) {}
  }

  console.log('\n' + (fails.length === 0
    ? `全部通过：${checks} 项` + (skips.length ? `（跳过 ${skips.length} 项）` : '')
    : `失败 ${fails.length}/${checks} 项：\n  - ` + fails.join('\n  - ') +
      (skips.length ? `\n跳过 ${skips.length} 项：\n  - ` + skips.join('\n  - ') : '')));
  process.exit(fails.length === 0 ? 0 : 1);
})();
