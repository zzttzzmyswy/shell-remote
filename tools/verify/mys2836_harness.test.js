'use strict';
/**
 * MYS-2842 验收工具契约测试。
 *
 * 分三层，因为第一次验收退回的误报都属于"接线"而非"纯逻辑"：
 *
 *   A. 检测逻辑（mys2836_harness.js）——用假 probe 驱动；
 *   B. 运行时适配层（mys2836_runtime_adapter.js）——用假 page 驱动，
 *      测的就是运行时真正跑的那份接线（MSE 真实帧指标、重连计数）；
 *   C. 负责人退回时附的**原始复现脚本**，在本文件里以等价形式固化，
 *      保证同一类误报不会回归。
 *
 * 运行：node --test tools/verify/mys2836_harness.test.js
 */

const test = require('node:test');
const assert = require('node:assert');

const H = require('./mys2836_harness.js');
const A = require('./mys2836_runtime_adapter.js');

// ── 可控时钟 ─────────────────────────────────────────────────────
function fakeClock(start = 1_000_000) {
  let t = start;
  return {
    now: () => t,
    sleep: async (ms) => { t += Math.max(0, Number(ms) || 0); },
    advance: (ms) => { t += ms; },
    at: () => t,
  };
}

// ── 可控 probe（纯逻辑层）────────────────────────────────────────
function fakeProbe(opts) {
  const clock = opts.clock;
  const script = opts.script || {};
  const sent = [];
  const probe = {
    sent,
    async frames() { return script.framesAt ? script.framesAt(clock.now(), ctx) : 0; },
    async inits() { return script.initAt ? script.initAt(clock.now(), ctx) : 0; },
    async decoder() {
      return script.codecAt ? script.codecAt(clock.now(), ctx)
        : { codec: null, config: '', at: 0, accepted: true };
    },
    async saturation() { return script.satAt ? script.satAt(clock.now(), ctx) : null; },
    async send(cmd) {
      const rec = Object.assign({}, cmd, { at: clock.now() });
      sent.push(rec);
      if (opts.onSend) opts.onSend(rec, clock, probe);
    },
  };
  const ctx = { sent, clock, probe };
  probe.ctx = ctx;
  return probe;
}

const ids = (r) => r.checks.map((c) => c.id);
const byId = (r, id) => r.checks.find((c) => c.id === id);
const failedIds = (r) => r.checks.filter((c) => !c.ok).map((c) => c.id);

// ══════════════════════════════════════════════════════════════════
// C. 负责人退回附件的原始反例（最高优先级回归）
// ══════════════════════════════════════════════════════════════════

test('[退回反例] 旧 H264 灰度帧的增长不得冒充新 AV1 configure 的恢复证据', async () => {
  // 与 review3-false-positive.js 等价：
  //   0–5s：仍是 H264，画面是灰度(0)，帧数一路涨到 12；
  //   5s：才 configure AV1；
  //   5s 之后：0 新帧，画面变回彩色(127)。
  // 正确结论：AV1 代次内没有任何新帧、也没有灰度画面 → 必须失败。
  const clock = fakeClock(100000);
  const start = clock.at();
  const dt = () => clock.at() - start;
  const probe = fakeProbe({
    clock,
    script: {
      framesAt: () => Math.min(12, Math.floor(dt() / 120)),
      initAt: () => (dt() >= 5000 ? 2 : 1),
      codecAt: () => ({
        codec: dt() >= 5000 ? 'av1' : 'h264',
        config: dt() >= 5000 ? 'av01.0.04M.08' : 'avc1.42E01E',
        at: start + (dt() >= 5000 ? 5000 : -10000),
        accepted: true,
      }),
      satAt: () => (dt() < 5000 ? 0 : 127),
    },
  });

  const r = await H.runSwitch(probe, {
    name: 'late-codec',
    command: { type: 'desktop:codec', codec: 'av1' },
    wantCodec: 'av1',
    isTarget: (v) => v < 10,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000 });

  assert.strictEqual(r.checks.every((c) => c.ok), false,
    `四项全部 PASS 即为误报：${JSON.stringify(r.checks)}`);
  assert.strictEqual(byId(r, 'late-codec.target').ok, false,
    'AV1 生效时画面已是彩色(127)，旧 H264 的灰度画面不能算目标态');
  assert.strictEqual(byId(r, 'late-codec.growth').ok, false,
    'AV1 生效后 0 新帧，旧 H264 的帧增长不能算持续产帧');
  assert.strictEqual(byId(r, 'late-codec.codec').ok, true,
    'AV1 configure 确实发生了，这一项应当通过');
  assert.strictEqual(r.recoveryMs, null, '目标态未在新代次内达成，不得给出恢复时间');
});

test('目标态曾命中后回退，必须失败（清除旧成功状态）', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => 10 + Math.max(0, Math.floor(((t - (sentAt || t)) / 33))),
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
      // 先灰后彩：目标态在 ~100ms 确认、~400ms 增长成立（增长窗口 300ms），
      // 随后在 hold 观察窗口内（1000ms）回退到彩色——必须被检出。
      satAt: (t) => {
        if (sentAt === null) return 128;
        return (t - sentAt) < 1000 ? 0.5 : 128;
      },
    },
  });

  const r = await H.runSwitch(probe, {
    name: 'revert',
    command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 10,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 6000, growthWindowMs: 300, settleMs: 100, pollMs: 100 });

  assert.strictEqual(r.checks.every((c) => c.ok), false, '目标态回退后整体不得通过');
  assert.strictEqual(byId(r, 'revert.target').ok, false, '回退后目标态必须判失败');
});

// ══════════════════════════════════════════════════════════════════
// A. 检测逻辑
// ══════════════════════════════════════════════════════════════════

test('LAN 请求失败、Relay 出帧时不得通过 LAN 判定', () => {
  const setup = H.judgeModeSetup('lan', {
    transport: 'relay',
    lanAddrs: ['10.0.0.2:9000'],
    lanResponses: [{ ok: false, status: 0 }],
    lanBytes: 0,
    wsBytes: 98304,
    relayBytes: 0,
  });
  assert.strictEqual(byId({ checks: setup }, 'setup.transport-lan').ok, false);
  assert.strictEqual(byId({ checks: setup }, 'setup.lan-bytes').ok, false);
  assert.strictEqual(byId({ checks: setup }, 'setup.lan-response').ok, false);
});

test('LAN 只有请求没有字节也不得通过', () => {
  const setup = H.judgeModeSetup('lan', {
    transport: 'lan',
    lanAddrs: ['10.0.0.2:9000'],
    lanResponses: [{ ok: true, status: 200 }],
    lanBytes: 0, wsBytes: 0, relayBytes: 0,
  });
  assert.strictEqual(byId({ checks: setup }, 'setup.lan-bytes').ok, false);
});

test('LAN 真正被选用并消费到字节时通过', () => {
  const setup = H.judgeModeSetup('lan', {
    transport: 'lan',
    lanAddrs: ['10.0.0.2:9000'],
    lanResponses: [{ ok: true, status: 200 }],
    lanBytes: 131072, wsBytes: 0, relayBytes: 0,
  });
  assert.deepStrictEqual(failedIds({ checks: setup }), []);
});

test('Relay 模式要求 P2P 关闭且 LAN 零字节', () => {
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

test('目标态只出 1 帧后停止，不得通过"持续产帧"', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => (sentAt !== null && t >= sentAt + 200 ? 11 : 10),
      satAt: () => 0.2,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'grow', command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 5,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'grow.frame').ok, true);
  assert.strictEqual(byId(r, 'grow.target').ok, true);
  assert.strictEqual(byId(r, 'grow.growth').ok, false);
});

test('目标态后帧计数持续增长才通过，且恢复时间覆盖增长窗口', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => (sentAt === null ? 10 : 10 + Math.max(0, Math.floor((t - sentAt) / 33))),
      satAt: () => 0.4,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'grow-ok', command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 5,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.deepStrictEqual(failedIds(r), []);
  assert.ok(r.recoveryMs !== null && r.recoveryMs < 15000);
});

test('过渡帧（只命中一次目标判据）不得当成目标态收敛', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => 10 + Math.max(0, Math.floor(((t - (sentAt || t)) / 33))),
      satAt: (t) => {
        if (sentAt === null) return 128;
        const dt = t - sentAt;
        return dt >= 300 && dt < 350 ? 8.67 : 128;
      },
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'transient', command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 10,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 5000, growthWindowMs: 300, settleMs: 100, pollMs: 100 });

  assert.strictEqual(byId(r, 'transient.target').ok, false);
  assert.strictEqual(r.recoveryMs, null);
});

test('新代次（重建）后目标态重新建立时不算回退失败', async () => {
  // 编码器重建期间画面短暂离开目标态是正常过渡：代次切换会把"曾回退"
  // 一并清掉，新代次内重新建立的目标态必须能正常通过。
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => 10 + Math.max(0, Math.floor(((t - (sentAt || t)) / 33))),
      // 600ms 时发生一次重建（init 2），重建前后都应该是灰度
      initAt: (t) => (sentAt !== null && t - sentAt >= 600 ? 2 : 1),
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
      // 重建瞬间有一小段过渡（彩色），但很快回到灰度
      satAt: (t) => {
        if (sentAt === null) return 128;
        const dt = t - sentAt;
        return (dt >= 600 && dt < 700) ? 128 : 0.5;
      },
    },
  });

  const r = await H.runSwitch(probe, {
    name: 'rebuild', command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 10,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 8000, growthWindowMs: 300, settleMs: 100, pollMs: 100 });

  assert.strictEqual(byId(r, 'rebuild.target').ok, true,
    '重建过渡后在新代次内重新达成的目标态不得被判成"曾回退"');
});

test('codec 命令被 agent 忽略（实际配置不变）时不得通过', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'codec', command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264', isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'codec.codec').ok, false);
});

test('codec 只认命令发出之后生效的配置（旧配置不算）', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'stale', command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264', isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'stale.codec').ok, false);
});

test('codec 真正生效（发送后新 configure）时通过，并计入恢复时间', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: () => 120,
      initAt: () => 2,
      codecAt: (t) => (sentAt !== null && t >= sentAt + 400
        ? { codec: 'h264', config: 'avc1.42E01E', at: sentAt + 400, accepted: true }
        : { codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'ok', command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264', isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.deepStrictEqual(failedIds(r), []);
  assert.ok(r.recoveryMs >= 400);
});

test('configure 被拒绝（accepted=false）不得算生效', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => 20 + (sentAt === null ? 0 : Math.floor((t - sentAt) / 33)),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: (t) => (t >= (sentAt || 0) + 100
        ? { codec: 'h264', config: 'avc1.42E01E', at: sentAt + 100, accepted: false }
        : { codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'rejected', command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264', isTarget: (v) => v > 50,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'rejected.codec').ok, false);
});

// ── 同值 gray：完整窗口 ──────────────────────────────────────────
test('同值 gray 在完整窗口内触发重配置时必须失败', async () => {
  const clock = fakeClock();
  let sent = 0;
  const probe = fakeProbe({
    clock,
    onSend: () => { sent += 1; },
    script: {
      framesAt: (t) => 30 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 120,
      initAt: () => 1 + sent,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runNoopCheck(probe, {
    name: 'noop', command: { type: 'desktop:gray', enabled: false }, repeats: 3,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 3000, pollMs: 100, noopObserveMs: 1500 });

  assert.strictEqual(byId(r, 'noop.no-reinit').ok, false);
  assert.strictEqual(byId(r, 'noop.frames').ok, true);
});

test('同值 gray 先出帧、稍后重建必须失败（不得收到第一帧就提前成功）', async () => {
  const clock = fakeClock();
  const t0 = clock.at();
  const probe = fakeProbe({
    clock,
    script: {
      framesAt: (t) => 30 + Math.floor((t - t0) / 33),
      satAt: () => 120,
      // 窗口前段平静，800ms 时发生一次重建
      initAt: (t) => (t - t0 >= 800 ? 2 : 1),
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runNoopCheck(probe, {
    name: 'late-rebuild', command: { type: 'desktop:gray', enabled: false }, repeats: 2,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 4000, pollMs: 100, noopObserveMs: 2000 });

  assert.strictEqual(byId(r, 'late-rebuild.no-reinit').ok, false,
    '窗口内发生的重建必须在窗口结束后被检出，不能因提前 break 而漏掉');
});

test('同值 gray 先出帧、稍后停流必须失败', async () => {
  const clock = fakeClock();
  const t0 = clock.at();
  const probe = fakeProbe({
    clock,
    script: {
      // 0–600ms 涨帧，之后完全停住
      framesAt: (t) => 30 + Math.min(18, Math.floor((t - t0) / 33)),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runNoopCheck(probe, {
    name: 'stall', command: { type: 'desktop:gray', enabled: false }, repeats: 2,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 4000, pollMs: 100, noopObserveMs: 2000 });

  assert.strictEqual(byId(r, 'stall.frames').ok, false,
    '前半段出过帧但后半段停住，必须判失败');
});

test('同值 gray 不重配置且全程持续出帧时通过', async () => {
  const clock = fakeClock();
  const probe = fakeProbe({
    clock,
    script: {
      framesAt: (t) => 30 + Math.floor((t - 1_000_000) / 33),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runNoopCheck(probe, {
    name: 'noop-ok', command: { type: 'desktop:gray', enabled: false }, repeats: 3,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 3000, pollMs: 100, noopObserveMs: 1500 });

  assert.deepStrictEqual(failedIds(r), []);
});

// ── 首帧 / 截止时间 ──────────────────────────────────────────────
test('首帧未出时必须失败、通过数为 0，且未执行项只记为跳过', async () => {
  const clock = fakeClock();
  const session = {
    mode: 'relay',
    async open() { throw new Error('首帧超时（30000ms）'); },
    async frames() { return 0; },
    async inits() { return 0; },
    async decoder() { return { codec: null, config: '', at: 0, accepted: true }; },
    async saturation() { return null; },
    async send() {},
    async snapshot() { return { transport: 'relay' }; },
    async health() { return { decodeErrors: [], playerErrors: [], streamRestarts: null }; },
  };
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100, rounds: 5,
  });

  assert.strictEqual(r.passed, 0);
  assert.strictEqual(r.failed, 1);
  assert.strictEqual(r.exitCode, 1);
  assert.ok(r.skipped > 0);
  assert.ok(r.checks.every((c) => !c.ok));
});

test('同一命令的等待不会串成 30 秒', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      framesAt: (t) => (sentAt !== null && t >= sentAt + 14900 ? 11 : 10),
      satAt: () => 120,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const t0 = clock.at();
  const r = await H.runSwitch(probe, {
    name: 'budget', command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 5,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });
  const spent = clock.at() - t0;

  assert.ok(spent <= 15000 + 500, `总耗时 ${spent}ms 不得超过 15s 预算`);
  assert.strictEqual(byId(r, 'budget.frame').ok, true);
  assert.strictEqual(byId(r, 'budget.target').ok, false);
  assert.strictEqual(r.recoveryMs, null);
});

test('截止之后才出现的 codec 与目标态不得判通过', async () => {
  const clock = fakeClock();
  let sentAt = null;
  const probe = fakeProbe({
    clock,
    onSend: (c, clk) => { if (sentAt === null) sentAt = clk.now(); },
    script: {
      // 一切都在 15s 之后才就绪
      framesAt: (t) => 10 + (sentAt !== null && t >= sentAt + 15600 ? 500 : 0),
      satAt: (t) => (sentAt !== null && t >= sentAt + 15600 ? 0 : 128),
      initAt: () => 1,
      codecAt: (t) => (sentAt !== null && t >= sentAt + 15600
        ? { codec: 'h264', config: 'avc1.42E01E', at: sentAt + 15600, accepted: true }
        : { codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'after-deadline', command: { type: 'desktop:codec', codec: 'h264' },
    wantCodec: 'h264', isTarget: (v) => v < 5,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 15000, growthWindowMs: 1000, pollMs: 100 });

  assert.strictEqual(byId(r, 'after-deadline.codec').ok, false,
    '截止后出现的 codec 不能记成通过');
  assert.strictEqual(byId(r, 'after-deadline.target').ok, false,
    '截止后出现的目标态不能记成通过');
  assert.strictEqual(r.checks.every((c) => c.ok), false);
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

// ── 模式级 / 汇总 ────────────────────────────────────────────────
test('MSE 模式强制要求 WebCodecs 关闭且实际走 avc1', async () => {
  const clock = fakeClock();
  const session = {
    mode: 'mse',
    async open() { return { firstFrameMs: 900 }; },
    async frames() { return 100; },
    async inits() { return 1; },
    async decoder() { return { kind: 'webcodecs', codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }; },
    async saturation() { return 120; },
    async send() {},
    async snapshot() {
      return {
        transport: 'relay', lanAddrs: [], lanResponses: [], lanBytes: 0,
        wsBytes: 999, relayBytes: 0, rtcAvailable: false,
        webcodecs: true, viewMode: 'webcodecs', h264Supported: true,
        decoderKind: 'webcodecs', decoderCodec: 'av1', decoderConfig: 'av01.0.04M.08',
        transportSamples: ['relay'],
      };
    },
    async health() { return { decodeErrors: [], playerErrors: [], streamRestarts: 0 }; },
  };
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
  const session = {
    mode: 'mse',
    async open() { return { firstFrameMs: 900 }; },
    async frames() { return 100; },
    async inits() { return 1; },
    async decoder() { return { kind: 'mse', codec: 'h264', config: 'avc1.42E01E', at: 0, accepted: true }; },
    async saturation() { return 120; },
    async send() {},
    async snapshot() {
      return {
        transport: 'relay', lanAddrs: [], lanResponses: [], lanBytes: 0,
        wsBytes: 999, relayBytes: 0, rtcAvailable: false,
        webcodecs: false, viewMode: 'mse', h264Supported: true,
        decoderKind: 'mse', decoderCodec: 'h264', decoderConfig: 'avc1.42E01E',
        transportSamples: ['relay'],
      };
    },
    async health() { return { decodeErrors: [], playerErrors: [], streamRestarts: 2 }; },
  };
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.strictEqual(byId(r, 'end.mse-no-drop').ok, false);
});

test('重连计数"不可观测"时不得宣称不断流', async () => {
  const clock = fakeClock();
  const session = {
    mode: 'mse',
    async open() { return { firstFrameMs: 900 }; },
    async frames() { return 100; },
    async inits() { return 1; },
    async decoder() { return { kind: 'mse', codec: 'h264', config: 'avc1.42E01E', at: 0, accepted: true }; },
    async saturation() { return 120; },
    async send() {},
    async snapshot() {
      return {
        transport: 'relay', lanAddrs: [], lanResponses: [], lanBytes: 0,
        wsBytes: 999, relayBytes: 0, rtcAvailable: false,
        webcodecs: false, viewMode: 'mse', h264Supported: true,
        decoderKind: 'mse', decoderCodec: 'h264', decoderConfig: 'avc1.42E01E',
        transportSamples: ['relay'],
      };
    },
    async health() { return { decodeErrors: [], playerErrors: [], streamRestarts: null }; },
  };
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.strictEqual(byId(r, 'end.mse-no-drop').ok, false,
    '计数不可用 = 不能宣称不断流，必须失败');
});

test('模式级错误统计覆盖该模式全部用例', async () => {
  const clock = fakeClock();
  let anyCommandSent = false;
  const session = {
    mode: 'lan',
    async open() { return { firstFrameMs: 900 }; },
    async frames() { return 10 + Math.floor((clock.now() - 1_000_000) / 33); },
    async inits() { return 1; },
    async decoder() { return { kind: 'webcodecs', codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }; },
    async saturation() { return 120; },
    async send() { anyCommandSent = true; },
    async snapshot() {
      return {
        transport: 'lan', lanAddrs: ['10.0.0.2:9000'],
        lanResponses: [{ ok: true, status: 200 }], lanBytes: 65536,
        wsBytes: 0, relayBytes: 0, rtcAvailable: false,
        webcodecs: true, viewMode: 'webcodecs', h264Supported: true,
        decoderKind: 'webcodecs', decoderCodec: 'av1', decoderConfig: 'av01.0.04M.08',
        transportSamples: ['lan'],
      };
    },
    async health() {
      return {
        decodeErrors: anyCommandSent ? ['Decoding error. | cfg=av01.0.04M.08'] : [],
        playerErrors: [], streamRestarts: null,
      };
    },
  };
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 1000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.ok(anyCommandSent);
  assert.strictEqual(byId(r, 'end.no-decode-error').ok, false);
});

test('LAN 中途掉回 relay 时 end.lan-throughout 必须失败', async () => {
  const clock = fakeClock();
  let calls = 0;
  const session = {
    mode: 'lan',
    async open() { return { firstFrameMs: 900 }; },
    async frames() { return 10 + Math.floor((clock.now() - 1_000_000) / 33); },
    async inits() { return 1; },
    async decoder() { return { kind: 'webcodecs', codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }; },
    async saturation() { return 120; },
    async send() {},
    async snapshot() {
      calls += 1;
      return {
        transport: calls === 1 ? 'lan' : 'relay',
        lanAddrs: ['10.0.0.2:9000'], lanResponses: [{ ok: true, status: 200 }],
        lanBytes: 65536, wsBytes: 1024, relayBytes: 0, rtcAvailable: false,
        webcodecs: true, viewMode: 'webcodecs', h264Supported: true,
        decoderKind: 'webcodecs', decoderCodec: 'av1', decoderConfig: 'av01.0.04M.08',
        transportSamples: calls === 1 ? ['lan'] : ['lan', 'relay'],
      };
    },
    async health() { return { decodeErrors: [], playerErrors: [], streamRestarts: null }; },
  };
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 1000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  assert.strictEqual(byId(r, 'end.lan-throughout').ok, false);
  assert.strictEqual(byId(r, 'end.transport-unchanged').ok, false);
});

test('彩色基线被污染时必须判前置失败，不进入灰度用例', async () => {
  const clock = fakeClock();
  const session = {
    mode: 'relay',
    async open() { return { firstFrameMs: 100 }; },
    async frames() { return 10 + Math.floor((clock.now() - 1_000_000) / 33); },
    async inits() { return 1; },
    async decoder() { return { kind: 'webcodecs', codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }; },
    async saturation() { return 14.39; },
    async send() {},
    async snapshot() {
      return {
        transport: 'relay', lanAddrs: [], lanResponses: [], lanBytes: 0,
        wsBytes: 1024, relayBytes: 0, rtcAvailable: false,
        webcodecs: true, viewMode: 'webcodecs', h264Supported: true,
        decoderKind: 'webcodecs', decoderCodec: 'av1', decoderConfig: 'av01.0.04M.08',
        transportSamples: ['relay'],
      };
    },
    async health() { return { decodeErrors: [], playerErrors: [], streamRestarts: null }; },
  };
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 1000, growthWindowMs: 200, pollMs: 100, rounds: 5,
  });
  assert.strictEqual(byId(r, 'pre.color-baseline').ok, false);
  assert.ok(!ids(r).some((id) => /^r\d+\./.test(id)));
  assert.ok(r.skipped > 0);
});

test('跳过项不计入通过数', () => {
  const r = H.summarize([{ id: 'a', ok: true }, { id: 'b', ok: false }], ['c — 未执行']);
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

test('帧指标不可用（null）时必须判不可验证，不得伪造通过', async () => {
  const clock = fakeClock();
  const probe = fakeProbe({
    clock,
    script: {
      framesAt: () => null,   // 适配层发现没有真实帧指标
      satAt: () => 0,
      initAt: () => 1,
      codecAt: () => ({ codec: 'av1', config: 'av01.0.04M.08', at: 0, accepted: true }),
    },
  });
  const r = await H.runSwitch(probe, {
    name: 'noframes', command: { type: 'desktop:gray', enabled: true },
    isTarget: (v) => v < 5,
  }, { now: clock.now, sleep: clock.sleep, deadlineMs: 3000, growthWindowMs: 300, pollMs: 100 });

  assert.strictEqual(r.checks.every((c) => c.ok), false);
  assert.strictEqual(byId(r, 'noframes.frame').ok, false);
  assert.strictEqual(byId(r, 'noframes.growth').ok, false);
});

// ══════════════════════════════════════════════════════════════════
// B. 运行时适配层（用假 page 驱动真实接线）
// ══════════════════════════════════════════════════════════════════

/** 假 page：按"注入函数是哪一个"分派返回值。 */
function fakePage(handlers) {
  const calls = [];
  return {
    calls,
    async evaluate(fn, arg) {
      calls.push({ fn, arg });
      const r = await handlers(fn, arg);
      return r;
    },
  };
}

function mseSession(page, extra) {
  return A.makeSession(page, Object.assign({
    mode: 'mse',
    base: 'http://127.0.0.1:1',
    log: () => {},
    sleep: async () => {},
    openDesktop: async () => {},
    waitFirstFrame: async () => true,
    playVideo: async () => {},
    sendCommand: async () => {},
    probeSupport: async () => ({ h264: true, webcodecs: false, rtc: false }),
  }, extra || {}));
}

test('[退回反例] MSE 帧数必须来自真实呈现帧，不得用 currentTime 推算', async () => {
  // 与 review3-false-positive.js 等价：播放时间每次前进 1s，
  // 但真实呈现帧恒为 1。
  let playback = 0;
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 1, source: 'requestVideoFrameCallback', started: 1, completed: 0, connectedAt: 0 };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 1, source: 'requestVideoFrameCallback', started: 1 },
        viewMode: 'mse',
      };
    }
    // 关键：即使 currentTime 一直前进，也不能影响帧数
    playback += 1;
    return { currentTime: playback, totalVideoFrames: 1 };
  });

  const session = mseSession(page);
  const before = await session.frames();
  const after = await session.frames();
  assert.strictEqual(before, 1);
  assert.strictEqual(after, 1, '播放时间前进而真实呈现帧不变时，帧数必须不变');
  assert.notStrictEqual(after - before, 30, '绝不能再用 currentTime×FPS 推算');
});

test('MSE 真实呈现帧增长时帧数正常增长', async () => {
  let presented = 5;
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented, source: 'requestVideoFrameCallback', started: 1, completed: 0, connectedAt: 0 };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented }, viewMode: 'mse',
      };
    }
    return 0;
  });
  const session = mseSession(page);
  assert.strictEqual(await session.frames(), 5);
  presented = 12;
  assert.strictEqual(await session.frames(), 12);
});

test('MSE 真实帧指标不可用时 frames() 返回 null（不可验证）', async () => {
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: null, source: 'none', started: 1, completed: 0, connectedAt: 0 };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'none', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: null, source: 'none' }, viewMode: 'mse',
      };
    }
    return { currentTime: 99 }; // 时间在走，但不是帧证据
  });
  const session = mseSession(page);
  assert.strictEqual(await session.frames(), null,
    '没有真实帧指标时必须返回 null，交由判定层判不可验证');
});

test('[退回反例] MSE 重连必须来自真实流生命周期，不能恒为 0', async () => {
  // 适配层必须把 page 里的 started/completed 接成重连计数。
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 42, source: 'requestVideoFrameCallback', started: 4, completed: 3, connectedAt: 0 };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 42, source: 'requestVideoFrameCallback' }, viewMode: 'mse',
      };
    }
    return 0;
  });
  const session = mseSession(page);
  const health = await session.health();
  assert.strictEqual(health.streamRestarts, 3,
    `发生过 3 次"结束后重连"，计数必须是 3，实际 ${health.streamRestarts}`);
});

test('适配层：初次连接不得被误计为重连', async () => {
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 10, source: 'requestVideoFrameCallback', started: 1, completed: 0, connectedAt: 0 };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 10 }, viewMode: 'mse',
      };
    }
    return 0;
  });
  const session = mseSession(page);
  const health = await session.health();
  assert.strictEqual(health.streamRestarts, 0, '只有一次进行中的连接时，重连数必须是 0');
});

test('适配层：重连事件接入后 end.mse-no-drop 必须失败（端到端接线）', async () => {
  const clock = fakeClock();
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 90, source: 'requestVideoFrameCallback', started: 3, completed: 2, connectedAt: 0 };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [{ kind: 'mse', codec: 'h264', config: 'avc1.42E01E', at: 0, accepted: true }],
        configCount: 1, lan: [], lanBytes: 0, wsBytes: 888,
        transportSamples: ['relay'], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 90, source: 'requestVideoFrameCallback' }, viewMode: 'mse',
      };
    }
    if (fn === A.PAGE_SATURATION) return 120;
    return { h264: true, webcodecs: false, rtc: false };
  });
  const session = mseSession(page, { probeSupport: async () => ({ h264: true, webcodecs: false, rtc: false }) });
  const r = await H.runModePlan(session, {
    now: clock.now, sleep: clock.sleep, deadlineMs: 1000, growthWindowMs: 200, pollMs: 100, rounds: 1,
  });
  // 该模式还有别的失败（帧数不涨等），这里只关心重连项被正确接线。
  assert.strictEqual(byId(r, 'end.mse-no-drop').ok, false,
    '真实重连事件必须让 end.mse-no-drop 失败，而不是恒为 0 的假通过');
});

test('rVFC 被节流但质量计数在涨时，不得误判成卡流', async () => {
  // 后台/不可见时 requestVideoFrameCallback 会暂停，而
  // getVideoPlaybackQuality().totalVideoFrames 仍反映真实解码帧。
  // 只看前者会把"rVFC 被节流"误报成产品卡流。
  let quality = 100;
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 7, quality, source: 'requestVideoFrameCallback' };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 7 }, viewMode: 'mse',
      };
    }
    return 0;
  });
  const session = mseSession(page);
  assert.strictEqual(await session.frames(), 100, '必须取两个真实指标中较大的那个');
  quality = 160;
  assert.strictEqual(await session.frames(), 160, '质量计数增长必须被看见');
});

test('两个真实指标都不涨时帧数确实不涨（真卡流仍能检出）', async () => {
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 7, quality: 100, source: 'requestVideoFrameCallback' };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 7 }, viewMode: 'mse',
      };
    }
    return 0;
  });
  const session = mseSession(page);
  assert.strictEqual(await session.frames(), 100);
  assert.strictEqual(await session.frames(), 100, '两个指标都停住时必须如实不涨');
});

test('适配层优先用注入的非侵入式流生命周期（不依赖页面内计数）', async () => {
  // 运行时里页面不参与生命周期观测（页面内包装 fetch 会与产品抢同一个
  // ReadableStream）。这里锁定：只要注入了 streamLifecycle 就用它，
  // 即使页面里的计数是 0。
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 50, source: 'requestVideoFrameCallback', started: 1, completed: 0 };
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 50 }, viewMode: 'mse',
      };
    }
    return 0;
  });
  const session = mseSession(page, {
    streamLifecycle: () => ({ started: 3, completed: 2 }),
  });
  const health = await session.health();
  assert.strictEqual(health.streamRestarts, 2,
    '注入的生命周期（2 次结束+重连）必须覆盖页面内计数');
});

test('适配层：无任何生命周期来源时返回 null（不可观测）', async () => {
  const page = fakePage((fn) => {
    if (fn === A.PAGE_MSE_METRICS) {
      return { presented: 50, source: 'requestVideoFrameCallback' }; // 不含 started/completed
    }
    if (fn === A.PAGE_STATE) {
      return {
        frames: null, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: { presented: 50 }, viewMode: 'mse',
      };
    }
    return 0;
  });
  const session = mseSession(page);
  const health = await session.health();
  assert.strictEqual(health.streamRestarts, null,
    '没有生命周期来源时必须返回 null，让 end.mse-no-drop 判失败而不是假通过');
});

test('computeStreamRestarts 的配对语义', () => {
  // 初次连接
  assert.strictEqual(A.computeStreamRestarts({ started: 1, completed: 0 }), 0);
  // 断一次后重连
  assert.strictEqual(A.computeStreamRestarts({ started: 2, completed: 1 }), 1);
  // 连断两次
  assert.strictEqual(A.computeStreamRestarts({ started: 3, completed: 2 }), 2);
  // started 虚高但流没结束过：被 completed 夹住，不虚增
  assert.strictEqual(A.computeStreamRestarts({ started: 5, completed: 0 }), 0);
  // 不可观测
  assert.strictEqual(A.computeStreamRestarts(null), null);
  assert.strictEqual(A.computeStreamRestarts({}), null);
  assert.strictEqual(A.computeStreamRestarts({ started: 'x', completed: 1 }), null);
});

test('适配层 inits() 用累计计数，不受 configs 截断影响', async () => {
  const page = fakePage((fn) => {
    if (fn === A.PAGE_STATE) {
      return {
        frames: 5, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        // configs 只保留尾部 64 条，但累计计数是 200
        configs: new Array(64).fill({ kind: 'webcodecs', codec: 'av1', config: 'av01.0.04M.08', at: 1, accepted: true }),
        configCount: 200,
        lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: null, viewMode: 'webcodecs',
      };
    }
    return 0;
  });
  const session = A.makeSession(page, { mode: 'relay', base: 'x', log: () => {}, sleep: async () => {} });
  assert.strictEqual(await session.inits(), 200);
});

test('适配层 webcodecs 路径的 frames() 取真实解码输出计数', async () => {
  let n = 7;
  const page = fakePage((fn) => {
    if (fn === A.PAGE_STATE) {
      return {
        frames: n, framesSource: 'webcodecs', decodeErrors: [], playerErrors: [],
        configs: [], configCount: 1, lan: [], lanBytes: 0, wsBytes: 0,
        transportSamples: [], transport: 'relay', lanAddrs: [], instances: 1,
        mse: null, viewMode: 'webcodecs',
      };
    }
    return 0;
  });
  const session = A.makeSession(page, { mode: 'relay', base: 'x', log: () => {}, sleep: async () => {} });
  assert.strictEqual(await session.frames(), 7);
  n = 11;
  assert.strictEqual(await session.frames(), 11);
});
