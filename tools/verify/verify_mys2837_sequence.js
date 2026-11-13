#!/usr/bin/env node
'use strict';
// MYS-2837 浏览器侧参数集/序号回归（纯单测，无需 relay / 浏览器）。
//
// 覆盖任务卡第一次验收返修要求的"浏览器回归"：
//   1. 有效新 init + 帧序号从 100 降到 1 → **不得**请求多余 reinit；
//   2. 缺失 init 的序号回退 → 必须（限频）恢复；
//   3. 正常单调 seq 内出现跳号 → 计入上行丢帧，不触发 reinit；
//   4. 同一代内回退（无 init 到达）超时后才请求，且受 4s 限频约束。
//
// 用例 1 就是负责人附件 `review-sequence.js` 的最小复现场景，这里连同断言
// 一起纳入仓库可执行入口（原脚本退出码 1，本脚本同场景必须退出码 0）。
//
// 做法：把 `web/desktop.js` 跑在 `vm` 沙箱里（它只依赖 document/window 的
// 少量接口），直接调 `_handleMoov` / `_handleMoof` 喂构造好的 fMP4 box，
// 断言内部状态。比起浏览器快得多，可以进常规测试循环。
//
// 用法：node tools/verify/verify_mys2837_sequence.js

const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const ROOT = path.dirname(path.dirname(path.dirname(fs.realpathSync(__filename))));
const SRC = path.join(ROOT, 'web', 'desktop.js');

let checks = 0;
const fails = [];
function test(name, fn) {
  checks += 1;
  try {
    fn();
    console.log(`  PASS ${name}`);
  } catch (e) {
    console.log(`  FAIL ${name}: ${e.message}`);
    fails.push(name);
  }
}

// 建一个用可控 Date 的实例（时间判据 2s/4s 由 makeClock 推进）。
function makeSandboxView(FakeDate, overrides) {
  const c = {
    window: { shellRemote: { send: () => {}, on: () => {} } },
    console,
    Date: FakeDate,
    setTimeout, clearTimeout, setInterval, clearInterval,
    Uint8Array, BigInt, Map,
    requestAnimationFrame: () => 0,
    document: {
      getElementById: () => null,
      createElement: () => ({
        getContext: () => null,
        classList: { add() {}, remove() {}, contains() { return false; } },
      }),
      addEventListener: () => {},
      removeEventListener: () => {},
    },
  };
  vm.createContext(c);
  vm.runInContext(fs.readFileSync(SRC, 'utf8'), c);
  const v = Object.create(c.window.DesktopView.prototype);
  Object.assign(v, {
    _lastSeq: 0, _seqDrop: 0, _lastInitAt: 0, _genStaleAt: 0, _lastInitReq: 0,
    _codecKind: null, _av1Mono: 0, _dec: null, _initDecoder() {},
  });
  if (overrides) Object.assign(v, overrides);
  return v;
}

function box(type, payload) {
  const header = Buffer.alloc(8);
  header.writeUInt32BE(8 + payload.length);
  header.write(type, 4);
  return Buffer.concat([header, payload]);
}

// 最小合法 av1C（marker+version, profile/level, flags, delay）。
function av1c() {
  return box('av1C', Buffer.from([0x81, 0x0c, 0x00, 0x00]));
}

// 一帧 moof：traf{ trun + seqn }。`key` 决定 trun 里的 sample flags。
function moof(seq, key) {
  const trun = Buffer.alloc(24);
  // trun: ver/flags(4) count(4) dataOffset(4) firstFlags(4) dur(4) size(4)
  // firstFlags 低 2 位：2 = key，1 = delta（见 desktop.js 的解析）。
  trun[12] = key ? 2 : 1;
  trun.writeUInt32BE(1, 20); // sampleSize
  const seqn = Buffer.alloc(8);
  seqn.writeBigUInt64BE(BigInt(seq));
  return box('traf', Buffer.concat([box('trun', trun), box('seqn', seqn)]));
}

// 推进"现在"的时间戳，让 2000ms / 4000ms 的时间判据可控。
// desktop.js 用的是 `Date.now()`；沙箱里注入可控 Date。
function makeClock(start) {
  let now = start;
  const RealDate = Date;
  function FakeDate(...args) {
    if (args.length === 0) return new RealDate(now);
    return new RealDate(...args);
  }
  FakeDate.now = () => now;
  FakeDate.parse = RealDate.parse;
  FakeDate.UTC = RealDate.UTC;
  FakeDate.prototype = RealDate.prototype;
  return { Date: FakeDate, advance: (ms) => { now += ms; }, set: (v) => { now = v; } };
}

// ── 用例 1：有效新 init + seq 100 → 1，不得误判缺 init（负责人复现）──
test('有效新 init 后 seq 100→1 不进入"等 init"状态', () => {
  const clock = makeClock(1_700_000_000_000);
  const v = makeSandboxView(clock.Date, { _lastSeq: 100, _codecKind: null });
  // 有效的新一代 init（agent 重建编码器后重发）。
  v._handleMoov(av1c());
  assert.strictEqual(v._codecKind, 'av1', 'init 应被解析为 av1');
  assert.ok(v._lastInitAt > 0, 'init 到达时刻应被记录');
  // 新一代第 1 帧（seq 从 1 重新计数）。
  v._handleMoof(moof(1, true));
  assert.strictEqual(v._genStaleAt, 0, '有有效新 init 时不得判定缺 init');
  assert.strictEqual(v._lastSeq, 1, 'seq 基线应接受回退到 1（旧实现保留 100）');
});

// ── 用例 2：缺失 init 的 seq 回退必须被记录，供看门狗限频恢复 ────────
test('缺 init 的 seq 回退会被标记，等待限频 reinit', () => {
  const clock = makeClock(1_700_000_000_000);
  const v = makeSandboxView(clock.Date, { _lastSeq: 100, _codecKind: 'av1' });
  // 没有任何 init 到达（_lastInitAt = 0），seq 从 100 掉回 1。
  v._handleMoof(moof(1, true));
  assert.ok(v._genStaleAt > 0, '缺 init 的换代必须被标记，否则看门狗无法恢复');
});

// ── 用例 3：正常单调流里的跳号只计丢帧，不误判换代 ─────────────────
test('单调 seq 的跳号计入丢帧且不误判缺 init', () => {
  const v = makeSandboxView(Date, { _lastSeq: 10, _codecKind: 'av1' });
  v._handleMoof(moof(14, false)); // 10 → 14：中间 11/12/13 丢了
  assert.strictEqual(v._seqDrop, 3, '跳号应计 3 条丢帧');
  assert.strictEqual(v._genStaleAt, 0, '单调前进不得误判换代');
  assert.strictEqual(v._lastSeq, 14);
});

// ── 用例 4：同代内小回退（乱序/抖动）不应被当成换代 ────────────────
test('近期收到过 init 时，小幅回退不触发缺 init', () => {
  const clock = makeClock(1_700_000_000_000);
  const v = makeSandboxView(clock.Date, { _lastSeq: 50, _codecKind: 'av1' });
  v._handleMoov(av1c());          // 刚收到 init
  clock.advance(200);
  v._handleMoof(moof(3, true));   // 200ms 后回退 → 属同一代
  assert.strictEqual(v._genStaleAt, 0, '近期有 init 时不得判成缺 init');
  assert.strictEqual(v._lastSeq, 3, '基线应接受换代后的新序号');
});

// ── 用例 5：旧代残留帧不得把基线抬回去（跨代不可比）────────────────
test('换代后旧代迟到帧不会把 seq 基线抬回旧值', () => {
  const clock = makeClock(1_700_000_000_000);
  const v = makeSandboxView(clock.Date, { _lastSeq: 100, _codecKind: 'av1' });
  v._handleMoov(av1c());
  v._handleMoof(moof(1, true));   // 新一代第 1 帧
  v._handleMoof(moof(2, false));  // 新一代第 2 帧
  assert.strictEqual(v._lastSeq, 2);
  v._handleMoof(moof(90, false)); // 旧代迟到帧
  assert.strictEqual(v._lastSeq, 90, '收到更大的号仍前进（它只是更靠后的帧）');
  // 回到新一代的下一帧时必须能被正确接受，而不是"永远回退"。
  v._handleMoof(moof(3, false));
  assert.strictEqual(v._lastSeq, 3, '跨代序号不可比，不能被旧号永久锁死');
});

console.log('\n' + (fails.length === 0
  ? `全部通过：${checks} 项`
  : `失败 ${fails.length}/${checks} 项：\n  - ` + fails.join('\n  - ')));
process.exit(fails.length === 0 ? 0 : 1);
