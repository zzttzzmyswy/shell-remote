'use strict';
/**
 * MYS-2842 验收工具契约测试：验证 `mys2836_harness.js` 的**检测行为**，而不是
 * 复制它的实现常量。
 *
 * 每个用例构造"可控的假 session"（帧计数、decoder 配置、画布采样、健康统计
 * 全由测试驱动），断言检测逻辑在该输入下的判定结果——即：坏输入必须失败，
 * 好输入必须通过。这样验收工具本身不会退化成"永远 PASS 的脚本"。
 *
 * 运行：node --test tools/verify/mys2836_harness.test.js
 */

const test = require('node:test');
const assert = require('node:assert');

const H = require('./mys2836_harness.js');

// ── 可控时钟 ─────────────────────────────────────────────────────
// 真实 setTimeout 会让"截止时间"类用例跑几十秒；假时钟让 sleep 直接推进
// 虚拟时间，既不等待也不改变判定语义（模块只通过 opts.now/sleep 取时间）。
function fakeClock(start = 1_000_000) {
  let t = start;
  return {
    now: () => t,
    sleep: async (ms) => { t += Math.max(0, Number(ms) || 0); },
    advance: (ms) => { t += ms; },
    at: () => t,
  };
}

// ── 可控 session ─────────────────────────────────────────────────
/**
 * @param script 让测试完全掌控每个轮询刻度的观测值：
 *   {
 *     framesAt(t, ctx)   -> 累计帧数
 *     satAt(t, ctx)      -> 画面饱和度
 *     codecAt(t, ctx)    -> {codec, config, at, accepted}
 *     initAt(t, ctx)     -> 累计 init 次数
 *   }
 * ctx = { sent: [{type, codec, enabled, at}], mode, clock }
 */
function fakeSession(opts) {
  const clock = opts.clock;
  const script = opts.script || {};
  const sent = [];
  const sentLog = [];
  const session = {
    mode: opts.mode || 'relay',
    sent,
    sentLog,
    async open() {
      if (opts.open) return opts.open(clock, session);
      return { firstFrameMs: 120 };
    },
    async play() { if (opts.play) await opts.play(clock, session); },
    async frames() { return script.framesAt ? script.framesAt(clock.now(), ctx) : 0; },
    async inits() { return script.initAt ? script.initAt(clock.now(), ctx) : 0; },
    async decoder() {
      return script.codecAt ? script.codecAt(clock.now(), ctx) : { codec: null, config: '', at: 0, accepted: true };
    },
    async saturation() { return script.satAt ? script.satAt(clock.now(), ctx) : null; },
    async send(cmd) {
      const rec = Object.assign({}, cmd, { at: clock.now() });
      sent.push(rec);
      sentLog.push(rec);
      if (opts.onSend) opts.onSend(rec, clock, session);
    },
    async snapshot() {
      return opts.snapshot ? opts.snapshot(clock, session) : {
        transport: session.mode === 'lan' ? 'lan' : 'relay',
        lanAddrs: session.mode === 'lan' ? ['10.0.0.2:9000'] : [],
        lanResponses: session.mode === 'lan' ? [{ ok: true, status: 200 }] : [],
        lanBytes: session.mode === 'lan' ? 65536 : 0,
        wsBytes: session.mode === 'relay' ? 65536 : 0,
        relayBytes: 0,
        rtcAvailable: false,
        webcodecs: session.mode !== 'mse',
        viewMode: session.mode === 'mse' ? 'mse' : 'webcodecs',
        decoderKind: session.mode === 'mse' ? 'mse' : 'webcodecs',
        decoderCodec: session.mode === 'mse' ? 'h264' : 'av1',
        decoderConfig: session.mode === 'mse' ? 'video/mp4; codecs="avc1.42E01E"' : 'av01.0.04M.08',
        transportSamples: [session.mode],
      };
    },
    async health() {
      return opts.health ? opts.health(clock, session) : { decodeErrors: [], playerErrors: [], streamRestarts: 0 };
    },
  };
  const ctx = { sent, sentLog, mode: session.mode, clock, session };
  session.ctx = ctx;
  return session;
}

const ids = (r) => r.checks.map((c) => c.id);
const byId = (r, id) => r.checks.find((c) => c.id === id);
const failedIds = (r) => r.checks.filter((c) => !c.ok).map((c) => c.id);

// ── 1. LAN：请求失败但 Relay 在出帧，不得判 LAN 通过 ─────────────
test('LAN 请求失败、Relay 出帧时不得通过 LAN 判定', () => {
  const setup = H.judgeModeSetup('lan', {
    transport: 'relay',                      // 实例自报 relay
    lanAddrs: ['10.0.0.2:9000'],
    lanResponses: [{ ok: false, status: 0 }], // 探测请求失败
    lanBytes: 0,                              // 一字节都没消费到
    wsBytes: 98304,                           // Relay 在正常出帧
    relayBytes: 0,
  });
  const res = byId({ checks: setup }, 'setup.transport-lan');
  assert.strictEqual(res.ok, false, 'transport=relay 时不得判 LAN 通过');
  assert.strictEqual(byId({ checks: setup }, 'setup.lan-bytes').ok, false,
    'LAN 未消费到字节时不得判通过');
  assert.strictEqual(byId({ checks: setup }, 'setup.lan-response').ok, false,
    'LAN 请求失败时不得判通过');
});

test('LAN 只有请求没有字节也不得通过（"发起过请求"≠"在用直连"）', () => {
  const setup = H.judgeModeSetup('lan', {
    transport: 'lan',
    lanAddrs: ['10.0.0.2:9000'],
    lanResponses: [{ ok: true, status: 200 }],
    lanBytes: 0,             // 探测成功但媒体字节为 0
    wsBytes: 0,
    relayBytes: 0,
  });
  assert.strictEqual(byId({ checks: setup }, 'setup.lan-bytes').ok, false,
    '没有消费到媒体字节时不得判 LAN 通过');
});

test('LAN 真正被选用并消费到字节时通过', () => {
  const setup = H.judgeModeSetup('lan', {
    transport: 'lan',
    lanAddrs: ['10.0.0.2:9000'],
    lanResponses: [{ ok: true, status: 200 }],
    lanBytes: 131072,
    wsBytes: 0,
    relayBytes: 0,
  });
  assert.deepStrictEqual(failedIds({ checks: setup }), []);
});

test('Relay 模式要求 P2P 关闭且 LAN 零字节（不留掩盖空间）', () => {
  const leaked = H.judgeModeSetup('relay', {
    transport: 'relay', rtcAvailable: true, lanBytes: 4096,
    lanResponses: [{ ok: true, status: 200 }], wsBytes: 1024,
  });
  assert.strictEqual(byId({ checks: leaked }, 'setup.p2p-off').ok, false);
  assert.strictEqual(byId({ checks: leaked }, 'setup.no-lan-bytes').ok, false);

  const clean = H.judgeModeSetup('relay', {
    transport: 'relay', rtcAvailable: false, lanBytes: 0,
    lanResponses: [], wsBytes: 1024,
  });
  assert.deepStrictEqual(failedIds({ checks: clean }), []);
});

// ── 2. 持续增长：目标态只出 1 帧后停住不得通过 ────────────────────
test('目标态只出 1 帧后停止，不得通过"持续产帧"', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      // 收到命令后 +200ms 冒 1 帧，然后完全停住（静止画布的典型表现）
      framesAt: (t) => (sentAt !== null && t >= sentAt + 200 ? 11 : 10),
      // 画面立刻变灰并保持
      satAt: (t) => (sentAt !== null ? 0.2 : 120),
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });

  const r = await H.runSwitch(session, {
    name: 'grow',
    command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 5,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'grow.frame').ok, true, '出现了新帧，该项应通过');
  assert.strictEqual(byId(r, 'grow.target').ok, true, '画面确实变灰，该项应通过');
  assert.strictEqual(byId(r, 'grow.growth').ok, false,
    '目标态后帧计数不再增长时，必须判"持续产帧"失败');
});

test('目标态后帧计数持续增长才通过，且恢复时间覆盖增长窗口', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      framesAt: (t) => {
        if (sentAt === null) return 10;
        return 10 + Math.max(0, Math.floor((t - sentAt) / 33)); // ~30fps
      },
      satAt: () => 0.4,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });

  const r = await H.runSwitch(session, {
    name: 'grow-ok',
    command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 5,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.deepStrictEqual(failedIds(r), []);
  assert.ok(r.recoveryMs !== null && r.recoveryMs < 15000, `恢复时间应在预算内，实际 ${r.recoveryMs}`);
});

// ── 3. codec：命令被忽略不得通过 ─────────────────────────────────
test('过渡帧（只命中一次目标判据）不得当成目标态收敛', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      framesAt: (t) => 10 + Math.max(0, Math.floor(((t - (sentAt || t)) / 33))),
      // 切换途中只采到**一次**过渡值 8.67（窗口窄于轮询间隔，保证只命中
      // 一次），之后再没低过（卡住/停在过渡态）。
      satAt: (t) => {
        if (sentAt === null) return 128;
        const dt = t - sentAt;
        return dt >= 300 && dt < 350 ? 8.67 : 128;
      },
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });

  const r = await H.runSwitch(session, {
    name: 'transient',
    command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 10, // 8.67 命中判据
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 5000, growthWindowMs: 300, settleMs: 100, pollMs: 100 });

  assert.strictEqual(byId(r, 'transient.target').ok, false,
    '只命中一次的过渡值不得判为目标画面（必须连续采样收敛）');
  assert.strictEqual(r.recoveryMs, null);
});

test('codec 命令被 agent 忽略（实际配置不变）时不得通过', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: () => 120,
      initAt: () => 1,
      // 一直是 av1：命令发出去了但没有任何新的 configure
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });

  const r = await H.runSwitch(session, {
    name: 'codec',
    command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264',
    isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'codec.codec').ok, false,
    '未观察到 h264 的实际配置时必须判失败（不能只看命令已发送）');
  assert.ok(ids(r).includes('codec.codec'));
});

test('codec 只认命令发出之后生效的配置（旧配置不算）', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: () => 120,
      initAt: () => 1,
      // 历史上配置过 h264（at=0，远早于本次命令），现在实际是 av1
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(session, {
    name: 'stale',
    command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264',
    isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  // 把 decoder 描述成"曾经 h264 但 at=0"也必须失败
  assert.strictEqual(byId(r, 'stale.codec').ok, false,
    '历史配置不能充当本次切换的证据');
});

test('codec 真正生效（发送后新 configure）时通过，并计入恢复时间', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: (t) => 120,
      initAt: () => 2,
      codecAt: (t) => {
        if (sentAt !== null && t >= sentAt + 400) {
          return { codec: 'h264', config: 'avc1.42E01E', at: sentAt + 400, accepted: true };
        }
        return { codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true };
      },
    },
  });
  const r = await H.runSwitch(session, {
    name: 'ok',
    command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264',
    isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.deepStrictEqual(failedIds(r), []);
  assert.ok(r.recoveryMs >= 400, `恢复时间必须含 codec 生效时间，实际 ${r.recoveryMs}`);
});

test('configure 被拒绝（accepted=false）不得算生效', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: (t) => (t >= (sentAt || 0) + 100
        ? { codec: 'h264', config: 'avc1.42E01E', at: sentAt + 100, accepted: false }
        : { codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(session, {
    name: 'rejected',
    command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264',
    isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'rejected.codec').ok, false, 'configure 未 accepted 不得算生效');
});

// ── 4. 同值 gray：不得重配置 ────────────────────────────────────
test('同值 gray 触发重配置时必须失败', async () => {
  const clock = fakeClock();
  let sent = 0;
  const session = fakeSession({
    clock,
    onSend: () => { sent += 1; },
    script: {
      framesAt: (t) => 30 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 120,
      initAt: () => 1 + sent, // 每发一次就重建一次
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runNoopCheck(session, {
    name: 'noop',
    command: { type: 'desktop:gray', enabled: false },
    repeats: 3,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 3000, pollMs: 100 });

  assert.strictEqual(byId(r, 'noop.no-reinit').ok, false, '同值命令不得重建流');
  assert.strictEqual(byId(r, 'noop.frames').ok, true);
});

test('同值 gray 不重配置且继续出帧时通过', async () => {
  const clock = fakeClock();
  const session = fakeSession({
    clock,
    script: {
      framesAt: (t) => 30 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runNoopCheck(session, {
    name: 'noop-ok',
    command: { type: 'desktop:gray', enabled: false },
    repeats: 3,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 3000, pollMs: 100 });
  assert.deepStrictEqual(failedIds(r), []);
});

// ── 5. 首帧未出：必须失败且通过数为 0 ────────────────────────────
test('首帧未出时必须失败、通过数为 0，且未执行项只记为跳过', async () => {
  const clock = fakeClock();
  const session = fakeSession({
    clock,
    open: async () => { throw new Error('首帧超时（30000ms）'); },
    script: { framesAt: () => 0, satAt: () => null, initAt: () => 0 },
  });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100, rounds: 5,
  });

  assert.strictEqual(r.passed, 0, `首帧未出时通过数必须为 0，实际 ${r.passed}`);
  assert.strictEqual(r.failed, 1);
  assert.strictEqual(r.exitCode, 1);
  assert.strictEqual(byId(r, 'pre.first-frame').ok, false);
  assert.ok(r.skipped > 0, '未执行的用例必须显式记为跳过');
  // 不得出现任何"通过"的用例来虚增计数
  assert.ok(r.checks.every((c) => !c.ok), '首帧失败后不得有通过的断言');
});

// ── 6. 截止时间：一条命令只有一个 15s，不得串成 30s ───────────────
test('同一命令的等待不会串成 30 秒（新帧 14.9s + 目标态从 0 重新计时）', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const session = fakeSession({
    clock,
    onSend: (cmd, c) => { if (sentAt === null) sentAt = c.now(); },
    script: {
      // 14.9s 才冒第一帧：紧贴 15s 预算
      framesAt: (t) => (sentAt !== null && t >= sentAt + 14900 ? 11 : 10),
      // 画面永远不达标（饱和度高）
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });

  const t0 = clock.at();
  const r = await H.runSwitch(session, {
    name: 'budget',
    command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 5, // 永不满足
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });
  const spent = clock.at() - t0;

  assert.ok(spent <= 15000 + 500, `整条命令总耗时必须 ≤15s（实际 ${spent}ms）——不得串成 30s`);
  assert.strictEqual(byId(r, 'budget.frame').ok, true, '新帧紧贴预算出现，该项应通过');
  assert.strictEqual(byId(r, 'budget.target').ok, false, '目标态未达成必须失败');
  assert.strictEqual(r.recoveryMs, null, '目标态未达成时不得给出恢复时间');
});

test('decodeDeadline 的剩余时间单调递减且不会变负', () => {
  const clock = fakeClock();
  const d = H.createDeadline(15000, clock.now);
  assert.strictEqual(d.remaining(), 15000);
  clock.advance(5000);
  assert.strictEqual(d.remaining(), 10000);
  clock.advance(20000);
  assert.strictEqual(d.remaining(), 0);
  assert.strictEqual(d.expired(), true);
});

// ── 7. 端到端计划：模式判定 + 错误统计覆盖全程 ────────────────────
test('MSE 模式强制要求 WebCodecs 关闭且实际走 avc1', async () => {
  const clock = fakeClock();
  const session = fakeSession({
    clock,
    mode: 'mse',
    script: {
      framesAt: (t) => 10 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'h264', config: 'avc1.42E01E', at: 0, accepted: true }),
    },
    snapshot: () => ({
      transport: 'relay', lanAddrs: [], lanResponses: [], lanBytes: 0,
      wsBytes: 999, relayBytes: 0, rtcAvailable: false,
      webcodecs: true,           // 没禁用 WebCodecs → 应判失败
      viewMode: 'webcodecs',     // 没退到 MSE → 应判失败
      h264Supported: true,
      decoderKind: 'webcodecs',
      decoderCodec: 'av1',
      decoderConfig: 'av01.0.04M.08',
      transportSamples: ['relay'],
    }),
  });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.strictEqual(byId(r, 'setup.webcodecs-off').ok, false);
  assert.strictEqual(byId(r, 'setup.view-mse').ok, false);
  assert.strictEqual(byId(r, 'setup.codec-h264').ok, false);
  assert.strictEqual(r.exitCode, 1);
});

test('MSE 期间断流（重连次数 >0）必须判失败', async () => {
  const clock = fakeClock();
  const session = fakeSession({
    clock,
    mode: 'mse',
    script: {
      framesAt: (t) => 10 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'h264', config: 'avc1.42E01E', at: 0, accepted: true }),
    },
    health: () => ({ decodeErrors: [], playerErrors: [], streamRestarts: 2 }),
  });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.strictEqual(byId(r, 'end.mse-no-drop').ok, false, 'MSE 断流必须失败');
});

test('模式级错误统计覆盖该模式全部用例（LAN 段的错误不会被漏掉）', async () => {
  const clock = fakeClock();
  let anyCommandSent = false;
  const session = fakeSession({
    clock,
    mode: 'lan',
    onSend: () => { anyCommandSent = true; },
    script: {
      framesAt: (t) => 10 + Math.floor((t - 1_000_000) / 33),
      satAt: (t) => (Math.floor((t - 1_000_000) / 1000) % 2 ? 0.3 : 120),
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
    // 错误只在用例真正开始跑之后才出现——旧脚本在 LAN 用例之前就宣布
    // "全程无错误"，这类错误会被整个漏掉。
    health: () => ({
      decodeErrors: anyCommandSent ? ['Decoding error. | cfg=av01.0.04M.08'] : [],
      playerErrors: [],
      streamRestarts: 0,
    }),
  });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 1000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.ok(anyCommandSent, '本用例应至少执行了一条切换命令');
  assert.strictEqual(byId(r, 'end.no-decode-error').ok, false,
    'LAN 段出现的解码错误必须计入本模式统计');
});

test('LAN 中途掉回 relay 时 end.lan-throughout 必须失败', async () => {
  const clock = fakeClock();
  let calls = 0;
  const session = fakeSession({
    clock,
    mode: 'lan',
    script: {
      framesAt: (t) => 10 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
    snapshot: () => {
      calls += 1;
      return {
        transport: calls === 1 ? 'lan' : 'relay',
        lanAddrs: ['10.0.0.2:9000'],
        lanResponses: [{ ok: true, status: 200 }],
        lanBytes: 65536, wsBytes: 1024, relayBytes: 0, rtcAvailable: false,
        webcodecs: true, viewMode: 'webcodecs', h264Supported: true,
        decoderKind: 'webcodecs', decoderCodec: 'av1', decoderConfig: 'av01.0.04M.08',
        transportSamples: calls === 1 ? ['lan'] : ['lan', 'relay'],
      };
    },
  });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 1500, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.strictEqual(byId(r, 'end.lan-throughout').ok, false, 'LAN 中途掉回 relay 必须失败');
  assert.strictEqual(byId(r, 'end.transport-unchanged').ok, false);
});

test('全部达标的 relay 模式计划通过，退出码为 0', async () => {
  const clock = fakeClock();
  let gray = false;
  let codec = 'av1';
  let codecAt = 0;
  const session = fakeSession({
    clock,
    mode: 'relay',
    onSend: (cmd, c) => {
      if (cmd.type === 'desktop:gray') gray = !!cmd.enabled;
      if (cmd.type === 'desktop:codec' && cmd.codec !== codec) { codec = cmd.codec; codecAt = c.now(); }
    },
    script: {
      framesAt: (t) => 100 + Math.floor((t - 1_000_000) / 33),
      satAt: () => (gray ? 0.2 : 120),
      initAt: () => 1,
      codecAt: () => ({
        codec,
        config: codec === 'av1' ? 'av01.0.04M.08' : 'avc1.42E01E',
        at: codecAt, accepted: true,
      }),
    },
  });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 300, pollMs: 100, rounds: 2,
  });
  assert.deepStrictEqual(failedIds(r), [], `不应有失败项：${JSON.stringify(failedIds(r))}`);
  assert.strictEqual(r.exitCode, 0);
  assert.ok(r.passed > 0);
});

// ── 8. 汇总口径：跳过不算通过 ────────────────────────────────────
test('彩色基线被污染（饱和度过低）时必须判前置失败，不进入灰度用例', async () => {
  const clock = fakeClock();
  const session = fakeSession({
    clock,
    script: {
      // 实测：浏览器与抓屏共用 display 时基线从 ~127 掉到 ~14
      framesAt: (t) => 10 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 14.39,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 1000, growthWindowMs: 200, pollMs: 100, rounds: 5,
  });
  assert.strictEqual(byId(r, 'pre.color-baseline').ok, false,
    '被污染的彩色基线必须判失败');
  assert.strictEqual(r.failed, 1);
  // 只有"首帧 + 模式隔离判定"这类前置项执行并通过；灰度/codec 用例一项
  // 都不得执行（它们依赖可信的彩色基线）。
  assert.ok(!ids(r).some((id) => /^r\d+\./.test(id)), `灰度往返用例不得执行：${ids(r)}`);
  assert.ok(!ids(r).some((id) => id.startsWith('codec.')), 'codec 用例不得执行');
  assert.ok(!ids(r).some((id) => id.startsWith('noop.')), '同值 gray 用例不得执行');
  assert.ok(r.skipped > 0, '未执行的灰度用例必须显式记为跳过');
});

test('跳过项不计入通过数', () => {
  const r = H.summarize([
    { id: 'a', ok: true }, { id: 'b', ok: false },
  ], ['c — 未执行']);
  assert.strictEqual(r.total, 2);
  assert.strictEqual(r.passed, 1);
  assert.strictEqual(r.failed, 1);
  assert.strictEqual(r.skipped, 1);
  assert.strictEqual(r.exitCode, 1);
});

test('codecKindOf 归一 WebCodecs 与 MSE 两类写法', () => {
  assert.strictEqual(H.codecKindOf('av01.0.04M.08'), 'av1');
  assert.strictEqual(H.codecKindOf('video/mp4; codecs="av01.0.04M.08"'), 'av1');
  assert.strictEqual(H.codecKindOf('avc1.42E01E'), 'h264');
  assert.strictEqual(H.codecKindOf('video/mp4; codecs="avc1.42E01E"'), 'h264');
  assert.strictEqual(H.codecKindOf(''), 'other');
});
