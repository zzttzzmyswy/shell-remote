#!/usr/bin/env node
'use strict';
// MYS-2383-G 单测：网页映射 v2 的 Service Worker 与注入脚本的纯函数。
//
// 覆盖任务卡验收标准 2 的逐条：
//   - 非导航请求的去向（同源 / 已带前缀 / targetOrigin 绝对地址 / http→https
//     升级后的同 host / 他源放行 / 保留路径放行）；
//   - 导航前缀剥离（保留 query）；
//   - `<head>` 字节注入：`<head>`、`<HEAD lang=..>`、`</head>`、无 head 插最前、
//     GBK 字节逐字节不变、`<header>` 不算；
//   - WebSocket URL 改写（相对、同源、targetOrigin、他源不变、ws/wss 选择）；
//   - fetch/XHR/EventSource 的绝对地址改写；
//   - shim 的 Service Worker 防护（register 拒绝、枚举为空）。
//
// 用法：node tools/test_sr_web.js

const assert = require('node:assert');
const path = require('node:path');

const ROOT = path.dirname(path.dirname(require('node:fs').realpathSync(__filename)));
const sw = require(path.join(ROOT, 'web', 'sr-web-sw.js'));
const shim = require(path.join(ROOT, 'web', 'sr-web-shim.js'));

let checks = 0;
const fails = [];
function test(name, fn) {
  checks += 1;
  try {
    fn();
    console.log(`  PASS ${name}`);
  } catch (e) {
    console.log(`  FAIL ${name}\n         ${(e && e.message) || e}`);
    fails.push(name);
  }
}

const RELAY = 'https://sshx.example.com';
const MAP = 'a'.repeat(64);
const TARGET = 'http://192.168.0.1:8080';

// ══════════════════════ A. SW：非导航请求去向 ══════════════════════

console.log('\n[A] Service Worker：非导航请求 URL 判定');

const base = { origin: RELAY, mapId: MAP, targetOrigin: TARGET };

test('同源绝对 URL → 加前缀', () => {
  assert.strictEqual(
    sw.decideNonNav({ ...base, url: `${RELAY}/static/app.js` }), 'prefix');
});

test('同源已带前缀 → 原样请求（不重复加）', () => {
  assert.strictEqual(
    sw.decideNonNav({ ...base, url: `${RELAY}/agent/web/m/${MAP}/static/app.js` }), 'as-is');
});

test('targetOrigin 绝对地址 → 加前缀', () => {
  assert.strictEqual(
    sw.decideNonNav({ ...base, url: `${TARGET}/api/ping?v=1` }), 'prefix');
});

test('http→https 自动升级后的同 host → 加前缀', () => {
  // 浏览器把 http 升级成 https 后 host 仍是 192.168.0.1:8080。
  assert.strictEqual(
    sw.decideNonNav({ ...base, url: 'https://192.168.0.1:8080/api/ping' }), 'prefix');
});

test('他源 → 放行', () => {
  assert.strictEqual(
    sw.decideNonNav({ ...base, url: 'https://cdn.example.net/lib.js' }), 'pass');
});

test('保留路径 /sr-web-sw.js、/sr-web-shim.js → 放行', () => {
  assert.strictEqual(sw.decideNonNav({ ...base, url: `${RELAY}/sr-web-sw.js` }), 'pass');
  assert.strictEqual(
    sw.decideNonNav({ ...base, url: `${RELAY}/sr-web-shim.js?m=x&o=y` }), 'pass');
});

test('保留路径在带前缀时也放行（SHIM 由页面直接请求）', () => {
  assert.strictEqual(
    sw.decideNonNav({ ...base, url: `${RELAY}/agent/web/m/${MAP}/sr-web-shim.js?m=x` }),
    'as-is');
});

test('非法 URL → 放行（绝不因解析失败而拦下请求）', () => {
  assert.strictEqual(sw.decideNonNav({ ...base, url: '::::not-a-url' }), 'pass');
});

// ══════════════════════ A2. 前缀改写与剥离 ══════════════════════

console.log('\n[A2] Service Worker：前缀改写 / 剥离');

test('prefixUrl：同源路径 + query', () => {
  assert.strictEqual(
    sw.prefixUrl(`${RELAY}/a/b?x=1&y=2`, MAP, RELAY),
    `${RELAY}/agent/web/m/${MAP}/a/b?x=1&y=2`);
});

test('prefixUrl：根路径保留结尾斜杠', () => {
  assert.strictEqual(sw.prefixUrl(`${RELAY}/`, MAP, RELAY), `${RELAY}/agent/web/m/${MAP}/`);
});

test('prefixUrl：targetOrigin 绝对地址改写到 relay 源', () => {
  assert.strictEqual(
    sw.prefixUrl(`${TARGET}/api/me`, MAP, RELAY), `${RELAY}/agent/web/m/${MAP}/api/me`);
});

test('mapIdFromPath：取 map_id', () => {
  assert.strictEqual(sw.mapIdFromPath(`/agent/web/m/${MAP}/a`), MAP);
  assert.strictEqual(sw.mapIdFromPath(`/agent/web/m/${MAP}`), MAP);
  assert.strictEqual(sw.mapIdFromPath(`/agent/web/m/${MAP}/`), MAP);
  assert.strictEqual(sw.mapIdFromPath('/agent/web/proxy'), null);
  assert.strictEqual(sw.mapIdFromPath('/session'), null);
  assert.strictEqual(sw.mapIdFromPath('/agent/web/m/'), null);
});

test('stripPrefix：去前缀（带尾斜杠/不带）', () => {
  assert.strictEqual(sw.stripPrefix(`/agent/web/m/${MAP}/dashboard`, MAP), '/dashboard');
  assert.strictEqual(sw.stripPrefix(`/agent/web/m/${MAP}`, MAP), '/');
  assert.strictEqual(sw.stripPrefix(`/agent/web/m/${MAP}/`, MAP), '/');
  // 另一个 map_id 不能被误剥
  assert.strictEqual(sw.stripPrefix(`/agent/web/m/other/a`, MAP), `/agent/web/m/other/a`);
});

test('stripNavTarget：导航前缀剥离并保留 query', () => {
  assert.strictEqual(
    sw.stripNavTarget(`${RELAY}/agent/web/m/${MAP}/dashboard?tab=2`, MAP, RELAY),
    `${RELAY}/dashboard?tab=2`);
  assert.strictEqual(
    sw.stripNavTarget(`${RELAY}/agent/web/m/${MAP}/`, MAP, RELAY), `${RELAY}/`);
  assert.strictEqual(
    sw.stripNavTarget(`${RELAY}/agent/web/m/${MAP}`, MAP, RELAY), `${RELAY}/`);
});

test('shimTag：指向 /sr-web-shim.js 并带 m/o 参数', () => {
  const tag = sw.shimTag(MAP, TARGET);
  assert.ok(tag.indexOf('<script src="/sr-web-shim.js?m=' + MAP) === 0, tag);
  assert.ok(tag.indexOf('o=' + encodeURIComponent(TARGET)) > 0, tag);
  assert.ok(/<\/script>$/.test(tag), tag);
});

// ══════════════════════ B. SW：<head> 字节注入 ══════════════════════

console.log('\n[B] Service Worker：HTML 字节注入');

function ascii(s) { return Buffer.from(s, 'binary'); }
function str(bytes) { return Buffer.from(bytes).toString('binary'); }

test('普通 <head> 插在 > 之后', () => {
  const out = str(sw.injectHtml(ascii('<html><head><title>t</title></head></html>'),
    '<script src="x"></script>'));
  assert.strictEqual(out,
    '<html><head><script src="x"></script><title>t</title></head></html>');
});

test('<HEAD lang=..> 大小写与属性都兼容', () => {
  const out = str(sw.injectHtml(ascii('<HTML><HEAD lang="zh"><meta charset="gbk"></HEAD>'),
    '<script src="x"></script>'));
  assert.strictEqual(out,
    '<HTML><HEAD lang="zh"><script src="x"></script><meta charset="gbk"></HEAD>');
});

test('<head > 带空格、换行也能定位到 >', () => {
  const out = str(sw.injectHtml(ascii('<html><head  data-x="1"\n ><body>'),
    '<script></script>'));
  assert.strictEqual(out, '<html><head  data-x="1"\n ><script></script><body>');
});

test('没有 <head> → 插在最前面', () => {
  const out = str(sw.injectHtml(ascii('<!DOCTYPE html><html><body>hi</body></html>'),
    '<script src="x"></script>'));
  assert.strictEqual(out,
    '<script src="x"></script><!DOCTYPE html><html><body>hi</body></html>');
});

test('<header> 不被误判成 <head>', () => {
  const out = str(sw.injectHtml(ascii('<html><body><header>x</header></body></html>'),
    '<script src="x"></script>'));
  assert.strictEqual(out,
    '<script src="x"></script><html><body><header>x</header></body></html>');
});

test('GBK 字节在注入后逐字节不变', () => {
  // "设备首页" 的 GBK 编码 + 一段 GBK 注释；注入只插入 ASCII，其余字节必须
  // 原样保留（用 TextDecoder 先解码再编码会把它们变成 U+FFFD）。
  const gbk = Buffer.from([
    0x3c, 0x68, 0x65, 0x61, 0x64, 0x3e,          // <head>
    0xc9, 0xe8, 0xb1, 0xb8, 0xca, 0xd7, 0xd2, 0xb3,  // 设备首页（GBK）
    0x3c, 0x2f, 0x68, 0x65, 0x61, 0x64, 0x3e,    // </head>
  ]);
  const out = sw.injectHtml(gbk, '<script src="/sr-web-shim.js?m=x"></script>');
  const head = Buffer.from('<head><script src="/sr-web-shim.js?m=x"></script>');
  assert.strictEqual(out.length, gbk.length + head.length - 6);
  assert.deepStrictEqual(Buffer.from(out.subarray(0, 6)), Buffer.from('<head>'));
  assert.deepStrictEqual(
    Buffer.from(out.subarray(6 + (head.length - 6), out.length)),
    Buffer.from([0xc9, 0xe8, 0xb1, 0xb8, 0xca, 0xd7, 0xd2, 0xb3, 0x3c, 0x2f, 0x68,
      0x65, 0x61, 0x64, 0x3e]));
  // 关键断言：GBK 那 8 个字节在输出里连续出现，一个都没被改成 U+FFFD。
  const idx = Buffer.from(out).indexOf(Buffer.from([0xc9, 0xe8, 0xb1, 0xb8]));
  assert.ok(idx > 0, 'GBK 中文字节应原样保留');
});

test('findHeadInsertIndex：找不到返回 -1，位置正确', () => {
  assert.strictEqual(sw.findHeadInsertIndex(ascii('<html><head><x>')), 12);
  assert.strictEqual(sw.findHeadInsertIndex(ascii('<html><body>')), -1);
  // 标签还没收完 → -1（流式处理时继续等）
  assert.strictEqual(sw.findHeadInsertIndex(ascii('<html><head lang="zh"')), -1);
});

// ══════════════════════ C. shim：WebSocket 改写 ══════════════════════

console.log('\n[C] shim：WebSocket URL 改写');

const loc = {
  href: `${RELAY}/dashboard`,
  host: 'sshx.example.com',
  protocol: 'https:',
  origin: RELAY
};
const wsOpts = { mapId: MAP, targetHost: '192.168.0.1:8080', location: loc };

test('相对 URL（//host/path）→ 反代路径', () => {
  assert.strictEqual(
    shim.rewriteWsUrl('//sshx.example.com/ws', wsOpts),
    `wss://sshx.example.com/agent/web/m/${MAP}/ws`);
});

test('同源绝对 URL → 反代路径（wss，由本页协议决定）', () => {
  assert.strictEqual(
    shim.rewriteWsUrl('wss://sshx.example.com/ws?token=1', wsOpts),
    `wss://sshx.example.com/agent/web/m/${MAP}/ws?token=1`);
});

test('targetOrigin 主机（ws://192.168.0.1:8080/ws）→ relay 源 + 前缀', () => {
  assert.strictEqual(
    shim.rewriteWsUrl('ws://192.168.0.1:8080/ws', wsOpts),
    `wss://sshx.example.com/agent/web/m/${MAP}/ws`);
});

test('http 页面（relay 未启用 TLS）→ ws: 前缀', () => {
  const httpLoc = {
    href: 'http://127.0.0.1:3000/dashboard', host: '127.0.0.1:3000',
    protocol: 'http:', origin: 'http://127.0.0.1:3000'
  };
  assert.strictEqual(
    shim.rewriteWsUrl('ws://127.0.0.1:3000/ws', { ...wsOpts, location: httpLoc }),
    `ws://127.0.0.1:3000/agent/web/m/${MAP}/ws`);
});

test('他源 WebSocket 原样返回', () => {
  assert.strictEqual(
    shim.rewriteWsUrl('wss://push.example.net/socket.io', wsOpts),
    'wss://push.example.net/socket.io');
});

test('已带映射前缀 → 不重复改写', () => {
  const u = `wss://sshx.example.com/agent/web/m/${MAP}/ws`;
  assert.strictEqual(shim.rewriteWsUrl(u, wsOpts), u);
});

test('非 ws/wss 的输入原样返回', () => {
  assert.strictEqual(shim.rewriteWsUrl('http://192.168.0.1:8080/ws', wsOpts),
    'http://192.168.0.1:8080/ws');
  assert.strictEqual(shim.rewriteWsUrl('not a url', wsOpts), 'not a url');
});

// ══════════════════════ D. shim：fetch/XHR/ES 绝对地址改写 ══════════════════════

console.log('\n[D] shim：fetch / XHR / EventSource 绝对地址改写');

test('targetOrigin 绝对地址 → 同源相对路径（保留 query）', () => {
  assert.strictEqual(
    shim.toRelativeIfTarget(`${TARGET}/api/ping?a=1`, wsOpts), '/api/ping?a=1');
});

test('相对地址不改写（交给 SW）', () => {
  assert.strictEqual(shim.toRelativeIfTarget('/api/ping', wsOpts), null);
  assert.strictEqual(shim.toRelativeIfTarget('api/ping', wsOpts), null);
});

test('同源但非 targetOrigin 的绝对地址不改写（本来就同源）', () => {
  assert.strictEqual(shim.toRelativeIfTarget(`${RELAY}/api/ping`, wsOpts), null);
});

test('他源绝对地址不改写', () => {
  assert.strictEqual(shim.toRelativeIfTarget('https://cdn.example.net/x.js', wsOpts), null);
});

test('非 http/https（data:/blob:）不改写', () => {
  assert.strictEqual(shim.toRelativeIfTarget('data:text/plain,hi', wsOpts), null);
  assert.strictEqual(shim.toRelativeIfTarget('blob:https://x/y', wsOpts), null);
});

test('已带映射前缀的绝对地址不改写', () => {
  assert.strictEqual(
    shim.toRelativeIfTarget(`${RELAY}/agent/web/m/${MAP}/api/ping`, wsOpts), null);
});

test('targetHostOf：取 host（含端口）', () => {
  assert.strictEqual(shim.targetHostOf('http://192.168.0.1:8080'), '192.168.0.1:8080');
  assert.strictEqual(shim.targetHostOf('https://a.example/'), 'a.example');
  assert.strictEqual(shim.targetHostOf('nope'), '');
});

// ══════════════════════ E. shim：Service Worker 防护 ══════════════════════

console.log('\n[E] shim：Service Worker 防护');

// 防护接口都是 Promise：单独用异步断言，与上面同步 test() 同一口径统计。
(async () => {
  const fake = shim.fakeSwContainer();

  checks += 1;
  try {
    await fake.register('/evil-sw.js');
    console.log('  FAIL register() 应 reject（否则页面能抢走本站 SW）');
    fails.push('register() reject');
  } catch (e) {
    if (/disabled inside shell-remote web mapping/.test(String(e && e.message))) {
      console.log('  PASS register() → rejected（错误文案明确）');
    } else {
      console.log('  FAIL register() reject 文案不符: ' + (e && e.message));
      fails.push('register() reject message');
    }
  }

  checks += 1;
  try {
    const list = await fake.getRegistrations();
    const one = await fake.getRegistration();
    if (Array.isArray(list) && list.length === 0 && one === undefined) {
      console.log('  PASS getRegistrations() → [] 且 getRegistration() → undefined');
    } else {
      console.log('  FAIL getRegistrations/getRegistration 返回值不符');
      fails.push('sw enumeration');
    }
  } catch (e) {
    console.log('  FAIL getRegistrations 抛错: ' + e.message);
    fails.push('sw enumeration throws');
  }

  console.log('\n' + (fails.length === 0
    ? `全部通过：${checks} 项`
    : `失败 ${fails.length}/${checks} 项：\n  - ` + fails.join('\n  - ')));
  process.exit(fails.length === 0 ? 0 : 1);
})();
