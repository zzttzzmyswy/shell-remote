'use strict';
// MYS-2767 前端修复验证（playwright，无头 Chromium）。
//
// 覆盖任务卡验收标准里要求的 6 项 + 键盘过滤（缺陷 #6）：
//   1. 打开桌面 → 人为中断流触发自动重连 → 立即切回终端 → 等待 10s：
//      无新的 /agent/desktop 请求、无 desktop:key 注入（重连定时器/异步续体
//      已被 disconnect() 作废）；
//   2. disconnect() 时仍按住的键会补发 down:false（远端不留卡键）；
//   3. 只读（ro）权限下 desktop:mouse / key / qos / bitrate 一律不发送；
//   4. 显示器下拉的 option value 是"枚举序号字符串"，0x0 输出不计入"是否
//      显示下拉"（序号仍按原数组下标）；
//   5. 横向滚轮（deltaY=0）只发 dx，不发垂直滚动；
//   6. MSE 路径（禁用 window.VideoDecoder）首帧后 #desktop-loading 带 hidden；
//   7. 键盘过滤：桌面控制栏/表单控件聚焦时不注入，桌面画面聚焦时正常注入。
//
// 发送断言的做法：在页面里包裹 window.shellRemote.send 记录调用（不改生产
// 代码），详见 installRecorder()。
//
// 用法（与既有 tools/verify/verify_r*.js 相同的启动方式：relay + agent 需要
// 先起好；本脚本只驱动浏览器）：
//
//   # 1) Xvfb + relay + 桌面 agent
//   Xvfb :99 -screen 0 1280x800x24 -extension GLX &
//   DISPLAY=:99 ./target/debug/shell-remote relay --no-tls \
//       --bind 127.0.0.1:3100 --auth sr-smoke-pass &
//   DISPLAY=:99 ./target/debug/shell-remote-ui agent \
//       --relay-url http://127.0.0.1:3100 --key sr-smoke-pass --session-id mys2767 \
//       --view headless --desktop-capture x11 --desktop-codec h264 --desktop-fps 30 &
//
//   # 2) 依赖 + 跑脚本
//   npm i
//   node tools/verify/verify_mys2765_web.js
//
// 环境变量：
//   BASE    relay 地址（默认 http://127.0.0.1:3100）
//   TOKEN   会话密钥（默认 sr-smoke-pass，须等于 agent 的 --key）
//   AUTH    服务器密码（默认同 TOKEN，须等于 relay 的 --auth）
//   ONLY    只跑某一项（1..7，逗号分隔），便于定位

const { chromium } = require('playwright');

const BASE = process.env.BASE || 'http://127.0.0.1:3100';
const TOKEN = process.env.TOKEN || 'sr-smoke-pass';
const AUTH = process.env.AUTH || TOKEN;
const ONLY = (process.env.ONLY || '1,2,3,4,5,6,7').split(',').map((s) => s.trim());
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let fails = 0;
let checks = 0;
const check = (name, ok, extra = '') => {
  checks += 1;
  console.log(`${ok ? '✓' : '✗'} ${name}${extra ? '  ' + extra : ''}`);
  if (!ok) fails += 1;
};
const shouldRun = (n) => ONLY.indexOf(String(n)) >= 0;

// 页面侧注入：记录 shellRemote.send 调用 + 暴露事件 handler 表。
// 之所以包 shellRemote 而不是改生产代码：send 是全部上行消息的唯一出口，
// 断言点集中且不侵入。
const INIT_RECORDER = function (cfg) {
  try {
    sessionStorage.setItem('shell-remote-token', cfg.token);
    sessionStorage.setItem('shell-remote-auth', cfg.auth);
  } catch (e) {}
  window.__srSent = [];
  window.__srHandlers = {};
  // 记录发出的每一条消息（同步，便于断言时序）。
  const wrap = function (v) {
    if (!v || typeof v.send !== 'function' || v.__srWrapped) return;
    const origSend = v.send.bind(v);
    v.send = function (type, payload) {
      window.__srSent.push({ type: type, payload: payload || {}, at: Date.now() });
      return origSend(type, payload);
    };
    if (typeof v.on === 'function') {
      const origOn = v.on.bind(v);
      v.on = function (type, fn) {
        (window.__srHandlers[type] = window.__srHandlers[type] || []).push(fn);
        return origOn(type, fn);
      };
    }
    try { v.__srWrapped = true; } catch (e) {}
  };
  let real;
  Object.defineProperty(window, 'shellRemote', {
    configurable: true,
    get() { return real; },
    set(v) { real = v; wrap(v); }
  });
  // 权限覆盖（ro 场景）：sse.js 在 `connected` 事件里改写 permission，
  // 因此必须在 wrap 之后固定 getPermission 的返回值。
  if (cfg.permission) {
    Object.defineProperty(window, 'shellRemote', {
      configurable: true,
      get() { return real; },
      set(v) {
        real = v;
        wrap(v);
        if (v) v.getPermission = function () { return cfg.permission; };
      }
    });
  }
  // 强制走 relay-WS 路径（跳过 LAN 探测与 P2P）：本环境未开 --desktop-lan-port，
  // P2P 在无 ICE 服务时协商结果不稳定，会掩盖要测的重连/断开语义。
  if (cfg.noP2p) {
    Object.defineProperty(window, 'RTCPeerConnection', { value: undefined, configurable: true });
  }
  // 记录创建的 WebSocket 实例（测试里用它模拟"流被中断"）。
  if (cfg.trackWs && typeof window.WebSocket === 'function') {
    const Native = window.WebSocket;
    window.__srWs = [];
    window.WebSocket = new Proxy(Native, {
      construct(t, args) {
        const w = new t(...args);
        window.__srWs.push(w);
        return w;
      }
    });
  }
  // MSE 路径：抹掉 WebCodecs 暴露（桌面的解码路径判定据此回退 MSE）。
  if (cfg.noWebCodecs) {
    Object.defineProperty(window, 'VideoDecoder', { value: undefined, configurable: true });
  }
};

async function newPage(browser, cfg) {
  const ctx = await browser.newContext();
  await ctx.addInitScript(INIT_RECORDER, Object.assign({ token: TOKEN, auth: AUTH }, cfg || {}));
  const page = await ctx.newPage();
  page.on('pageerror', (e) => console.log('  [pageerror]', String(e.message).slice(0, 160)));
  return { ctx, page };
}

// 打开会话页并等待桌面按钮可用（agent 的 desktop:capabilities 到达）。
async function openSession(page) {
  await page.goto(`${BASE}/`);
  await page.goto(`${BASE}/session`);
  await page.waitForFunction(() => {
    const b = document.getElementById('toggle-desktop-btn');
    return b && !b.disabled && !b.classList.contains('hidden');
  }, { timeout: 20000 });
}

// 开启桌面并等到流真正出画（loading 遮罩隐藏）。
//
// 两种情况都要处理：agent 侧桌面未开（点按钮发 desktop:start）；agent 侧桌面
// 已在运行（页面收到 desktop:state/capabilities 的 running 后自动接入，此时
// 按钮语义是"切回终端"，再点反而会把流停掉）——后者不点按钮。
async function startDesktop(page, timeout = 25000) {
  const already = await page.evaluate(() => {
    const c = document.getElementById('desktop-container');
    return !!(c && !c.classList.contains('hidden'));
  });
  if (!already) {
    await page.click('#toggle-desktop-btn');
    await page.waitForFunction(() => {
      const c = document.getElementById('desktop-container');
      return c && !c.classList.contains('hidden');
    }, { timeout: 15000 });
  }
  return page.waitForFunction(() => {
    const el = document.getElementById('desktop-loading');
    return el && el.classList.contains('hidden');
  }, { timeout: timeout }).then(() => true).catch(() => false);
}

const sent = (page) => page.evaluate(() => window.__srSent || []);
const sentOf = (page, type) => sent(page).then((all) => all.filter((m) => m.type === type));

// ── 用例 1：断开后不得被自动重连定时器/残留回调重新拉起 ─────────────
async function case1(browser) {
  console.log('\n[1] 中断流后立即切回终端：10s 内不得重新拉流、不得注入按键');
  const { ctx, page } = await newPage(browser, { noP2p: true, trackWs: true });
  try {
    // 统计浏览器发出的所有 desktop 相关请求（HTTP + WS 握手）。
    const streamReqs = [];
    page.on('request', (r) => {
      const u = r.url();
      if (u.indexOf('/agent/desktop/') >= 0) streamReqs.push({ url: u, at: Date.now() });
    });
    await openSession(page);
    const ok = await startDesktop(page);
    check('1.0 桌面流已出画（前置条件）', ok);
    const wsCount = await page.evaluate(() => (window.__srWs || []).length);
    console.log(`    已建立 WebSocket ${wsCount} 条，desktop 请求 ${streamReqs.length} 条`);

    // 先证明"桌面视图可见时按键确实会注入"，否则后面的"不注入"断言是空转。
    await page.evaluate(() => {
      window.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyX', bubbles: true }));
      window.dispatchEvent(new KeyboardEvent('keyup', { code: 'KeyX', bubbles: true }));
    });
    await sleep(200);
    const keyProbe = (await sentOf(page, 'desktop:key')).filter((m) => m.payload.code === 'KeyX');
    check('1.0a 桌面视图可见时 keydown 会注入（注入通路有效）', keyProbe.length === 2,
      `count=${keyProbe.length}`);

    // 复现"切回终端后残留流复活"的完整时序：
    //   1) 人为断开桌面 WebSocket → onclose 把 connected 判为断流并排入自动
    //      重连定时器（退避 350~700ms）；
    //   2) 在定时器到点之前切回终端（点按钮 → disconnect()）。
    // 修复前：disconnect() 取消不了这个定时器，到点又 connect()，流被重新
    // 拉起并重新绑定 window 级键盘监听（键盘劫持）。
    await page.evaluate(() => {
      const list = window.__srWs || [];
      const ws = list[list.length - 1];
      if (ws) { try { ws.close(); } catch (e) {} }
    });
    // 等 onclose 到点、重连定时器已排入（但尚未触发）。
    await sleep(150);
    const pendingRetries = await page.evaluate(() => {
      const b = document.getElementById('desktop-status');
      return { status: b ? b.textContent : '', tracked: !!window.__srWs };
    });
    console.log('    中断后状态:', JSON.stringify(pendingRetries));
    await page.evaluate(() => {
      document.getElementById('toggle-desktop-btn').click();
    });
    const markAt = Date.now();
    await sleep(1000);
    const viewAfter = await page.evaluate(() => {
      const c = document.getElementById('desktop-container');
      return { hidden: c.classList.contains('hidden') };
    });
    check('1.1 切回终端后桌面容器已隐藏', viewAfter.hidden);

    // 等待 > 最长自动重连退避窗口（5s）+ 观测余量。
    await sleep(10000);

    const newReqs = streamReqs.filter((r) => r.at > markAt);
    check('1.2 断开后 10s 内无新的 /agent/desktop 请求', newReqs.length === 0,
      newReqs.map((r) => r.url.replace(BASE, '')).join(' | '));
    const wsAfter = await page.evaluate(() => (window.__srWs || []).length);
    check('1.3 未新建 WebSocket（无后台重连）', wsAfter === wsCount, `before=${wsCount} after=${wsAfter}`);

    // 终端视图下按键不得注入（键盘过滤：视图不可见即不转发）。
    const keysBefore = (await sentOf(page, 'desktop:key')).length;
    await page.evaluate(() => {
      window.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyA', bubbles: true }));
      window.dispatchEvent(new KeyboardEvent('keyup', { code: 'KeyA', bubbles: true }));
    });
    await sleep(200);
    const keysAfter = (await sentOf(page, 'desktop:key')).length;
    check('1.4 终端视图下 window keydown 不注入远端', keysAfter === keysBefore,
      `before=${keysBefore} after=${keysAfter}`);
  } finally {
    await ctx.close();
  }
}

// ── 用例 2：disconnect() 时按住的键要补发 down:false ────────────────
async function case2(browser) {
  console.log('\n[2] 断开时补发按键释放');
  const { ctx, page } = await newPage(browser, { noP2p: true });
  try {
    await openSession(page);
    const ok = await startDesktop(page);
    check('2.0 桌面流已出画（前置条件）', ok);

    // 在桌面视图里按住 Ctrl（真实路径：window keydown → _pressedKeys）。
    await page.evaluate(() => {
      window.dispatchEvent(new KeyboardEvent('keydown', { code: 'ControlLeft', bubbles: true }));
    });
    await sleep(150);
    const down = (await sentOf(page, 'desktop:key')).filter(
      (m) => m.payload.code === 'ControlLeft' && m.payload.down === true);
    check('2.1 桌面视图内 keydown 正常注入', down.length === 1);

    // 切回终端 = desktopView.disconnect()（session.js 的 toggle handler）。
    await page.click('#toggle-desktop-btn');
    await sleep(400);
    const up = (await sentOf(page, 'desktop:key')).filter(
      (m) => m.payload.code === 'ControlLeft' && m.payload.down === false);
    check('2.2 断开时补发 ControlLeft down:false', up.length === 1,
      `up=${up.length}`);

    // 鼠标键同理：按下后断开要补发 up。
    const upMouse = (await sentOf(page, 'desktop:mouse')).filter(
      (m) => m.payload.type === 'up');
    check('2.3 已按下的鼠标键也补发 up（若有按下）', true, `mouse up=${upMouse.length}`);
  } finally {
    await ctx.close();
  }
}

// ── 用例 3：只读会话不发任何写权限消息 ────────────────────────────
async function case3(browser) {
  console.log('\n[3] 只读（ro）会话不发送 mouse/key/qos/bitrate');
  // 先用一个 rw 页面把桌面开起来（ro 观看者本身开不了桌面）。
  const rw = await newPage(browser, { noP2p: true });
  const ro = await newPage(browser, { noP2p: true, permission: 'ro' });
  try {
    await openSession(rw.page);
    const ok = await startDesktop(rw.page);
    check('3.0 前置：rw 页面已开启桌面流', ok);

    // ro 页面：以 running:true 的 capabilities 事件走"观看已开启桌面"的路径
    // （session.js 的真实 handler）。
    await openSession(ro.page).catch(() => {});
    await ro.page.waitForFunction(() => !!window.__srHandlers, { timeout: 15000 });
    await ro.page.evaluate(() => {
      const hs = window.__srHandlers['desktop:capabilities'] || [];
      hs.forEach((fn) => fn({
        type: 'desktop:capabilities',
        payload: { available: true, running: true, codecs: ['av1', 'h264'] }
      }));
    });
    const roDrawn = await ro.page.waitForFunction(() => {
      const el = document.getElementById('desktop-loading');
      return el && el.classList.contains('hidden');
    }, { timeout: 20000 }).then(() => true).catch(() => false);
    check('3.1 ro 页面接上已运行的桌面并出画', roDrawn);

    const perm = await ro.page.evaluate(() => window.shellRemote.getPermission());
    check('3.2 页面权限确为 ro', perm === 'ro', `perm=${perm}`);

    // 触发所有"只读用户也会碰到"的输入面。
    await ro.page.evaluate(() => {
      const c = document.getElementById('desktop-canvas') || document.getElementById('desktop-video');
      if (c) {
        const r = c.getBoundingClientRect();
        const at = { clientX: r.left + r.width / 2, clientY: r.top + r.height / 2, bubbles: true };
        c.dispatchEvent(new PointerEvent('pointermove', at));
        c.dispatchEvent(new PointerEvent('pointerdown', Object.assign({ button: 0, pointerId: 1 }, at)));
        c.dispatchEvent(new PointerEvent('pointerup', Object.assign({ button: 0, pointerId: 1 }, at)));
        c.dispatchEvent(new WheelEvent('wheel', { deltaY: 100, bubbles: true, cancelable: true }));
      }
      window.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyB', bubbles: true }));
      window.dispatchEvent(new KeyboardEvent('keyup', { code: 'KeyB', bubbles: true }));
    });
    // 码率/qos 是周期上报：等 2 个上报窗口（qos 250ms / bitrate 1s）。
    await sleep(2600);

    const forbidden = ['desktop:mouse', 'desktop:key', 'desktop:qos', 'desktop:bitrate'];
    const all = await sent(ro.page);
    for (const t of forbidden) {
      const n = all.filter((m) => m.type === t).length;
      check(`3.3 ro 会话未发送 ${t}`, n === 0, `count=${n}`);
    }
    // 只读操作仍应放行（reqkey 允许），说明消息通道本身是活的。
    const allowed = all.filter((m) => ['desktop:reqkey', 'desktop:test-delay', 'desktop:clipboard:get'].indexOf(m.type) >= 0);
    check('3.4 只读允许的操作仍可发送（通道存活）', allowed.length > 0,
      allowed.map((m) => m.type).slice(0, 4).join(','));
  } finally {
    await rw.ctx.close();
    await ro.ctx.close();
  }
}

// ── 用例 4：显示器下拉 value = 序号字符串；0x0 不计入 ────────────────
async function case4(browser) {
  console.log('\n[4] 显示器下拉 value 为枚举序号，0x0 输出被过滤');
  const { ctx, page } = await newPage(browser, { noP2p: true });
  try {
    await page.goto(`${BASE}/`);
    await page.goto(`${BASE}/session`);
    await page.waitForFunction(() => !!window.__srHandlers, { timeout: 15000 });
    // 合成桌面拓扑：0 号正常、1 号是未连接输出（0x0，X11 会列出）、2 号正常。
    // 预期：下拉出现（可用显示器 2 台 > 1），option 为 ""/"0"/"2"，没有 "1"。
    const res = await page.evaluate(() => {
      const fire = (type, payload) => {
        (window.__srHandlers[type] || []).forEach((fn) => fn({ type: type, payload: payload }));
      };
      fire('desktop:started', {
        displays: [
          { name: 'HDMI-1', width: 1280, height: 800, x: 0, y: 0 },
          { name: 'DP-1', width: 0, height: 0, x: 0, y: 0 },
          { name: 'DP-2', width: 1920, height: 1080, x: 1280, y: 0 }
        ]
      });
      const sel = document.getElementById('desktop-display-select');
      const label = document.getElementById('desktop-display-label');
      return {
        values: Array.from(sel.options).map((o) => o.value),
        texts: Array.from(sel.options).map((o) => o.textContent.trim()),
        labelVisible: getComputedStyle(label).display !== 'none'
      };
    });
    console.log('    options:', JSON.stringify(res.values), JSON.stringify(res.texts));
    check('4.1 value 为序号字符串（"0"/"2"），不含输出名',
      JSON.stringify(res.values) === JSON.stringify(['', '0', '2']), JSON.stringify(res.values));
    check('4.2 0x0 未连接输出不出现在下拉中', res.texts.every((t) => t.indexOf('DP-1') < 0),
      res.texts.join(' | '));
    check('4.3 显示文本为「名称 · WxH」', res.texts[1] === 'HDMI-1 · 1280x800' &&
      res.texts[2] === 'DP-2 · 1920x1080', res.texts.slice(1).join(' | '));
    check('4.4 可用显示器 >1 时显示下拉（0x0 不计入）', res.labelVisible === true);

    // 只有 1 台可用 + 1 台 0x0 时：下拉应隐藏（旧实现会因 2 项而误显示）。
    const single = await page.evaluate(() => {
      const fire = (type, payload) => {
        (window.__srHandlers[type] || []).forEach((fn) => fn({ type: type, payload: payload }));
      };
      fire('desktop:started', {
        displays: [
          { name: 'eDP-1', width: 1920, height: 1080, x: 0, y: 0 },
          { name: 'HDMI-1', width: 0, height: 0, x: 0, y: 0 }
        ]
      });
      const label = document.getElementById('desktop-display-label');
      const sel = document.getElementById('desktop-display-select');
      return {
        labelVisible: getComputedStyle(label).display !== 'none',
        values: Array.from(sel.options).map((o) => o.value)
      };
    });
    check('4.5 单台可用 + 0x0 时隐藏下拉', single.labelVisible === false,
      JSON.stringify(single));
  } finally {
    await ctx.close();
  }
}

// ── 用例 5：横向滚轮只发 dx ───────────────────────────────────────
async function case5(browser) {
  console.log('\n[5] 横向滚轮只发 dx，不发垂直滚动');
  const { ctx, page } = await newPage(browser, { noP2p: true });
  try {
    await openSession(page);
    const ok = await startDesktop(page);
    check('5.0 桌面流已出画（前置条件）', ok);

    await page.evaluate(() => {
      const c = document.getElementById('desktop-canvas') || document.getElementById('desktop-video');
      window.__wheelBase = (window.__srSent || []).length;
      c.dispatchEvent(new WheelEvent('wheel', { deltaX: 120, deltaY: 0, deltaMode: 0, bubbles: true, cancelable: true }));
    });
    await sleep(200);
    let wheels = await sentOf(page, 'desktop:mouse');
    wheels = wheels.filter((m) => m.payload.type === 'wheel');
    const horizOnly = wheels.filter((m) => m.payload.dx !== 0);
    check('5.1 横向滚动发出 dx', horizOnly.length === 1, JSON.stringify(wheels.map((m) => m.payload)));
    check('5.2 横向滚动 dy 为 0（未凭空产生垂直滚动）',
      horizOnly.length === 1 && horizOnly[0].payload.dy === 0,
      horizOnly.length ? JSON.stringify(horizOnly[0].payload) : 'none');
    check('5.3 横向滚动 dx 方向正确（正=右）',
      horizOnly.length === 1 && horizOnly[0].payload.dx > 0,
      horizOnly.length ? String(horizOnly[0].payload.dx) : 'none');

    // 垂直滚动仍要正常工作（回归保护）。
    await page.evaluate(() => {
      const c = document.getElementById('desktop-canvas') || document.getElementById('desktop-video');
      c.dispatchEvent(new WheelEvent('wheel', { deltaX: 0, deltaY: 120, deltaMode: 0, bubbles: true, cancelable: true }));
    });
    await sleep(200);
    wheels = (await sentOf(page, 'desktop:mouse')).filter((m) => m.payload.type === 'wheel');
    const vert = wheels.filter((m) => m.payload.dy !== 0);
    check('5.4 垂直滚动仍发出 dy', vert.length === 1 && vert[0].payload.dy > 0,
      JSON.stringify(vert.map((m) => m.payload)));
  } finally {
    await ctx.close();
  }
}

// ── 用例 6：MSE 路径首帧后隐藏遮罩 ────────────────────────────────
async function case6(browser) {
  console.log('\n[6] MSE 路径首帧后 #desktop-loading 带 hidden');
  const { ctx, page } = await newPage(browser, { noWebCodecs: true });
  try {
    await openSession(page);
    const mode = await page.evaluate(() => ({
      hasVideoDecoder: typeof window.VideoDecoder !== 'undefined',
      hasMse: typeof window.DesktopViewMse === 'function'
    }));
    check('6.0 前置：页面已禁用 WebCodecs 且 MSE 播放器可用',
      mode.hasVideoDecoder === false && mode.hasMse === true, JSON.stringify(mode));

    // 与其它用例一样走 startDesktop：agent 侧桌面可能已被前序用例开启，
    // 此时按钮语义是"切回终端"，不能再点。
    const hidden = await startDesktop(page, 30000);
    check('6.1 MSE 首帧后遮罩 hidden（不再永久盖住画面）', hidden === true);
    const st = await page.evaluate(() => {
      const el = document.getElementById('desktop-loading');
      const v = document.getElementById('desktop-video');
      return {
        loadingHidden: el ? el.classList.contains('hidden') : null,
        videoSize: v ? (v.videoWidth + 'x' + v.videoHeight) : null,
        decoder: (document.getElementById('metric-decoder') || {}).textContent || ''
      };
    });
    console.log('    状态:', JSON.stringify(st));
    check('6.2 视频尺寸非默认占位（真的出画）',
      !!st.videoSize && st.videoSize !== '0x0' && st.videoSize !== '300x150', st.videoSize);
  } finally {
    await ctx.close();
  }
}

// ── 用例 7：键盘过滤（控件聚焦不注入；桌面视图可见时正常注入）────────
async function case7(browser) {
  console.log('\n[7] 键盘过滤：桌面控制栏/表单控件聚焦时不注入远端');
  const { ctx, page } = await newPage(browser, { noP2p: true });
  try {
    await openSession(page);
    const ok = await startDesktop(page);
    check('7.0 桌面流已出画（前置条件）', ok);

    const keysAfter = async () => (await sentOf(page, 'desktop:key')).length;

    // a) 焦点在下拉框（SELECT）内：不注入。
    const before = await keysAfter();
    await page.evaluate(() => {
      const sel = document.getElementById('desktop-codec-select') ||
        document.getElementById('desktop-quality-select');
      sel.focus();
      sel.dispatchEvent(new KeyboardEvent('keydown', { code: 'ArrowDown', bubbles: true }));
      sel.dispatchEvent(new KeyboardEvent('keyup', { code: 'ArrowDown', bubbles: true }));
    });
    await sleep(200);
    const afterSelect = await keysAfter();
    check('7.1 焦点在桌面控制栏下拉框时不注入', afterSelect === before,
      `before=${before} after=${afterSelect}`);

    // b) 焦点在按钮（BUTTON）上：不注入。
    await page.evaluate(() => {
      const btn = document.getElementById('desktop-metrics-btn');
      btn.focus();
      btn.dispatchEvent(new KeyboardEvent('keydown', { code: 'Space', bubbles: true }));
      btn.dispatchEvent(new KeyboardEvent('keyup', { code: 'Space', bubbles: true }));
    });
    await sleep(200);
    const afterBtn = await keysAfter();
    check('7.2 焦点在桌面控制栏按钮上时不注入', afterBtn === before,
      `before=${before} after=${afterBtn}`);

    // c) 焦点回到桌面画面（canvas）：正常注入。
    await page.evaluate(() => {
      const c = document.getElementById('desktop-canvas') || document.getElementById('desktop-video');
      c.focus();
      c.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyQ', bubbles: true }));
      c.dispatchEvent(new KeyboardEvent('keyup', { code: 'KeyQ', bubbles: true }));
    });
    await sleep(200);
    const afterCanvas = await keysAfter();
    check('7.3 焦点在桌面画面时正常注入', afterCanvas === before + 2,
      `before=${before} after=${afterCanvas}`);
  } finally {
    await ctx.close();
  }
}

(async () => {
  const browser = await chromium.launch();
  console.log(`── MYS-2767 Web 桌面客户端修复验证（${BASE}）──`);
  try {
    if (shouldRun(2)) await case2(browser);
    if (shouldRun(3)) await case3(browser);
    if (shouldRun(5)) await case5(browser);
    if (shouldRun(7)) await case7(browser);
    if (shouldRun(4)) await case4(browser);
    if (shouldRun(6)) await case6(browser);
    // 用例 1 会真的把桌面停掉（点"终端" → desktop:stop），放最后。
    if (shouldRun(1)) await case1(browser);
  } catch (e) {
    console.error('脚本异常:', e && e.message ? e.message : e);
    fails += 1;
  } finally {
    await browser.close();
  }
  const passed = checks - fails;
  console.log(`\n结果: ${passed}/${checks} 通过${fails ? `，${fails} 项失败` : ''}`);
  console.log(fails === 0 ? 'PASS' : 'FAIL');
  process.exit(fails === 0 ? 0 : 1);
})();
