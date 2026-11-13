'use strict';
/**
 * MYS-2842：桌面传输 / 灰度 / 编码验收的**检测逻辑**（纯逻辑，不做任何 I/O）。
 *
 * 为什么单独拆一个模块：验收脚本本身必须能被"可控输入"测试。本模块只接受
 * 一个 `probe` / `session` 抽象（帧计数、decoder 配置、画布采样、发送命令），
 * 真实运行时由 `verify_mys2836_runtime.js` 用 Playwright 实现，契约测试由
 * `mys2836_harness.test.js` 用假数据实现——两边跑的是**同一份判定逻辑**，
 * 不存在"测试里另写一套判据"的偏差。
 *
 * 三条硬约束（对应任务卡"固定设计"）：
 *   1. 一条 gray/codec 命令只给**一个**截止时间：发命令起算，目标 codec、
 *      目标画面、新解码帧、持续增长全部在这一个窗口内确认。禁止"先等 15s
 *      新帧、再额外等 15s 饱和度"这类串联等待把窗口扩成 30s。
 *   2. "持续产帧"必须看到帧计数**真的增长**：同一张静止画布采样两次不算。
 *      目标态达成后继续观察 ≥ growthWindowMs 并要求帧计数增加。
 *   3. codec 只能由**实际生效的配置**确认（WebCodecs configure / MSE
 *      addSourceBuffer 的 mime），不能只确认"命令发出去了"。
 */

const DEFAULTS = {
  /** 单条命令的总预算（任务卡要求 15s）。 */
  deadlineMs: 15000,
  /** 目标态达成后继续观察的时长（任务卡要求 ≥1s）。 */
  growthWindowMs: 1000,
  /**
   * 目标态达成后先等画面稳定再取增长基线（ms）。切换瞬间那一帧（或那一批
   * 队列里的旧帧）不能被当成"持续产帧"的证据——目标态刚确认就取基线，
   * 一个只出 1 帧然后卡死的流会因为"计数比基线大"而假通过。
   */
  settleMs: 300,
  /**
   * 判定"目标画面已达成"所需的**连续**采样次数。切换过程中画布会短暂停在
   * 过渡值（实测 AV1 切灰度途中采到 saturation=8.67，而真正的灰度态是 0），
   * 只认第一次命中就是把"正在变灰"当成"已经是灰度"，等于放松验收。与
   * 2e321fb「目标模式判定要求连续两次采样收敛」同一口径。
   */
  confirmSamples: 2,
  /** 轮询间隔。 */
  pollMs: 120,
  /** 前置：首帧出画预算。 */
  preflightMs: 30000,
  /** 彩色→灰度→彩色 轮数（任务卡要求 5）。 */
  rounds: 5,
  /** 同值 gray 重复发送次数。 */
  noopRepeats: 4,
  /**
   * 同值命令后"仍在正常出帧"的观察预算。刻意远小于 15s：同值命令根本
   * 不该扰动正在解码的流，用满 15s 会把真实卡顿掩盖过去（测试画面每 33ms
   * 产一帧，正常状态下几百毫秒内必出新帧）。
   */
  noopFrameBudgetMs: 3000,
  /**
   * 彩色基线最低饱和度。确定性彩色测试画面实测 ≈127；一旦画面被别的窗口
   * 污染（例如浏览器和被抓屏幕共用 display），基线会掉到 ~14——那时灰度
   * 判据（基线的 10%）只有 1.4，任何噪声都能"通过"。用一个远高于噪声、
   * 远低于真实彩色的地板值把这种情况挡在前面。
   */
  minColorBaseline: 40,
};

function normalizeOpts(opts) {
  const o = Object.assign({}, DEFAULTS, opts || {});
  o.now = o.now || Date.now;
  o.sleep = o.sleep || ((ms) => new Promise((r) => setTimeout(r, ms)));
  o.log = o.log || (() => {});
  return o;
}

// ── 截止时间：一条命令只有一个 ────────────────────────────────────
function createDeadline(deadlineMs, now, startAt) {
  const t0 = startAt === undefined ? now() : startAt;
  return {
    startAt: t0,
    deadlineMs,
    elapsed: () => now() - t0,
    remaining: () => Math.max(0, t0 + deadlineMs - now()),
    expired: () => now() - t0 >= deadlineMs,
  };
}

// ── codec 串归一 ─────────────────────────────────────────────────
// WebCodecs configure 的 codec / MSE 的 mime 都要归一到产品语义的
// av1|h264，后续判据不依赖具体 profile/level 写法。
function codecKindOf(str) {
  const s = String(str || '').toLowerCase();
  if (s.startsWith('av01') || s.includes('av01')) return 'av1';
  if (s.startsWith('avc1') || s.includes('avc1')) return 'h264';
  return 'other';
}

const CHECK = (id, ok, detail) => ({ id, ok: !!ok, detail: detail || '' });

/**
 * 执行一条 gray/codec 切换并判定。
 *
 * @param probe {{
 *   frames: () => Promise<number>,      // 累计解码/上屏帧数（只增）
 *   inits: () => Promise<number>,       // 累计 decoder 重配置/init 次数
 *   decoder: () => Promise<{kind,codec,config,at,accepted}>, // 最近一次生效的配置
 *   saturation: () => Promise<?number>, // 当前画面平均饱和度（null=不可测）
 *   send: (cmd) => Promise<void>,       // 把命令发给 agent
 * }}
 * @param spec {{
 *   name: string, command: object,
 *   wantCodec?: ?('av1'|'h264'),   // 必须出现**发送之后**的新 configure 且为此 codec
 *   expectCodec?: ?('av1'|'h264'), // 结束时当前生效 codec 必须是它（不要求重配置）
 *   isTarget: (sat:number) => boolean, // 目标画面判据
 *   requireNoReinit?: boolean,     // 不得发生重配置/init 增加
 * }}
 */
async function runSwitch(probe, spec, opts) {
  const o = normalizeOpts(opts);
  const t0 = o.now();
  const deadline = createDeadline(o.deadlineMs, o.now, t0);

  const framesBefore = Number(await probe.frames()) || 0;
  const initsBefore = Number(await probe.inits()) || 0;
  await probe.send(spec.command);
  const sentAt = o.now();

  const st = {
    frameAt: null,
    codecAt: null,
    codecSeen: null,
    codecConfig: '',
    targetAt: null,
    targetValue: null,
    targetStreak: 0,
    lastNonTargetSat: null,
    framesAtTarget: 0,
    growthBase: null,
    growthBaseAt: 0,
    growthFrames: 0,
    growthWindowMs: 0,
    growthOk: false,
    lastFrames: framesBefore,
    lastSat: null,
    lastCodec: null,
    samples: 0,
  };

  for (;;) {
    st.samples += 1;
    st.lastFrames = Number(await probe.frames()) || 0;
    const dec = (await probe.decoder()) || {};
    st.lastSat = await probe.saturation();
    st.lastCodec = dec.codec || null;
    const t = o.now();

    // 1) 新解码帧
    if (st.frameAt === null && st.lastFrames > framesBefore) st.frameAt = t;

    // 2) codec 真的生效：必须是发送之后发生的 configure（旧配置不算）
    if (spec.wantCodec && st.codecAt === null) {
      const at = Number(dec.at) || 0;
      if (dec.codec === spec.wantCodec && dec.accepted !== false && at >= sentAt) {
        st.codecAt = t;
        st.codecSeen = dec.codec;
        st.codecConfig = dec.config || '';
      }
    }

    // 3) 目标画面真的上屏：要求**连续 confirmSamples 次**采样都命中，
    //    过渡帧（切换途中停在中间饱和度）不算目标态。
    if (st.targetAt === null) {
      if (st.lastSat !== null && spec.isTarget(st.lastSat)) {
        st.targetStreak += 1;
        if (st.targetStreak >= o.confirmSamples) {
          st.targetAt = t;
          st.targetValue = st.lastSat;
          st.framesAtTarget = st.lastFrames;
        }
      } else {
        st.targetStreak = 0;
        // 过渡帧留证：失败时能说清"看到的是过渡值还是从没接近过"。
        if (st.lastSat !== null) st.lastNonTargetSat = st.lastSat;
      }
    }

    // 4) 目标态稳定后帧计数仍在增长（同一张静止画布重复采样不会让计数变）。
    //    基线在 settleMs 之后才取：切换瞬间那一帧/队列里的旧帧不能充数。
    if (st.targetAt !== null) {
      if (st.growthBase === null && t - st.targetAt >= o.settleMs) {
        st.growthBase = st.lastFrames;
        st.growthBaseAt = t;
      }
      if (st.growthBase !== null) {
        st.growthFrames = st.lastFrames - st.growthBase;
        st.growthWindowMs = t - st.growthBaseAt;
        if (st.growthFrames > 0 && st.growthWindowMs >= o.growthWindowMs) st.growthOk = true;
      }
    }

    const codecDone = !spec.wantCodec || st.codecAt !== null;
    if (codecDone && st.targetAt !== null && st.growthOk) break;
    if (deadline.expired()) break;
    await o.sleep(o.pollMs);
  }

  const initsAfter = Number(await probe.inits()) || 0;
  const decEnd = (await probe.decoder()) || {};
  const checks = [];

  checks.push(CHECK(
    `${spec.name}.frame`,
    st.frameAt !== null,
    st.frameAt === null
      ? `${o.deadlineMs}ms 内无新解码帧（帧计数停在 ${framesBefore}）`
      : `${st.frameAt - t0}ms 后出新解码帧`,
  ));

  if (spec.wantCodec) {
    checks.push(CHECK(
      `${spec.name}.codec`,
      st.codecAt !== null,
      st.codecAt !== null
        ? `${st.codecAt - t0}ms 后实际配置为 ${st.codecConfig || st.codecSeen}`
        : `命令发出后未观察到 ${spec.wantCodec} 的实际配置（当前 ${decEnd.config || decEnd.codec || '无'}）`,
    ));
  }
  if (spec.expectCodec) {
    checks.push(CHECK(
      `${spec.name}.codec-stable`,
      decEnd.codec === spec.expectCodec && decEnd.accepted !== false,
      `当前生效 ${decEnd.config || decEnd.codec || '无'}`,
    ));
  }

  checks.push(CHECK(
    `${spec.name}.target`,
    st.targetAt !== null,
    st.targetAt !== null
      ? `${st.targetAt - t0}ms 后目标画面出现（连续 ${o.confirmSamples} 次采样收敛，saturation=${st.targetValue}）`
      : `${o.deadlineMs}ms 内未出现目标画面（末次 saturation=${st.lastSat}${
        st.lastNonTargetSat !== null ? `，曾采到 ${st.lastNonTargetSat} 但未收敛` : ''}）`,
  ));

  checks.push(CHECK(
    `${spec.name}.growth`,
    st.growthOk,
    st.growthOk
      ? `目标态后 ${st.growthWindowMs}ms 内继续增帧 ${st.growthFrames} 帧`
      : `目标态后 ${st.growthWindowMs}ms 仅增 ${st.growthFrames} 帧（要求 ≥${o.growthWindowMs}ms 内持续增长）`,
  ));

  if (spec.requireNoReinit) {
    const delta = initsAfter - initsBefore;
    checks.push(CHECK(
      `${spec.name}.no-reinit`,
      delta === 0,
      delta === 0 ? '未发生重配置/init 增加' : `发生 ${delta} 次重配置/init（同值命令不应重建）`,
    ));
  }

  // 恢复耗时：目标画面与 codec 都真正生效才算恢复（任务卡明确要求含
  // codec 生效时间，不能只算"画面看起来变了"）。
  let recoveryMs = null;
  if (st.targetAt !== null && (!spec.wantCodec || st.codecAt !== null)) {
    recoveryMs = Math.max(st.targetAt, st.codecAt === null ? t0 : st.codecAt) - t0;
  }

  return {
    name: spec.name,
    checks,
    recoveryMs,
    framesBefore,
    framesAfter: st.lastFrames,
    targetValue: st.targetValue,
    growthFrames: st.growthFrames,
    codecConfig: st.codecConfig || decEnd.config || '',
    elapsedMs: o.now() - t0,
  };
}

/**
 * 同值命令不得重建并持续产帧（对应任务卡"同值 gray 验证不得发生重配置/
 * init 增加，并持续产帧"）。与 runSwitch 共用同一个截止时间口径。
 */
async function runNoopCheck(probe, spec, opts) {
  const o = normalizeOpts(opts);
  const t0 = o.now();
  const framesBefore = Number(await probe.frames()) || 0;
  const initsBefore = Number(await probe.inits()) || 0;
  for (let i = 0; i < (spec.repeats || o.noopRepeats); i++) {
    await probe.send(spec.command);
    await o.sleep(Math.min(120, o.pollMs));
  }
  let frameAt = null;
  const budget = spec.frameBudgetMs || o.noopFrameBudgetMs;
  while (o.now() - t0 < budget) {
    if ((Number(await probe.frames()) || 0) > framesBefore) { frameAt = o.now(); break; }
    await o.sleep(o.pollMs);
  }
  const initsAfter = Number(await probe.inits()) || 0;
  const delta = initsAfter - initsBefore;
  return {
    name: spec.name,
    checks: [
      CHECK(`${spec.name}.no-reinit`, delta === 0,
        delta === 0 ? '同值命令未触发重配置' : `同值命令触发了 ${delta} 次重配置/init`),
      CHECK(`${spec.name}.frames`, frameAt !== null,
        frameAt === null
          ? `${budget}ms 内无新解码帧（同值命令打断了正在解码的流）`
          : `${frameAt - t0}ms 内继续出帧`),
    ],
    recoveryMs: null,
    framesBefore,
    framesAfter: Number(await probe.frames()) || 0,
  };
}

// ── 传输路径判定 ─────────────────────────────────────────────────
/**
 * 按模式判定"路径真的被用上"。这是本次返修的核心：旧脚本把
 * "发起过 LAN 请求"当作 LAN 生效，于是 LAN 请求失败、Relay 在出帧时
 * 仍然报 LAN 通过。现在 LAN 必须同时满足：
 *   - 实例自报 transport === 'lan'（不是从请求推断）
 *   - LAN 端点返回 200
 *   - 从 LAN 端点**消费到媒体字节**（读流计数 > 0）
 * 少了任何一条都判失败。
 *
 * @param mode 'relay'|'lan'|'mse'
 * @param snap 页面快照，见 runtime 的 session.snapshot()
 */
function judgeModeSetup(mode, snap) {
  const s = snap || {};
  const bytes = (v) => (typeof v === 'number' && isFinite(v) ? v : 0);
  const okResponses = (arr) => (Array.isArray(arr) ? arr.filter((r) => r && r.ok) : []);

  if (mode === 'relay') {
    return [
      CHECK('setup.p2p-off', s.rtcAvailable === false,
        s.rtcAvailable === false ? 'P2P 已禁用（测试隔离，排除 p2p 掩盖）' : 'P2P 仍可用，relay 结论不可信'),
      CHECK('setup.transport-relay', s.transport === 'relay',
        `实例 transport=${s.transport || '未知'}`),
      CHECK('setup.no-lan-bytes', bytes(s.lanBytes) === 0 && okResponses(s.lanResponses).length === 0,
        `LAN 字节=${bytes(s.lanBytes)}，成功 LAN 响应=${okResponses(s.lanResponses).length}（要求均为 0）`),
      CHECK('setup.relay-bytes', bytes(s.wsBytes) > 0 || bytes(s.relayBytes) > 0,
        `relay 字节：ws=${bytes(s.wsBytes)} fetch=${bytes(s.relayBytes)}`),
    ];
  }

  if (mode === 'lan') {
    const addrs = Array.isArray(s.lanAddrs) ? s.lanAddrs : [];
    return [
      CHECK('setup.lan-addrs', addrs.length > 0, `lan_addrs=${JSON.stringify(addrs)}`),
      CHECK('setup.transport-lan', s.transport === 'lan',
        `实例 transport=${s.transport || '未知'}（要求 lan）`),
      CHECK('setup.lan-response', okResponses(s.lanResponses).length > 0,
        `LAN 200 响应=${okResponses(s.lanResponses).length}，全部响应=${JSON.stringify((s.lanResponses || []).map((r) => r && r.status))}`),
      CHECK('setup.lan-bytes', bytes(s.lanBytes) > 0,
        `LAN 端点消费媒体字节=${bytes(s.lanBytes)}（>0 才算真的在用直连）`),
    ];
  }

  // mse：必须真的退到 MSE 且用的是受支持的 H264
  return [
    CHECK('setup.webcodecs-off', s.webcodecs === false,
      s.webcodecs === false ? 'WebCodecs 已禁用（强制 MSE）' : 'WebCodecs 仍可用，MSE 结论不可信'),
    CHECK('setup.view-mse', s.viewMode === 'mse', `播放器模式=${s.viewMode || '未知'}`),
    CHECK('setup.h264-supported', s.h264Supported === true, 'MP4/avc1 被浏览器支持'),
    CHECK('setup.codec-h264', s.decoderKind === 'mse' && s.decoderCodec === 'h264',
      `实际配置：${s.decoderConfig || '无'}（kind=${s.decoderKind || '?'}）`),
  ];
}

// ── 单个模式的完整验收计划 ────────────────────────────────────────
/**
 * 一个模式 = 一条独立的 relay/agent/Xvfb + 一个独立浏览器会话。
 *
 * session 接口（由 runtime 用 Playwright 实现，测试用假对象实现）：
 *   mode                                   'relay'|'lan'|'mse'
 *   open() -> {firstFrameMs:?number}        等首帧出画
 *   frames()/inits()/decoder()/saturation()/send(cmd)   见 runSwitch
 *   snapshot() -> 见 judgeModeSetup
 *   health()  -> {decodeErrors: string[], playerErrors: string[], streamRestarts:number}
 *   play()    -> 可选：确保播放器在播（MSE 用）
 */
async function runModePlan(session, opts) {
  const o = normalizeOpts(opts);
  const mode = session.mode;
  const checks = [];
  const skipped = [];
  const log = o.log;
  const metrics = { mode, recoveries: [], frames: 0, codecConfig: '', transport: null };

  const push = (list) => { for (const c of list) checks.push(c); };
  const skip = (what, why) => { skipped.push(`${mode}:${what} — ${why}`); };

  // ── 前置：真实解码出画。失败则通过数必须为 0（后续全部标记未执行）──
  let first = null;
  try {
    first = await session.open();
  } catch (e) {
    first = { firstFrameMs: null, error: String((e && e.message) || e) };
  }
  const firstMs = first && typeof first.firstFrameMs === 'number' ? first.firstFrameMs : null;
  checks.push(CHECK('pre.first-frame', firstMs !== null,
    firstMs !== null ? `首帧 ${firstMs}ms` : `首帧未出：${(first && first.error) || '超时'}`));
  if (firstMs === null) {
    skip('传输/隔离判定', '首帧未出，路径未被真实消费');
    skip(`灰度往返 ${o.rounds} 轮`, '首帧未出，未执行');
    if (mode !== 'mse') skip('AV1↔H264 组合', '首帧未出，未执行');
    skip('模式级错误统计', '首帧未出，未执行');
    return finish(checks, skipped, metrics);
  }

  if (typeof session.play === 'function') { try { await session.play(); } catch (e) { /* 非关键 */ } }

  // ── 路径/隔离判定 ────────────────────────────────────────────────
  const snap = await session.snapshot();
  metrics.transport = snap.transport || null;
  push(judgeModeSetup(mode, snap));

  // ── 彩色基线：灰度断言必须相对真实彩色基线取阈值 ────────────────
  await o.sleep(o.growthWindowMs);
  const colorSat = await session.saturation();
  checks.push(CHECK('pre.color-baseline',
    colorSat !== null && colorSat >= o.minColorBaseline,
    `彩色基线 saturation=${colorSat}（要求 ≥${o.minColorBaseline}；过低说明抓到的画面被其它窗口污染，灰度判据不可信）`));
  if (!(colorSat !== null && colorSat >= o.minColorBaseline)) {
    skip(`灰度往返 ${o.rounds} 轮`, '彩色基线不可用，灰度判据无意义');
    if (mode !== 'mse') skip('AV1↔H264 组合', '彩色基线不可用');
    skip('同值 gray no-op', '彩色基线不可用');
    skip('模式级错误统计', '前置未满足');
    return finish(checks, skipped, metrics);
  }
  const grayMax = Math.max(1.0, colorSat * 0.1);
  log(`灰度判据：saturation < ${grayMax.toFixed(2)}（彩色基线 ${colorSat} 的 10%）`);

  const isGray = (v) => v < grayMax;
  const isColor = (v) => v > colorSat * 0.5;

  // ── 5 轮彩色→灰度→彩色（Relay、LAN 各自独立执行）────────────────
  for (let r = 1; r <= o.rounds; r++) {
    const on = await runSwitch(session, {
      name: `r${r}.gray-on`,
      command: { type: 'desktop:gray', enabled: true },
      isTarget: isGray,
    }, o);
    push(on.checks);
    if (on.recoveryMs !== null) metrics.recoveries.push(on.recoveryMs);

    const off = await runSwitch(session, {
      name: `r${r}.gray-off`,
      command: { type: 'desktop:gray', enabled: false },
      isTarget: isColor,
    }, o);
    push(off.checks);
    if (off.recoveryMs !== null) metrics.recoveries.push(off.recoveryMs);

    // 上一轮若关键项失败，继续跑只会把同一条流越拖越死；但已执行的断言
    // 照实计数（不跳过、不虚增）。这里保留后续轮次，只为拿到完整证据。
    metrics.frames = off.framesAfter;
  }

  // ── 同值 gray：不得重配置，且持续产帧 ────────────────────────────
  const noop = await runNoopCheck(session, {
    name: 'noop.gray',
    command: { type: 'desktop:gray', enabled: false }, // 当前就是彩色
  }, o);
  push(noop.checks);

  // ── AV1 → H264 → AV1 与灰度组合（MSE 是固定 H264，不做编码往返）───
  if (mode !== 'mse') {
    const toH264 = await runSwitch(session, {
      name: 'codec.h264',
      command: { type: 'desktop:codec', codec: 'h264' },
      wantCodec: 'h264',
      isTarget: isColor,
    }, o);
    push(toH264.checks);
    if (toH264.recoveryMs !== null) metrics.recoveries.push(toH264.recoveryMs);
    metrics.codecConfig = toH264.codecConfig || metrics.codecConfig;

    const h264Gray = await runSwitch(session, {
      name: 'codec.h264-gray-on',
      command: { type: 'desktop:gray', enabled: true },
      expectCodec: 'h264',
      isTarget: isGray,
    }, o);
    push(h264Gray.checks);
    if (h264Gray.recoveryMs !== null) metrics.recoveries.push(h264Gray.recoveryMs);

    const backAv1 = await runSwitch(session, {
      name: 'codec.av1-while-gray',
      command: { type: 'desktop:codec', codec: 'av1' },
      wantCodec: 'av1',
      isTarget: isGray, // 灰度仍开着，切回 AV1 不得丢单色状态
    }, o);
    push(backAv1.checks);
    if (backAv1.recoveryMs !== null) metrics.recoveries.push(backAv1.recoveryMs);
    metrics.codecConfig = backAv1.codecConfig || metrics.codecConfig;

    const colorBack = await runSwitch(session, {
      name: 'codec.av1-gray-off',
      command: { type: 'desktop:gray', enabled: false },
      expectCodec: 'av1',
      isTarget: isColor,
    }, o);
    push(colorBack.checks);
    if (colorBack.recoveryMs !== null) metrics.recoveries.push(colorBack.recoveryMs);
  }

  // ── 模式级错误统计（必须在本模式全部用例跑完之后才判定）─────────
  // 旧脚本在 LAN 用例之前就宣布"全程无错误"，等于 LAN/MSE 段的错误从未
  // 被覆盖。这里每条断言都标了「本模式」，语义与执行范围一致。
  const snapEnd = await session.snapshot();
  const health = (await session.health()) || { decodeErrors: [], playerErrors: [], streamRestarts: 0 };
  const decErrs = Array.isArray(health.decodeErrors) ? health.decodeErrors : [];
  const playErrs = Array.isArray(health.playerErrors) ? health.playerErrors : [];
  checks.push(CHECK('end.no-decode-error', decErrs.length === 0,
    decErrs.length ? decErrs.slice(0, 3).join(' | ') : '本模式全程无解码器报错'));
  checks.push(CHECK('end.no-player-error', playErrs.length === 0,
    playErrs.length ? playErrs.slice(0, 3).join(' | ') : '本模式全程无播放器报错'));
  checks.push(CHECK('end.transport-unchanged', snapEnd.transport === snap.transport,
    `起始 transport=${snap.transport} 结束 transport=${snapEnd.transport}`));

  if (mode === 'lan') {
    const values = Array.isArray(snapEnd.transportSamples) ? snapEnd.transportSamples : [];
    const bad = values.filter((v) => v !== 'lan');
    checks.push(CHECK('end.lan-throughout', values.length > 0 && bad.length === 0,
      `全程 transport 采样=${JSON.stringify(values)}${bad.length ? `（出现 ${JSON.stringify(bad)}）` : ''}`));
    metrics.transport = 'lan-final=' + snapEnd.transport;
  }
  if (mode === 'mse') {
    const restarts = Number(health.streamRestarts) || 0;
    checks.push(CHECK('end.mse-no-drop', restarts === 0,
      restarts === 0 ? 'MSE 全程未重连（不断流）' : `MSE 期间流重连 ${restarts} 次（断流）`));
  }

  metrics.frames = Number(await session.frames()) || metrics.frames;
  if (!metrics.codecConfig) {
    const d = (await session.decoder()) || {};
    metrics.codecConfig = d.config || '';
  }
  return finish(checks, skipped, metrics);
}

function finish(checks, skipped, metrics) {
  const sum = summarize(checks, skipped);
  return Object.assign({}, sum, { metrics });
}

function summarize(checks, skipped) {
  const list = Array.isArray(checks) ? checks : [];
  const passed = list.filter((c) => c.ok).length;
  const failed = list.length - passed;
  const results = {
    total: list.length,
    passed,
    failed,
    skipped: (skipped || []).length,
    skippedList: skipped || [],
    exitCode: failed > 0 ? 1 : 0,
  };
  // 只有真正执行过断言才算"通过"：前置失败时 list 里只有那条失败项，
  // passed 天然为 0，未执行的项只出现在 skippedList 里。
  results.checks = list;
  return results;
}

module.exports = {
  DEFAULTS,
  createDeadline,
  codecKindOf,
  runSwitch,
  runNoopCheck,
  judgeModeSetup,
  runModePlan,
  summarize,
};
