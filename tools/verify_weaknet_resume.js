'use strict';
// 弱网/断线恢复端到端验证（MYS-1766）：真实 relay + agent + 无头浏览器。
//
// agent 经一个可控 TCP 代理连 relay（浏览器直连 relay），依次制造：
//   A. relay 进程被 kill -9 后重启（断开所有连接）
//   B. agent↔relay 链路"黑洞" BLACKHOLE_S 秒（连接不断开、数据全丢 = 半开）
// 每次故障后等待页面恢复，在浏览器终端里输入 `echo $$ > 文件`，校验：
//   1) 输入能到达 shell（终端没有"假死"）
//   2) shell PID 与故障前一致（PTY 跨重连存活，正在跑的命令不丢）
// 并记录恢复耗时。
//
// 用法: BIN=./target/debug/shell-remote node tools/verify_weaknet_resume.js
// 依赖: playwright（chromium）、可访问 jsdelivr CDN（xterm）。
const { chromium } = require('playwright');
const { spawn } = require('child_process');
const net = require('net');
const fs = require('fs');
const os = require('os');
const path = require('path');

const BIN = process.env.BIN || './target/debug/shell-remote';
const RELAY_PORT = +(process.env.RELAY_PORT || 3197);
const PROXY_PORT = +(process.env.PROXY_PORT || 3198);
const BLACKHOLE_S = +(process.env.BLACKHOLE_S || 70);
const RECOVER_TIMEOUT_MS = +(process.env.RECOVER_TIMEOUT_S || 150) * 1000;
// 逐键延迟（ms）。旧版前端每个按键独立并发 POST、到达顺序不保证，快速输入会
// 乱序（本脚本即可复现）；对比旧版本时可设 TYPE_DELAY_MS=80 规避该问题。
const TYPE_DELAY_MS = +(process.env.TYPE_DELAY_MS || 0);
const W = fs.mkdtempSync(path.join(os.tmpdir(), 'sr-weaknet-'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(`[${new Date().toISOString().slice(11, 19)}]`, ...a);

let relay = null;
let agent = null;
const agentLog = [];

function startRelay() {
  relay = spawn(BIN, ['relay', '--bind', `127.0.0.1:${RELAY_PORT}`, '--auth', 'e2e-pass', '--no-tls'],
    { stdio: ['ignore', 'pipe', 'pipe'] });
  const out = fs.createWriteStream(path.join(W, 'relay.log'), { flags: 'a' });
  relay.stdout.pipe(out); relay.stderr.pipe(out);
}

// ── 可控代理：blackhole=true 时两个方向的数据都静默丢弃（连接保持）──
let blackhole = false;
const pairs = new Set();
const proxy = net.createServer((c) => {
  const u = net.connect(RELAY_PORT, '127.0.0.1');
  const pair = { c, u };
  pairs.add(pair);
  const kill = () => { c.destroy(); u.destroy(); pairs.delete(pair); };
  c.on('data', (d) => { if (!blackhole) u.write(d); });
  u.on('data', (d) => { if (!blackhole) c.write(d); });
  c.on('error', kill); u.on('error', kill); c.on('close', kill); u.on('close', kill);
});

function startAgent() {
  agent = spawn(BIN, ['agent', '--relay-url', `http://127.0.0.1:${PROXY_PORT}`, '--key', 'e2ekey1766',
    '--session-id', 'e2e1766', '--shell', '/bin/bash', '--root', W],
    { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, RUST_LOG: 'info' } });
  const onData = (d) => {
    const s = d.toString();
    agentLog.push(s);
    fs.appendFileSync(path.join(W, 'agent.log'), s);
  };
  agent.stdout.on('data', onData); agent.stderr.on('data', onData);
}

async function rwToken() {
  for (let i = 0; i < 100; i++) {
    const m = agentLog.join('').replace(/\x1b\[[0-9;]*m/g, '').match(/token: ([0-9a-zA-Z]+)[^\n]*permission=rw/);
    if (m) return m[1];
    await sleep(100);
  }
  throw new Error('agent 未输出 rw token');
}

async function waitFile(f, ms) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (fs.existsSync(f)) {
      const v = fs.readFileSync(f, 'utf8').trim();
      if (v) return v;
    }
    await sleep(200);
  }
  return null;
}

// 反复"输入命令 → 等文件"直到成功或超时：恢复耗时 = 首次故障结束到命令生效。
async function typeUntilWritten(page, name, ms) {
  const f = path.join(W, name);
  const end = Date.now() + ms;
  while (Date.now() < end) {
    await page.click('#terminal-container').catch(() => {});
    await page.keyboard.type(`echo $$ > ${f}\n`, { delay: TYPE_DELAY_MS });
    const v = await waitFile(f, 5000);
    if (v) return v;
  }
  return null;
}

(async () => {
  const result = { bin: BIN, scenarios: [] };
  let browser;
  try {
    startRelay();
    await new Promise((r) => proxy.listen(PROXY_PORT, '127.0.0.1', r));
    await sleep(800);
    startAgent();
    const token = await rwToken();
    log('rw token ok, 打开浏览器');

    browser = await chromium.launch();
    const page = await browser.newPage();
    page.on('pageerror', (e) => log('pageerror:', e.message));
    await page.goto(`http://127.0.0.1:${RELAY_PORT}/`);
    await page.evaluate((t) => sessionStorage.setItem('shell-remote-token', t), token);
    await page.goto(`http://127.0.0.1:${RELAY_PORT}/session`);
    await page.waitForSelector('.tab-item', { timeout: 20000 });
    const pid0 = await typeUntilWritten(page, 'pid0', 20000);
    log('初始 shell PID =', pid0);
    if (!pid0) throw new Error('初始输入未到达 shell');

    // A. relay kill -9 + 重启
    relay.kill('SIGKILL');
    await sleep(2000);
    startRelay();
    let t0 = Date.now();
    const pidA = await typeUntilWritten(page, 'pidA', RECOVER_TIMEOUT_MS);
    result.scenarios.push({ name: 'relay kill -9 + restart', recovered: !!pidA,
      recover_s: pidA ? +((Date.now() - t0) / 1000).toFixed(1) : null, shell_survived: pidA === pid0, pid_before: pid0, pid_after: pidA });
    log('A 结果', result.scenarios.at(-1));

    // B. 半开黑洞
    const pidBase = pidA || pid0;
    blackhole = true;
    log(`B: 黑洞 ${BLACKHOLE_S}s`);
    await sleep(BLACKHOLE_S * 1000);
    blackhole = false;
    t0 = Date.now();
    const pidB = await typeUntilWritten(page, 'pidB', RECOVER_TIMEOUT_MS);
    result.scenarios.push({ name: `half-open blackhole ${BLACKHOLE_S}s`, recovered: !!pidB,
      recover_s: pidB ? +((Date.now() - t0) / 1000).toFixed(1) : null, shell_survived: pidB === pidBase, pid_before: pidBase, pid_after: pidB });
    log('B 结果', result.scenarios.at(-1));

    await page.screenshot({ path: path.join(W, 'session-after.png') });
  } catch (e) {
    result.error = e.message;
  } finally {
    if (browser) await browser.close();
    if (agent) agent.kill('SIGTERM');
    if (relay) relay.kill('SIGTERM');
    for (const p of pairs) { p.c.destroy(); p.u.destroy(); }
    proxy.close();
    result.workdir = W;
    console.log(JSON.stringify(result, null, 2));
    const ok = !result.error && result.scenarios.length === 2 &&
      result.scenarios.every((s) => s.recovered && s.shell_survived);
    process.exit(ok ? 0 : 1);
  }
})();
