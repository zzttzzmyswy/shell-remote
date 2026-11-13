#!/usr/bin/env node
'use strict';
/**
 * MYS-2842 桌面运行时验收（AV1 灰度 / 编码切换 / 传输路径）。
 *
 * 与旧版（MYS-2836 阶段 2）的区别，也就是本次返修的全部内容：
 *
 *  1. **传输路径分开验证**。MODE=relay|lan|mse|all（默认 all 顺序执行），
 *     每个模式起**独立的 relay/agent/Xvfb + 独立浏览器上下文**：
 *       - relay：禁用 RTCPeerConnection 与 LAN 候选（不传 --desktop-lan-port），
 *         断言实例自报 transport=relay、LAN 字节为 0、relay 侧确实有字节；
 *       - lan：开启 --desktop-lan-port，断言 transport=lan、LAN 返回 200、
 *         **并且真的从 LAN 端点消费到媒体字节**（"发起过请求"不算）。
 *       - mse：注入脚本把 window.VideoDecoder 置空强制退 MSE，agent 用 h264，
 *         断言实际 addSourceBuffer 的 mime 是 avc1（不是只看命令发出去了）。
 *
 *  2. **判定逻辑与 I/O 分离**。所有判据在 `mys2836_harness.js` 里，由
 *     `mys2836_harness.test.js` 用可控输入验证；本文件只负责把浏览器/进程
 *     真实观测值喂给那套判据。这样"验收工具自己"也有可测契约。
 *
 *  3. **一条命令一个 15 秒截止时间**。目标 codec + 目标画面 + 新解码帧 +
 *     持续增长全部在同一个窗口内确认（不会串成 30 秒），恢复时间含 codec
 *     真正生效的时刻。
 *
 *  4. **同值 gray 不得重建**，并持续产帧；错误统计覆盖该模式全部用例。
 *
 *  5. **观测真实实例**。旧 PAGE_INIT 在 DesktopView 尚未定义时只"看 desc 存
 *     不存在"就安装包装，结果抓不到实例；现在改为始终安装 setter（首次定义
 *     时立刻生效），并在 setter 里同时包装 VideoDecoder / RTCPeerConnection，
 *     每个实例都记录自己的 transport 与 LAN 字节。
 *
 * 只做观测与测试路径隔离：**不修改任何产品源码**，不伪造解码数据。
 *
 * 用法：
 *   node tools/verify/verify_mys2836_runtime.js                # relay → lan → mse
 *   MODE=relay node tools/verify/verify_mys2836_runtime.js
 *   MODE=lan   node tools/verify/verify_mys2836_runtime.js
 *   MODE=mse   node tools/verify/verify_mys2836_runtime.js
 *
 * 环境变量：
 *   BIN          二进制目录（含 shell-remote 与 shell-remote-ui；默认 target/debug）
 *   MODE         relay|lan|mse|all（默认 all）
 *   WIDTH/HEIGHT 测试画面分辨率（默认 1280x720，任务卡禁止为过测降低）
 *   FPS          编码帧率（默认 30）
 *   ROUNDS       彩色→灰度→彩色 轮数（默认 5）
 *   DEADLINE_MS  单条命令预算（默认 15000）
 *   GROWTH_MS    目标态后持续产帧观察窗口（默认 1000）
 *   CHROME       Chromium 可执行文件（默认用 playwright 自带那份）
 *   KEEP=1       保留进程与日志目录
 *   OUT          结果 JSON 输出路径（默认写到日志目录 result.json）
 *
 * 退出码：0 全通过；1 有失败；2 环境不满足（缺依赖/二进制/X11）。
 */

const { spawn, spawnSync } = require('child_process');
const crypto = require('crypto');
const fs = require('fs');
const net = require('net');
const os = require('os');
const path = require('path');

const H = require('./mys2836_harness.js');
const A = require('./mys2836_runtime_adapter.js');

const ROOT = path.resolve(__dirname, '..', '..');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const cfg = {
  BIN: process.env.BIN || path.join(ROOT, 'target', 'debug'),
  MODE: String(process.env.MODE || 'all').toLowerCase(),
  WIDTH: Number(process.env.WIDTH || 1280),
  HEIGHT: Number(process.env.HEIGHT || 720),
  FPS: Number(process.env.FPS || 30),
  ROUNDS: Number(process.env.ROUNDS || 5),
  DEADLINE_MS: Number(process.env.DEADLINE_MS || 15000),
  GROWTH_MS: Number(process.env.GROWTH_MS || 1000),
  SETTLE_MS: Number(process.env.SETTLE_MS || 300),
  POLL_MS: Number(process.env.POLL_MS || 120),
  CHROME: process.env.CHROME || '',
  KEEP: process.env.KEEP === '1',
  OUT: process.env.OUT || '',
};

const MODES = ['relay', 'lan', 'mse'];
const now = () => Date.now();
const logs = [];
function log(...a) {
  const line = a.join(' ');
  logs.push(line);
  console.log(line);
}

// ── 进程管理：只清理本脚本自己记录在案的 PID，绝不按名字杀 ──────────
const procs = [];
function track(name, child) {
  procs.push({ name, child, pid: child.pid });
  return child;
}
function alive(pid) {
  try { process.kill(pid, 0); return true; } catch (e) { return false; }
}
async function stopAll() {
  for (const p of procs) { try { process.kill(p.pid, 'SIGTERM'); } catch (e) { /* 已退出 */ } }
  const deadline = Date.now() + 2500;
  while (Date.now() < deadline && procs.some((p) => alive(p.pid))) await sleep(100);
  for (const p of procs) {
    if (alive(p.pid)) { try { process.kill(p.pid, 'SIGKILL'); } catch (e) { /* ignore */ } }
  }
  procs.length = 0;
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
  const t0 = now();
  while (now() - t0 < timeoutMs) {
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
    console.error(`FAIL 缺少 relay 二进制：${relay}\n  用 BIN=<构建输出目录> 指定。`);
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

function resolveChrome(playwright) {
  if (cfg.CHROME) return cfg.CHROME;
  try {
    const p = playwright.chromium.executablePath();
    if (p && fs.existsSync(p)) return p;
  } catch (e) { /* 落到手工探测 */ }
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

// 找真正空闲的 X display：既无 lock 文件也无活着的 socket。
function pickFreeDisplay() {
  for (let n = 40; n < 200; n++) {
    if (fs.existsSync(`/tmp/.X${n}-lock`)) continue;
    if (fs.existsSync(`/tmp/.X11-unix/X${n}`)) continue;
    return ':' + n;
  }
  return '';
}

function displayUsable(display) {
  return spawnSync('sh', ['-c', `DISPLAY=${display} xdpyinfo >/dev/null 2>&1`]).status === 0;
}

// ── 注入页面的观测代码（只观测，不改产品逻辑）────────────────────
// 通过 addInitScript 在每个文档最早期执行。关键修正：DesktopView 的 setter
// **无条件安装**——旧实现用 `Object.getOwnPropertyDescriptor` 判断"desc 存
// 不存在"，而 PAGE_INIT 早于 desktop.js 执行，那时 DesktopView 尚未定义，
// 于是包装从未安装、`window.__dv` 永远是 undefined，LAN 断言只能靠请求
// 推断（这正是上一轮把 LAN 掩盖成 Relay 的原因之一）。
const PAGE_INIT = function (opts) {
  const cfgIn = opts || {};
  try {
    sessionStorage.setItem('shell-remote-token', cfgIn.token);
    sessionStorage.setItem('shell-remote-auth', cfgIn.token);
  } catch (e) { /* 隐私模式 */ }

  const S = {
    instances: [],
    frames: 0,               // WebCodecs 累计解码帧（真实解码输出）
    framesSource: 'webcodecs',
    decodeErrors: [],
    playerErrors: [],
    configs: [],             // 每次 configure / addSourceBuffer 的记录（尾部保留）
    configCount: 0,          // 累计条数：configs 会截断，判定必须用这个
    lan: [],                 // LAN 端点响应记录
    lanBytes: 0,
    wsBytes: 0,
    relayBytes: 0,
    rtcAvailable: typeof RTCPeerConnection !== 'undefined',
    transportSamples: [],
    // MSE 真实呈现帧。presented 为 null = 该环境没有可用的真实帧指标，
    // 判定层据此明确失败（绝不用 currentTime×FPS 推算补位）。
    // 流生命周期**不放在页面里**（页面内包装 fetch 会与产品抢同一个
    // ReadableStream），改由 Playwright 请求事件观测。
    mse: { presented: null, source: 'none' },
  };
  window.__mys2842 = S;

  const recordConfig = function (kind, codec, extra) {
    const rec = {
      kind,
      codec: (function () {
        const s = String(codec || '').toLowerCase();
        if (s.indexOf('av01') >= 0) return 'av1';
        if (s.indexOf('avc1') >= 0 || s.indexOf('avc3') >= 0) return 'h264';
        return 'other';
      })(),
      config: String(codec || ''),
      at: Date.now(),
      accepted: true,
    };
    if (extra) Object.assign(rec, extra);
    S.configs.push(rec);
    S.configCount += 1;
    // 只保留尾部若干条供排障读取；判定（代次）走 configCount，不受截断影响。
    if (S.configs.length > 64) S.configs.splice(0, S.configs.length - 64);
    return rec;
  };

  // ── WebCodecs 观测 ──────────────────────────────────────────────
  const NativeVD = window.VideoDecoder;
  if (cfgIn.disableWebCodecs) {
    // MSE 模式：必须让产品走到 window.DesktopViewMse 分支。
    try { window.VideoDecoder = undefined; } catch (e) {}
    try { delete window.VideoDecoder; } catch (e) {}
  } else if (NativeVD) {
    const Wrapped = function (init) {
      const rec = { configure: null };
      const inner = new NativeVD({
        output: function (frame) {
          S.frames += 1;
          try { init.output(frame); } catch (e) { try { frame.close(); } catch (e2) {} }
        },
        error: function (e) {
          S.decodeErrors.push(
            String((e && e.message) || e).slice(0, 160) +
            ' | cfg=' + (rec.configure ? rec.configure.codec : '?') +
            ' | framesBefore=' + S.frames);
          try { init.error(e); } catch (e2) {}
        },
      });
      const origConfigure = inner.configure.bind(inner);
      return new Proxy(inner, {
        get(t, p) {
          if (p === 'configure') {
            return function (c) {
              rec.configure = c;
              const r = recordConfig('webcodecs', c && c.codec, {
                hasDescription: !!(c && c.description),
              });
              try {
                const out = origConfigure(c);
                return out;
              } catch (e) {
                r.accepted = false;
                r.error = String(e.message || e);
                throw e;
              }
            };
          }
          const v = t[p];
          return typeof v === 'function' ? v.bind(t) : v;
        },
        set(t, p, v) { t[p] = v; return true; },
      });
    };
    Wrapped.isConfigSupported = NativeVD.isConfigSupported && NativeVD.isConfigSupported.bind(NativeVD);
    window.VideoDecoder = Wrapped;
  }

  // ── MSE 观测：真实呈现帧 + 流生命周期 ───────────────────────────
  // 帧数**只**取浏览器的真实指标：
  //   - requestVideoFrameCallback（呈现帧回调，最接近"真的上屏了"）
  //   - 否则 getVideoPlaybackQuality().totalVideoFrames（解码/呈现质量计数）
  // 两者都不可用 → mse.presented 保持 null，判定层据此明确失败。
  // 绝不退回 currentTime×FPS 之类的推算：播放时间前进但一帧都没呈现时，
  // 推算值照样会涨，那就等于伪造证据。
  (function instrumentMse() {
    const install = function () {
      const v = document.getElementById('desktop-video');
      if (!v || v.__mys2842Observed) return;
      v.__mys2842Observed = true;
      let presented = 0;
      // 同时采样浏览器自己的质量计数，用于**交叉校验** rVFC：
      // 后台/不可见时 rVFC 会被节流甚至暂停，若两个指标不同步增长，
      // 就说明"没涨"是 rVFC 被节流而不是真的没出帧——这时判失败会是误报。
      // 两个指标都不可用才是真的"不可验证"。
      S.mse.quality = null;
      if (typeof v.getVideoPlaybackQuality === 'function') {
        setInterval(function () {
          try {
            const q = v.getVideoPlaybackQuality();
            if (q && typeof q.totalVideoFrames === 'number') S.mse.quality = q.totalVideoFrames;
          } catch (e) { /* ignore */ }
        }, 200);
      }
      if (typeof v.requestVideoFrameCallback === 'function') {
        S.mse.source = 'requestVideoFrameCallback';
        S.mse.presented = 0;
        const onFrame = function () {
          presented += 1;
          S.mse.presented = presented;
          try { v.requestVideoFrameCallback(onFrame); } catch (e) { /* 视图已销毁 */ }
        };
        try { v.requestVideoFrameCallback(onFrame); } catch (e) { /* 降级到质量计数 */ }
      } else if (typeof v.getVideoPlaybackQuality === 'function') {
        S.mse.source = 'getVideoPlaybackQuality';
        S.mse.presented = 0;
        setInterval(function () {
          try {
            const q = v.getVideoPlaybackQuality();
            if (q && typeof q.totalVideoFrames === 'number') S.mse.presented = q.totalVideoFrames;
          } catch (e) { /* ignore */ }
        }, 200);
      } else {
        // 明确"不可验证"：不设 presented，让判定层失败而不是伪报通过。
        S.mse.source = 'none';
      }
    };
    // video 元素在 session.html 里已存在；仍在 DOMContentLoaded 后再兜一次，
    // 防止脚本先于 DOM 解析完成。
    install();
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', install, { once: true });
    }
  })();

  // 流生命周期**不在这里**观测：页面内包装 fetch 会与产品自己的
  // getReader() 抢同一个 body（ReadableStream 只能有一个 reader），
  // 直接把流打死——实测导致 LAN 探测失败退回 p2p、MSE 永远没有首帧。
  // 改为在 Playwright 层用 request/requestfinished 事件统计（见 runMode），
  // 完全不碰响应体。

  // ── MSE 观测：记录真实 addSourceBuffer 的 mime ──────────────────
  if (typeof MediaSource !== 'undefined' && MediaSource.prototype.addSourceBuffer) {
    const origAdd = MediaSource.prototype.addSourceBuffer;
    MediaSource.prototype.addSourceBuffer = function (mime) {
      recordConfig('mse', mime);
      const sb = origAdd.call(this, mime);
      try {
        sb.addEventListener('error', function () {
          S.playerErrors.push('SourceBuffer error | mime=' + mime);
        });
      } catch (e) { /* 老浏览器 */ }
      return sb;
    };
  }

  // ── 网络字节归属：LAN 端点 vs relay ─────────────────────────────
  // 用 PerformanceObserver 观察资源大小是"事后"的；这里同时挂 fetch 包装，
  // 直接量 LAN 流实际读到的字节（探测成功 + 后续 body 读取都算）。
  const isLanUrl = function (url) {
    try {
      const u = new URL(url, location.href);
      if (u.origin === location.origin) return false;
      return /\/agent\/desktop\/stream/.test(u.pathname);
    } catch (e) { return false; }
  };
  const origFetch = window.fetch;
  if (origFetch) {
    window.fetch = function (input, init2) {
      const url = typeof input === 'string' ? input : (input && input.url);
      const lan = isLanUrl(url);
      return origFetch.apply(this, arguments).then(function (resp) {
        if (lan) S.lan.push({ ok: !!resp.ok, status: resp.status, url: String(url) });
        if (!lan || !resp.ok || !resp.body) return resp;
        // 分流 body：每读一块累加 LAN 字节，同时原样交给调用方。
        const src = resp.body.getReader();
        const counted = new ReadableStream({
          pull: async function (ctrl) {
            const r = await src.read();
            if (r.done) { ctrl.close(); return; }
            S.lanBytes += r.value ? r.value.byteLength : 0;
            ctrl.enqueue(r.value);
          },
          cancel: function (reason) { try { return src.cancel(reason); } catch (e) { /* ignore */ } },
        });
        return new Response(counted, {
          status: resp.status, statusText: resp.statusText, headers: resp.headers,
        });
      });
    };
  }
  const origWS = window.WebSocket;
  if (origWS) {
    const WrappedWS = function (url, protocols) {
      const ws = protocols === undefined ? new origWS(url) : new origWS(url, protocols);
      ws.addEventListener('message', function (ev) {
        if (typeof ev.data === 'string') return;
        S.wsBytes += (ev.data.byteLength || ev.data.size || 0);
      });
      return ws;
    };
    WrappedWS.prototype = origWS.prototype;
    Object.setPrototypeOf(WrappedWS, origWS);
    window.WebSocket = WrappedWS;
  }

  // ── DesktopView 实例捕获：**无条件**安装 setter ────────────────
  // 无论此刻 DesktopView 是否已定义都安装；首次被赋值时立刻拿到真实构造器
  // 并包装它，之后每个实例都记进 S.instances。
  (function installDesktopViewHook() {
    let ctor;
    const inner = function () {
      const inst = new ctor();
      S.instances.push(inst);
      window.__dv = inst; // 兼容既有脚本的排障入口
      return inst;
    };
    let attached = false;
    const attach = function (C) {
      if (attached || typeof C !== 'function') return;
      attached = true;
      ctor = C;
      inner.prototype = C.prototype;
      try { Object.setPrototypeOf(inner, C); } catch (e) { /* ignore */ }
    };
    try {
      Object.defineProperty(window, 'DesktopView', {
        configurable: true,
        enumerable: true,
        get: function () {
          // 已捕获则返回包装器（新实例都会被记录）；否则返回原构造器。
          return attached ? inner : ctor;
        },
        set: function (C) {
          attach(C);
          // 保留原构造器可读：未被包装前 get 返回它，行为不变。
          if (!attached) ctor = C;
        },
      });
    } catch (e) {
      // 兜底：属性不可重定义时，至少记录到 window.__dvInitError。
      window.__mys2842HookError = String(e && e.message);
    }
  })();

  // 定期采样实例 transport（LAN 断言要"全程"证据，不只看首尾两点）。
  // **只在已连上时采样**：产品的 _transport 初值就是 'relay'，连上之前
  // 采到的那个 "relay" 是未连接的默认值，不是"从 lan 掉回 relay"，把它
  // 计入会得到假失败（实测 LAN 模式首采样就是 relay）。
  const sampleTransport = function () {
    try {
      const vs = S.instances;
      if (!vs.length) return;
      const v = vs[vs.length - 1];
      if (!v || !v.connected) return;
      const t = v._transport ? String(v._transport) : 'unknown';
      if (S.transportSamples[S.transportSamples.length - 1] !== t) S.transportSamples.push(t);
    } catch (e) { /* ignore */ }
  };
  setInterval(sampleTransport, 250);

  // 页面级未捕获错误也计入播放器错误（MSE 的 video 错误另由事件捕获）。
  window.addEventListener('error', function (e) {
    if (e && e.message) S.playerErrors.push(String(e.message).slice(0, 160));
  });
};

// 画布饱和度采样、MSE 真实帧指标、session 适配器都在
// `mys2836_runtime_adapter.js` 里——那是运行时真正跑的那份接线，同时被
// `mys2836_harness.test.js` 用假 page 覆盖（第一次验收退回指出：只测纯逻辑
// 抓不到接线缺失）。

// ── 起一套独立的 relay/agent/Xvfb ────────────────────────────────
async function startStack(deps, mode) {
  const relayPort = await freePort();
  const base = `http://127.0.0.1:${relayPort}`;
  const token = 'sr2842-' + crypto.randomBytes(6).toString('hex');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `mys2842-${mode}-`));

  // 两个**独立**的 Xvfb：agent 只抓 displayA（上面只有确定性测试画面），
  // 浏览器跑在 displayB 上。这是必须的——把浏览器和被抓屏幕放在同一
  // display，浏览器窗口本身就会被采进画面：实测彩色基线从 ~127 掉到 ~14，
  // 灰度判据（基线的 10%）随之失去意义，等于放松了验收。
  const displays = {};
  for (const label of ['capture', 'browser']) {
    // 逐个起：后一个必须在**前一个的 lock 文件已存在**之后再挑号，否则
    // pickFreeDisplay 会把同一号返回两次。
    const d = pickFreeDisplay();
    if (!d) throw new Error('没有空闲 X display（40..199 全部被占用）');
    const lf = path.join(dir, `xvfb-${label}.log`);
    track(`xvfb-${label}`, spawn('Xvfb', [
      d, '-screen', '0', `${cfg.WIDTH}x${cfg.HEIGHT}x24`, '-extension', 'GLX',
    ], { stdio: ['ignore', fs.openSync(lf, 'w'), fs.openSync(lf, 'a')] }));
    await waitFor(() => displayUsable(d), 15000, `Xvfb ${d} (${label}) ready`);
    displays[label] = d;
  }
  const display = displays.capture;          // agent 抓这块
  const browserDisplay = displays.browser;   // 浏览器跑这块

  // 确定性彩色测试画面（有明确色度 + 持续运动），只画在抓屏 display 上。
  const patternScript = path.join(__dirname, 'mys2836_pattern.py');
  if (!fs.existsSync(patternScript)) throw new Error('缺少 tools/verify/mys2836_pattern.py');
  if (spawnSync('sh', ['-c', 'command -v python3']).status !== 0) throw new Error('缺少 python3（测试画面依赖 tkinter）');
  const ready = path.join(dir, 'pattern.ready');
  const patternLog = path.join(dir, 'pattern.log');
  track('pattern', spawn('python3', [patternScript, String(cfg.WIDTH), String(cfg.HEIGHT)], {
    env: Object.assign({}, process.env, { DISPLAY: display, MYS2836_READY_FILE: ready }),
    stdio: ['ignore', fs.openSync(patternLog, 'w'), fs.openSync(patternLog, 'a')],
  }));
  await waitFor(() => fs.existsSync(ready), 15000, 'test pattern painted');

  const relayLog = path.join(dir, 'relay.log');
  // 独立 settings 文件：绝不读写用户真实 ~/.shell-remote/relay-settings.json
  const settingsFile = path.join(dir, 'relay-settings.json');
  track('relay', spawn(deps.relay, [
    'relay', '--no-tls', '--bind', `127.0.0.1:${relayPort}`, '--auth', token,
    '--settings-file', settingsFile,
  ], {
    env: Object.assign({}, process.env, { RUST_LOG: process.env.RELAY_LOG || 'info' }),
    stdio: ['ignore', fs.openSync(relayLog, 'w'), fs.openSync(relayLog, 'a')],
  }));
  await waitFor(() => fs.existsSync(relayLog) && /listening/i.test(fs.readFileSync(relayLog, 'utf8')),
    20000, 'relay listen');

  // 每个模式独立 agent：relay 模式不传 --desktop-lan-port（=不监听 LAN），
  // lan 模式给真实端口；mse 用 h264（MSE 只支持受支持的 H264）。
  const agentArgs = [
    'agent', '--relay-url', base, '--key', token,
    '--session-id', 'mys2842' + mode.slice(0, 2),
    '--view', 'headless', '--desktop-capture', 'x11',
    '--desktop-codec', mode === 'mse' ? 'h264' : 'av1',
    '--desktop-fps', String(cfg.FPS),
  ];
  let lanPort = 0;
  if (mode === 'lan') {
    lanPort = await freePort();
    agentArgs.push('--desktop-lan-port', String(lanPort));
  }
  const agentLog = path.join(dir, 'agent.log');
  track('agent', spawn(deps.ui, agentArgs, {
    env: Object.assign({}, process.env, { DISPLAY: display, RUST_LOG: process.env.AGENT_LOG || 'info' }),
    stdio: ['ignore', fs.openSync(agentLog, 'w'), fs.openSync(agentLog, 'a')],
  }));
  await waitFor(() => fs.existsSync(agentLog) && /agent session established/.test(fs.readFileSync(agentLog, 'utf8')),
    25000, 'agent register');
  if (mode === 'lan') {
    await waitFor(() => /LAN desktop stream server up/.test(fs.readFileSync(agentLog, 'utf8')),
      10000, 'LAN server up');
  }

  return { base, token, dir, display, browserDisplay, relayLog, agentLog, lanPort };
}

// ── 打开会话并进桌面视图 ─────────────────────────────────────────
async function openDesktop(page, base) {
  await page.goto(base + '/');
  await page.goto(base + '/session');
  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-desktop-btn');
    return b && !b.classList.contains('hidden');
  }, { timeout: 25000 });
  // 能力快照到达后才 enable；等按钮可点（桌面可用）。
  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-desktop-btn');
    return b && !b.disabled;
  }, { timeout: 25000 });
  const already = await page.evaluate(() => {
    const c = document.getElementById('desktop-container');
    return !!(c && !c.classList.contains('hidden'));
  });
  if (!already) {
    await page.click('#toggle-desktop-btn');
    await page.waitForFunction(() => {
      const c = document.getElementById('desktop-container');
      return !c || !c.classList.contains('hidden');
    }, { timeout: 25000 });
  }
}

/**
 * 等首帧。WebCodecs 看真实解码帧计数；MSE 看真实呈现帧指标
 * （requestVideoFrameCallback / totalVideoFrames，见适配层的 MSE 观测）。
 * 返回 boolean。
 */
async function waitFirstFrame(page, mode) {
  try {
    await page.waitForFunction((m) => {
      if (m === 'mse') {
        const S = window.__mys2842 || {};
        const mm = S.mse || {};
        if (typeof mm.presented === 'number') return mm.presented > 0;
        const v = document.getElementById('desktop-video');
        return !!(v && v.readyState >= 2 && v.videoWidth > 0);
      }
      const S = window.__mys2842 || {};
      return (S.frames || 0) > 0;
    }, mode, { timeout: 40000 });
    return true;
  } catch (e) {
    // MSE 下若真实呈现帧指标不可用，退一步用 readyState 作为"有画面"的前置判据；
    // 但后续的 frames() 仍返回 null（不可验证），绝不用时间推算补位。
    if (mode === 'mse') {
      try {
        return await page.evaluate(() => {
          const v = document.getElementById('desktop-video');
          return !!(v && v.readyState >= 2 && v.videoWidth > 0);
        });
      } catch (e2) { return false; }
    }
    return false;
  }
}

/** 确保 MSE 播放器在播（自动播放策略可能把它留在 paused）。 */
async function playVideo(page) {
  await page.evaluate(async () => {
    const v = document.getElementById('desktop-video');
    if (v && v.paused) { try { await v.play(); } catch (e) { /* 自动播放策略 */ } }
  });
}

/** 发一条桌面控制命令（与产品 UI 走同一个 shellRemote.send）。 */
async function sendCommand(page, cmd) {
  await page.evaluate((c) => {
    const seq = (window.__cmdSeq = (window.__cmdSeq || 0) + 1);
    const payload = { seq };
    if (c.type === 'desktop:gray') payload.enabled = !!c.enabled;
    if (c.type === 'desktop:codec') payload.codec = c.codec;
    window.shellRemote.send(c.type, payload);
  }, cmd);
}

/** 页面侧的编解码能力探测（隔离是否真的生效取实时状态，不用脚本自记）。 */
async function probeSupport(page) {
  return page.evaluate(() => ({
    h264: typeof MediaSource !== 'undefined'
      && MediaSource.isTypeSupported('video/mp4; codecs="avc1.42E01E"'),
    webcodecs: typeof VideoDecoder !== 'undefined',
    rtc: typeof RTCPeerConnection !== 'undefined',
  }));
}

// ── 单模式执行 ───────────────────────────────────────────────────
async function runMode(deps, chrome, mode) {
  log(`\n════ MODE=${mode} ════`);
  const stack = await startStack(deps, mode);
  const res = {
    mode,
    relay: stack.base,
    logDir: stack.dir,
    transport: null,
    checks: [],
    passed: 0,
    failed: 0,
    skipped: 0,
    skippedList: [],
    exitCode: 1,
    metrics: {},
  };
  let browser = null;
  try {
    browser = await deps.playwright.chromium.launch({
      executablePath: chrome,
      headless: false, // WebCodecs / canvas 采样需要真实合成器
      // 浏览器跑在**自己的** display 上（不是 agent 抓屏那块），否则浏览器
      // 窗口会被采进远程画面，把彩色基线稀释掉。两者都在本模式自己的
      // Xvfb 里，不碰机器上其它任务的 display。
      env: Object.assign({}, process.env, { DISPLAY: stack.browserDisplay }),
      args: [
        '--no-sandbox',
        '--autoplay-policy=no-user-gesture-required',
        // relay 模式：禁用 P2P，确保不会用 p2p 掩盖 relay
        ...(mode === 'relay' ? ['--disable-features=WebRtcHideLocalIpsWithMdns'] : []),
      ],
    });
    const ctx = await browser.newContext({ viewport: { width: 1400, height: 900 } });
    await ctx.addInitScript(PAGE_INIT, {
      token: stack.token,
      disableWebCodecs: mode === 'mse',
    });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => log(`  [pageerror] ${String(e.message).slice(0, 160)}`));
    page.on('console', (m) => {
      if (m.type() === 'error') log(`  [console.error] ${m.text().slice(0, 200)}`);
    });

    // relay 模式：砍掉两条回退路径（任务卡"Relay 禁用 LAN/P2P 回退候选"）。
    // 这是**测试隔离**，不修改产品源码：
    //   1. RTCPeerConnection 抹掉 → _startP2p 直接落 _startWs；
    //   2. 浏览器侧把到 agent 的跨源请求拒绝掉 → 即使 agent 意外上报了
    //      lan_addrs，_startLan 的探测也会失败并落 relay。
    // 两条都在网络/API 层生效，判定仍由真实观测（transport、字节归属）给出。
    if (mode === 'relay') {
      await ctx.addInitScript(() => {
        window.RTCPeerConnection = undefined;
        try { delete window.RTCPeerConnection; } catch (e) {}
      });
      // 网络层隔离：同源页面正常放行，跨源一律 abort（LAN 端点必然跨源）。
      // 用 route 而不是页面脚本，避免产品内部改动绕过。
      await ctx.route('**/*', async (route) => {
        const req = route.request();
        let sameOrigin = false;
        try {
          sameOrigin = new URL(req.url()).origin === new URL(stack.base).origin;
        } catch (e) { sameOrigin = false; }
        if (!sameOrigin && /\/agent\/desktop\/stream/.test(req.url())) {
          log(`  [isolation] 拒绝跨源桌面流请求：${req.url().replace(/token=[^&]*/, 'token=<redacted>')}`);
          return route.abort();
        }
        return route.continue();
      });
    }

    // 流生命周期观测（MSE 不断流断言的数据来源）。
    // 用 Playwright 的请求事件在**页面之外**统计，绝不碰响应体：
    // 页面内包装 fetch 再去 read() 会和产品自己的 getReader() 抢同一个
    // ReadableStream，把流锁死（实测 LAN 退回 p2p、MSE 无首帧）。
    const lifecycle = { started: 0, finished: 0, failed: 0, connectedAt: 0 };
    const isDesktopStreamReq = (url) => {
      try {
        return /\/agent\/desktop\/(stream|ws)(\?|$)/.test(new URL(url, stack.base).pathname + (url.includes('?') ? '?' : ''));
      } catch (e) { return /\/agent\/desktop\/(stream|ws)/.test(url); }
    };
    page.on('request', (req) => {
      if (!isDesktopStreamReq(req.url())) return;
      lifecycle.started += 1;
      lifecycle.connectedAt = Date.now();
    });
    page.on('requestfinished', (req) => {
      if (!isDesktopStreamReq(req.url())) return;
      lifecycle.finished += 1;
    });
    page.on('requestfailed', (req) => {
      if (!isDesktopStreamReq(req.url())) return;
      lifecycle.failed += 1;
      lifecycle.finished += 1; // 失败的请求同样"结束过"
    });

    const session = A.makeSession(page, {
      mode,
      base: stack.base,
      log,
      sleep,
      openDesktop,
      waitFirstFrame,
      playVideo,
      sendCommand,
      probeSupport,
      // 适配层据此计算 MSE 重连次数（见 computeStreamRestarts 的配对语义）
      streamLifecycle: () => ({
        started: lifecycle.started,
        completed: lifecycle.finished,
      }),
    });
    const plan = await H.runModePlan(session, {
      deadlineMs: cfg.DEADLINE_MS,
      growthWindowMs: cfg.GROWTH_MS,
      settleMs: cfg.SETTLE_MS,
      pollMs: cfg.POLL_MS,
      rounds: cfg.ROUNDS,
      log,
    });

    res.transport = plan.metrics ? plan.metrics.transport : null;
    res.metrics = plan.metrics || {};
    res.checks = plan.checks;
    res.passed = plan.passed;
    res.failed = plan.failed;
    res.skipped = plan.skipped;
    res.skippedList = plan.skippedList;
    res.exitCode = plan.exitCode;

    for (const c of plan.checks) {
      log(`${c.ok ? '✓' : '✗'} [${mode}] ${c.id}  ${c.detail}`);
    }
    for (const s of plan.skippedList) log(`- SKIP [${mode}] ${s}`);
    const rec = (plan.metrics && plan.metrics.recoveries) || [];
    if (rec.length) {
      log(`  最慢一次恢复：${Math.max(...rec)}ms（单条命令预算 ${cfg.DEADLINE_MS}ms，含 codec 生效时间）`);
    }
    log(`  传输=${res.transport}  实际配置=${(plan.metrics && plan.metrics.codecConfig) || 'n/a'}  累计帧=${(plan.metrics && plan.metrics.frames) || 0}`);
    log(`  ${plan.passed}/${plan.total} 通过，${plan.failed} 失败，${plan.skipped} 跳过`);
  } catch (e) {
    // 异常同样计一次失败：不得因为脚本自身报错而"零失败"退出。
    const msg = String((e && e.message) || e);
    log(`  FAIL [${mode}] 验证中断：${msg}`);
    res.checks = res.checks || [];
    res.checks.push({ id: `${mode}.aborted`, ok: false, detail: msg.split('\n')[0].slice(0, 300) });
    res.failed = (res.failed || 0) + 1;
    res.exitCode = 1;
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (cfg.KEEP) log(`  KEEP=1：进程与日志保留在 ${stack.dir}`);
  }

  res.logDir = stack.dir;
  if (!cfg.KEEP) {
    await stopAll();
    // 日志脱敏后写入结果目录（token 不落盘）
    try { scrubDir(stack.dir, stack.token); } catch (e) { /* 日志已尽力 */ }
  } else {
    await stopAll(); // 进程仍要收（KEEP 只保留目录）
  }
  return res;
}

// 日志脱敏：把本次 token 换成固定占位，附带端口也保留（排障需要）。
function scrubDir(dir, token) {
  for (const f of fs.readdirSync(dir)) {
    const p = path.join(dir, f);
    let st;
    try { st = fs.statSync(p); } catch (e) { continue; }
    if (!st.isFile() || st.size > 32 * 1024 * 1024) continue;
    if (!/\.(log|json|txt)$/.test(f)) continue;
    try {
      const s = fs.readFileSync(p, 'utf8');
      const out = token ? s.split(token).join('<REDACTED_TOKEN>') : s;
      if (out !== s) fs.writeFileSync(p, out);
    } catch (e) { /* 二进制/权限问题忽略 */ }
  }
}

// ── main ─────────────────────────────────────────────────────────
async function main() {
  const deps = preflight();
  if (!deps) process.exit(2);

  const chrome = resolveChrome(deps.playwright);
  if (!chrome) {
    console.error('FAIL 找不到 Chromium：设 CHROME=<可执行文件> 或 `npx playwright install chromium`。');
    process.exit(2);
  }

  const modes = cfg.MODE === 'all' ? MODES.slice() : [cfg.MODE];
  for (const m of modes) {
    if (!MODES.includes(m)) {
      console.error(`FAIL MODE=${m} 不支持（relay|lan|mse|all）`);
      process.exit(2);
    }
  }

  const binHash = {};
  for (const b of ['shell-remote', 'shell-remote-ui']) {
    const p = path.join(cfg.BIN, b);
    try {
      binHash[b] = crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
    } catch (e) { binHash[b] = 'unreadable'; }
  }
  log(`MYS-2842 桌面运行时验收`);
  log(`  chrome=${chrome}`);
  log(`  BIN=${cfg.BIN}`);
  log(`  shell-remote sha256=${binHash['shell-remote']}`);
  log(`  shell-remote-ui sha256=${binHash['shell-remote-ui']}`);
  log(`  模式=${modes.join(',')}  分辨率=${cfg.WIDTH}x${cfg.HEIGHT}@${cfg.FPS}fps  轮数=${cfg.ROUNDS}  单命令预算=${cfg.DEADLINE_MS}ms`);

  const results = [];
  for (const m of modes) {
    results.push(await runMode(deps, chrome, m));
  }
  await stopAll();

  // ── 总摘要：按模式分别列，绝不合并成"全部通过" ──────────────────
  log('\n──────── 总摘要 ────────');
  let total = 0, passed = 0, failed = 0, skipped = 0;
  for (const r of results) {
    total += r.passed + r.failed;
    passed += r.passed;
    failed += r.failed;
    skipped += r.skipped;
    log(`${r.mode.padEnd(5)}  transport=${String(r.transport).padEnd(12)} ${r.passed}/${r.passed + r.failed} 通过，${r.failed} 失败，${r.skipped} 跳过  日志=${r.logDir}`);
    for (const c of r.checks.filter((x) => !x.ok)) log(`        ✗ ${c.id}  ${c.detail}`);
  }
  log(`合计 ${passed}/${total} 通过，${failed} 失败，${skipped} 跳过`);

  const out = {
    tool: 'verify_mys2836_runtime',
    issue: 'MYS-2842',
    when: new Date().toISOString(),
    config: {
      bin: cfg.BIN, binSha256: binHash, modes,
      width: cfg.WIDTH, height: cfg.HEIGHT, fps: cfg.FPS,
      rounds: cfg.ROUNDS, deadlineMs: cfg.DEADLINE_MS, growthMs: cfg.GROWTH_MS,
      chrome,
    },
    results,
    summary: { total, passed, failed, skipped, exitCode: failed > 0 ? 1 : 0 },
  };
  const outPath = cfg.OUT || path.join(results[results.length - 1].logDir, 'result.json');
  try {
    fs.writeFileSync(outPath, JSON.stringify(out, null, 2));
    log(`结果 JSON：${outPath}`);
  } catch (e) {
    log(`WARN 结果 JSON 写入失败：${e.message}`);
  }

  process.exit(out.summary.exitCode);
}

main().catch(async (e) => {
  console.error('FAIL', e && e.stack ? e.stack : e);
  await stopAll();
  process.exit(1);
});
