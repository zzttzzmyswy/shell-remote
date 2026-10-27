#!/usr/bin/env node
// MYS-2383-F 真实验证：relay 同源网页映射 v2（TCP 管道上的 HTTP/1.1 反代 +
// WebSocket 透传）。
//
// 一条命令跑完整链路：**真 relay 二进制 + 真 agent 二进制 + 只监听 127.0.0.1 的
// 目标站**，只走公开 HTTP 接口（不 mock relay 内部状态）。agent 用 `--key` 固定
// 密钥启动，因此 rw/ro token 就是脚本可见的值。
//
// 覆盖（任务卡验收标准 3 的逐条）：
//   1. GET HTML、二进制图片，逐字节一致（含 >2 MiB 文件）；
//   2. POST 表单与 JSON body 回显一致；10 MiB 上传 sha256 一致；
//   3. 302 → Location 已改写为映射前缀，跟随后得到正确页面；
//   4. Set-Cookie 改写正确；带 Cookie 的后续请求被目标识别（模拟登录态）；
//   5. 目标响应中的 CSP / XFO / HSTS 头被剥离；
//   6. WebSocket：握手 101，文本/二进制 echo 各 100 条一致；目标主动推送可达；
//      任一侧关闭后另一侧收到关闭；
//   7. https 目标（自签证书）可访问；
//   8. 16 个并发 GET 全部成功（验证排队与连接池）；
//   9. 无效 map_id → 404；ro token 建映射 → 403；无 net:tcp-v1 的旧 agent → 404；
//      agent 断线后映射 → 404/503；
//  10. 原 /agent/web/proxy（v1）路径仍可用（回归一条）；
//  11. 映射复用：同目标两次创建返回同一 map_id。
//
// 用法（先构建）：cargo build --bin shell-remote && node tools/verify_web_map_relay.js
// 环境变量：BIN / RELAY_PORT / SITE_PORT / TLS_PORT / RELAY_PASSWORD。

'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const { spawn, execFileSync } = require('node:child_process');

const ROOT = path.dirname(path.dirname(fs.realpathSync(__filename)));
const BIN = process.env.BIN || path.join(ROOT, 'target', 'debug', 'shell-remote');
const RELAY_PORT = Number(process.env.RELAY_PORT || 18920);
const SITE_PORT = Number(process.env.SITE_PORT || 18921);
const TLS_PORT = Number(process.env.TLS_PORT || 18922);
const RELAY_PASSWORD = process.env.RELAY_PASSWORD || 'e2e-map-pass';
const BASE = `http://127.0.0.1:${RELAY_PORT}`;
const SITE = `http://127.0.0.1:${SITE_PORT}`;
const SITE_TLS = `https://127.0.0.1:${TLS_PORT}`;

const RW_KEY = 'maprwkey01';
const RO_KEY = 'maprokey01';
const RW_SID = 'mapagent01';
const RO_SID = 'mapagent02';
const OLD_SID = 'mapoldage1';

const FAILS = [];
let CHECKS = 0;

function check(name, cond, detail = '') {
  CHECKS += 1;
  if (cond) {
    console.log(`  PASS ${name}`);
  } else {
    console.log(`  FAIL ${name}${detail ? `   <- ${detail}` : ''}`);
    FAILS.push(name);
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ───────────────────────── 目标站（= 设备自身的页面） ─────────────────────────

const BIG = crypto.randomBytes(3 * 1024 * 1024 + 12345);   // >2 MiB
const IMG = crypto.randomBytes(64 * 1024);
const UPLOAD_BYTES = 10 * 1024 * 1024;
let uploadSha = '';
let uploadReceived = 0;

const loginState = { authenticated: false, cookieValue: '' };
// 目标站接受的 TCP 连接数（验证 relay 侧连接池复用，而不是每请求一条）。
let siteConnCount = 0;

function sendBody(res, code, body, headers = {}) {
  const buf = Buffer.isBuffer(body) ? body : Buffer.from(body);
  res.writeHead(code, {
    'Content-Type': 'application/octet-stream',
    'Content-Length': buf.length,
    ...headers,
  });
  res.end(buf);
}

function siteHandler(req, res) {
  const url = new URL(req.url, SITE);
  const p = url.pathname;

  if (p === '/') {
    return sendBody(
      res,
      200,
      '<html><head><title>t</title></head><body>device-local-page</body></html>',
      {
        'Content-Type': 'text/html; charset=utf-8',
        // 这些头必须被 relay 剥掉（CSP 拦脚本、HSTS/Clear-Site-Data 作用到
        // relay 源、XFO 挡 iframe）。
        'Content-Security-Policy': "default-src 'none'",
        'X-Frame-Options': 'DENY',
        'Strict-Transport-Security': 'max-age=31536000',
        'Clear-Site-Data': '"storage"',
        'Service-Worker-Allowed': '/',
        'Alt-Svc': 'h3=":443"',
      },
    );
  }
  if (p === '/img.png') {
    return sendBody(res, 200, IMG, { 'Content-Type': 'image/png' });
  }
  if (p === '/big.bin') {
    return sendBody(res, 200, BIG, { 'Content-Type': 'application/octet-stream' });
  }
  if (p === '/redirect') {
    // 绝对同源 Location：relay 必须改写成映射前缀。
    res.writeHead(302, { Location: `${SITE}/final`, 'Content-Length': 0 });
    return res.end();
  }
  if (p === '/redirect-relative') {
    res.writeHead(302, { Location: '/final', 'Content-Length': 0 });
    return res.end();
  }
  if (p === '/final') {
    return sendBody(res, 200, '<html><body>final-target</body></html>', {
      'Content-Type': 'text/html; charset=utf-8',
    });
  }
  if (p === '/set-cookie') {
    return sendBody(res, 200, 'cookie-set', {
      'Content-Type': 'text/plain; charset=utf-8',
      // Domain 必须被去掉（目标站的域作用不到 relay 源）；Path=/ 保持。
      'Set-Cookie': `session=abc123; Domain=127.0.0.1; Path=/; HttpOnly`,
    });
  }
  if (p === '/echo-cookie') {
    const cookie = req.headers.cookie || '';
    return sendBody(res, 200, cookie, { 'Content-Type': 'text/plain; charset=utf-8' });
  }
  if (p === '/login') {
    loginState.authenticated = true;
    loginState.cookieValue = 'logged-in';
    return sendBody(res, 200, 'ok', {
      'Content-Type': 'text/plain; charset=utf-8',
      'Set-Cookie': 'authstate=logged-in; Path=/; HttpOnly',
    });
  }
  if (p === '/whoami') {
    const cookie = req.headers.cookie || '';
    const body = cookie.includes('authstate=logged-in') ? 'user=alice' : 'user=anonymous';
    return sendBody(res, 200, body, { 'Content-Type': 'text/plain; charset=utf-8' });
  }
  if (p === '/headers') {
    // 回显收到的请求头，用于验证 Host/Origin/Referer 改写与凭据头剥离。
    const filtered = {};
    for (const [k, v] of Object.entries(req.headers)) {
      filtered[k] = v;
    }
    return sendBody(res, 200, JSON.stringify(filtered), {
      'Content-Type': 'application/json',
    });
  }
  if (p === '/upload' && req.method === 'POST') {
    const hash = crypto.createHash('sha256');
    let n = 0;
    req.on('data', (c) => {
      hash.update(c);
      n += c.length;
    });
    req.on('end', () => {
      uploadSha = hash.digest('hex');
      uploadReceived = n;
      sendBody(res, 200, JSON.stringify({ sha256: uploadSha, bytes: n }), {
        'Content-Type': 'application/json',
      });
    });
    return undefined;
  }
  if (p === '/form' && req.method === 'POST') {
    let raw = '';
    req.on('data', (c) => {
      raw += c.toString();
    });
    req.on('end', () => {
      sendBody(res, 200, raw, { 'Content-Type': 'text/plain; charset=utf-8' });
    });
    return undefined;
  }
  if (p === '/echo' && req.method === 'POST') {
    let raw = '';
    req.on('data', (c) => {
      raw += c.toString();
    });
    req.on('end', () => {
      sendBody(res, 200, raw, { 'Content-Type': 'application/json' });
    });
    return undefined;
  }
  if (p === '/slow') {
    // 长响应：用于并发用例与「body 无总时长上限」的观察。
    const n = Number(url.searchParams.get('n') || 20);
    res.writeHead(200, { 'Content-Type': 'text/plain', 'Transfer-Encoding': 'chunked' });
    let i = 0;
    const tick = () => {
      if (i >= n) {
        res.end();
        return;
      }
      i += 1;
      res.write(`chunk-${i}\n`);
      setTimeout(tick, 10);
    };
    tick();
    return undefined;
  }
  return sendBody(res, 404, 'missing', { 'Content-Type': 'text/plain; charset=utf-8' });
}

// WebSocket echo 服务（不引第三方库：自己按 RFC6455 处理握手与帧）。
const wsPeers = new Set();

function wsUpgrade(req, socket) {
  const key = req.headers['sec-websocket-key'];
  if (!key) {
    socket.destroy();
    return;
  }
  const accept = crypto
    .createHash('sha1')
    .update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11')
    .digest('base64');
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\n' +
      'Upgrade: websocket\r\n' +
      'Connection: Upgrade\r\n' +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  socket.setNoDelay(true);
  wsPeers.add(socket);

  let buf = Buffer.alloc(0);
  const sendFrame = (opcode, payload) => {
    const len = payload.length;
    let head;
    if (len < 126) {
      head = Buffer.from([0x80 | opcode, len]);
    } else if (len < 65536) {
      head = Buffer.alloc(4);
      head[0] = 0x80 | opcode;
      head[1] = 126;
      head.writeUInt16BE(len, 2);
    } else {
      head = Buffer.alloc(10);
      head[0] = 0x80 | opcode;
      head[1] = 127;
      head.writeBigUInt64BE(BigInt(len), 2);
    }
    socket.write(Buffer.concat([head, payload]));
  };

  socket.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const fin = (buf[0] & 0x80) !== 0;
      const opcode = buf[0] & 0x0f;
      const masked = (buf[1] & 0x80) !== 0;
      let len = buf[1] & 0x7f;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        off = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        off = 10;
      }
      const maskLen = masked ? 4 : 0;
      if (buf.length < off + maskLen + len) return;
      const mask = masked ? buf.subarray(off, off + 4) : null;
      const payload = Buffer.from(buf.subarray(off + maskLen, off + maskLen + len));
      buf = buf.subarray(off + maskLen + len);
      if (masked) {
        for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
      }
      if (opcode === 0x8) {
        // 对端关闭：回一帧 close 然后断开。
        sendFrame(0x8, Buffer.alloc(0));
        socket.end();
        wsPeers.delete(socket);
        return;
      }
      if (opcode === 0x9) {
        sendFrame(0xa, payload); // ping → pong
        continue;
      }
      if (opcode === 0x1 || opcode === 0x2) {
        const text = payload.toString('utf8');
        if (text === 'close-me') {
          // 目标主动关闭：继电器另一侧必须收到 close。
          sendFrame(0x8, Buffer.alloc(0));
          socket.end();
          wsPeers.delete(socket);
          return;
        }
        if (text === 'push-request') {
          // 目标主动推送（非 echo）。
          sendFrame(0x1, Buffer.from('server-push-1'));
          sendFrame(0x1, Buffer.from('server-push-2'));
          continue;
        }
        sendFrame(opcode, payload);
        continue;
      }
      if (!fin && opcode === 0x0) continue;
    }
  });
  socket.on('error', () => wsPeers.delete(socket));
  socket.on('close', () => wsPeers.delete(socket));
}

function startSite(port, tlsOptions) {
  const server = tlsOptions
    ? https.createServer(tlsOptions, siteHandler)
    : http.createServer(siteHandler);
  server.on('upgrade', (req, socket) => wsUpgrade(req, socket));
  if (!tlsOptions) {
    server.on('connection', () => {
      siteConnCount += 1;
    });
  }
  server.listen(port, '127.0.0.1');
  return server;
}

// ───────────────────────────── HTTP 客户端 ─────────────────────────────

async function req(url, opts = {}) {
  const res = await fetch(url, { redirect: 'manual', ...opts });
  const buf = Buffer.from(await res.arrayBuffer());
  return { status: res.status, headers: res.headers, body: buf, text: buf.toString('utf8') };
}

function authHeaders(token) {
  const h = { 'X-Auth': RELAY_PASSWORD };
  if (token) h['X-SR-Token'] = token;
  return h;
}

async function createMap(token, targetUrl, password = RELAY_PASSWORD) {
  const headers = { 'Content-Type': 'application/json' };
  if (password !== null) headers['X-Auth'] = password;
  if (token) headers['X-SR-Token'] = token;
  return req(`${BASE}/agent/web/map`, {
    method: 'POST',
    headers,
    body: JSON.stringify({ url: targetUrl }),
  });
}

async function waitRelay(timeoutMs = 30000) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    try {
      await new Promise((resolve, reject) => {
        const s = net.connect(RELAY_PORT, '127.0.0.1', () => {
          s.end();
          resolve();
        });
        s.on('error', reject);
        s.setTimeout(500, () => {
          s.destroy();
          reject(new Error('timeout'));
        });
      });
      return true;
    } catch {
      await sleep(100);
    }
  }
  return false;
}

/// 轮询浏览器 SSE 直到 200（= 该会话的 agent 通道已建立）。SSE 是永不结束的
/// 流，只读头就断开。
async function waitAgentOnline(token, timeoutMs = 60000) {
  const end = Date.now() + timeoutMs;
  const q = new URLSearchParams({ auth: RELAY_PASSWORD, token }).toString();
  while (Date.now() < end) {
    const ok = await new Promise((resolve) => {
      const s = net.connect(RELAY_PORT, '127.0.0.1', () => {
        s.write(
          `GET /agent/session/sse?${q} HTTP/1.1\r\n` +
            `Host: 127.0.0.1:${RELAY_PORT}\r\nAccept: text/event-stream\r\n` +
            `Connection: close\r\n\r\n`,
        );
      });
      let got = '';
      s.setTimeout(3000, () => {
        s.destroy();
        resolve(false);
      });
      s.on('data', (c) => {
        got += c.toString('latin1');
        if (got.startsWith('HTTP/1.1 200')) {
          s.destroy();
          resolve(true);
        } else if (got.length > 20) {
          s.destroy();
          resolve(false);
        }
      });
      s.on('error', () => resolve(false));
    });
    if (ok) return true;
    await sleep(300);
  }
  return false;
}

// 最小 WebSocket 客户端（Node 内置 WebSocket 足够，但需要帧级控制做二进制与
// 关闭观测，所以这里直接用它 + 原始事件）。
async function wsConnect(url) {
  const ws = new WebSocket(url);
  const queue = [];
  const waiters = [];
  const events = [];
  ws.binaryType = 'arraybuffer';
  ws.addEventListener('message', (ev) => {
    const data = typeof ev.data === 'string' ? ev.data : Buffer.from(ev.data);
    events.push({ kind: 'message', data });
    const w = waiters.shift();
    if (w) w({ kind: 'message', data });
    else queue.push({ kind: 'message', data });
  });
  ws.addEventListener('close', () => {
    events.push({ kind: 'close' });
    const w = waiters.shift();
    if (w) w({ kind: 'close' });
    else queue.push({ kind: 'close' });
  });
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true });
    ws.addEventListener('error', () => reject(new Error('ws error')), { once: true });
    setTimeout(() => reject(new Error('ws open timeout')), 15000);
  });
  return {
    ws,
    next(timeoutMs = 15000) {
      if (queue.length) return Promise.resolve(queue.shift());
      return new Promise((resolve, reject) => {
        const t = setTimeout(() => reject(new Error('ws message timeout')), timeoutMs);
        waiters.push((v) => {
          clearTimeout(t);
          resolve(v);
        });
      });
    },
    close() {
      try {
        ws.close();
      } catch {
        /* ignore */
      }
    },
  };
}

// ────────────────────────────── 断言流程 ──────────────────────────────

async function runChecks(ctx) {
  const { rw, ro, relayProcs } = ctx;

  console.log('\n[0] 目标站可用（直连，不经 relay）');
  check('目标站 HTML 可直连', (await req(`${SITE}/`)).status === 200);
  check('目标站 WebSocket 可直连', await (async () => {
    const c = await wsConnect(`ws://127.0.0.1:${SITE_PORT}/ws`);
    c.ws.send('direct');
    const m = await c.next();
    c.close();
    return m.data.toString() === 'direct';
  })());
  check('目标站 https（自签）可直连', await (async () => {
    const prev = process.env.NODE_TLS_REJECT_UNAUTHORIZED;
    process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';
    try {
      return (await req(`${SITE_TLS}/`)).status === 200;
    } finally {
      process.env.NODE_TLS_REJECT_UNAUTHORIZED = prev;
    }
  })());

  console.log('\n[1] POST /agent/web/map 鉴权与能力协商');
  let r = await createMap(rw, `${SITE}/`, 'wrong');
  check('错误服务器口令 → 401', r.status === 401, String(r.status));
  r = await createMap('bogus', `${SITE}/`);
  check('无效会话 token → 401', r.status === 401, String(r.status));
  r = await createMap(null, `${SITE}/`);
  check('缺会话 token → 401', r.status === 401, String(r.status));
  r = await createMap(ro, `${SITE}/`);
  check('ro token → 403', r.status === 403, String(r.status));
  check('ro token 403 正文含 read-only', r.text.includes('read-only'), r.text);
  r = await createMap(rw, 'ftp://127.0.0.1/');
  check('非法 scheme → 400', r.status === 400, String(r.status));
  r = await createMap(rw, '/relative');
  check('相对 URL → 400', r.status === 400, String(r.status));
  r = await createMap(rw, `http://127.0.0.1/${'a'.repeat(4100)}`);
  check('URL 超 4096 → 400', r.status === 400, String(r.status));

  r = await createMap(rw, `${SITE}/`);
  check('rw token 建映射 → 200', r.status === 200, `${r.status} ${r.text}`);
  const created = JSON.parse(r.text);
  const mapId = created.map_id;
  check('map_id 为 64 位 hex',
    typeof mapId === 'string' && mapId.length === 64 && /^[0-9a-f]+$/.test(mapId), mapId);
  check('base 为映射前缀', created.base === `/agent/web/m/${mapId}/`, created.base);
  check('target_origin 无尾斜杠且为请求的源',
    created.target_origin === SITE, created.target_origin);
  check('expires_in = 1800', created.expires_in === 1800, String(created.expires_in));
  check('响应体不含会话 token', !r.text.includes(rw));

  const r2 = await createMap(rw, `${SITE}/`);
  const created2 = JSON.parse(r2.text);
  check('同目标复用同一 map_id', created2.map_id === mapId, `${created2.map_id} vs ${mapId}`);

  const M = `${BASE}/agent/web/m/${mapId}`;

  console.log('\n[2] GET：HTML / 二进制 / >2 MiB 文件逐字节一致');
  r = await req(`${M}/`);
  check('GET / → 200', r.status === 200, String(r.status));
  check('HTML body 一致', r.text.includes('device-local-page'), r.text.slice(0, 80));
  check('content-type 透传', (r.headers.get('content-type') || '').includes('text/html'),
    r.headers.get('content-type'));
  check('relay 凭据未回显', !r.text.includes(RELAY_PASSWORD));

  r = await req(`${M}/img.png`);
  check('二进制图片逐字节一致', r.status === 200 && r.body.equals(IMG),
    `${r.body.length} vs ${IMG.length}`);

  r = await req(`${M}/big.bin`);
  check('3MiB 文件逐字节一致', r.status === 200 && r.body.equals(BIG),
    `${r.body.length} vs ${BIG.length}`);

  console.log('\n[3] POST：表单与 JSON body 回显一致；10 MiB 上传 sha256 一致');
  r = await req(`${M}/form`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: 'a=1&b=%E4%B8%AD%E6%96%87',
  });
  check('表单 body 回显一致', r.status === 200 && r.text === 'a=1&b=%E4%B8%AD%E6%96%87',
    `${r.status} ${r.text}`);

  const jsonBody = JSON.stringify({ hello: 'world', n: [1, 2, 3] });
  r = await req(`${M}/echo`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: jsonBody,
  });
  check('JSON body 回显一致', r.status === 200 && r.text === jsonBody, `${r.status} ${r.text}`);

  const upload = crypto.randomBytes(UPLOAD_BYTES);
  const uploadExpectedSha = crypto.createHash('sha256').update(upload).digest('hex');
  r = await req(`${M}/upload`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/octet-stream' },
    body: upload,
  });
  check('10 MiB 上传 → 200', r.status === 200, `${r.status} ${r.text.slice(0, 120)}`);
  check('10 MiB 上传 sha256 一致', uploadSha === uploadExpectedSha,
    `${uploadSha} vs ${uploadExpectedSha}`);
  check('10 MiB 上传字节数一致', uploadReceived === UPLOAD_BYTES, String(uploadReceived));

  console.log('\n[4] 302：Location 改写为映射前缀，跟随后正确');
  r = await req(`${M}/redirect`);
  check('302 状态透传', r.status === 302, String(r.status));
  const loc = r.headers.get('location') || '';
  check('绝对同源 Location 改写为映射前缀', loc === `/agent/web/m/${mapId}/final`, loc);
  check('Location 不含目标源', !loc.includes('127.0.0.1:'), loc);
  r = await req(`${BASE}${loc}`);
  check('跟随改写后的 Location 得到正确页面', r.status === 200 && r.text.includes('final-target'),
    `${r.status} ${r.text.slice(0, 80)}`);

  r = await req(`${M}/redirect-relative`);
  check('根路径 Location 改写为映射前缀',
    (r.headers.get('location') || '') === `/agent/web/m/${mapId}/final`,
    r.headers.get('location'));

  console.log('\n[5] Set-Cookie 改写 + 登录态');
  r = await req(`${M}/set-cookie`);
  const sc = r.headers.getSetCookie ? r.headers.getSetCookie() : [r.headers.get('set-cookie')];
  const cookie0 = sc[0] || '';
  check('Set-Cookie 已下发', cookie0.includes('session=abc123'), cookie0);
  check('Set-Cookie 去掉 Domain=', !/domain=/i.test(cookie0), cookie0);
  check('Set-Cookie 保持 Path=/', /path=\//i.test(cookie0), cookie0);
  check('Set-Cookie 保留 HttpOnly', /httponly/i.test(cookie0), cookie0);

  r = await req(`${M}/login`);
  const loginCookies = r.headers.getSetCookie ? r.headers.getSetCookie() : [];
  const authCookie = loginCookies.find((c) => c.includes('authstate=logged-in')) || '';
  check('登录接口下发 authstate cookie', authCookie !== '', JSON.stringify(loginCookies));
  const cookieHeader = authCookie.split(';')[0];
  r = await req(`${M}/whoami`, { headers: { Cookie: cookieHeader } });
  check('带 Cookie 的请求被目标识别为登录态', r.text === 'user=alice', r.text);

  console.log('\n[6] 响应头剥离：CSP / XFO / HSTS / Clear-Site-Data / SW-Allowed');
  r = await req(`${M}/`);
  for (const h of [
    'content-security-policy',
    'x-frame-options',
    'strict-transport-security',
    'clear-site-data',
    'service-worker-allowed',
    'alt-svc',
  ]) {
    check(`${h} 已剥离`, r.headers.get(h) === null, String(r.headers.get(h)));
  }

  console.log('\n[7] 请求头改写：Host / Origin / Referer / 凭据头剥离');
  r = await req(`${M}/headers`, {
    headers: {
      Origin: BASE,
      Referer: `${BASE}/agent/web/m/${mapId}/page`,
      Cookie: 'k=v',
      Authorization: 'Bearer t',
      'X-SR-Token': rw,
      'X-Auth': RELAY_PASSWORD,
    },
  });
  const seen = JSON.parse(r.text);
  check('Host 改为目标 authority', seen.host === `127.0.0.1:${SITE_PORT}`, String(seen.host));
  check('Origin 改为 target_origin', seen.origin === SITE, String(seen.origin));
  check('Referer 去映射前缀后拼 target_origin',
    seen.referer === `${SITE}/page`, String(seen.referer));
  check('X-SR-Token 未外发', seen['x-sr-token'] === undefined, String(seen['x-sr-token']));
  check('X-Auth 未外发', seen['x-auth'] === undefined, String(seen['x-auth']));
  check('Cookie 原样转发', seen.cookie === 'k=v', String(seen.cookie));
  check('Authorization 原样转发', seen.authorization === 'Bearer t', String(seen.authorization));

  console.log('\n[8] WebSocket：101 + 文本/二进制 echo 各 100 条 + 目标推送 + 双向关闭');
  const wsUrl = `ws://127.0.0.1:${RELAY_PORT}/agent/web/m/${mapId}/ws`;
  const c = await wsConnect(wsUrl);
  check('WebSocket 握手 101（经映射路径）', true);

  let textOk = 0;
  for (let i = 0; i < 100; i += 1) {
    const msg = `text-${i}-${'x'.repeat(i)}`;
    c.ws.send(msg);
    const m = await c.next();
    if (m.kind === 'message' && m.data.toString() === msg) textOk += 1;
  }
  check('文本 echo 100 条全部一致', textOk === 100, `${textOk}/100`);

  let binOk = 0;
  for (let i = 0; i < 100; i += 1) {
    const payload = crypto.randomBytes(32 + i);
    c.ws.send(payload);
    const m = await c.next();
    if (m.kind === 'message' && Buffer.from(m.data).equals(payload)) binOk += 1;
  }
  check('二进制 echo 100 条全部一致', binOk === 100, `${binOk}/100`);

  c.ws.send('push-request');
  const p1 = await c.next();
  const p2 = await c.next();
  check('目标主动推送可达（2 条）',
    p1.data.toString() === 'server-push-1' && p2.data.toString() === 'server-push-2',
    `${p1.data} / ${p2.data}`);

  // 单条大帧（> 单个 tcp:data 块，验证跨块拼接）。
  const bigMsg = 'B'.repeat(300 * 1024);
  c.ws.send(bigMsg);
  const bigBack = await c.next(30000);
  check('300KiB 单帧往返一致（跨块拼接）',
    bigBack.kind === 'message' && bigBack.data.toString() === bigMsg,
    `${bigBack.kind} len=${bigBack.data ? bigBack.data.length : -1}`);

  // 目标主动关闭 → 浏览器侧收到 close。
  c.ws.send('close-me');
  const closeEv = await c.next(15000);
  check('目标关闭后浏览器侧收到 close', closeEv.kind === 'close', closeEv.kind);

  // 浏览器主动关闭 → 目标侧连接结束（目标站的 socket 集合会减少）。
  const c2 = await wsConnect(wsUrl);
  const peersBefore = wsPeers.size;
  c2.close();
  await sleep(800);
  check('浏览器关闭后目标侧连接被释放', wsPeers.size < peersBefore,
    `${wsPeers.size} vs ${peersBefore}`);

  console.log('\n[9] https 目标（自签证书）可访问');
  const tlsMapRes = await createMap(rw, `${SITE_TLS}/`);
  check('https 目标建映射 → 200', tlsMapRes.status === 200, `${tlsMapRes.status} ${tlsMapRes.text}`);
  const tlsMapId = JSON.parse(tlsMapRes.text).map_id;
  r = await req(`${BASE}/agent/web/m/${tlsMapId}/`);
  check('经映射访问 https 目标 → 200', r.status === 200, `${r.status} ${r.text.slice(0, 120)}`);
  check('https 目标内容一致', r.text.includes('device-local-page'), r.text.slice(0, 80));

  console.log('\n[10] 16 个并发 GET 全部成功（排队 + 连接池）');
  const conc = await Promise.all(
    Array.from({ length: 16 }, (_, i) => req(`${M}/slow?n=${6 + (i % 4)}`)),
  );
  const concOk = conc.filter((x) => x.status === 200 && x.text.startsWith('chunk-1')).length;
  check('16 个并发 GET 全部 200 且内容正确', concOk === 16, `${concOk}/16`);
  const counts = conc.map((x) => x.text.split('\n').filter(Boolean).length);
  check('并发响应内容完整（每条约 6~9 块）',
    counts.every((n) => n >= 6 && n <= 9), counts.join(','));

  console.log('\n[10b] 连接池：顺序请求复用上游 TCP 连接');
  await sleep(300);
  const connBefore = siteConnCount;
  for (let i = 0; i < 10; i += 1) {
    // eslint-disable-next-line no-await-in-loop
    const rr = await req(`${M}/img.png`);
    if (rr.status !== 200) throw new Error(`顺序请求失败 ${rr.status}`);
  }
  const used = siteConnCount - connBefore;
  check('10 个顺序请求复用连接（新建 ≤ 2 条）', used <= 2, `新建 ${used} 条`);

  console.log('\n[11] 拒绝路径：无效 map_id / SW 注册 / 凭据头');
  r = await req(`${BASE}/agent/web/m/${'0'.repeat(64)}/`);
  check('无效 map_id → 404', r.status === 404, String(r.status));
  r = await req(`${BASE}/agent/web/m/deadbeef/`);
  check('短 map_id → 404', r.status === 404, String(r.status));
  r = await req(`${M}/sw.js`, { headers: { 'Service-Worker': 'script' } });
  check('Service-Worker: script → 403', r.status === 403, String(r.status));

  console.log('\n[12] 旧 agent（未声明 net:tcp-v1）→ 404');
  const oldReg = await req(`${BASE}/agent/send`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ type: 'agent:register', token_type: 'rw', session_id: OLD_SID }),
  });
  check('旧 agent 会话注册成功', oldReg.status === 200, String(oldReg.status));
  const oldToken = JSON.parse(oldReg.text).payload.tokens[0].token;
  const oldEvents = spawn('curl', [
    '-sN',
    `${BASE}/agent/events?session=${OLD_SID}`,
    '-H',
    `X-SR-Agent-Token: ${oldToken}`,
  ], { stdio: 'ignore' });
  try {
    await sleep(1200);
    r = await createMap(oldToken, `${SITE}/`);
    check('旧 agent 建映射 → 404', r.status === 404, `${r.status} ${r.text}`);
  } finally {
    oldEvents.kill();
  }

  console.log('\n[13] 原 v1 路径回归（/agent/web/proxy）');
  r = await req(`${BASE}/agent/web/ticket`, {
    method: 'POST',
    headers: authHeaders(rw),
  });
  check('v1 ticket 签发仍可用', r.status === 200, `${r.status} ${r.text.slice(0, 120)}`);
  if (r.status === 200) {
    const ticket = JSON.parse(r.text).ticket;
    const u = `${BASE}/agent/web/proxy?${new URLSearchParams({ ticket, url: `${SITE}/` })}`;
    r = await req(u);
    check('v1 代理 GET 仍可用', r.status === 200 && r.text.includes('device-local-page'),
      `${r.status} ${r.text.slice(0, 120)}`);
  }

  console.log('\n[13b] 在途长响应跨越空闲窗口（body 无总时长上限）');
  const longStart = Date.now();
  r = await req(`${M}/slow?n=40`);
  const longMs = Date.now() - longStart;
  check('慢响应完整收完（40 块）',
    r.status === 200 && r.text.split('\n').filter(Boolean).length === 40,
    `${r.status} ${r.text.split('\n').filter(Boolean).length}`);
  check('慢响应耗时超过 300ms（确实在慢慢吐，未被打断）', longMs > 300, `${longMs}ms`);

  console.log('\n[14] agent 断线：映射失效 + 管道报错');
  // 先确认映射此刻仍可用。
  r = await req(`${M}/`);
  check('断线前映射可用', r.status === 200, String(r.status));

  for (const p of relayProcs) {
    p.kill('SIGTERM');
  }
  for (const p of relayProcs) {
    await new Promise((resolve) => {
      const t = setTimeout(() => {
        p.kill('SIGKILL');
        resolve();
      }, 15000);
      p.on('exit', () => {
        clearTimeout(t);
        resolve();
      });
    });
  }
  // relay 的断线清理在 SSE Drop 里 spawn，给一点收敛时间。
  await sleep(3000);

  r = await req(`${M}/`);
  check('agent 断线后映射 → 404 或 503', r.status === 404 || r.status === 503, String(r.status));

  r = await createMap(rw, `${SITE}/`);
  check('agent 断线后建映射 → 404（映射已撤销）或 503', r.status === 404 || r.status === 503,
    `${r.status} ${r.text}`);
}

// ────────────────────────────── 主流程 ──────────────────────────────

function makeSelfSigned() {
  // 用 relay 自带的 rcgen 生成不方便，这里用 openssl（本机常备）。
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sr-map-tls-'));
  const key = path.join(dir, 'key.pem');
  const cert = path.join(dir, 'cert.pem');
  execFileSync('openssl', [
    'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
    '-subj', '/CN=127.0.0.1', '-keyout', key, '-out', cert,
    '-addext', 'subjectAltName=IP:127.0.0.1',
  ], { stdio: 'ignore' });
  return { key: fs.readFileSync(key), cert: fs.readFileSync(cert) };
}

async function main() {
  if (!fs.existsSync(BIN)) {
    console.error(`找不到二进制 ${BIN}；先跑 cargo build --bin shell-remote，或用 BIN 指定`);
    process.exit(2);
  }

  const site = startSite(SITE_PORT);
  const siteTls = startSite(TLS_PORT, makeSelfSigned());

  const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'sr-map-relay-'));
  const relayLog = fs.openSync(path.join(workdir, 'relay.log'), 'w');
  const relay = spawn(
    BIN,
    [
      'relay',
      '--bind', `127.0.0.1:${RELAY_PORT}`,
      '--auth', RELAY_PASSWORD,
      '--no-tls',
      '--settings-file', path.join(workdir, 'settings.json'),
    ],
    { stdio: ['ignore', relayLog, relayLog] },
  );

  const agents = [];
  try {
    if (!(await waitRelay())) {
      throw new Error(`relay 未在 ${RELAY_PORT} 就绪（日志 ${workdir}/relay.log）`);
    }
    console.log(`relay ${BASE} / 目标站 ${SITE} / https 目标 ${SITE_TLS} / 工作目录 ${workdir}`);

    const rwAgent = spawn(BIN, [
      'agent', '--relay-url', BASE, '--key', RW_KEY, '--session-id', RW_SID,
      '--token-type', 'both', '--shell', '/bin/bash', '--root', '/tmp', '--no-cmd-log',
    ], { stdio: ['ignore', fs.openSync(path.join(workdir, 'agent-rw.log'), 'w'), fs.openSync(path.join(workdir, 'agent-rw.log'), 'w')] });
    agents.push(rwAgent);

    const roAgent = spawn(BIN, [
      'agent', '--relay-url', BASE, '--key', RO_KEY, '--session-id', RO_SID,
      '--token-type', 'ro', '--shell', '/bin/bash', '--root', '/tmp', '--no-cmd-log',
    ], { stdio: ['ignore', fs.openSync(path.join(workdir, 'agent-ro.log'), 'w'), fs.openSync(path.join(workdir, 'agent-ro.log'), 'w')] });
    agents.push(roAgent);

    check('rw agent 注册并建立通道', await waitAgentOnline(RW_KEY), RW_SID);
    check('ro agent 注册并建立通道', await waitAgentOnline(RO_KEY), RO_SID);
    if (FAILS.length) {
      throw new Error('agent 未上线，后续用例无意义');
    }

    await runChecks({ rw: RW_KEY, ro: RO_KEY, relayProcs: agents });
  } finally {
    for (const a of agents) {
      if (a.exitCode === null) a.kill('SIGTERM');
    }
    await sleep(500);
    for (const a of agents) {
      if (a.exitCode === null) a.kill('SIGKILL');
    }
    relay.kill('SIGTERM');
    await sleep(300);
    if (relay.exitCode === null) relay.kill('SIGKILL');
    site.close();
    siteTls.close();
    for (const s of wsPeers) s.destroy();
  }

  console.log();
  if (FAILS.length) {
    console.log(`FAIL：${FAILS.length}/${CHECKS} 项未通过`);
    for (const f of FAILS) console.log(`  - ${f}`);
    return 1;
  }
  console.log(`PASS：${CHECKS}/${CHECKS} 项全部通过`);
  return 0;
}

main()
  .then((c) => process.exit(c))
  .catch((e) => {
    console.error(String(e && e.stack ? e.stack : e));
    process.exit(1);
  });
