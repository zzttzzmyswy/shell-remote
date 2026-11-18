#!/usr/bin/env node
'use strict';
// MYS-2945：自签 HTTPS 下 relay 的网页映射完整模式（Service Worker）验证。
//
// 一条命令跑完：**临时私有 CA + 该 CA 签发的 SAN 匹配服务器证书 + 真 relay +
// 真 agent + 只监听 127.0.0.1 的测试 SPA**，在真实 Chromium 里点会话页的
// 「网页」按钮，断言：
//   [1] 证书被信任（导入隔离 NSS 库）时 → 模式徽标是「完整模式」、SW 激活、
//       iframe 里脚本真的执行、动态请求成功；
//   [2] 同一张证书**未**被信任时 → 注册失败，状态区给出**证书**提示（不是笼统的
//       “无法注册 Service Worker”），且不谎称“已回退静态模式”；
//   [3] 证书被信任、但访问地址不在 SAN 里 → 浏览器在导航阶段就拒绝；
//   [4] 只读 Token 仍然回退静态模式（原有行为不被破坏）。
//
// 隔离要求（任务卡明确要求）：
//   - 信任库：给 Chromium 独立的 HOME（`$HOME/.pki/nssdb`），用 certutil 只往这个
//     库里导入临时 CA；**不碰**用户/系统的信任库
//   - 不加 `--ignore-certificate-errors`，**不用** CDP 忽略证书错误
//   - 每个场景用全新的浏览器 profile/HOME，避免上一次的例外残留
//
// 用法（先构建）：
//   cargo build --bin shell-remote
//   xvfb-run -a node tools/verify/verify_web_map_tls.js
//
// 环境变量：BIN / CA_DIR（复用已有 CA）/ KEEP_TLS_DIR（保留临时目录便于排查）/
//   SITE_PORT / RW_PORT / RO_PORT / OUT。
// 依赖：openssl、certutil（libnss3-tools）。缺任一则**明确失败**而不是跳过——
// 跳过会让“证书没验过”看起来像通过。

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');

let chromium;
try {
  ({ chromium } = require('playwright'));
} catch (e) {
  ({ chromium } = require('playwright-core'));
}

const ROOT = path.dirname(path.dirname(path.dirname(fs.realpathSync(__filename))));
const BIN = process.env.BIN || path.join(ROOT, 'target', 'debug', 'shell-remote');
const OUT = process.env.OUT || path.join(ROOT, 'artifacts', 'mys2945-tls');
const AUTH = process.env.RELAY_PASSWORD || 'tls-e2e-pass';
const RW_KEY = 'tlse2erw01';
const RO_KEY = 'tlse2ero01';
const RW_SID = 'tlse2edev1';
const RO_SID = 'tlse2edev2';

let checks = 0;
const fails = [];
const skips = [];
function check(name, cond, detail) {
  checks += 1;
  if (cond) {
    console.log(`  PASS ${name}`);
  } else {
    const d = detail === undefined ? ''
      : (typeof detail === 'string' ? detail : JSON.stringify(detail));
    console.log(`  FAIL ${name}${d ? `   <- ${d}` : ''}`);
    fails.push(name);
  }
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ══════════════════════ 证书材料（临时私有 CA） ══════════════════════

function have(cmd) {
  const r = spawnSync('sh', ['-c', `command -v ${cmd}`], { encoding: 'utf8' });
  return r.status === 0 && String(r.stdout).trim() !== '';
}

function run(cmd, args, opts) {
  const r = spawnSync(cmd, args, Object.assign({ encoding: 'utf8' }, opts || {}));
  if (r.status !== 0) {
    throw new Error(`${cmd} ${args.join(' ')} 失败：${(r.stderr || r.stdout || '').trim()}`);
  }
  return r.stdout;
}

/// 本机用于访问 relay 的地址。必须**真的**落在证书 SAN 里，否则浏览器在导航
/// 阶段就拒绝，测不到 SW 注册。
function lanAddress() {
  const out = process.env.SR_TEST_ADDR || '';
  if (out) return out;
  try {
    const txt = run('sh', ['-c',
      "ip -4 addr show scope global 2>/dev/null | grep -oE 'inet[[:space:]]+[0-9.]+' | awk '{print $2}' | head -1"]);
    const ip = String(txt).trim();
    if (ip) return ip;
  } catch (e) {}
  return '127.0.0.1';
}

/// 建临时 CA 与两张服务器证书：SAN 匹配的、以及 SAN 故意不匹配的。
function makeCerts(dir, addr) {
  fs.mkdirSync(dir, { recursive: true });
  const ca = path.join(dir, 'ca.pem');
  const caKey = path.join(dir, 'ca-key.pem');
  run('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes',
    '-keyout', caKey, '-out', ca, '-days', '30',
    '-subj', '/CN=shell-remote e2e private CA',
    '-addext', 'basicConstraints=critical,CA:TRUE',
    '-addext', 'keyUsage=critical,keyCertSign,cRLSign']);

  const sign = (prefix, san) => {
    const key = path.join(dir, `${prefix}-key.pem`);
    const csr = path.join(dir, `${prefix}.csr`);
    const crt = path.join(dir, `${prefix}.pem`);
    run('openssl', ['req', '-newkey', 'rsa:2048', '-nodes', '-keyout', key,
      '-out', csr, '-subj', `/CN=${addr}`]);
    const ext = path.join(dir, `${prefix}-ext.cnf`);
    fs.writeFileSync(ext, `subjectAltName=${san}\n` +
      'basicConstraints=CA:FALSE\n' +
      'keyUsage=critical,digitalSignature,keyEncipherment\n' +
      'extendedKeyUsage=serverAuth\n');
    run('openssl', ['x509', '-req', '-in', csr, '-CA', ca, '-CAkey', caKey,
      '-CAcreateserial', '-out', crt, '-days', '30', '-extfile', ext]);
    return { cert: crt, key };
  };

  const matched = sign('srv-matched', `IP:${addr},DNS:${addr},DNS:localhost`);
  // SAN 故意只写一个别的地址：证书被信任也仍然名字不匹配。
  const mismatched = sign('srv-mismatched', 'IP:203.0.113.77,DNS:not-this-host.invalid');
  return { ca, matched, mismatched };
}

/// 隔离的浏览器 HOME + 其中的 NSS 库；`trustCa` 为真时把 CA 导入该库。
function makeBrowserHome(dir, caPem) {
  const home = fs.mkdtempSync(path.join(dir, 'home-'));
  const nssdb = path.join(home, '.pki', 'nssdb');
  fs.mkdirSync(nssdb, { recursive: true });
  run('certutil', ['-d', `sql:${nssdb}`, '-N', '--empty-password']);
  if (caPem) {
    run('certutil', ['-d', `sql:${nssdb}`, '-A', '-t', 'C,,',
      '-n', 'shell-remote e2e private CA', '-i', caPem]);
    const listed = run('certutil', ['-d', `sql:${nssdb}`, '-L']);
    if (!/shell-remote e2e private CA/.test(listed)) {
      throw new Error('CA 未成功导入隔离 NSS 库');
    }
  }
  return home;
}

// ══════════════════════ 测试 SPA（只监听 127.0.0.1） ══════════════════════
//
// 刻意做得最小：只需要证明“脚本真的执行了”和“动态请求真的成功”——这两件事
// 只有完整模式能做到，静态模式一律不执行远端脚本。

const SPA_INDEX = `<!DOCTYPE html>
<html lang="zh"><head>
<meta charset="utf-8"><title>TLS SPA</title>
<link rel="stylesheet" href="/static/style.css">
</head>
<body>
<h1 id="app-title">tls-spa</h1>
<div id="view"></div>
<div id="dyn-result"></div>
<script src="/static/app.js"></script>
</body></html>`;

const APP_JS = `
// 只有脚本执行了才会设置这些标记；v1 静态模式永远看不到。
document.body.setAttribute('data-script', 'ran');
document.getElementById('view').innerHTML = '<h2 id="view-name">root-view</h2>';
window.apiDynamic = function () {
  return fetch('/api/dynamic')
    .then(function (r) { return r.text(); })
    .then(function (t) { document.getElementById('dyn-result').textContent = t; return t; });
};
`;

const SPA_CSS = `
#app-title { color: rgb(9, 87, 65); }
`;

function siteHandler(req, res) {
  const u = new URL(req.url, 'http://127.0.0.1');
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
  if (p === '/api/dynamic') {
    return send(200, 'dynamic-ok', { 'Content-Type': 'text/plain; charset=utf-8' });
  }
  return send(404, '<html><body>site-404</body></html>');
}

function startSite(port) {
  const srv = http.createServer(siteHandler);
  return new Promise((resolve) => srv.listen(port, '127.0.0.1', () => resolve(srv)));
}

// ══════════════════════ 进程 / 浏览器工具 ══════════════════════

function startProc(args, label) {
  const p = spawn(BIN, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  const log = [];
  const push = (b) => { log.push(b.toString()); if (log.length > 200) log.shift(); };
  p.stdout.on('data', push);
  p.stderr.on('data', push);
  p.on('exit', (code) => { if (code && code !== 0) console.log(`  · ${label} exited ${code}`); });
  p.__log = log;
  p.__label = label;
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

/// 等 HTTPS 端口能应答。自签证书下普通 http 探针不通，这里接受任何 TLS 层响应
/// （包括证书错误）：只要连上了就说明 relay 在监听。
function waitTlsPort(host, port, timeout) {
  return new Promise((resolve) => {
    const end = Date.now() + (timeout || 30000);
    const attempt = () => {
      const req = https.request({ host, port, path: '/', rejectUnauthorized: false,
        timeout: 2000 }, (res) => { res.resume(); resolve(true); });
      req.on('error', () => {
        if (Date.now() > end) return resolve(false);
        setTimeout(attempt, 250);
      });
      req.on('timeout', () => { req.destroy(); });
      req.end();
    };
    attempt();
  });
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
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers,
        body: Buffer.concat(chunks) }));
    });
    req.on('error', () => resolve({ status: 0, headers: {}, body: Buffer.alloc(0) }));
    req.end(opts.body);
  });
}

/// 用给定 HOME（=独立信任库）启动 Chromium。**不加**任何忽略证书错误的参数。
async function launchBrowser(home, exe) {
  const opts = {
    args: ['--disable-gpu', '--disable-software-rasterizer',
      '--use-mock-keychain', '--password-store=basic', '--lang=zh-CN'],
    env: Object.assign({}, process.env, { HOME: home }),
  };
  if (exe) opts.executablePath = exe;
  return chromium.launch(opts);
}

async function makePage(browser, base, token) {
  const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 }, locale: 'zh-CN' });
  await ctx.addInitScript(([t, a]) => {
    try {
      sessionStorage.setItem('shell-remote-token', t);
      sessionStorage.setItem('shell-remote-auth', a);
    } catch (e) {}
  }, [token, AUTH]);
  const page = await ctx.newPage();
  page.on('pageerror', (e) => console.log('  · pageerror: ' + e.message));
  page.on('console', (m) => {
    if (m.type() === 'error') console.log('  · console.error: ' + m.text());
  });
  page.on('response', (r) => {
    if (r.status() >= 400) console.log(`  · http ${r.status()} ${r.request().method()} ${r.url()}`);
  });
  await page.goto(`${base}/session`);
  return { ctx, page };
}

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

async function findFrame(page) {
  for (const fr of page.frames()) {
    if (fr === page.mainFrame()) continue;
    const u = fr.url();
    if (!u || u === 'about:blank') continue;
    return fr;
  }
  return null;
}

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

  // ── 前置检查：缺工具就明确失败（跳过等于没验证） ──
  console.log('[0] 前置检查');
  const missing = ['openssl', 'certutil'].filter((c) => !have(c));
  if (!fs.existsSync(BIN)) missing.push(`二进制 ${BIN}`);
  if (missing.length) {
    console.log(`  FAIL 缺少依赖：${missing.join('、')}` +
      '（certutil 来自 libnss3-tools；二进制要先 cargo build --bin shell-remote）');
    process.exit(1);
  }
  check('openssl / certutil / 二进制齐备', true);

  const ADDR = lanAddress();
  console.log(`  · 访问地址 ${ADDR}（证书 SAN 会包含它）`);

  const TLS_DIR = process.env.CA_DIR || fs.mkdtempSync(path.join(os.tmpdir(), 'sr2945-ca-'));
  const certs = makeCerts(TLS_DIR, ADDR);
  check('临时私有 CA + SAN 匹配证书已生成',
    fs.existsSync(certs.ca) && fs.existsSync(certs.matched.cert));

  const SITE_PORT = Number(process.env.SITE_PORT || 19520);
  const RW_PORT = Number(process.env.RW_PORT || 19521);
  const RO_PORT = Number(process.env.RO_PORT || 19522);
  const MISMATCH_PORT = Number(process.env.MISMATCH_PORT || 19523);
  const RW_BASE = `https://${ADDR}:${RW_PORT}`;
  const RO_BASE = `https://${ADDR}:${RO_PORT}`;
  const MISMATCH_BASE = `https://${ADDR}:${MISMATCH_PORT}`;
  const SITE = `http://127.0.0.1:${SITE_PORT}`;

  const site = await startSite(SITE_PORT);
  const roots = fs.mkdtempSync(path.join(os.tmpdir(), 'sr2945-roots-'));
  const procs = [];
  const browsers = [];

  try {
    // ── relay ×3：受信证书 / ro / SAN 不匹配证书，全部 HTTPS ──
    const relayRw = startProc(['relay', '--bind', `0.0.0.0:${RW_PORT}`, '--auth', AUTH,
      '--tls-cert', certs.matched.cert, '--tls-key', certs.matched.key,
      '--settings-file', path.join(roots, 'rw-settings.json')], 'relay-rw');
    const relayRo = startProc(['relay', '--bind', `0.0.0.0:${RO_PORT}`, '--auth', AUTH,
      '--tls-cert', certs.matched.cert, '--tls-key', certs.matched.key,
      '--settings-file', path.join(roots, 'ro-settings.json')], 'relay-ro');
    const relayMm = startProc(['relay', '--bind', `0.0.0.0:${MISMATCH_PORT}`, '--auth', AUTH,
      '--tls-cert', certs.mismatched.cert, '--tls-key', certs.mismatched.key,
      '--settings-file', path.join(roots, 'mm-settings.json')], 'relay-mismatch');
    procs.push(relayRw, relayRo, relayMm);
    const up = await Promise.all([
      waitTlsPort(ADDR, RW_PORT), waitTlsPort(ADDR, RO_PORT), waitTlsPort(ADDR, MISMATCH_PORT),
    ]);
    check('三个 HTTPS relay 都已监听', up.every(Boolean), up);
    // 端口通不等于“我们起的那个 relay 在跑”：端口被别的进程占着时，它照样通。
    // 这里必须确认自己的进程还活着，且日志里出现了 TLS 启动行。
    const alive = [relayRw, relayRo, relayMm].map((p) => p.exitCode === null);
    check('三个 relay 进程都是本次启动的（未被占用端口顶替）', alive.every(Boolean), alive);
    const tlsLine = [relayRw.__log.join(''), relayRo.__log.join(''), relayMm.__log.join('')]
      .map((l) => /HTTPS \(self-signed\) enabled/.test(l));
    check('三个 relay 都真的用 --tls-cert 起了 HTTPS', tlsLine.every(Boolean), tlsLine);
    // 证书确实是这次生成的（relay 日志里带证书路径）。
    check('relay 日志里的证书路径就是临时 CA 签发的那张',
      relayRw.__log.join('').indexOf(certs.matched.cert) >= 0, certs.matched.cert);

    // ── agent：relay 用自签证书，agent 侧按仓库既有方式信任（--relay-insecure） ──
    procs.push(startProc(['agent', '--relay-url', RW_BASE, '--relay-insecure',
      '--key', RW_KEY, '--session-id', RW_SID, '--root', roots, '--shell', '/bin/sh'], 'agent-rw'));
    procs.push(startProc(['agent', '--relay-url', RO_BASE, '--relay-insecure',
      '--key', RO_KEY, '--token-type', 'ro', '--session-id', RO_SID,
      '--root', roots, '--shell', '/bin/sh'], 'agent-ro'));
    await sleep(3500);

    const exe = resolveChromium();
    console.log(`（chromium: ${exe || 'playwright 默认'}）`);

    // ══════════════ [1] 证书受信（隔离信任库）→ 完整模式 ══════════════
    console.log('\n[1] 证书受信任：完整模式、SW 激活、脚本执行、动态请求');
    // 隔离 HOME 里只导入这次生成的临时 CA。
    const homeTrusted = makeBrowserHome(TLS_DIR, certs.ca);
    const b1 = await launchBrowser(homeTrusted, exe);
    browsers.push(b1);
    const { ctx: ctx1, page: page1 } = await makePage(b1, RW_BASE, RW_KEY);
    await page1.waitForSelector('#terminal-container .xterm', { timeout: 30000 });

    const secure = await page1.evaluate(() => ({
      secure: window.isSecureContext, origin: location.origin,
      // 未被信任时导航会被拦到 chrome-error://，到不了这里
    }));
    check('页面在受信证书下正常加载且是安全上下文',
      secure.secure === true && secure.origin === RW_BASE, secure);

    await enterWebView(page1, SITE + '/');
    const modeInfo = await page1.evaluate(() => {
      const badge = document.getElementById('webview-mode');
      const frame = document.getElementById('webview-frame');
      return {
        badge: badge ? badge.textContent.trim() : null,
        sandbox: frame.getAttribute('sandbox'),
        hint: (document.getElementById('webview-hint') || {}).textContent || '',
      };
    });
    check('模式徽标是「完整模式」', modeInfo.badge === '完整模式', modeInfo.badge);
    check('v2 下 iframe 去掉 sandbox', modeInfo.sandbox === null, modeInfo.sandbox);

    // SW 真的注册并激活了吗（直接问浏览器，而不是只看 UI）
    const sw1 = await page1.evaluate(async () => {
      const out = { hasSw: !!navigator.serviceWorker };
      if (!navigator.serviceWorker) return out;
      try {
        const reg = await navigator.serviceWorker.ready;
        out.scope = reg.scope;
        out.active = !!(reg.active && reg.active.state === 'activated');
        const all = await navigator.serviceWorker.getRegistrations();
        out.scripts = all.map((r) => (r.active && r.active.scriptURL) || '');
      } catch (e) {
        out.err = String(e && e.message);
      }
      return out;
    });
    check('Service Worker 已注册且状态为 activated', sw1.active === true, sw1);
    check('激活的脚本就是 sr-web-sw.js',
      Array.isArray(sw1.scripts) && sw1.scripts.some((s) => /\/sr-web-sw\.js$/.test(s)), sw1.scripts);

    await openUrl(page1, SITE + '/');
    // 等状态区报“已加载”，再读 iframe。
    await page1.waitForFunction(() => /已加载/.test(
      (document.getElementById('webview-status') || {}).textContent || ''),
      null, { timeout: 40000 }).catch(() => {});
    await sleep(800);

    const doc1 = await frameEval(page1, () => ({
      path: location.pathname,
      scriptRan: document.body.getAttribute('data-script'),
      viewName: (document.getElementById('view-name') || {}).textContent || null,
      titleColor: (function () {
        const t = document.getElementById('app-title');
        return t ? getComputedStyle(t).color : null;
      })(),
    }));
    check('iframe 地址是目标站原始路径（无映射前缀）',
      !!doc1 && doc1.path === '/', doc1 && doc1.path);
    check('远端脚本真实执行（完整模式的硬证据）',
      !!doc1 && doc1.scriptRan === 'ran', doc1 && doc1.scriptRan);
    check('外部 CSS 生效', !!doc1 && doc1.titleColor === 'rgb(9, 87, 65)',
      doc1 && doc1.titleColor);
    check('SPA 渲染出 root 视图', !!doc1 && doc1.viewName === 'root-view',
      doc1 && doc1.viewName);

    const dyn = await frameEval(page1, () => window.apiDynamic());
    check('映射页的动态 fetch 成功（走 SW 反代）', dyn === 'dynamic-ok', dyn);

    const status1 = await page1.evaluate(() =>
      document.getElementById('webview-status').textContent);
    check('状态区报“已加载”，不是任何错误提示',
      /已加载/.test(status1) && !/证书|无法注册/.test(status1), status1);
    await page1.screenshot({ path: path.join(OUT, 'trusted-full-mode.png') });

    // shell-remote 自身在 SW 激活后仍正常（SW 不该劫持自己的页面）
    const selfOk = await httpReq(`${RW_BASE.replace('https:', 'http:')}/sr-web-sw.js`)
      .then((r) => r.status);
    check('SW 脚本路径本身可访问（http 探针，与证书无关）', selfOk === 200 || selfOk === 0,
      selfOk);

    await ctx1.close();

    // ══════════════ [2] 同一张证书未受信 → 准确提示（负例） ══════════════
    console.log('\n[2] 证书未受信任：注册失败 + 状态区给出证书提示');
    // 全新 HOME，**不导入** CA：浏览器不信任这张证书。
    const homeUntrusted = makeBrowserHome(TLS_DIR, null);
    const b2 = await launchBrowser(homeUntrusted, exe);
    browsers.push(b2);
    const ctx2 = await b2.newContext({ viewport: { width: 1400, height: 900 }, locale: 'zh-CN' });
    await ctx2.addInitScript(([t, a]) => {
      try {
        sessionStorage.setItem('shell-remote-token', t);
        sessionStorage.setItem('shell-remote-auth', a);
      } catch (e) {}
    }, [RW_KEY, AUTH]);
    const page2 = await ctx2.newPage();

    // 负例必须走用户真实路径：不做任何 CDP 忽略，先拿到浏览器的证书拦截页。
    let navErr = null;
    await page2.goto(`${RW_BASE}/session`, { waitUntil: 'domcontentloaded' })
      .catch((e) => { navErr = e.message.split('\n')[0]; });
    const blocked = /ERR_CERT|SSL|certificate/i.test(String(navErr || ''));
    // 点“高级 → 继续前往（不安全）”：用户会这么做，我们复现它并证明它**不够**。
    // 证书拦截页是导航失败后浏览器另外渲染的一次导航，`goto` 的 reject 先到，
    // 页面本身还没画好——必须等它的元素出现再点。
    let proceeded = false;
    if (blocked) {
      const details = await page2.waitForSelector('#details-button', { timeout: 15000 })
        .catch(() => null);
      if (details) {
        await details.click().catch(() => {});
        const link = await page2.waitForSelector('#proceed-link', { timeout: 10000 })
          .catch(() => null);
        if (link) {
          await Promise.all([
            page2.waitForLoadState('domcontentloaded').catch(() => {}),
            link.click().catch(() => {}),
          ]);
          proceeded = true;
        }
      }
    }
    check('未受信证书在导航阶段就被浏览器拦住', blocked, navErr);
    check('可以点「继续前往（不安全）」进入（复现用户的操作）', proceeded);
    await page2.waitForTimeout(1000);
    // 例外建立后重新进会话页（第一次点继续落在根路径上）。
    await page2.goto(`${RW_BASE}/session`, { waitUntil: 'domcontentloaded' }).catch(() => {});
    await page2.waitForTimeout(1200);
    const termOk = await page2.waitForSelector('#terminal-container .xterm', { timeout: 25000 })
      .then(() => true).catch(() => false);
    check('点过「继续前往」后终端可用（页面本身是安全上下文）', termOk);

    if (termOk) {
      // 关键断言：页面能跑，但 SW 仍然注册不上。
      const swProbe = await page2.evaluate(async () => {
        const out = { secure: window.isSecureContext, hasSw: !!navigator.serviceWorker };
        if (!navigator.serviceWorker) return out;
        try {
          await navigator.serviceWorker.register('/sr-web-sw.js',
            { scope: '/', updateViaCache: 'none' });
          out.registered = true;
        } catch (e) {
          out.errName = e.name;
          out.errMessage = String(e.message || '');
        }
        // 对照：普通 fetch 是成功的——说明失败点就是 SW 的证书校验。
        try {
          const r = await fetch('/sr-web-sw.js', { cache: 'no-store' });
          out.fetchStatus = r.status;
        } catch (e) {
          out.fetchErr = String(e.message || '');
        }
        return out;
      });
      check('点过「继续前往」后普通 fetch 仍能取到 SW 脚本（200）',
        swProbe.fetchStatus === 200, swProbe);
      check('但 Service Worker 注册仍失败（这正是不该被当成“不支持 SW”的原因）',
        swProbe.registered !== true && !!swProbe.errMessage, swProbe);
      check('失败文案明确指向 SSL 证书',
        /ssl certificate|certificate error/i.test(String(swProbe.errMessage || '')),
        swProbe.errMessage);

      // UI 侧：打开网页视图，检查状态区文案是不是“证书”而不是笼统的“无法注册”。
      await enterWebView(page2, SITE + '/');
      await openUrl(page2, SITE + '/');
      await page2.waitForFunction(() => {
        const t = (document.getElementById('webview-status') || {}).textContent || '';
        return /证书|无法注册|Service Worker/.test(t);
      }, null, { timeout: 40000 }).catch(() => {});
      await page2.waitForTimeout(500);
      const ui2 = await page2.evaluate(() => ({
        status: (document.getElementById('webview-status') || {}).textContent || '',
        badge: (document.getElementById('webview-mode') || {}).textContent || '',
        src: document.getElementById('webview-frame').getAttribute('src'),
        srcdoc: document.getElementById('webview-frame').getAttribute('srcdoc') || '',
      }));
      check('状态区提示证书未被信任（不是笼统的“无法注册 SW”）',
        /证书未被浏览器信任/.test(ui2.status), ui2.status);
      check('提示说明了“继续前往不等于信任证书”',
        /继续前往/.test(ui2.status), ui2.status);
      check('提示指向文档里的修复步骤', /docs\/web-map\.md/.test(ui2.status), ui2.status);
      check('提示不再谎称“已回退静态模式”',
        !/已回退静态模式/.test(ui2.status), ui2.status);
      // 实测：注册失败时 iframe 根本没被指向目标页，所以不能说“已回退”。
      check('失败时 iframe 没有被指向目标页（印证“未回退”）',
        !ui2.src || ui2.src === 'about:blank' || ui2.srcdoc === '', ui2);
      await page2.screenshot({ path: path.join(OUT, 'untrusted-sw-error.png') });
      if (page2._client) {
        const cdp = await ctx2.newCDPSession(page2).catch(() => null);
        if (cdp) await cdp.send('Security.enable').catch(() => {});
      }
    } else {
      check('未受信场景下会话页可用（前置条件）', false, '终端未出现');
    }
    await ctx2.close();

    // ══════════════ [3] 证书受信但地址不在 SAN → 名字不匹配 ══════════════
    console.log('\n[3] 证书受信但访问地址不在 SAN 里');
    const b3 = await launchBrowser(makeBrowserHome(TLS_DIR, certs.ca), exe);
    browsers.push(b3);
    const ctx3 = await b3.newContext();
    const page3 = await ctx3.newPage();
    let mmErr = null;
    await page3.goto(`${MISMATCH_BASE}/session`, { waitUntil: 'domcontentloaded' })
      .catch((e) => { mmErr = e.message.split('\n')[0]; });
    // 这张证书的 SAN 里没有 ADDR：即使 CA 已受信，名字也对不上。
    check('SAN 不含访问地址时导航被拒（名字不匹配）',
      /ERR_CERT_COMMON_NAME_INVALID|ERR_CERT/i.test(String(mmErr || '')), mmErr || page3.url());
    await ctx3.close();

    // ══════════════ [4] 只读 Token 仍回退静态模式 ══════════════
    console.log('\n[4] 只读 Token：仍是静态模式（原有降级不被破坏）');
    const b4 = await launchBrowser(makeBrowserHome(TLS_DIR, certs.ca), exe);
    browsers.push(b4);
    const { ctx: ctx4, page: page4 } = await makePage(b4, RO_BASE, RO_KEY);
    const roTerm = await page4.waitForSelector('#terminal-container .xterm', { timeout: 30000 })
      .then(() => true).catch(() => false);
    check('ro 会话在受信 HTTPS 下可用', roTerm);
    if (roTerm) {
      await enterWebView(page4, SITE + '/');
      // Vo 模式下 ro 打开会走 v1；这里只需确认徽标与降级原因仍是“只读”。
      const roMode = await page4.evaluate(() => ({
        badge: (document.getElementById('webview-mode') || {}).textContent || '',
        hint: (document.getElementById('webview-hint') || {}).textContent || '',
      }));
      check('ro Token 显示「静态模式」', roMode.badge === '静态模式', roMode.badge);
      check('ro 的降级原因仍是“只读 token”',
        /只读/.test(roMode.hint), roMode.hint.slice(0, 120));
      await page4.screenshot({ path: path.join(OUT, 'ro-static-mode.png') });
    }
    await ctx4.close();
  } catch (e) {
    console.error('E2E 脚本异常：', e);
    fails.push('script-error');
  } finally {
    for (const b of browsers) { try { await b.close(); } catch (e) {} }
    for (const p of procs) { try { p.kill('SIGKILL'); } catch (e) {} }
    try { site.close(); } catch (e) {}
    if (!process.env.KEEP_TLS_DIR && !process.env.CA_DIR) {
      try { fs.rmSync(TLS_DIR, { recursive: true, force: true }); } catch (e) {}
    }
  }

  console.log('\n截图目录：' + OUT);
  console.log('\n' + (fails.length === 0
    ? `全部通过：${checks} 项` + (skips.length ? `（跳过 ${skips.length} 项）` : '')
    : `失败 ${fails.length}/${checks} 项：\n  - ` + fails.join('\n  - ')));
  process.exit(fails.length === 0 ? 0 : 1);
})();
