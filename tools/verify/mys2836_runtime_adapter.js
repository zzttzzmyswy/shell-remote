'use strict';
/**
 * MYS-2842：浏览器观测适配层。
 *
 * 从 `verify_mys2836_runtime.js` 里**抽出来**的原因：第一次验收退回指出接线
 * 缺失（例如 `end.mse-no-drop` 依赖的 `streamRestarts` 从未传入，导致该项
 * 恒为 0 的假通过），而纯逻辑的假 session 测试抓不到这类问题。本模块只依赖
 * 一个极小的 `page` 接口（`evaluate` + 可选的 `on`），因此测试可以用假 page
 * 驱动真实的适配代码——测的就是线上跑的那份接线。
 *
 * 两条硬规矩：
 *
 *  1. **MSE 帧数必须是真实呈现帧**。旧实现用 `currentTime × 请求 FPS` 推算，
 *     播放时间前进但一帧都没呈现时也会"涨帧"。现在只用
 *     `requestVideoFrameCallback`（呈现帧）或 `getVideoPlaybackQuality().totalVideoFrames`
 *     （质量计数）；两者都不可用时返回 `null` = 不可验证，交由判定层明确失败，
 *     **绝不退回时间推算**。
 *
 *  2. **MSE 断流必须来自真实的流生命周期**。用 `started`（每次 connect 发起
 *     新拉流）与 `completed`（上述拉流的 read 循环结束）的配对来计数重连：
 *     只有"连通后又被结束并重新开始"才算一次重连，初次连接不算。
 */

const PAGE_STATE = function () {
  const S = window.__mys2842 || {};
  const v = S.instances && S.instances.length ? S.instances[S.instances.length - 1] : null;
  const cfgCount = (S.configs || []).length;
  return {
    frames: typeof S.frames === 'number' ? S.frames : null,
    framesSource: S.framesSource || 'none',
    decodeErrors: (S.decodeErrors || []).slice(),
    playerErrors: (S.playerErrors || []).slice(),
    configs: (S.configs || []).slice(-20),
    configCount: cfgCount,
    lan: (S.lan || []).slice(),
    lanBytes: S.lanBytes || 0,
    wsBytes: S.wsBytes || 0,
    rtcAvailable: !!S.rtcAvailable,
    transportSamples: (S.transportSamples || []).slice(),
    transport: v && v._transport ? String(v._transport) : 'unknown',
    lanAddrs: v && Array.isArray(v._lanAddrs) ? v._lanAddrs.slice() : [],
    instances: (S.instances || []).length,
    mse: S.mse || null,
    viewMode: (function () {
      const c = document.getElementById('desktop-canvas');
      if (c && !c.classList.contains('hidden')) return 'webcodecs';
      const el = document.getElementById('desktop-video');
      if (el && !el.classList.contains('hidden')) return 'mse';
      return 'unknown';
    })(),
  };
}

// MSE 真实呈现帧 + 流生命周期。与 PAGE_STATE 分开，便于测试单独驱动。
const PAGE_MSE_METRICS = function () {
  const S = window.__mys2842 || {};
  const m = S.mse || { presented: null, source: 'none' };
  return {
    presented: typeof m.presented === 'number' ? m.presented : null,
    // 浏览器自己的质量计数，用于交叉校验 rVFC 是否被节流（后台/不可见时
    // rVFC 会暂停，仅凭它会误判成"卡流"）。
    quality: typeof m.quality === 'number' ? m.quality : null,
    source: m.source || 'none',
  };
}

const PAGE_SATURATION = function () {
  const c = document.getElementById('desktop-canvas');
  const active = c && !c.classList.contains('hidden') && c.width && c.height;
  const src = active ? c : document.getElementById('desktop-video');
  if (!src) return null;
  const w = src.videoWidth || src.width || 0;
  const h = src.videoHeight || src.height || 0;
  if (!w || !h) return null;
  const t = document.createElement('canvas');
  t.width = 64; t.height = 64;
  const g = t.getContext('2d', { willReadFrequently: true });
  if (!g) return null;
  try { g.drawImage(src, 0, 0, 64, 64); } catch (e) { return null; }
  const d = g.getImageData(0, 0, 64, 64).data;
  let sum = 0;
  for (let i = 0; i < d.length; i += 4) {
    sum += Math.max(d[i], d[i + 1], d[i + 2]) - Math.min(d[i], d[i + 1], d[i + 2]);
  }
  return +(sum / (d.length / 4)).toFixed(2);
};

/**
 * 把"重连"从流生命周期累计计数里算出来。
 *
 * `started` = 发起新拉流的次数，`completed` = 已发起的拉流被结束的次数。
 * 一次重连的语义是「流确实结束过，随后又开始了新的」：
 *
 *   - 初次连接：started=1, completed=0 → `started-1=0` → 0 次（不误计）；
 *   - 断流后重连：started=2, completed=1 → `min(1, 1)=1` 次；
 *   - 连续断两次：started=3, completed=2 → `min(2, 2)=2` 次；
 *   - started 虚高但流没结束过（completed 小）→ 被 completed 夹住，不虚增。
 *
 * 不可用（缺字段）时返回 null = 无法观测，交由判定层判"不能宣称不断流"。
 */
function computeStreamRestarts(m) {
  if (!m || typeof m.started !== 'number' || typeof m.completed !== 'number') return null;
  if (!Number.isFinite(m.started) || !Number.isFinite(m.completed)) return null;
  return Math.max(0, Math.min(Math.max(0, m.started - 1), Math.max(0, m.completed)));
}

/**
 * 创建 session 适配器。
 *
 * @param page {evaluate(fn, arg?): Promise<any>, on?(ev, cb), off?(ev, cb)}
 * @param opts {{mode, base, log?, now?}}
 */
function makeSession(page, opts) {
  const mode = opts.mode;
  const log = opts.log || (() => {});
  const now = opts.now || Date.now;

  const readState = () => page.evaluate(PAGE_STATE);

  const readMse = async () => {
    if (mode !== 'mse') return null;
    return page.evaluate(PAGE_MSE_METRICS);
  };

  return {
    mode,

    async open() {
      const t0 = now();
      await opts.openDesktop(page, opts.base);
      const ok = await opts.waitFirstFrame(page, mode).catch(() => false);
      if (!ok) return { firstFrameMs: null, error: '首帧未出（40s 内）' };
      // 等实例被 hook 捕获、transport 采样稳定
      await opts.sleep(600);
      return { firstFrameMs: now() - t0 };
    },

    async play() {
      if (mode !== 'mse') return;
      await opts.playVideo(page);
    },

    /**
     * 累计真实帧数。
     * - webcodecs：页面 VideoDecoder 输出计数（本来就是真实解码帧）；
     * - mse：**只**用 requestVideoFrameCallback / totalVideoFrames；
     *   都不可用 → null（不可验证），绝不返回时间推算值。
     */
    async frames() {
      if (mode !== 'mse') {
        const st = await readState();
        return typeof st.frames === 'number' ? st.frames : 0;
      }
      const m = await readMse();
      if (!m) return null;
      // rVFC 与浏览器质量计数取"两者中更大的那一个"：rVFC 在后台会被节流，
      // 质量计数不会——只看前者会把"rVFC 被节流"误判成卡流；只看后者在
      // 老浏览器上又可能不可用。两个都不可用才返回 null（不可验证）。
      const vals = [];
      if (typeof m.presented === 'number') vals.push(m.presented);
      if (typeof m.quality === 'number') vals.push(m.quality);
      if (!vals.length) return null;
      return Math.max.apply(null, vals);
    },

    async inits() {
      const st = await readState();
      // 用累计计数（包含被截断的旧记录），不能用 configs.length——
      // configs 只保留尾部若干条，长度会在长跑中饱和，令 gate 永远触发。
      return typeof st.configCount === 'number' ? st.configCount : 0;
    },

    async decoder() {
      const st = await readState();
      const list = st.configs || [];
      const last = list.length ? list[list.length - 1] : null;
      if (!last) {
        return { codec: null, config: '', at: 0, accepted: true, kind: mode === 'mse' ? 'mse' : 'webcodecs' };
      }
      return {
        kind: last.kind,
        codec: last.codec,
        config: last.config,
        at: last.at,
        accepted: last.accepted !== false,
      };
    },

    async saturation() {
      return page.evaluate(PAGE_SATURATION);
    },

    async send(cmd) {
      await opts.sendCommand(page, cmd);
      log(`  → ${cmd.type}${cmd.enabled === undefined ? '' : ' enabled=' + cmd.enabled}${cmd.codec ? ' codec=' + cmd.codec : ''}`);
    },

    async snapshot() {
      const st = await readState();
      const list = st.configs || [];
      const dec = list.length ? list[list.length - 1] : null;
      const support = await opts.probeSupport(page);
      const transport = st.instances > 0
        ? st.transport
        : (st.transportSamples.length ? st.transportSamples[st.transportSamples.length - 1] : 'unknown');
      return {
        transport,
        lanAddrs: st.lanAddrs,
        lanResponses: st.lan,
        lanBytes: st.lanBytes,
        wsBytes: st.wsBytes,
        relayBytes: st.wsBytes,
        rtcAvailable: support.rtc,
        webcodecs: support.webcodecs,
        viewMode: st.viewMode,
        h264Supported: support.h264,
        decoderKind: dec ? dec.kind : 'none',
        decoderCodec: dec ? dec.codec : 'none',
        decoderConfig: dec ? dec.config : '',
        transportSamples: st.transportSamples,
        instances: st.instances,
        framesSource: st.framesSource,
      };
    },

    async health() {
      const st = await readState();
      let streamRestarts = null;
      if (mode === 'mse') {
        // 流生命周期优先取调用方注入的**非侵入式**观测（Playwright 请求事件）；
        // 拿不到时才回退到页面计数器。两者都不可用 → null（不可观测）。
        let life = null;
        if (typeof opts.streamLifecycle === 'function') {
          try { life = opts.streamLifecycle(); } catch (e) { life = null; }
        }
        if (!life || typeof life.started !== 'number') {
          const m = await readMse();
          if (m && typeof m.started === 'number') {
            life = { started: m.started, completed: m.completed };
          }
        }
        streamRestarts = computeStreamRestarts(life);
      }
      return {
        decodeErrors: st.decodeErrors,
        playerErrors: st.playerErrors,
        streamRestarts,
        mseSource: st.mse ? st.mse.source : null,
      };
    },
  };
}

module.exports = {
  PAGE_STATE,
  PAGE_MSE_METRICS,
  PAGE_SATURATION,
  computeStreamRestarts,
  makeSession,
};
