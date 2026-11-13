#!/usr/bin/env node
'use strict';
// MYS-2836 阶段2 运行时验证：AV1 灰度切换 + 编码切换的真实浏览器解码验证。
//
// 验证的是**实际编解码链路**（不是 gray 布尔值、不是等待提示是否可见）：
//   浏览器 WebCodecs VideoDecoder 每解出一帧就记一次，画布像素用于判断
//   灰度是否真的无色度。切换后必须在 DEADLINE_MS 内出现新的解码帧。
//
// 覆盖（对应任务卡验收标准）：
//   1. 彩色 → 灰度 → 彩色，至少 ROUNDS 轮；每轮切换后 DEADLINE_MS 内
//      必须出现新解码帧，且灰度态画面饱和度为 0（真·无色度）；
//   2. 重复设置相同 gray 是 no-op（不打断正在解码的流）；
//   3. AV1 → H264 → AV1 与灰度组合不残留旧状态（切回后仍出帧、灰度关时
//      饱和度恢复）。
//
// 用法（脚本自己起 relay/agent，不连任何生产地址）：
//   node tools/verify/verify_mys2836_runtime.js
//
// 环境变量（全部可选；默认值只用于本地回环）：
//   BIN      shell-remote 二进制目录（含 shell-remote 与 shell-remote-ui）
//   BASE     relay 地址；默认由脚本按 PORT 生成 http://127.0.0.1:<PORT>
//   TOKEN    会话密钥（agent --key / relay --auth），默认随机生成
//   PORT     relay 端口，默认 0（由内核选空闲端口）
//   WIDTH/HEIGHT  测试画面分辨率（默认 1280x720）
//   ROUNDS   彩色→灰度→彩色 的轮数（默认 5）
//   DEADLINE_MS  每次切换后允许的恢复时间（默认 15000，任务卡要求 15s 内）
//   DISPLAY  使用已有 X display；未提供则脚本自建 Xvfb（不存在则跳过 X11 用例）
//   CHROME   Chromium 可执行文件路径（默认用 playwright 安装的那份）
//   KEEP=1   结束后保留 relay/agent/Xvfb 进程与临时目录（排障用）
//
// 退出码：0 = 全部通过；1 = 有用例失败；2 = 环境不满足（缺依赖/二进制/X11）。
// 任何"跳过"都必须计入 skipped 并在摘要里明确列出，绝不把跳过当通过。

const { spawn, spawnSync } = require('child_process');
const crypto = require('crypto');
const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');

const ROOT = path.resolve(__dirname, '..', '..');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const cfg = {
  BIN: process.env.BIN || path.join(ROOT, 'target', 'debug'),
  PORT: Number(process.env.PORT || 0),
  TOKEN: process.env.TOKEN || 'sr-verify-' + crypto.randomBytes(6).toString('hex'),
  WIDTH: Number(process.env.WIDTH || 1280),
  HEIGHT: Number(process.env.HEIGHT || 720),
  ROUNDS: Number(process.env.ROUNDS || 5),
  DEADLINE_MS: Number(process.env.DEADLINE_MS || 15000),
  FPS: Number(process.env.FPS || 30),
  // LAN 直连：默认开启（任务卡要求验证可用直连路径）。注意 agent 侧
  // `--desktop-lan-port 0` 表示**不开启**（不是 OS 分配），所以这里必须给一个
  // 真实端口；设成负数表示"只测 relay"。
  LAN_PORT: process.env.LAN_PORT === undefined ? 0 : Number(process.env.LAN_PORT),
  DISPLAY: process.env.DISPLAY || '',
  CHROME: process.env.CHROME || '',
  KEEP: process.env.KEEP === '1',
};

let checks = 0;
let fails = 0;
const skipped = [];
const check = (name, ok, extra = '') => {
  checks += 1;
  console.log(`${ok ? '✓' : '✗'} ${name}${extra ? '  ' + extra : ''}`);
  if (!ok) fails += 1;
  return ok;
};
const skip = (name, why) => {
  skipped.push(`${name}: ${why}`);
  console.log(`- SKIP ${name} — ${why}`);
};

// ── 进程管理：只清理本脚本自己拉起的 PID，绝不按名字杀 ──────────────
const procs = [];
function track(name, child) {
  procs.push({ name, child, pid: child.pid });
  return child;
}
function stopAll() {
  for (const p of procs) {
    try { process.kill(p.pid, 'SIGTERM'); } catch (e) { /* 已退出 */ }
  }
  // 给 2s 优雅退出，再强杀仍未退出的（只针对记录在案的 PID）。
  const deadline = Date.now() + 2000;
  while (Date.now() < deadline && procs.some((p) => alive(p.pid))) {
    spawnSync(process.execPath, ['-e', 'setTimeout(()=>{},50)']);
  }
  for (const p of procs) {
    if (alive(p.pid)) { try { process.kill(p.pid, 'SIGKILL'); } catch (e) { /* ignore */ } }
  }
}
function alive(pid) {
  try { process.kill(pid, 0); return true; } catch (e) { return false; }
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const p = srv.address().port;
      srv.close(() => resolve(p));
    });
  });
}

async function waitFor(fn, timeoutMs, label) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    if (await fn()) return true;
    await sleep(150);
  }
  throw new Error(`timeout waiting for ${label} (${timeoutMs}ms)`);
}

// ── 前置检查 ─────────────────────────────────────────────────────
function preflight() {
  const relay = path.join(cfg.BIN, 'shell-remote');
  const ui = path.join(cfg.BIN, 'shell-remote-ui');
  if (!fs.existsSync(relay)) {
    console.error(`FAIL 缺少 relay 二进制：${relay}\n  用 BIN=<构建输出目录> 指定（含 shell-remote 与 shell-remote-ui）。`);
    return null;
  }
  if (!fs.existsSync(ui)) {
    console.error(`FAIL 缺少 UI/agent 二进制：${ui}`);
    return null;
  }
  let playwright;
  try { playwright = require('playwright'); } catch (e) {
    console.error('FAIL 缺少 playwright：在仓库根执行 `npm i` 后重试。');
    return null;
  }
  return { relay, ui, playwright };
}

// 找一份可用的 Chromium（优先环境变量，其次 playwright 缓存）。
function resolveChrome(playwright) {
  if (cfg.CHROME) return cfg.CHROME;
  try {
    const p = playwright.chromium.executablePath();
    if (p && fs.existsSync(p)) return p;
  } catch (e) { /* 落到下面的手工探测 */ }
  const cache = path.join(os.homedir(), '.cache', 'ms-playwright');
  if (fs.existsSync(cache)) {
    for (const d of fs.readdirSync(cache)) {
      if (!d.startsWith('chromium-')) continue;
      for (const rel of ['chrome-linux64/chrome', 'chrome-linux/chrome']) {
        const p = path.join(cache, d, rel);
        if (fs.existsSync(p)) return p;
      }
    }
  }
  return '';
}

// 找一个真正空闲的 X display：既没有 lock 文件，也没有活着的 socket。
// 机器上常有其它任务遗留的 .X<n>-lock，直接随机取号会撞上并让 Xvfb 起不来。
function pickFreeDisplay() {
  for (let n = 40; n < 200; n++) {
    if (fs.existsSync(`/tmp/.X${n}-lock`)) continue;
    if (fs.existsSync(`/tmp/.X11-unix/X${n}`)) continue;
    return ':' + n;
  }
  return '';
}

function displayUsable(display) {
  const r = spawnSync('sh', ['-c', `DISPLAY=${display} xdpyinfo >/dev/null 2>&1`]);
  return r.status === 0;
}

async function startStack(deps) {
  const port = cfg.PORT || (await freePort());
  // LAN 端口：显式给 0 时由这里挑一个空闲端口（agent 的 0 = 关闭）。
  if (cfg.LAN_PORT === 0) cfg.LAN_PORT = await freePort();
  const base = process.env.BASE || `http://127.0.0.1:${port}`;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'mys2836-'));

  let display = cfg.DISPLAY;
  let ownXvfb = false;
  if (!display) {
    const which = spawnSync('sh', ['-c', 'command -v Xvfb'], { encoding: 'utf8' });
    if (which.status !== 0) throw new Error('no DISPLAY and Xvfb not installed');
    display = pickFreeDisplay();
    if (!display) throw new Error('no free X display number (40..199 all locked)');
    ownXvfb = true;
  }

  if (ownXvfb) {
    const log = path.join(dir, 'xvfb.log');
    track('xvfb', spawn('Xvfb', [display, '-screen', '0', `${cfg.WIDTH}x${cfg.HEIGHT}x24`, '-extension', 'GLX'], {
      stdio: ['ignore', fs.openSync(log, 'w'), fs.openSync(log, 'a')],
    }));
    try {
      await waitFor(() => displayUsable(display), 15000, 'Xvfb ready');
    } catch (e) {
      const why = fs.existsSync(log) ? fs.readFileSync(log, 'utf8').slice(-400) : '';
      throw new Error(`Xvfb ${display} 未就绪：${e.message} ${why}`);
    }
  } else if (!displayUsable(display)) {
    throw new Error(`DISPLAY=${display} 不可用`);
  }

  // 确定性彩色测试画面（有明确色度 + 持续运动）。脚本自己确认它真的画上了
  // ——空/黑画面会让"灰度无色度"这类断言失去意义，必须在跑用例前就排除。
  let pattern = null;
  const patternScript = path.join(__dirname, 'mys2836_pattern.py');
  const py = spawnSync('sh', ['-c', 'command -v python3'], { encoding: 'utf8' });
  if (py.status === 0 && fs.existsSync(patternScript)) {
    const ready = path.join(dir, 'pattern.ready');
    const log = path.join(dir, 'pattern.log');
    pattern = track('pattern', spawn('python3', [patternScript, String(cfg.WIDTH), String(cfg.HEIGHT)], {
      env: Object.assign({}, process.env, { DISPLAY: display, MYS2836_READY_FILE: ready }),
      stdio: ['ignore', fs.openSync(log, 'w'), fs.openSync(log, 'a')],
    }));
    try {
      await waitFor(() => fs.existsSync(ready), 15000, 'test pattern painted');
    } catch (e) {
      const why = fs.existsSync(log) ? fs.readFileSync(log, 'utf8').slice(-400) : '';
      throw new Error(`测试画面未就绪：${e.message} ${why}`);
    }
  } else {
    throw new Error('缺少 python3 或 tools/verify/mys2836_pattern.py，无法生成有色彩的测试画面');
  }

  const relayLog = path.join(dir, 'relay.log');
  // 独立 settings 文件（任务卡要求）：绝不读写用户真实的
  // ~/.shell-remote/relay-settings.json——否则本脚本会覆盖运维的带宽配置。
  const settingsFile = path.join(dir, 'relay-settings.json');
  track('relay', spawn(deps.relay, [
    'relay', '--no-tls', '--bind', `127.0.0.1:${port}`, '--auth', cfg.TOKEN,
    '--settings-file', settingsFile,
  ], {
    // 排障需要时用 RELAY_LOG/AGENT_LOG 打开 debug（默认 info）。
    env: Object.assign({}, process.env, { RUST_LOG: process.env.RELAY_LOG || 'info' }),
    stdio: ['ignore', fs.openSync(relayLog, 'w'), fs.openSync(relayLog, 'a')],
  }));
  await waitFor(() => fs.existsSync(relayLog) && /listening/i.test(fs.readFileSync(relayLog, 'utf8')), 20000, 'relay listen');

  const agentLog = path.join(dir, 'agent.log');
  track('agent', spawn(deps.ui, [
    'agent', '--relay-url', base, '--key', cfg.TOKEN, '--session-id', 'mys2837',
    '--view', 'headless', '--desktop-capture', 'x11', '--desktop-codec', 'av1', '--desktop-fps', String(cfg.FPS),
    // LAN 直连监听端口（agent 通过 desktop:capabilities 的 lan_addrs 上报
    // 实际地址，浏览器自己探测）。0 在 agent 侧等于关闭，所以这里用负数表示
    // "不测直连"，其余情况给一个真实端口。
    ...(cfg.LAN_PORT >= 0 ? ['--desktop-lan-port', String(cfg.LAN_PORT)] : []),
  ], {
    env: Object.assign({}, process.env, { DISPLAY: display, RUST_LOG: process.env.AGENT_LOG || 'info' }),
    stdio: ['ignore', fs.openSync(agentLog, 'w'), fs.openSync(agentLog, 'a')],
  }));
  await waitFor(() => fs.existsSync(agentLog) && /agent session established/.test(fs.readFileSync(agentLog, 'utf8')), 25000, 'agent register');

  return { base, dir, display, pattern, relayLog, agentLog };
}

// ── 页面侧的度量代码（注入浏览器）─────────────────────────────────
// 只做观测，不改生产代码：包一层 VideoDecoder 记解码帧；画布采样算饱和度。
const PAGE_INIT = function (cfg) {
  try {
    sessionStorage.setItem('shell-remote-token', cfg.token);
    sessionStorage.setItem('shell-remote-auth', cfg.token);
  } catch (e) {}
  window.__frames = [];        // 每次解码输出一帧的时间戳
  window.__decodeErrors = [];
  const NativeVD = window.VideoDecoder;
  if (NativeVD) {
    const Wrapped = function (init) {
      const inner = new NativeVD({
        output: function (frame) {
          window.__frames.push(Date.now());
          try { init.output(frame); } catch (e) { try { frame.close(); } catch (e2) {} }
        },
        error: function (e) {
          // 带上当时的解码器配置串与已解帧数——"Decoding error." 单独一条
          // 无法区分"参数集不匹配"和"码流本身坏了"，排障必须看到上下文。
          window.__decodeErrors.push(
            String(e && e.message).slice(0, 120) +
            ' | cfg=' + (init && init.codec ? init.codec : '?') +
            ' | framesBefore=' + window.__frames.length);
          try { init.error(e); } catch (e2) {}
        },
      });
      return new Proxy(inner, {
        get(t, p) { const v = t[p]; return typeof v === 'function' ? v.bind(t) : v; },
        set(t, p, v) { t[p] = v; return true; },
      });
    };
    Wrapped.isConfigSupported = NativeVD.isConfigSupported && NativeVD.isConfigSupported.bind(NativeVD);
    window.VideoDecoder = Wrapped;
  }
  // 捕获 DesktopView 实例：LAN 断言要读 _transport / _lanAddrs。只包构造器，
  // 不改任何产品逻辑。
  try {
    const desc = Object.getOwnPropertyDescriptor(window, 'DesktopView');
    if (desc && desc.configurable !== false) {
      let ctor;
      Object.defineProperty(window, 'DesktopView', {
        configurable: true,
        get() { return ctor; },
        set(C) {
          const Wrapped = function () {
            const inst = new C();
            window.__dv = inst;
            return inst;
          };
          Wrapped.prototype = C.prototype;
          Object.setPrototypeOf(Wrapped, C);
          ctor = Wrapped;
        },
      });
    }
  } catch (e) { /* 拿不到实例时 LAN 断言会显式失败，不静默通过 */ }
  // 不屏蔽 RTCPeerConnection：_startLan 在 P2P 之前探测，屏蔽 P2P 反而
  // 改变了回退顺序。灰度与传输方式无关，保持浏览器默认即可。
};

// 画布平均饱和度（0 = 全灰）。用 64x64 降采样，抗单像素噪声。
const PAGE_SATURATION = function () {
  const c = document.getElementById('desktop-canvas');
  if (!c || !c.width || !c.height) return null;
  const t = document.createElement('canvas');
  t.width = 64; t.height = 64;
  const g = t.getContext('2d');
  g.drawImage(c, 0, 0, 64, 64);
  const d = g.getImageData(0, 0, 64, 64).data;
  let sum = 0;
  for (let i = 0; i < d.length; i += 4) {
    sum += Math.max(d[i], d[i + 1], d[i + 2]) - Math.min(d[i], d[i + 1], d[i + 2]);
  }
  return +(sum / (d.length / 4)).toFixed(2);
};

async function openSession(page, base) {
  await page.goto(base + '/');
  await page.goto(base + '/session');
  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-desktop-btn');
    return b && !b.disabled && !b.classList.contains('hidden');
  }, { timeout: 25000 });
  const already = await page.evaluate(() => {
    const c = document.getElementById('desktop-container');
    return !!(c && !c.classList.contains('hidden'));
  });
  if (!already) {
    await page.click('#toggle-desktop-btn');
    await page.waitForFunction(() => {
      const c = document.getElementById('desktop-container');
      return c && !c.classList.contains('hidden');
    }, { timeout: 20000 });
  }
}

// 等到"有新的解码帧"（相对 before 计数），返回耗时 ms 或 null。
async function waitNewFrame(page, before, deadlineMs) {
  const t0 = Date.now();
  while (Date.now() - t0 < deadlineMs) {
    const n = await page.evaluate(() => window.__frames.length);
    if (n > before) return Date.now() - t0;
    await sleep(150);
  }
  return null;
}

// 等到画布饱和度满足条件（目标模式真的上屏），而不是"固定 sleep 后采样
// 一次"。任务卡明确要求"每次目标状态的新帧 15 秒内恢复且持续增长、codec
// 组合不使用旧队列帧冒充目标模式"——固定 1.2s 后取一次样，采到的很可能
// 还是切换前的旧画面（AV1 1080p 单帧编码可达 100ms+，队列里还压着旧帧）。
//
// 返回 {ok, ms, sat}：在 deadline 内出现过满足条件的画面即 ok。
async function waitSaturation(page, want, deadlineMs) {
  // 要求**连续两次**采样都满足才判定收敛：切换过程中画布会短暂停在过渡值
  // （实测 LAN 路径第一次采样到 8.66、再采一次才是 0）。只认第一次命中会把
  // "正在变灰"当成"已经是目标模式"，等于放松了验收。
  const t0 = Date.now();
  let last = null;
  let prevOk = false;
  while (Date.now() - t0 < deadlineMs) {
    last = await page.evaluate(PAGE_SATURATION);
    const ok = last !== null && want(last);
    if (ok && prevOk) return { ok: true, ms: Date.now() - t0, sat: last };
    prevOk = ok;
    await sleep(200);
  }
  return { ok: false, ms: Date.now() - t0, sat: last };
}

async function sendGray(page, enabled, seq) {
  await page.evaluate((a) => {
    window.shellRemote.send('desktop:gray', { enabled: a.enabled, seq: a.seq });
  }, { enabled, seq });
}

async function sendCodec(page, codec, seq) {
  await page.evaluate((a) => {
    window.shellRemote.send('desktop:codec', { codec: a.codec, seq: a.seq });
  }, { codec, seq });
}

async function main() {
  const deps = preflight();
  if (!deps) { process.exit(2); }

  const chrome = resolveChrome(deps.playwright);
  if (!chrome) {
    console.error('FAIL 找不到 Chromium：设 CHROME=<可执行文件> 或执行 `npx playwright install chromium`。');
    process.exit(2);
  }
  console.log(`MYS-2836 运行时验证  chrome=${chrome}`);

  let stack;
  try {
    stack = await startStack(deps);
  } catch (e) {
    console.error('FAIL 无法准备验证环境（relay/agent/X11）：', e.message);
    stopAll();
    process.exit(2);
  }
  console.log(`relay=${stack.base}  日志目录=${stack.dir}`);

  const browser = await deps.playwright.chromium.launch({
    executablePath: chrome,
    args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'],
  });
  let exitCode = 0;
  try {
    const ctx = await browser.newContext();
    await ctx.addInitScript(PAGE_INIT, { token: cfg.TOKEN });
    const page = await ctx.newPage();
    // 记录打到 agent LAN 直连端口的请求（浏览器直连 http://<lan-ip>:<port>）。
    // 这是"直连路径真的被用上"的客观证据，不依赖页面私有字段。
    page.__lanHits = [];
    page.on('request', (r) => {
      const u = r.url();
      if (/:\d+\/agent\/desktop\/stream/.test(u) && !u.includes(new URL(stack.base).host)) {
        page.__lanHits.push(u);
      }
    });
    page.on('pageerror', (e) => console.log('  [pageerror]', String(e.message).slice(0, 160)));

    await openSession(page, stack.base);
    const firstFrames = await waitNewFrame(page, 0, 30000);
    if (!check('0. 桌面流已出画（前置条件）', firstFrames !== null, `首帧 ${firstFrames}ms`)) {
      console.log('  后续用例依赖真实解码，前置未满足 → 记为失败。');
      throw new Error('no first frame');
    }
    // 等画面稳定几帧再看基线饱和度（彩色）。
    await sleep(1500);
    const colorSat = await page.evaluate(PAGE_SATURATION);
    check('0b. 基线画面确实有色彩（否则灰度用例无意义）', colorSat !== null && colorSat > 2,
      `saturation=${colorSat}`);
    // 灰度阈值按实测彩色基线取相对值：AV1 monochrome 码流实际饱和度为 0，
    // 而 H.264 走"UV 填 128 + 有损编码"，低码率下会残留少量色度（实测 <3）。
    // 用"远低于彩色基线"作为判据，比写死一个绝对值更能反映"画面确实变灰"。
    const GRAY_MAX = Math.max(1.0, colorSat * 0.1);
    console.log(`    灰度判据：saturation < ${GRAY_MAX.toFixed(2)}（彩色基线 ${colorSat} 的 10%）`);

    // ── 用例 1：彩色 → 灰度 → 彩色，ROUNDS 轮 ────────────────────
    console.log(`\n[1] 彩色→灰度→彩色 ${cfg.ROUNDS} 轮（每轮 ${cfg.DEADLINE_MS}ms 内须有新解码帧）`);
    let seq = 100;
    let worstMs = 0;
    for (let round = 1; round <= cfg.ROUNDS; round++) {
      // → 灰度
      let before = await page.evaluate(() => window.__frames.length);
      await sendGray(page, true, ++seq);
      const onMs = await waitNewFrame(page, before, cfg.DEADLINE_MS);
      worstMs = Math.max(worstMs, onMs === null ? Infinity : onMs);
      check(`1.${round}a 第${round}轮 切灰度后出新解码帧`, onMs !== null,
        onMs === null ? '（超时：卡在拉流）' : `${onMs}ms`);
      if (onMs === null) continue; // 流已卡死，后续断言在这轮没有意义
      // 灰度必须**真的**上屏：等到画布无色度（而不是固定 sleep 后采一次样，
      // 那样采到的可能还是切换前的旧画面）。
      const grayVis = await waitSaturation(page, (v) => v < GRAY_MAX, cfg.DEADLINE_MS);
      check(`1.${round}b 第${round}轮 灰度态画面无色度（目标模式真的上屏）`, grayVis.ok,
        grayVis.ok ? `${grayVis.ms}ms 后 saturation=${grayVis.sat}`
          : `超时 saturation=${grayVis.sat}（阈值 ${GRAY_MAX.toFixed(2)}）`);

      // → 彩色
      before = await page.evaluate(() => window.__frames.length);
      await sendGray(page, false, ++seq);
      const offMs = await waitNewFrame(page, before, cfg.DEADLINE_MS);
      worstMs = Math.max(worstMs, offMs === null ? Infinity : offMs);
      check(`1.${round}c 第${round}轮 切回彩色后出新解码帧`, offMs !== null,
        offMs === null ? '（超时：卡在拉流）' : `${offMs}ms`);
      if (offMs === null) continue;
      const colorVis = await waitSaturation(page, (v) => v > colorSat * 0.5, cfg.DEADLINE_MS);
      check(`1.${round}d 第${round}轮 彩色态画面恢复色度（目标模式真的上屏）`, colorVis.ok,
        colorVis.ok ? `${colorVis.ms}ms 后 saturation=${colorVis.sat}`
          : `超时 saturation=${colorVis.sat}`);
    }
    if (Number.isFinite(worstMs)) {
      console.log(`    最慢一次切换恢复：${worstMs}ms（阈值 ${cfg.DEADLINE_MS}ms）`);
    }

    // ── 用例 2：重复设置相同 gray 是 no-op ───────────────────────
    console.log('\n[2] 重复设置相同 gray 不得打断流');
    // 当前是彩色（上一轮结尾已关灰度）。重复发 enabled:false 应当无扰动。
    let before = await page.evaluate(() => window.__frames.length);
    for (let i = 0; i < 4; i++) {
      await sendGray(page, false, ++seq);
      await sleep(120);
    }
    const noopMs = await waitNewFrame(page, before, 3000);
    check('2.1 重复相同 gray 后仍在正常出帧（无重建/无长时间空档）', noopMs !== null,
      noopMs === null ? '（3s 内无新帧）' : `${noopMs}ms 内继续出帧`);

    // ── 用例 3：AV1 → H264 → AV1 与灰度组合不残留旧状态 ─────────
    console.log('\n[3] AV1→H264→AV1 × 灰度 不残留旧状态');
    before = await page.evaluate(() => window.__frames.length);
    await sendCodec(page, 'h264', ++seq);
    const h264Ms = await waitNewFrame(page, before, cfg.DEADLINE_MS);
    check('3.1 切 H264 后出新解码帧', h264Ms !== null, h264Ms === null ? '超时' : `${h264Ms}ms`);

    if (h264Ms !== null) {
      // H264 下开灰度：像素级 UV 置 128，应立即变灰但仍出帧。
      before = await page.evaluate(() => window.__frames.length);
      await sendGray(page, true, ++seq);
      const h264GrayMs = await waitNewFrame(page, before, cfg.DEADLINE_MS);
      check('3.2 H264 + 灰度 仍出新解码帧', h264GrayMs !== null, h264GrayMs === null ? '超时' : `${h264GrayMs}ms`);
      if (h264GrayMs !== null) {
        const vis = await waitSaturation(page, (v) => v < GRAY_MAX, cfg.DEADLINE_MS);
        check('3.3 H264 + 灰度 画面无色度（目标模式真的上屏）', vis.ok,
          vis.ok ? `${vis.ms}ms 后 saturation=${vis.sat}`
            : `超时 saturation=${vis.sat}（阈值 ${GRAY_MAX.toFixed(2)}；H264 走 UV=128 有损编码）`);
      }

      // 灰度仍开着时切回 AV1 —— 这是旧实现失步的经典路径。
      before = await page.evaluate(() => window.__frames.length);
      await sendCodec(page, 'av1', ++seq);
      const backMs = await waitNewFrame(page, before, cfg.DEADLINE_MS);
      check('3.4 灰度开着切回 AV1 后出新解码帧', backMs !== null, backMs === null ? '超时' : `${backMs}ms`);

      if (backMs !== null) {
        const vis = await waitSaturation(page, (v) => v < GRAY_MAX, cfg.DEADLINE_MS);
        check('3.5 灰度仍开 → AV1 单色（状态未丢）', vis.ok,
          vis.ok ? `${vis.ms}ms 后 saturation=${vis.sat}` : `超时 saturation=${vis.sat}`);

        // 关灰度：必须回到彩色且继续出帧（不残留单色状态）。
        before = await page.evaluate(() => window.__frames.length);
        await sendGray(page, false, ++seq);
        const finalMs = await waitNewFrame(page, before, cfg.DEADLINE_MS);
        check('3.6 关灰度后出新解码帧', finalMs !== null, finalMs === null ? '超时' : `${finalMs}ms`);
        if (finalMs !== null) {
          const vis = await waitSaturation(page, (v) => v > colorSat * 0.5, cfg.DEADLINE_MS);
          check('3.7 关灰度后画面恢复彩色（无残留单色）', vis.ok,
            vis.ok ? `${vis.ms}ms 后 saturation=${vis.sat}` : `超时 saturation=${vis.sat}`);
        }
      }
    }

    const errs = await page.evaluate(() => window.__decodeErrors);
    check('4. 全程无解码器报错', errs.length === 0, errs.length ? errs.slice(0, 3).join(' | ') : '');

    // ── 直连（LAN）路径（任务卡要求：可用直连路径必须验证）──────────
    // 换一个连接周期重新进桌面：清掉钉住 relay 的标记，让 _startLan 真的去
    // 探测 agent 上报的直连地址。断言 transport 变成 'lan'，再跑一轮灰度
    // 切换，证明直连路径上灰度同样能恢复。
    console.log('\n[4] LAN 直连路径');
    if (cfg.LAN_PORT < 0) {
      skip('LAN 直连路径的灰度切换', 'LAN_PORT<0：调用方显式关闭（默认开启）');
    } else {
      const beforeLan = await page.evaluate(() => window.__frames.length);
      await page.evaluate(() => {
        // 清 P2P 拒绝标记 + 强制下一轮 connect 重新探测 LAN。
        if (window.__dv) { window.__dv._p2pRejected = false; window.__dv._p2pRejectedAt = 0; }
      });
      await sendGray(page, false, ++seq);
      await sleep(500);
      const lanAddrs = await page.evaluate(() => window._srLanAddrs || null);
      const lanHits = page.__lanHits || [];
      const transport = lanHits.length > 0 ? 'lan' : null;
      check('4.1 agent 上报了 LAN 直连地址', Array.isArray(lanAddrs) && lanAddrs.length > 0,
        JSON.stringify(lanAddrs));
      check('4.2 浏览器实际走 LAN 直连（transport=lan）', transport === 'lan',
        `transport=${transport}`);
      if (transport === 'lan') {
        const lanMs = await waitNewFrame(page, beforeLan, cfg.DEADLINE_MS);
        check('4.3 LAN 直连路径出新解码帧', lanMs !== null, lanMs === null ? '超时' : `${lanMs}ms`);
        // 直连路径上再做一次灰度往返，证明该路径也能切换。
        const b2 = await page.evaluate(() => window.__frames.length);
        await sendGray(page, true, ++seq);
        const g2 = await waitNewFrame(page, b2, cfg.DEADLINE_MS);
        check('4.4 LAN 直连路径切灰度后出新解码帧', g2 !== null, g2 === null ? '超时' : `${g2}ms`);
        if (g2 !== null) {
          const vis = await waitSaturation(page, (v) => v < GRAY_MAX, cfg.DEADLINE_MS);
          check('4.5 LAN 直连路径灰度画面无色度', vis.ok,
            vis.ok ? `${vis.ms}ms 后 saturation=${vis.sat}` : `超时 saturation=${vis.sat}`);
        }
        await sendGray(page, false, ++seq);
        await sleep(500);
      }
    }
  } catch (e) {
    console.error('FAIL 验证中断：', e.message);
    if (fails === 0) fails += 1;
    checks += 1;
  } finally {
    await browser.close().catch(() => {});
    if (!cfg.KEEP) stopAll();
    else console.log(`KEEP=1：进程与日志保留在 ${stack.dir}`);
  }

  // ── 摘要 ─────────────────────────────────────────────────────
  console.log('\n──────── 摘要 ────────');
  for (const s of skipped) console.log('SKIP ' + s);
  console.log(`${checks - fails}/${checks} 通过，${fails} 失败，${skipped.length} 跳过`);
  exitCode = fails === 0 ? 0 : 1;
  if (fails > 0) console.log('日志目录（保留排障）：' + stack.dir);
  process.exit(exitCode);
}

main().catch((e) => {
  console.error('FAIL', e && e.stack ? e.stack : e);
  stopAll();
  process.exit(1);
});
