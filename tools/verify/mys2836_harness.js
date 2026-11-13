'use strict';
/**
 * MYS-2842：桌面传输 / 灰度 / 编码验收的**检测逻辑**（纯逻辑，不做任何 I/O）。
 *
 * 为什么单独拆一个模块：验收脚本本身必须能被"可控输入"测试。本模块只接受
 * 一个 `probe` / `session` 抽象（帧计数、decoder 配置、画布采样、发送命令），
 * 真实运行时由 `mys2836_runtime_adapter.js` 用 Playwright 实现，契约测试由
 * `mys2836_harness.test.js` 用假数据实现——两边跑的是**同一份判定逻辑**。
 *
 * 四条硬约束（对应任务卡"固定设计" + 第一次验收退回意见）：
 *
 *   1. **证据必须属于同一个生效配置**。旧实现分别锁存 codec/target/growth，
 *      于是"旧 H264 灰度帧的增长"可以和第 5 秒才生效的 AV1 configure 拼成
 *      一次成功。现在引入**配置代次（epoch）**：任何一次新的 configure/init
 *      都会开启新代次并**清空**上一代的全部成功证据；目标画面与新帧增长都
 *      必须在新代次内重新建立。目标态确认后若被观测到持续回退，同样清空。
 *
 *   2. **一条命令只有一个截止时间**。到期后**不再采样**，因此在截止之后才
 *      出现的 codec / 目标态不可能被记成通过。
 *
 *   3. **"持续产帧"必须是帧计数真的增长**：目标态稳定后才取增长基线，
 *      同一张静止画布重复采样不会让计数变。
 *
 *   4. **同值命令要观察完整窗口再判**：不能"收到第一帧就提前成功"，窗口
 *      结束时既要求没有重配置，也要求末尾仍在产帧（先出帧、稍后停流要失败）。
 */

const DEFAULTS = {
  /** 单条命令的总预算（任务卡要求 15s）。 */
  deadlineMs: 15000,
  /** 目标态达成后继续观察的时长（任务卡要求 ≥1s）。 */
  growthWindowMs: 1000,
  /**
   * 目标态达成后先等画面稳定再取增长基线（ms）。切换瞬间那一帧（或那一批
   * 队列里的旧帧）不能被当成"持续产帧"的证据。
   */
  settleMs: 300,
  /**
   * 判定"目标画面已达成"所需的**连续**采样次数。切换过程中画布会短暂停在
   * 过渡值（实测 AV1 切灰度途中采到 saturation=8.67，真正的灰度态是 0），
   * 只认第一次命中等于放松验收。
   */
  confirmSamples: 2,
  /** 已确认的目标态连续多少次不达标才算"回退"（对称防抖，避免单帧噪声）。 */
  loseSamples: 2,
  /**
   * 目标态 + 增长都成立后，再保持观察多久才收工。不能一拿到增长就立刻
   * break：那会让"目标态刚命中、随即回退"这一类失败完全测不到（证据锁存
   * 在成功那一刻，之后的回退没人看）。
   */
  holdMs: 1000,
  /** 轮询间隔。 */
  pollMs: 120,
  /** 前置：首帧出画预算。 */
  preflightMs: 30000,
  /** 彩色→灰度→彩色 轮数（任务卡要求 5）。 */
  rounds: 5,
  /** 同值 gray 重复发送次数。 */
  noopRepeats: 4,
  /**
   * 同值命令的**完整观察窗口**：窗口跑完才判"无重建 + 仍在产帧"，不能收到
   * 第一帧就提前成功（否则"先出帧、稍后重建/停流"会被判通过）。
   */
  noopObserveMs: 2000,
  /**
   * 彩色基线最低饱和度。确定性彩色测试画面实测 ≈127；一旦画面被别的窗口
   * 污染（例如浏览器和被抓屏幕共用 display），基线会掉到 ~14——那时灰度
   * 判据（基线的 10%）只有 1.4，任何噪声都能"通过"。
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

/** probe.frames() 允许返回 null = 该路径没有可用的真实帧指标（不可验证）。 */
async function readCount(fn) {
  const v = await fn();
  if (v === null || v === undefined) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
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
 *   frames: () => Promise<?number>,     // 累计真实解码/呈现帧数（null=不可验证）
 *   inits: () => Promise<number>,       // 累计 decoder 重配置/init 次数
 *   decoder: () => Promise<{kind,codec,config,at,accepted}>,
 *   saturation: () => Promise<?number>,
 *   send: (cmd) => Promise<void>,
 * }}
 * @param spec {{
 *   name: string, command: object,
 *   wantCodec?: ?('av1'|'h264'),   // 必须在发送之后观察到该 codec 的实际 configure
 *   expectCodec?: ?('av1'|'h264'), // 结束时当前生效 codec 必须是它（不要求重配置）
 *   isTarget: (sat:number) => boolean,
 *   requireNoReinit?: boolean,
 * }}
 */
async function runSwitch(probe, spec, opts) {
  const o = normalizeOpts(opts);
  const t0 = o.now();
  const deadlineAt = t0 + o.deadlineMs;

  const framesBefore = await readCount(probe.frames);
  const initsBefore = await readCount(probe.inits);
  await probe.send(spec.command);
  const sentAt = o.now();

  // 配置代次：任何一次新 configure/init 都开启新代次并清空旧证据。
  let epochAt = sentAt;
  let epochInits = initsBefore === null ? 0 : initsBefore;
  let framesAtEpoch = framesBefore;
  let epochs = 0;

  let frameAt = null;        // 当前代次内首次出现新帧的时刻
  let codecAt = null;        // 当前代次内首此确认目标 codec 的时刻
  let codecSeen = null;
  let codecConfig = '';
  let targetAt = null;       // 当前代次内目态确认时刻
  let targetValue = null;
  let targetStreak = 0;
  let loseStreak = 0;
  let growthBase = null;
  let growthBaseAt = 0;
  let growthFrames = 0;
  let growthWindowMs = 0;
  let growthOk = false;
  let growthOkAt = null;
  let targetLost = false;
  let lastSat = null;
  let lastFrames = framesBefore;
  let lastNonTargetSat = null;
  let unsupportedFrames = false;
  let samples = 0;

  const resetEvidence = () => {
    frameAt = null;
    codecAt = null;
    targetAt = null;
    targetValue = null;
    targetStreak = 0;
    loseStreak = 0;
    growthBase = null;
    growthBaseAt = 0;
    growthFrames = 0;
    growthWindowMs = 0;
    growthOk = false;
    growthOkAt = null;
    // targetLost 不在这里清：它由**显式回退**置位（见步骤 3）。但代次切换
    // 意味着"上一次的证据整体作废"，所以在代次切换处单独清一次——
    // 编码器重建期间画面短暂离开目标态是正常过渡，不该被判成回退失败。
  };

  for (;;) {
    // 到期即停：截止之后出现的 codec / 目标态绝不能被记成通过。
    if (o.now() >= deadlineAt) break;

    const t = o.now();
    const f = await readCount(probe.frames);
    if (f === null) { unsupportedFrames = true; break; }
    const inits = await readCount(probe.inits);
    const dec = (await probe.decoder()) || {};
    lastSat = await probe.saturation();
    lastFrames = f;
    samples += 1;

    // ── 配置代次切换：旧配置下的一切证据作废 ──────────────────────
    if (inits !== null && inits > epochInits) {
      epochInits = inits;
      // 代次的起点取"触发这次代次切换的那次 configure 的时间"（dec.at），
      // 而不是我们**轮询发现**它的时间——后者总是更晚（最长晚一个
      // pollMs），会把同一代次的 configure 误判成"上一代"的产物。
      const at = Number(dec.at) || 0;
      epochAt = (at >= sentAt && at <= t) ? at : t;
      framesAtEpoch = f;
      epochs += 1;
      // 新代次 = 全新证据：连"曾回退"也一并清掉（重建期间的短暂离态不算退）。
      targetLost = false;
      resetEvidence();
    }

    // 1) 新代次内的新解码帧
    if (frameAt === null && f > framesAtEpoch) frameAt = t;

    // 2) codec 真的生效：命令发出之后发生的 configure，且当前仍生效。
    //    代次切换会清空本条，但触发切换的那次 configure 本身立即符合条件
    //    —— 所以"新 codec 立刻可用"不会因为重置而丢失，而"旧 codec 时期
    //    攒下的目标态/增长"仍然被清掉，这正是本次返修要的效果。
    if (spec.wantCodec && codecAt === null) {
      const at = Number(dec.at) || 0;
      if (dec.codec === spec.wantCodec && dec.accepted !== false && at >= sentAt) {
        codecAt = t;
        codecSeen = dec.codec;
        codecConfig = dec.config || '';
      }
    }

    // 3) 目标画面真的上屏：本代次内连续 confirmSamples 次采样都命中
    if (targetAt === null) {
      if (lastSat !== null && spec.isTarget(lastSat)) {
        targetStreak += 1;
        loseStreak = 0;
        if (targetStreak >= o.confirmSamples) {
          targetAt = t;
          targetValue = lastSat;
        }
      } else {
        targetStreak = 0;
        if (lastSat !== null) lastNonTargetSat = lastSat;
      }
    } else if (lastSat !== null && !spec.isTarget(lastSat)) {
      // 已确认的目标态回退：连续 loseSamples 次不达标即撤销确认（并清增长）
      loseStreak += 1;
      lastNonTargetSat = lastSat;
      if (loseStreak >= o.loseSamples) {
        // 曾确认过的目标态又回退：这是粘性失败，之后即使再次命中也不翻案
        // ——否则"命中→回退→再命中"会被最后那一刻的成功掩盖。
        targetLost = true;
        targetAt = null;
        targetValue = null;
        targetStreak = 0;
        loseStreak = 0;
        growthBase = null;
        growthBaseAt = 0;
        growthFrames = 0;
        growthWindowMs = 0;
        growthOk = false;
        growthOkAt = null;
      }
    } else {
      loseStreak = 0;
    }

    // 4) 目标态稳定后帧计数仍在增长（基线在 settleMs 之后才取）
    if (targetAt !== null && !growthOk) {
      if (growthBase === null && t - targetAt >= o.settleMs) {
        growthBase = f;
        growthBaseAt = t;
      }
      if (growthBase !== null) {
        growthFrames = f - growthBase;
        growthWindowMs = t - growthBaseAt;
        if (growthFrames > 0 && growthWindowMs >= o.growthWindowMs) {
          if (!growthOk) { growthOk = true; growthOkAt = t; }
        }
      }
    }

    // 增长成立后还要继续观察 holdMs：一旦目标态回退，resetEvidence 会把
    // growthOk 清掉，循环继续跑到截止/重新达成，不会被"早成功"掩盖。
    const codecDone = !spec.wantCodec || codecAt !== null;
    if (codecDone && targetAt !== null && growthOk && (t - growthOkAt) >= o.holdMs) break;
    await o.sleep(o.pollMs);
  }

  const initsAfter = await readCount(probe.inits);
  const decEnd = (await probe.decoder()) || {};
  const endAt = o.now();
  const checks = [];

  if (unsupportedFrames) {
    // 没有可用的真实帧指标 → 明确判不可验证，绝不退回时间推算。
    checks.push(CHECK(`${spec.name}.frame`, false,
      '该路径没有可用的真实帧指标（requestVideoFrameCallback / getVideoPlaybackQuality 均不可用），无法验证真实产帧'));
    if (spec.wantCodec) {
      checks.push(CHECK(`${spec.name}.codec`, false, '帧指标不可用，未完成切换验证'));
    }
    if (spec.expectCodec) {
      checks.push(CHECK(`${spec.name}.codec-stable`, false, '帧指标不可用，未完成切换验证'));
    }
    checks.push(CHECK(`${spec.name}.target`, false, '帧指标不可用，无法确认目标画面随新帧上屏'));
    checks.push(CHECK(`${spec.name}.growth`, false, '帧指标不可用，无法验证持续产帧'));
    if (spec.requireNoReinit) {
      checks.push(CHECK(`${spec.name}.no-reinit`, false, '帧指标不可用，未完成验证'));
    }
    return {
      name: spec.name, checks, recoveryMs: null,
      framesBefore, framesAfter: null, targetValue: null,
      growthFrames: 0, codecConfig: '', elapsedMs: endAt - t0, epochs,
    };
  }

  checks.push(CHECK(
    `${spec.name}.frame`,
    frameAt !== null,
    frameAt === null
      ? `本代次内（自 ${epochAt - t0}ms 起）${o.deadlineMs}ms 预算内无新解码帧（帧计数停在 ${framesAtEpoch}）`
      : `${frameAt - t0}ms 后出新解码帧（本代次内）`,
  ));

  if (spec.wantCodec) {
    checks.push(CHECK(
      `${spec.name}.codec`,
      codecAt !== null,
      codecAt !== null
        ? `${codecAt - t0}ms 后实际配置为 ${codecConfig || codecSeen}`
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
    targetAt !== null && !targetLost,
    targetLost
      ? `目标画面曾确认后回退（末次 saturation=${lastSat}）：不得判为目标态达成`
      : targetAt !== null
        ? `${targetAt - t0}ms 后目标画面出现（连续 ${o.confirmSamples} 次采样收敛，saturation=${targetValue}）`
        : `${o.deadlineMs}ms 内未出现目标画面（末次 saturation=${lastSat}${
          lastNonTargetSat !== null ? `，曾采到 ${lastNonTargetSat} 但未收敛` : ''}）`,
  ));

  checks.push(CHECK(
    `${spec.name}.growth`,
    growthOk,
    growthOk
      ? `目标态后 ${growthWindowMs}ms 内继续增帧 ${growthFrames} 帧`
      : `目标态后 ${growthWindowMs}ms 仅增 ${growthFrames} 帧（要求 ≥${o.growthWindowMs}ms 内持续增长）`,
  ));

  if (spec.requireNoReinit) {
    const delta = (initsAfter === null ? 0 : initsAfter) - (initsBefore === null ? 0 : initsBefore);
    checks.push(CHECK(
      `${spec.name}.no-reinit`,
      delta === 0,
      delta === 0 ? '未发生重配置/init 增加' : `发生 ${delta} 次重配置/init（同值命令不应重建）`,
    ));
  }

  // 恢复耗时：目标画面与 codec 都真正生效才算恢复。
  let recoveryMs = null;
  if (targetAt !== null && !targetLost && (!spec.wantCodec || codecAt !== null)) {
    recoveryMs = Math.max(targetAt, codecAt === null ? t0 : codecAt) - t0;
  }

  return {
    name: spec.name,
    checks,
    recoveryMs,
    framesBefore,
    framesAfter: lastFrames,
    targetValue,
    growthFrames,
    codecConfig: codecConfig || decEnd.config || '',
    elapsedMs: endAt - t0,
    epochs,
  };
}

/**
 * 同值命令不得重建并持续产帧。
 *
 * 关键：**观察完整窗口之后**才判定。旧实现在收到第一个新帧时立即 break，
 * 于是"先出帧、随后的重建/停流"完全测不到。现在窗口跑满，并同时要求
 * 窗口末尾仍在产帧（不只看窗口内"曾经"出过帧）。
 */
async function runNoopCheck(probe, spec, opts) {
  const o = normalizeOpts(opts);
  const t0 = o.now();
  const observeMs = Math.min(spec.observeMs || o.noopObserveMs, o.deadlineMs);
  const windowEnd = t0 + observeMs;

  const framesBefore = await readCount(probe.frames);
  const initsBefore = await readCount(probe.inits);

  for (let i = 0; i < (spec.repeats || o.noopRepeats); i++) {
    await probe.send(spec.command);
    await o.sleep(Math.min(120, o.pollMs));
  }

  const samples = [];
  let initsMax = initsBefore === null ? 0 : initsBefore;
  let unsupported = false;
  for (;;) {
    if (o.now() >= windowEnd) break;
    const f = await readCount(probe.frames);
    if (f === null) { unsupported = true; break; }
    samples.push({ t: o.now(), f });
    const i2 = await readCount(probe.inits);
    if (i2 !== null && i2 > initsMax) initsMax = i2;
    await o.sleep(o.pollMs);
  }
  const initsAfter = await readCount(probe.inits);
  const delta = (initsAfter === null ? 0 : initsAfter) - (initsBefore === null ? 0 : initsBefore);
  const maxDelta = initsMax - (initsBefore === null ? 0 : initsBefore);

  const checks = [];
  if (unsupported) {
    checks.push(CHECK(`${spec.name}.no-reinit`, false,
      '该路径没有可用的真实帧指标，无法完成无重建观察'));
    checks.push(CHECK(`${spec.name}.frames`, false,
      '该路径没有可用的真实帧指标，无法验证持续产帧'));
    return {
      name: spec.name, checks, recoveryMs: null,
      framesBefore, framesAfter: null,
    };
  }

  checks.push(CHECK(
    `${spec.name}.no-reinit`,
    delta === 0 && maxDelta === 0,
    (delta === 0 && maxDelta === 0)
      ? `完整 ${observeMs}ms 窗口内未发生重配置/init 增加`
      : `完整 ${observeMs}ms 窗口内发生 ${Math.max(delta, maxDelta)} 次重配置/init（同值命令不应重建）`,
  ));

  const finalF = samples.length ? samples[samples.length - 1].f : framesBefore;
  const midF = samples.length >= 3 ? samples[Math.floor(samples.length / 2)].f : null;
  const grewAtAll = finalF !== null && framesBefore !== null && finalF > framesBefore;
  // 末尾仍在产帧：窗口后半段还要继续增帧，"先出帧、稍后停流"不能被判通过。
  const grewAtEnd = midF === null ? grewAtAll : finalF > midF;
  checks.push(CHECK(
    `${spec.name}.frames`,
    grewAtAll && grewAtEnd,
    !grewAtAll
      ? `完整 ${observeMs}ms 窗口内无新解码帧（帧计数停在 ${framesBefore}）`
      : (!grewAtEnd
        ? `窗口前半段出过帧（${midF}）但后半段停住（${finalF}）：同值命令打断了正在解码的流`
        : `完整 ${observeMs}ms 窗口内持续产帧（${framesBefore} → ${finalF}，中段 ${midF}）`),
  ));

  return {
    name: spec.name,
    checks,
    recoveryMs: null,
    framesBefore,
    framesAfter: finalF,
  };
}

// ── 传输路径判定 ─────────────────────────────────────────────────
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
async function runModePlan(session, opts) {
  const o = normalizeOpts(opts);
  const mode = session.mode;
  const checks = [];
  const skipped = [];
  const log = o.log;
  const metrics = { mode, recoveries: [], frames: 0, codecConfig: '', transport: null };

  const push = (list) => { for (const c of list) checks.push(c); };
  const skip = (what, why) => { skipped.push(`${mode}:${what} — ${why}`); };

  // ── 前置：真实解码出画。失败则通过数必须为 0 ─────────────────────
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

  // ── 彩色基线 ────────────────────────────────────────────────────
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

  // ── 5 轮彩色→灰度→彩色 ──────────────────────────────────────────
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

    metrics.frames = off.framesAfter === null ? metrics.frames : off.framesAfter;
  }

  // ── 同值 gray：完整窗口内不得重建，且末尾仍在产帧 ─────────────────
  const noop = await runNoopCheck(session, {
    name: 'noop.gray',
    command: { type: 'desktop:gray', enabled: false },
  }, o);
  push(noop.checks);

  // ── AV1 → H264 → AV1 与灰度组合（MSE 固定 H264，不做编码往返）─────
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
      isTarget: isGray,
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

  // ── 模式级错误统计（本模式全部用例跑完之后才判定）────────────────
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
    const restarts = health.streamRestarts;
    checks.push(CHECK('end.mse-no-drop',
      typeof restarts === 'number' && restarts === 0,
      typeof restarts !== 'number'
        ? '未能观测 MSE 流生命周期（重连计数不可用），不能宣称不断流'
        : (restarts === 0 ? 'MSE 全程未重连（不断流）' : `MSE 期间流重连 ${restarts} 次（断流）`)));
  }

  const endFrames = await readCount(session.frames);
  if (endFrames !== null) metrics.frames = endFrames;
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
  return {
    total: list.length,
    passed,
    failed,
    skipped: (skipped || []).length,
    skippedList: skipped || [],
    exitCode: failed > 0 ? 1 : 0,
    checks: list,
  };
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
