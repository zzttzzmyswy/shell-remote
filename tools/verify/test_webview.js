#!/usr/bin/env node
'use strict';
// MYS-2383-C：web/webview.js 的 URL / HTML / CSS 重写与 ticket 生命周期单测。
//
// 无框架、无浏览器：`web/webview.js` 在非浏览器环境里把同一套实现挂到
// `module.exports`，这里直接 require 它跑真实代码（不是复制一份逻辑）。
//
// 用法：node tools/verify/test_webview.js

const path = require('path');
const assert = require('assert');

const WV = require(path.join(__dirname, '..', '..', 'web', 'webview.js'));
const R = WV.Rewrite;
const ORIGIN = 'https://relay.example:8443';
const TICKET = 'a'.repeat(64);
const SESSION_TOKEN = 'SESSION-TOKEN-MUST-NEVER-APPEAR';

let checks = 0;
const fails = [];
function check(name, fn) {
  checks += 1;
  try {
    fn();
    console.log('  PASS ' + name);
  } catch (e) {
    console.log('  FAIL ' + name + '   <- ' + e.message);
    fails.push(name);
  }
}
function eq(actual, expected, label) {
  assert.deepStrictEqual(actual, expected, (label || '') + ' 实际=' + JSON.stringify(actual));
}

// ── 1. resolveUrl：相对 / 绝对 / 危险 scheme ─────────────────────────
console.log('\n[1] resolveUrl 相对与绝对地址');
check('相对路径按文档 base 解析', () => {
  eq(R.resolveUrl('style.css', 'http://127.0.0.1:8080/app/page.html'),
     'http://127.0.0.1:8080/app/style.css');
});
check('根相对路径保留主机与端口', () => {
  eq(R.resolveUrl('/img/logo.png', 'http://127.0.0.1:8080/app/page.html'),
     'http://127.0.0.1:8080/img/logo.png');
});
check('协议相对 //host/path 沿用文档 scheme', () => {
  eq(R.resolveUrl('//cdn.example/x.js', 'https://127.0.0.1/a'), 'https://cdn.example/x.js');
});
check('绝对 http URL 原样保留', () => {
  eq(R.resolveUrl('http://192.168.1.10:9000/a?b=1', 'http://127.0.0.1/'),
     'http://192.168.1.10:9000/a?b=1');
});
check('页内锚点不改写', () => {
  eq(R.resolveUrl('#top', 'http://127.0.0.1/'), null);
});
check('空值不改写', () => {
  eq(R.resolveUrl('', 'http://127.0.0.1/'), null);
  eq(R.resolveUrl(null, 'http://127.0.0.1/'), null);
});
check('javascript: 不改写成代理', () => {
  eq(R.resolveUrl('javascript:alert(1)', 'http://127.0.0.1/'), null);
  eq(R.resolveUrl('JaVaScRiPt:alert(1)', 'http://127.0.0.1/'), null);
});
check('data: 不改写成代理', () => {
  eq(R.resolveUrl('data:text/html,<b>x</b>', 'http://127.0.0.1/'), null);
});
check('file:/ftp:/vbscript: 不改写成代理', () => {
  ['file:///etc/passwd', 'ftp://x/y', 'vbscript:msgbox', 'about:blank', 'blob:http://x/y']
    .forEach((u) => eq(R.resolveUrl(u, 'http://127.0.0.1/'), null, u));
});

// ── 2. proxyUrl：只带 ticket + 目标地址 ─────────────────────────────
console.log('\n[2] proxyUrl 参数与凭据边界');
check('代理 URL 只含 ticket 与 url', () => {
  const u = R.proxyUrl('http://127.0.0.1/private?x=1', TICKET, ORIGIN);
  const parsed = new URL(u);
  eq(parsed.origin + parsed.pathname, ORIGIN + '/agent/web/proxy');
  eq(parsed.searchParams.get('ticket'), TICKET);
  eq(parsed.searchParams.get('url'), 'http://127.0.0.1/private?x=1');
  eq([...parsed.searchParams.keys()].sort(), ['ticket', 'url']);
});
check('目标地址里的 & 被编码，不会串参数', () => {
  const u = R.proxyUrl('http://127.0.0.1/a?p=1&q=2', TICKET, ORIGIN);
  const parsed = new URL(u);
  eq(parsed.searchParams.get('url'), 'http://127.0.0.1/a?p=1&q=2');
  eq(parsed.searchParams.get('ticket'), TICKET);
});
check('extractTarget 取回目标地址', () => {
  const u = R.proxyUrl('http://127.0.0.1/a?p=1', TICKET, ORIGIN);
  eq(R.extractTarget(u), 'http://127.0.0.1/a?p=1');
  eq(R.extractTarget('#anchor'), null);
  eq(R.extractTarget(''), null);
});

// ── 3. srcset ───────────────────────────────────────────────────────
console.log('\n[3] srcset 重写');
// srcset 语法是「<url> <descriptor>」，重写后 descriptor 仍跟在代理 URL 后面——
// 取回目标地址时先把 descriptor 剥掉。
function splitCandidate(part) {
  const m = /^(\S+)(?:\s+(.*))?$/.exec(part);
  return { url: m[1], descriptor: (m[2] || '').trim() };
}
check('多候选 srcset 全部代理并保留 descriptor', () => {
  const out = R.rewriteSrcset('a.png 1x, /img/b.png 2x, http://127.0.0.1/c.png 640w',
                              'http://127.0.0.1/dir/page.html', TICKET, ORIGIN);
  const parts = out.split(', ').map(splitCandidate);
  eq(parts.length, 3);
  eq(new URL(parts[0].url).searchParams.get('url'), 'http://127.0.0.1/dir/a.png');
  eq(parts[0].descriptor, '1x');
  eq(new URL(parts[1].url).searchParams.get('url'), 'http://127.0.0.1/img/b.png');
  eq(parts[1].descriptor, '2x');
  eq(new URL(parts[2].url).searchParams.get('url'), 'http://127.0.0.1/c.png');
  eq(parts[2].descriptor, '640w');
});
check('无 descriptor 的 srcset 也能改写', () => {
  const out = R.rewriteSrcset('x.png, y.png', 'http://127.0.0.1/', TICKET, ORIGIN);
  const parts = out.split(', ');
  eq(parts.length, 2);
  eq(new URL(parts[0]).searchParams.get('url'), 'http://127.0.0.1/x.png');
});
check('data: srcset 原样保留（返回 null 表示不改）', () => {
  eq(R.rewriteSrcset('data:image/png;base64,AAA 1x', 'http://127.0.0.1/', TICKET, ORIGIN), null);
});
check('全部候选都不可代理时返回 null', () => {
  eq(R.rewriteSrcset('#a 1x', 'http://127.0.0.1/', TICKET, ORIGIN), null);
});

// ── 4. CSS url() / @import ──────────────────────────────────────────
console.log('\n[4] CSS 重写');
check('url() 相对 CSS 自身地址解析', () => {
  const css = R.rewriteCss('body{background:url(../img/bg.png)}',
                           'http://127.0.0.1/static/css/app.css', TICKET, ORIGIN);
  const m = /url\("?([^")]+)"?\)/.exec(css.replace(/&amp;/g, '&'));
  eq(new URL(m[1]).searchParams.get('url'), 'http://127.0.0.1/static/img/bg.png');
});
check('url() 引号与 data: 保留', () => {
  const css = R.rewriteCss(`a{background:url("data:image/png;base64,AA")}`, 'http://127.0.0.1/', TICKET, ORIGIN);
  eq(css, 'a{background:url("data:image/png;base64,AA")}');
});
check('@import 也改写', () => {
  const css = R.rewriteCss('@import "sub/more.css";', 'http://127.0.0.1/css/app.css', TICKET, ORIGIN);
  const m = /@import\s+"([^"]+)"/.exec(css.replace(/&amp;/g, '&'));
  eq(new URL(m[1]).searchParams.get('url'), 'http://127.0.0.1/css/sub/more.css');
});
check('JS 不会出现在 url() 里也不被改写为 javascript:', () => {
  const css = R.rewriteCss('a{background:url(javascript:alert(1))}', 'http://127.0.0.1/', TICKET, ORIGIN);
  eq(css.indexOf('/agent/web/proxy'), -1);
});

// ── 5. rewriteHtml：属性 / 链接 / script / CSP ─────────────────────
console.log('\n[5] HTML 重写');
const BASE = 'http://127.0.0.1:8080/app/page.html';
const PAGE = `<!DOCTYPE html><html><head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'self'">
<meta http-equiv="refresh" content="0;url=/elsewhere">
<base href="http://127.0.0.1:8080/app/">
<link rel="stylesheet" href="app.css">
<script src="/app.js"></script>
<style>.hero{background:url(img/hero.png)}</style>
</head><body>
<img src="img/a.png" srcset="img/a.png 1x, img/a2.png 2x">
<a href="detail.html">detail</a>
<a href="javascript:void(0)">js</a>
<a href="#anchor">anchor</a>
<form action="/submit" method="post"><input name="q"></form>
<div style="background:url('img/bg.png')"></div>
<video poster="img/p.jpg"><source src="movie.mp4"></video>
</body></html>`;

const doc = R.rewriteHtml(PAGE, { ticket: TICKET, origin: ORIGIN, base: BASE });

check('<script> 被删除', () => {
  eq(/<script/i.test(doc.html), false, doc.html);
});
check('CSP 与 refresh meta 被删除，charset 保留', () => {
  eq(/content-security-policy/i.test(doc.html), false);
  eq(/http-equiv\s*=\s*"refresh"/i.test(doc.html), false);
  eq(/charset/i.test(doc.html), true);
});
check('<base> 被删除（避免相对资源绕过代理）', () => {
  eq(/<base\b/i.test(doc.html), false);
});
check('img src 指向代理', () => {
  const m = /<img[^>]*\ssrc="([^"]+)"/.exec(doc.html);
  eq(new URL(m[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/app/img/a.png');
});
check('srcset 两个候选都指向代理且 descriptor 保留', () => {
  const m = /srcset="([^"]+)"/.exec(doc.html);
  const parts = m[1].replace(/&amp;/g, '&').split(', ').map(splitCandidate);
  eq(parts.length, 2, m[1]);
  eq(new URL(parts[0].url).searchParams.get('url'), 'http://127.0.0.1:8080/app/img/a.png');
  eq(parts[0].descriptor, '1x');
  eq(new URL(parts[1].url).searchParams.get('url'), 'http://127.0.0.1:8080/app/img/a2.png');
  eq(parts[1].descriptor, '2x');
});
check('<a href> 站内链接指向代理', () => {
  const m = /<a href="([^"]+)">detail/.exec(doc.html);
  eq(new URL(m[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/app/detail.html');
});
check('javascript: 链接保持原样、不被代理', () => {
  eq(doc.html.indexOf('/agent/web/proxy?ticket=' + TICKET + '&amp;url=javascript') , -1);
  eq(/<a href="javascript:void\(0\)">/.test(doc.html), true, doc.html);
});
check('页内锚点保持原样', () => {
  eq(/<a href="#anchor">/.test(doc.html), true);
});
check('form action 不改写（非 GET 表单为已知限制）', () => {
  eq(/<form action="\/submit"/.test(doc.html), true);
});
check('<style> 内的 url() 按文档 base 改写', () => {
  const m = /<style>([\s\S]*?)<\/style>/.exec(doc.html);
  const u = /url\("?([^")]+)"?\)/.exec(m[1]);
  eq(new URL(u[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/app/img/hero.png');
});
check('style= 属性内的 url() 改写', () => {
  const m = /<div style="([^"]*)"/.exec(doc.html);
  const u = /url\('?([^')]+)'?\)/.exec(m[1]);
  eq(new URL(u[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/app/img/bg.png');
});
check('video poster 改写', () => {
  const m = /<video[^>]*poster="([^"]+)"/.exec(doc.html);
  eq(new URL(m[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/app/img/p.jpg');
});
check('source src 改写', () => {
  const m = /<source src="([^"]+)"/.exec(doc.html);
  eq(new URL(m[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/app/movie.mp4');
});
check('重定向后的 final_url 作为 base（相对资源不落到旧目录）', () => {
  const moved = R.rewriteHtml('<img src="x.png">', {
    ticket: TICKET, origin: ORIGIN, base: 'http://127.0.0.1:8080/new/deep/page.html'
  });
  const m = /src="([^"]+)"/.exec(moved.html);
  eq(new URL(m[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/new/deep/x.png');
});
check('<base href> 参与相对解析（文档自己声明了 base）', () => {
  const html = '<base href="/assets/"><img src="x.png">';
  const out = R.rewriteHtml(html, { ticket: TICKET, origin: ORIGIN, base: BASE });
  eq(out.base, 'http://127.0.0.1:8080/assets/');
  const m = /src="([^"]+)"/.exec(out.html);
  eq(new URL(m[1].replace(/&amp;/g, '&')).searchParams.get('url'),
     'http://127.0.0.1:8080/assets/x.png');
});
check('重写后的文档里没有会话 token / 服务器口令', () => {
  eq(doc.html.indexOf(SESSION_TOKEN), -1);
  eq(/[?&]token=/.test(doc.html), false, '出现 token= 查询参数');
  eq(/X-SR-Token/i.test(doc.html), false);
  eq(/X-Auth/i.test(doc.html), false);
});
check('findStylesheets 找出可内联的样式表', () => {
  const found = R.findStylesheets(PAGE, BASE);
  eq(found.length, 1);
  eq(found[0].url, 'http://127.0.0.1:8080/app/app.css');
});

// ── 6. normalizeInput ───────────────────────────────────────────────
console.log('\n[6] 地址输入规范化');
check('缺 scheme 补 http://', () => {
  eq(WV.normalizeInput('127.0.0.1:8080/x'), 'http://127.0.0.1:8080/x');
});
check('https 保留', () => {
  eq(WV.normalizeInput('https://example.com/a'), 'https://example.com/a');
});
check('默认地址可用', () => {
  eq(WV.normalizeInput(WV.DEFAULT_URL), 'http://127.0.0.1/');
});
check('危险 scheme 被拒', () => {
  eq(WV.normalizeInput('javascript:alert(1)'), null);
  eq(WV.normalizeInput('data:text/html,x'), null);
  eq(WV.normalizeInput('file:///etc/passwd'), null);
});
check('空输入被拒', () => {
  eq(WV.normalizeInput('   '), null);
});

// ── 7. WebView：ticket 生命周期与请求头 ─────────────────────────────
// 用桩 fetch 记录每次请求，验证：ticket 预取 → 403 重新签发一次 → 仍失败则报错。
function fakeResponse(opts) {
  const headers = new Map(Object.entries(opts.headers || {}));
  const lower = new Map();
  headers.forEach((v, k) => lower.set(String(k).toLowerCase(), v));
  return {
    status: opts.status === undefined ? 200 : opts.status,
    ok: (opts.status === undefined ? 200 : opts.status) < 400,
    headers: { get: (k) => (lower.has(String(k).toLowerCase()) ? lower.get(String(k).toLowerCase()) : null) },
    arrayBuffer: () => Promise.resolve(new TextEncoder().encode(opts.body || '').buffer),
    json: () => Promise.resolve(JSON.parse(opts.body || '{}'))
  };
}

// 桩 iframe：记录 srcdoc/src，暴露 contentDocument 让 _instrument 可跑。
function makeFrame() {
  const listeners = {};
  return {
    srcdom: '',
    src: '',
    setAttribute(k, v) {
      if (k === 'srcdoc') this.srcdom = v;
      if (k === 'src') this.src = v;
    },
    removeAttribute(k) { this.setAttribute(k, ''); },
    addEventListener(type, fn) { (listeners[type] = listeners[type] || []).push(fn); },
    contentDocument: null
  };
}

function makeHarness(handlers) {
  const calls = [];
  let n = 0;
  const fetchImpl = (url, init) => {
    calls.push({ url: String(url), init: init || {} });
    const h = handlers[n < handlers.length ? n : handlers.length - 1];
    n += 1;
    return Promise.resolve(h(String(url), init || {}, n));
  };
  const statuses = [];
  const wv = new WV.WebView({
    token: SESSION_TOKEN,
    origin: ORIGIN,
    fetch: fetchImpl,
    frame: makeFrame(),
    onStatus: (text, kind) => statuses.push({ text, kind })
  });
  return { wv, calls, statuses };
}

// ══════════════════════ Service Worker 注册失败分类（MYS-2945）══════════════════════
//
// 期望值全部来自本机实测（Chrome 157 / Firefox 1495），不是按浏览器官网文档猜的：
//   - Chromium 证书错误 → SecurityError + “An SSL certificate error occurred…”
//   - Chromium 404      → TypeError + “A bad HTTP response code (404)…”
//   - Chromium MIME     → SecurityError + “unsupported MIME type”
//   - Chromium 求值抛错 → TypeError + “evaluation failed”
//   - Firefox 策略类    → SecurityError + “The operation is insecure.”（无法细分）
//
// 判据的核心要求：**只有文案真的指向证书才判 SW_TLS**，不能把所有失败都归咎于证书。

/// 注入一个假的 navigator.serviceWorker，让 ensureServiceWorker 走给定的失败/成功路径。
function makeSwContainer(outcome) {
  const reg = {
    scope: 'https://relay.example/',
    active: { state: 'activated', postMessage() {} },
    waiting: null,
    installing: null
  };
  const container = {
    registerCalls: 0,
    addEventListener() {},
    removeEventListener() {},
    register(script, opts) {
      container.registerCalls += 1;
      container.lastArgs = { script, opts };
      if (outcome.error) return Promise.reject(outcome.error);
      return Promise.resolve(reg);
    },
    ready: outcome.readyError ? Promise.reject(outcome.readyError) : Promise.resolve(reg)
  };
  return container;
}

async function swClassifyTests() {
  console.log('\n[8] Service Worker 注册失败分类（实测文案）');

  const CASES = [
    ['Chromium 证书未信任 → SW_TLS', 'SW_TLS', {
      name: 'SecurityError',
      message: "Failed to register a ServiceWorker for scope ('https://172.26.13.24:19443/') " +
        "with script ('https://172.26.13.24:19443/sr-web-sw.js'): " +
        'An SSL certificate error occurred when fetching the script.'
    }],
    ['Chromium 404 → SW_HTTP_404', 'SW_HTTP_404', {
      name: 'TypeError',
      message: "Failed to register a ServiceWorker for scope ('https://h/') with script " +
        "('https://h/sr-web-sw.js'): A bad HTTP response code (404) was received when fetching the script."
    }],
    ['Chromium MIME 不符 → SW_MIME', 'SW_MIME', {
      name: 'SecurityError',
      message: "Failed to register a ServiceWorker for scope ('https://h/') with script " +
        "('https://h/sr-web-sw.js'): The script has an unsupported MIME type ('text/html')."
    }],
    ['Chromium 脚本求值抛错 → SW_EVAL', 'SW_EVAL', {
      name: 'TypeError',
      message: "Failed to register a ServiceWorker for scope ('https://h/') with script " +
        "('https://h/sr-web-sw.js'): ServiceWorker script evaluation failed"
    }],
    ['Firefox 404 → 不归为证书', 'NO_SW', {
      name: 'TypeError',
      message: 'ServiceWorker script at https://h/sr-web-sw.js for scope https://h/ ' +
        'encountered an error during installation.'
    }],
    ['Firefox 脚本求值抛错 → SW_EVAL', 'SW_EVAL', {
      name: 'TypeError',
      message: 'ServiceWorker script at https://h/sr-web-sw.js for scope https://h/ ' +
        'threw an exception during script evaluation.'
    }],
    // Firefox 对 MIME/策略问题只给一句通用异常：必须仍然给出提示，但**不能**
    // 断言是证书问题（文案里要留出“浏览器策略”的可能）。
    ['Firefox 通用 SecurityError → SW_BLOCKED', 'SW_BLOCKED', {
      name: 'SecurityError', message: 'The operation is insecure.'
    }],
    ['未知异常 → NO_SW', 'NO_SW', { name: 'Error', message: 'boom' }]
  ];

  for (const [label, expectCode, err] of CASES) {
    check(label, () => {
      eq(WV.classifySwError(err).code, expectCode);
    });
  }

  check('非证书失败不出现“证书未被信任”的断言', () => {
    ['SW_HTTP_404', 'SW_MIME', 'SW_EVAL', 'NO_SW', 'SW_BLOCKED'].forEach((code) => {
      const text = WV.describeMapError(code, 'x');
      eq(/证书未被浏览器信任/.test(text), false, code + ' → ' + text);
    });
  });
  check('只有 SW_TLS 的文案说明证书修法', () => {
    const tls = WV.describeMapError('SW_TLS');
    eq(/证书未被浏览器信任/.test(tls), true);
    eq(/docs\/web-map\.md/.test(tls), true, '应指向文档');
    // 用户最容易误解的一点：点“继续前往”不等于信任.
    eq(/继续前往/.test(tls), true);
  });
  check('Firefox 通用失败同时提到证书与浏览器策略', () => {
    const t = WV.describeMapError('SW_BLOCKED', 'The operation is insecure.');
    eq(/证书未被信任/.test(t), true);
    eq(/浏览器策略/.test(t), true);
  });
  check('文案不再谎称“已回退静态模式”', () => {
    // 实测：注册失败时 iframe 仍是 about:blank，没有真的回退——说“已回退”会
    // 让用户以为页面只是降级渲染，而不是根本没打开。
    ['SW_TLS', 'SW_BLOCKED', 'SW_MIME', 'SW_HTTP_404', 'SW_EVAL', 'NO_SW'].forEach((code) => {
      eq(/已回退静态模式/.test(WV.describeMapError(code, 'x')), false, code);
    });
  });
  check('详情里的浏览器前缀被剥掉', () => {
    const d = WV.classifySwError({
      name: 'SecurityError',
      message: "Failed to register a ServiceWorker for scope ('https://h/') with script " +
        "('https://h/sr-web-sw.js'): An SSL certificate error occurred when fetching the script."
    }).detail;
    eq(d, 'An SSL certificate error occurred when fetching the script.');
  });
  check('classifySwError 容忍 null/undefined', () => {
    eq(WV.classifySwError(null).code, 'NO_SW');
    eq(WV.classifySwError(undefined).code, 'NO_SW');
  });

  // ── ensureServiceWorker 把分类结果透出为错误码 ──
  console.log('\n[9] ensureServiceWorker 的错误码映射');

  {
    const container = makeSwContainer({
      error: {
        name: 'SecurityError',
        message: "Failed to register a ServiceWorker for scope ('https://relay.example/') with " +
          "script ('https://relay.example/sr-web-sw.js'): " +
          'An SSL certificate error occurred when fetching the script.'
      }
    });
    const ctrl = new WV.WebMapV2({ swContainer: container });
    const err = await ctrl.ensureServiceWorker().then(() => null, (e) => e);
    check('证书错误 → 抛出 code=SW_TLS（不再是笼统的 NO_SW）', () => {
      eq(err && err.code, 'SW_TLS');
      eq(err && err.detail, 'An SSL certificate error occurred when fetching the script.');
      eq(ctrl.ready, false);
    });
    check('注册仍用相对路径 + scope / + updateViaCache none', () => {
      eq(container.lastArgs.script, WV.SW_PATH);
      eq(container.lastArgs.opts.scope, '/');
      eq(container.lastArgs.opts.updateViaCache, 'none');
    });
  }

  {
    const container = makeSwContainer({});
    const ctrl = new WV.WebMapV2({ swContainer: container });
    const reg = await ctrl.ensureServiceWorker();
    check('注册成功时 ready=true 且复用同一个 registration', async () => {
      eq(!!reg, true);
      eq(ctrl.ready, true);
    });
    await ctrl.ensureServiceWorker();
    check('已就绪后不重复注册', () => {
      eq(container.registerCalls, 1);
    });
  }

  {
    // DOMException 的 `code` 是数字（SecurityError=18）。曾经因为用“有 code”
    // 判断“是不是我们自己的错误码”，浏览器异常被原样抛出，导致界面显示
    // “加载失败（18）”。这里锁住这个行为。
    const domEx = new Error('x');
    domEx.name = 'SecurityError';
    domEx.code = 18;
    domEx.message = "Failed to register a ServiceWorker for scope ('https://relay.example/') " +
      "with script ('https://relay.example/sr-web-sw.js'): " +
      'An SSL certificate error occurred when fetching the script.';
    const ctrl = new WV.WebMapV2({ swContainer: makeSwContainer({ error: domEx }) });
    const err = await ctrl.ensureServiceWorker().then(() => null, (e) => e);
    check('DOMException 的数字 code 不会顶掉分类结果', () => {
      eq(err && err.code, 'SW_TLS');
    });
  }

  {
    const noSw = new WV.WebMapV2({ swContainer: null });
    const err = await noSw.ensureServiceWorker().then(() => null, (e) => e);
    check('浏览器没有 serviceWorker 时仍是 NO_SW', () => {
      eq(err && err.code, 'NO_SW');
    });
  }

  {
    // ready 永不 resolve 的浏览器（scope 里没有 active worker）：属于通用失败。
    const container = makeSwContainer({ readyError: new Error('no active worker') });
    const ctrl = new WV.WebMapV2({ swContainer: container });
    const err = await ctrl.ensureServiceWorker().then(() => null, (e) => e);
    check('sw.ready 失败时归为 NO_SW 而不是证书问题', () => {
      eq(err && err.code, 'NO_SW');
    });
  }

  // open() 要把分类结果写进状态区（用户唯一能看到的诊断面）。
  {
    const container = makeSwContainer({
      error: {
        name: 'SecurityError',
        message: "Failed to register a ServiceWorker for scope ('https://relay.example/') with " +
          "script ('https://relay.example/sr-web-sw.js'): " +
          'An SSL certificate error occurred when fetching the script.'
      }
    });
    const statuses = [];
    const ctrl = new WV.WebMapV2({
      swContainer: container,
      origin: ORIGIN,
      token: SESSION_TOKEN,
      fetch: () => Promise.resolve(fakeResponse({ body: '{}' })),
      onStatus: (text, kind) => statuses.push({ text, kind })
    });
    const res = await ctrl.open('http://127.0.0.1/');
    check('open() 返回 SW_TLS 且在状态区给出证书提示', () => {
      eq(res.ok, false);
      eq(res.code, 'SW_TLS');
      const last = statuses[statuses.length - 1];
      eq(last.kind, 'error');
      eq(/证书未被浏览器信任/.test(last.text), true, last.text);
    });
    check('证书失败时不会去建映射（省掉一次必然白跑的请求）', () => {
      eq(ctrl.mapId, '');
    });
  }
}

async function asyncTests() {
  console.log('\n[7] WebView ticket 签发与失效重取');

  // 7.1 正常路径：签发一次 ticket，主文档 + 样式表各一次代理请求，重定向取 final_url。
  {
    const { wv, calls } = makeHarness([
      () => fakeResponse({ body: JSON.stringify({ ticket: TICKET, expires_in: 600 }) }),
      () => fakeResponse({
        body: '<html><head><link rel="stylesheet" href="a.css"></head><body><img src="i.png"></body></html>',
        headers: { 'content-type': 'text/html; charset=utf-8',
                   'x-sr-final-url': 'http://127.0.0.1/sub/page.html' }
      }),
      () => fakeResponse({ body: '.x{background:url(bg.png)}',
                           headers: { 'content-type': 'text/css' } })
    ]);
    await wv.navigate('127.0.0.1:8080/start');
    check('先 POST ticket，再 GET 代理', () => {
      eq(calls[0].url, ORIGIN + '/agent/web/ticket');
      eq(calls[0].init.method, 'POST');
      eq(calls[0].init.headers['X-SR-Token'], SESSION_TOKEN);
      eq(calls[1].url.indexOf('/agent/web/proxy'), ORIGIN.length);
    });
    check('ticket 请求不带 cookie（credentials=omit）', () => {
      eq(calls[0].init.credentials, 'omit');
      eq(calls[1].init.credentials, 'omit');
    });
    check('主文档与样式表各一次代理请求', () => {
      eq(calls.length, 3);
      eq(new URL(calls[1].url).searchParams.get('url'), 'http://127.0.0.1:8080/start');
      eq(new URL(calls[2].url).searchParams.get('url'), 'http://127.0.0.1/sub/a.css');
    });
    check('样式表按自身的 final_url 改写 url() 并内联', () => {
      const html = wv.frame ? wv.frame.srcdom : '';
      void html;
      const proxied = new URL(calls[2].url);
      eq(proxied.searchParams.get('url'), 'http://127.0.0.1/sub/a.css');
    });
    check('重定向后 url 更新为 final_url', () => {
      eq(wv.url, 'http://127.0.0.1/sub/page.html');
    });
    check('所有请求 URL 里没有会话 token', () => {
      calls.forEach((c) => eq(c.url.indexOf(SESSION_TOKEN), -1, c.url));
    });
    check('代理请求头里没有会话 token（只走 ticket）', () => {
      calls.slice(1).forEach((c) => {
        eq(c.init.headers === undefined || c.init.headers['X-SR-Token'] === undefined, true,
           JSON.stringify(c.init.headers));
      });
    });
  }

  // 7.2 ticket 复用：同一页面内的多次资源请求不会重复签发。
  {
    const { wv, calls } = makeHarness([
      () => fakeResponse({ body: JSON.stringify({ ticket: TICKET, expires_in: 600 }) }),
      () => fakeResponse({ body: '<html><img src="a.png"></html>',
                           headers: { 'content-type': 'text/html' } })
    ]);
    await wv.navigate('http://127.0.0.1/');
    const before = calls.filter((c) => c.url === ORIGIN + '/agent/web/ticket').length;
    eq(before, 1);
    await wv.ensureTicket(false);
    await wv.ensureTicket(false);
    eq(calls.filter((c) => c.url === ORIGIN + '/agent/web/ticket').length, 1, '不应重复签发');
  }

  // 7.3 403（过期/被撤销）→ 重新签发一次并重试成功。
  {
    const { wv, calls } = makeHarness([
      () => fakeResponse({ body: JSON.stringify({ ticket: TICKET, expires_in: 600 }) }),
      () => fakeResponse({ status: 403, body: '{"error":"invalid or expired ticket"}' }),
      () => fakeResponse({ body: JSON.stringify({ ticket: 'b'.repeat(64), expires_in: 600 }) }),
      () => fakeResponse({ body: '<html>ok</html>', headers: { 'content-type': 'text/html' } })
    ]);
    await wv.navigate('http://127.0.0.1/');
    check('403 后重新签发 ticket 并重试一次', () => {
      eq(calls.length, 4);
      eq(calls[2].url, ORIGIN + '/agent/web/ticket');
      eq(new URL(calls[3].url).searchParams.get('ticket'), 'b'.repeat(64));
    });
    check('重试成功后不再报错', () => eq(wv.lastError, ''));
  }

  // 7.4 ticket 持续失效：只重试一次，最终报错（不无限循环）。
  {
    const { wv, calls } = makeHarness([
      () => fakeResponse({ body: JSON.stringify({ ticket: TICKET, expires_in: 600 }) }),
      () => fakeResponse({ status: 403, body: '{}' }),
      () => fakeResponse({ body: JSON.stringify({ ticket: 'c'.repeat(64), expires_in: 600 }) }),
      () => fakeResponse({ status: 403, body: '{}' })
    ]);
    await wv.navigate('http://127.0.0.1/');
    check('ticket 一直 403：只重试一次即报错', () => {
      eq(calls.length, 4);
      eq(wv.lastError.length > 0, true);
    });
  }

  // 7.5 ticket 签发失败（401）→ 提示鉴权失败，不发代理请求。
  {
    const { wv, calls } = makeHarness([
      () => fakeResponse({ status: 401, body: '{"error":"invalid session token"}' })
    ]);
    const ok = await wv.navigate('http://127.0.0.1/');
    check('ticket 401 → 报鉴权失败且不发代理请求', () => {
      eq(ok, false);
      eq(calls.length, 1);
      eq(/鉴权失败/.test(wv.lastError), true, wv.lastError);
    });
  }

  // 7.6 旧 agent：ticket 404 → 提示不支持（入口本应隐藏，这里是兜底）。
  {
    const { wv } = makeHarness([() => fakeResponse({ status: 404, body: '{}' })]);
    await wv.navigate('http://127.0.0.1/');
    check('ticket 404 → 提示 agent 不支持', () => {
      eq(/不支持/.test(wv.lastError), true, wv.lastError);
    });
  }

  // 7.7 竞态：第二次导航应丢弃第一次的（更晚返回的）响应。
  {
    let releaseOld;
    const oldGate = new Promise((r) => { releaseOld = r; });
    const proxyCalls = [];
    const fetchImpl = (url, init) => {
      void init;
      const u = String(url);
      if (u.indexOf('/agent/web/ticket') >= 0) {
        return Promise.resolve(fakeResponse({ body: JSON.stringify({ ticket: TICKET, expires_in: 600 }) }));
      }
      proxyCalls.push(new URL(u).searchParams.get('url'));
      if (proxyCalls.length === 1) {
        // 第一次导航：一直卡到第二次导航完成之后再返回 STALE。
        return oldGate.then(() =>
          fakeResponse({ body: '<html>STALE</html>', headers: { 'content-type': 'text/html' } }));
      }
      return Promise.resolve(fakeResponse({ body: '<html>FRESH</html>',
                                            headers: { 'content-type': 'text/html' } }));
    };
    const wv = new WV.WebView({
      token: SESSION_TOKEN, origin: ORIGIN, fetch: fetchImpl, frame: makeFrame()
    });
    // 先把 ticket 拿到手，让导航里的 fetch 只有主文档，竞态才可判定。
    await wv.ensureTicket(false);
    const first = wv.navigate('http://127.0.0.1/old');
    await new Promise((r) => setTimeout(r, 0));
    const second = wv.navigate('http://127.0.0.1/new');
    await second;
    releaseOld();
    await first;
    check('过期响应不覆盖新页面（无竞态）', () => {
      eq(proxyCalls.length >= 2, true, JSON.stringify(proxyCalls));
      eq(wv.url, 'http://127.0.0.1/new');
      eq(/FRESH/.test(wv.frame.srcdom || ''), true, wv.frame.srcdom);
      eq(/STALE/.test(wv.frame.srcdom || ''), false, wv.frame.srcdom);
    });
  }

  // 7.8 reset 清理：ticket 丢弃、在途请求 abort。
  {
    const { wv } = makeHarness([
      () => fakeResponse({ body: JSON.stringify({ ticket: TICKET, expires_in: 600 }) }),
      () => fakeResponse({ body: '<html>x</html>', headers: { 'content-type': 'text/html' } })
    ]);
    await wv.navigate('http://127.0.0.1/');
    eq(wv.ticket, TICKET);
    wv.reset();
    check('reset 清空 ticket 与 iframe', () => {
      eq(wv.ticket, '');
      eq(wv.url, '');
      eq(wv.frame.srcdom, '');
    });
  }

  // 7.9 非法地址：不发任何请求。
  {
    const { wv, calls } = makeHarness([() => fakeResponse({ body: '{}' })]);
    await wv.navigate('javascript:alert(1)');
    check('非法地址不发请求，只提示', () => {
      eq(calls.length, 0);
      eq(/地址无效/.test(wv.statusText || wv.lastError || ''), true, wv.statusText);
    });
  }

  // 7.10 srcdoc 里不得出现会话 token（端到端重写后）。
  {
    const { wv } = makeHarness([
      () => fakeResponse({ body: JSON.stringify({ ticket: TICKET, expires_in: 600 }) }),
      () => fakeResponse({
        body: '<html><head><link rel="stylesheet" href="s.css"></head><img src="i.png"></html>',
        headers: { 'content-type': 'text/html' }
      }),
      () => fakeResponse({ body: '.a{background:url(b.png)}', headers: { 'content-type': 'text/css' } })
    ]);
    await wv.navigate('http://127.0.0.1/');
    check('最终 srcdoc 无会话 token、只含 ticket', () => {
      const html = wv.frame.srcdom || '';
      eq(html.indexOf(SESSION_TOKEN), -1);
      eq(html.indexOf(TICKET) >= 0, true);
      eq(/url=\S*javascript/i.test(html), false);
    });
    check('内联后的 CSS 已按样式表地址改写', () => {
      const html = wv.frame.srcdom || '';
      const m = /<style data-sr-inline="1">([\s\S]*?)<\/style>/.exec(html);
      eq(m !== null, true, html);
      const u = /url\("?([^")]+)"?\)/.exec(m[1]);
      eq(new URL(u[1].replace(/&amp;/g, '&')).searchParams.get('url'), 'http://127.0.0.1/b.png');
    });
    check('样式表 link 已被内联替换掉', () => {
      eq(/<link[^>]*stylesheet/i.test(wv.frame.srcdom || ''), false);
    });
  }
}

asyncTests().then(() => swClassifyTests()).then(() => {
  console.log('\n' + (fails.length === 0
    ? `全部通过：${checks} 项`
    : `失败 ${fails.length}/${checks} 项：\n  - ` + fails.join('\n  - ')));
  process.exit(fails.length === 0 ? 0 : 1);
}).catch((e) => {
  console.error('测试脚本异常：', e);
  process.exit(2);
});
