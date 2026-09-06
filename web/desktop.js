// desktop.js — WebCodecs H.264 player for the shell-remote desktop view.
//
// v0.21 pipeline: fetch fMP4 byte stream → in-browser demux (moof/trun/srtc)
// → WebCodecs VideoDecoder (AVCC samples) → requestAnimationFrame canvas 渲染。
// 相比 MSE：
//   - 无 SourceBuffer/播放时钟：解码一帧渲染一帧，帧到达即上屏（省 50-150ms
//     的 MSE 缓冲与追帧逻辑）；
//   - moof 内自定义 srtc box 携带采集 epoch ms → 可计算真实端到端延时
//     （采集→渲染），而非"缓冲尾部-播放头"代理指标。
// 不支持 WebCodecs 的浏览器自动回退到 MSE（web/desktop-mse.js）。
//
// fMP4 demux 契约（agent 端 mp4.rs 产出）：
//   - init: ftyp + moov(avc1/avcC)
//   - frag:  moof{ mfhd seq, traf{ tfhd, tfdt(v1 pts), trun(size,flags), srtc(capture_ms) } } + mdat(AVCC)

(function() {
  'use strict';

  // 解码队列上限：积压超过 N 帧时丢弃旧的非关键帧（对齐 RustDesk
  // frame_controller 的丢帧策略——旧帧无意义，追新才保流畅）。
  // 注意阈值不能太小: AV1 软解 1080p 的瞬时入队深度实测到 ~24,
  // 96 帧≈3s 的积压会让端到端延迟飙到秒级。降到 24（AV1 软解瞬时在飞
  // 帧 ~24）并配合"丢非关键帧即请求关键帧(reqkey)"：参考链撕口子由 agent
  // 立即 force_idr 补上（对齐 rustdesk 控制端 refresh_video 语义），
  // 不再靠大缓冲硬扛。
  const MAX_DECODE_QUEUE = 24;

  window.DesktopView = class {
    constructor() {
      this.video = document.getElementById('desktop-video');
      this.canvas = document.getElementById('desktop-canvas');
      this.statusEl = document.getElementById('desktop-status');
      this.controller = null;
      this.reader = null;
      this.connected = false;
      this._streamRetries = 0;
      this._bpsBytes = 0;
      this._bpsTs = 0;
      this._inputBound = false;
      this._dec = null;
      this._desc = null;          // avcC description (Uint8Array)
      this._codecKind = null;     // 'h264' | 'vp9' | 'av1'（由 init 段确定）
      this._vpcProfile = 0;
      this._vpcLevel = 10;
      this._frames = [];          // decoded VideoFrames pending render
      this._lastCaptureMs = 0;    // 最新解码帧的采集时间（e2e 延时）
      this._lastE2eMs = undefined; // 最近一次解码时测得的即时管线延时（不含帧陈旧度）
      this._lastNewFrameAt = 0;   // 最近一次解码帧到达的本地时刻（静止判定）
      this._lastDataAt = 0;     // 最近一次视频数据(init/moof/mdat)到达时刻（R4 30s 判死看门狗）
      this._e2eMs = undefined;
      this._gotFirstFrame = false; // 是否已渲染过首帧（接入 reqkey 快路径）
      // MSE 模式首帧监听句柄（WebCodecs 走 _onDecoded；MSE 用 video
      // loadeddata 事件作为"已有画面"信号，用于隐藏 loading 覆盖）。
      this._mseFirstFrame = null;
      this._decodeCount = 0;      // 本 1s 窗口已解码帧数（解码背压上传）
      this._reqKeyAt = 0;         // 上次 desktop:reqkey 发送时刻（限频）
      this._reqKeyCount = 0;      // 最近 10s 内 reqkey 次数
      this._lastSeq = 0;        // 最近收到的帧号（seqn box）
      this._seqDrop = 0;        // 真实上行丢帧数（seq gap 累计）
      this._arrivals = [];      // 帧到达间隔窗口（jitter 计算）
      this._lastArrival = 0;
      this._renderPending = false;
      this._droppedFrames = 0;
      // 浏览器与 relay 的时钟偏移（relay_epoch - 本地_epoch）。srtc 在 relay
      // 转发时被改写为 relay 墙钟，e2e 用 本地now+偏移 与 srtc 对齐，彻底摆脱
      // 双机系统时间差（MYS-886 指标失真根因）。
      this._clockOffset = 0;
      // TestDelay 探针（R4 甲 A1 / R5#148，对齐 rustdesk cm::TestDelay）：
      // 每 1s 发一个探测包，agent 收到即时原样 echo——浏览器用本地单调时钟
      // （performance.now）算往返 RTT。这是**纯网络层**往返：不含编码/解码/
      // 渲染管线、不依赖时钟校准（e2e 依赖 _clockOffset，探针不依赖）。随
      // qos 上报给 agent 作为"网络层拥塞证据"（e2e 高但网络健康 = 管线积压，
      // 不误降码率）。
      this._probeSeq = 0;
      this._probeRttMs = 0;
      this._testDelayTimer = null;
      // 远端光标 overlay（R5#64）：X11 GetImage 不含光标层，agent 独立发
      // 位置（100ms 节流），本 div 绝对定位叠加在视频上。懒创建。
      this._cursorEl = null;
      // agent 经 desktop:qos-ack 回传的当前 QoS 状态（目标帧率/码率档）。
      // 面板"目标帧率/活动"行展示：与渲染帧率对照，可分辨"是我在降帧
      // 还是解码跟不上"（对齐 rustdesk TestDelay/target fps，MYS-886）。
      this._ackFps = null;
      this._ackScale = null;
      // QoS 五态质量状态（agent qos-ack 回传；面板"QoS 状态"行 + 颜色）。
      this._qosState = null;
      // 首帧时间 TTFV 打点（R2 乙60：接入 → 首帧渲染毫秒），面板"链路状态"
      // 前放置；弱网模式判定也在此（e2e 连续命中阈值 → 弱网标记）。
      this._ttfvStart = 0;
      this._ttfvMs = null;
      this._weakNetStrikes = 0;   // 连续 e2e>500ms 的观测次数（>2 tick 判弱网）
      this._weakNet = false;
      // 帧超龄丢弃计数（R4 乙88：到达时已 >2s 旧 → 丢 + reqkey）
      this._staleDropped = 0;
      // 时钟慢校准定时器（R3 丙135：连接后每 15min 重校一次，对抗长会话漂移）
      this._clockRecheckTimer = null;
      // JS 内存峰值（R2 丁131 / R5#61）：面板内存行显示当前 + 会话峰值。
      this._jsMemPeak = 0;
      // QoS 250ms 上报（R5#72）：独立定时器（与 1s 面板刷新解耦），
      // 解码帧率 dfps 按 250ms 窗口 ×4 折算回"每秒"语义，agent 侧背压
      // 阈值（decode_fps<20 且队列>12）不受窗口改变影响。
      this._qosTimer = null;
      this._qosDfps = 0; // 最近一次折算后的每秒解码帧率（面板排队时延用）
      // demux 损坏重同步计数（R3 丁150）：连续 3 次非法 box → 重发 init。
      this._demuxCorrupt = 0;
      // 解码器黑名单（R2 己155/R3 丁155）：连续解码错误触发（这里是解码器
      // error 回调内的重建防抖已在 _scheduleDecodeRecover；此处补充"错误计数
      // 达阈值请求切 codec"——避免持续软解失败空转重连）。阈值：30s 内 3 次。
      this._decErrCount = 0;
      this._decErrWindowStart = 0;
    }

    // 向 relay /api/clock 做 NTP 式往返采样，求得 (relay_epoch - 本地_epoch)。
    // 采样 7 次、剔除单边 RTT>500ms 的脏样本后取中值，消除单向延迟与
    // 抖动造成的偏差（对齐 rustdesk 校准精度，MYS-886）。
    _calibrateClock() {
      const self = this;
      const samples = [];
      let pending = 7;
      return new Promise(function(resolve) {
        const done = function() {
          if (samples.length === 0) { resolve(); return; }
          samples.sort(function(a, b) { return a.offset - b.offset; });
          self._clockOffset = samples[Math.floor(samples.length / 2)].offset;
          resolve();
        };
        for (let i = 0; i < 7; i++) {
          const t0 = Date.now();
          fetch('/api/clock', { cache: 'no-store' }).then(function(r) { return r.json(); })
            .then(function(j) {
              const t1 = Date.now();
              const rtt = t1 - t0;
              // 脏样本（单边往返 >500ms，抖动过大）不计入：避免校准被一次
              // 高 RTT 污染，直接抬高 e2e 读数误导 QoS（13s 类事件时钟侧来源）。
              if (rtt <= 1000) {
                // 单程 ≈ rtt/2：relay 时钟等于 t0 时刻的 (j.epoch_ms - rtt/2)
                const relayAtT0 = j.epoch_ms - rtt / 2;
                samples.push({ offset: relayAtT0 - t0 });
              }
            })
            .catch(function() {})
            .then(function() {
              pending -= 1;
              if (pending === 0) done();
            });
        }
      });
    }

    // relay 预建桌面流的时机略晚于 desktop:started 广播; 404 时指数退避重试
    // （上限 10 次），返回 true 表示已接管重试。
    _retryDesktopStream() {
      if (this._streamRetries >= 10) return false;
      this._streamRetries += 1;
      const self = this;
      this.setStatus('等待桌面流就绪… (' + this._streamRetries + ')', false);
      const delay = Math.min(700 * Math.pow(1.5, this._streamRetries - 1), 5000);
      setTimeout(function() { self.connect(); }, delay);
      return true;
    }

    setStatus(text, isError) {
      if (!this.statusEl) return;
      this.statusEl.textContent = text;
      this.statusEl.style.color = isError ? '#ff6b6b' : '#b8c0cc';
    }

    _webcodecsAvailable() {
      // WebCodecs (VideoDecoder) 只在安全上下文暴露：https 或 localhost。
      // http://IP:port 访问 relay 时 Chrome 最新版也没有 VideoDecoder,
      // 回退 MSE 是预期行为（不是浏览器旧）。
      return typeof window.VideoDecoder !== 'undefined' && this.canvas !== null;
    }

    // 回退原因说明（指标面板"解码方案"行展示）。
    _decoderLabel() {
      if (this._mode === 'webcodecs') return 'WebCodecs (原生)';
      const secure = typeof window.isSecureContext !== 'undefined' ? window.isSecureContext : false;
      return secure ? 'MSE (浏览器无 VideoDecoder)' : 'MSE (http 访问未启用 WebCodecs，用 https 可解锁原生解码)';
    }

    // 当前编码方案：与解码同源，由 init 段的 codec box（av1C/vpcC/avcC）
    // 判定。指标面板展示用（MYS-886：新增指标）。
    _encoderLabel() {
      if (!this._codecKind) return '-';
      return { h264: 'H.264', vp9: 'VP9', av1: 'AV1' }[this._codecKind] || this._codecKind;
    }

    // 解码器错误计数（R2 己155/R3 丁155 黑名单）：30s 窗口内 >=3 次解码器
    // error → 判定当前 codec 在浏览器端不可解（黑名单），请求 agent 切换
    // 下一个可用编码（av1→vp9→h264）。与 _scheduleDecodeRecover（重建流）
    // 互补：重建治"参考链"，切 codec 治"解码器不支持该码流"。
    _onDecodeError(e) {
      const now = Date.now();
      if (now - this._decErrWindowStart > 30000) {
        this._decErrWindowStart = now;
        this._decErrCount = 0;
      }
      this._decErrCount += 1;
      if (this._decErrCount < 3) return;
      this._decErrCount = 0; // 切换到下一档前重置（避免同窗口反复触发）
      const codecs = ['av1', 'vp9', 'h264'];
      if (!this._codecKind || codecs.indexOf(this._codecKind) < 0) return;
      const nextIdx = codecs.indexOf(this._codecKind) + 1;
      if (nextIdx >= codecs.length) return; // 已到 h264，不再可切
      const next = codecs[nextIdx];
      this._codecBlacklisted = this._codecBlacklisted || [];
      if (this._codecBlacklisted.indexOf(next) >= 0) return; // 已黑名单过，防循环
      this._codecBlacklisted.push(next);
      this.setStatus('解码器持续报错，切换编码到 ' + next.toUpperCase(), false);
      const sel = document.getElementById('desktop-codec-select');
      if (sel && Array.from(sel.options).some(function(o) { return o.value === next; })) {
        sel.value = next;
        sel.dispatchEvent(new Event('change'));
      } else if (window.shellRemote && window.shellRemote.send) {
        window.shellRemote.send('desktop:codec', { codec: next });
      }
    }

    // 解码错误自动恢复（MYS-886）：WebCodecs 解码器持续报错时，延迟重建
    // 整个桌面流（disconnect → 清 init → connect 重新拉流）。带防抖避免
    // 连续报错触发重连风暴；正常关键帧自愈（_decErr）优先，这里兜底。
    _scheduleDecodeRecover() {
      const self = this;
      if (this._decRecoverTimer) return;
      this._decRecoverTimer = setTimeout(function() {
        self._decRecoverTimer = null;
        if (!self.connected) return;
        self.setStatus('解码异常，重建桌面流…', false);
        self.disconnect(false);
        self._codecKind = null; // 强制重新解析新流的 init 段
        self._decErr = false;
        setTimeout(function() { self.connect(); }, 800);
      }, 1500);
    }

    // 请求关键帧（对齐 rustdesk 控制端 refresh_video）：接入/参考链断裂/
    // 解码错误/解码积压丢帧时，让 agent 立即 force_idr 重同步，不再等周期
    // IDR（活跃期 6s）。限频：3s 最小间隔 + 10s 内最多 3 次，防刷新风暴。
    _requestKey() {
      if (!this.connected || !window.shellRemote || !window.shellRemote.send) return;
      const now = Date.now();
      if (now - this._reqKeyAt < 3000) return;
      if (now - this._reqKeyWindowStart > 10000) { this._reqKeyWindowStart = now; this._reqKeyCount = 0; }
      if (this._reqKeyCount >= 3) return;
      this._reqKeyAt = now;
      this._reqKeyCount += 1;
      this._reqKeyHistory = (this._reqKeyHistory || 0) + 1;
      window.shellRemote.send('desktop:reqkey', {});
    }

    // 白闪修复（R5#74-78）：连接/切流/重连时显示"正在连接桌面…"覆盖层，
    // 首帧解码后隐藏——避免 canvas 空白/白屏闪烁给用户卡死错觉。
    _showLoading() {
      const el = document.getElementById('desktop-loading');
      if (el) el.classList.remove('hidden');
    }
    _hideLoading() {
      const el = document.getElementById('desktop-loading');
      if (el) el.classList.add('hidden');
    }

    connect() {
      if (this._decRecoverTimer) { clearTimeout(this._decRecoverTimer); this._decRecoverTimer = null; }
      this.disconnect(false);
      this._showLoading();
      const self = this;
      // MSE 模式首帧信号：video loadeddata（WebCodecs 已走 _onDecoded）。
      if (this._mse && this.video) {
        this._mseFirstFrame = function() {
          self._hideLoading();
          self._gotFirstFrame = true;
          self._lastNewFrameAt = Date.now();
          if (self.video) self.video.removeEventListener('loadeddata', self._mseFirstFrame);
        };
        this.video.addEventListener('loadeddata', this._mseFirstFrame);
      }
      this._ttfvStart = Date.now(); // TTFV 打点起点（对齐 R2 乙60）
      this._ttfvMs = null;
      // 能力探测缓存（R3 己195 / R5#65）：第一次探测后把解码路径
      // （webcodecs / mse / none）写进 sessionStorage，重连/切页直接复用，
      // 不再重复实例化 VideoDecoder 探测（长会话里避免每次重连一帧的浪费）。
      if (typeof sessionStorage !== 'undefined') {
        const key = 'sr-capability-v1';
        const cached = sessionStorage.getItem(key);
        if (!cached || Date.now() - Number(cached.split('|')[0] || 0) > 3600 * 1000) {
          const cap = this._webcodecsAvailable() ? 'webcodecs' : (window.DesktopViewMse ? 'mse' : 'none');
          sessionStorage.setItem(key, Date.now() + '|' + cap);
        }
      }
      if (this._webcodecsAvailable()) {
        this._mode = 'webcodecs';
        if (this.canvas) this.canvas.classList.remove('hidden');
        if (this.video) this.video.classList.add('hidden');
        // 先校准时钟到 relay 时基，再拉流（不阻塞重连：校准失败也继续）。
        // 下行优先 WS，失败自动回退 HTTP fetch。
        const self = this;
        this._calibrateClock().then(function() { self._startWs(); });
        return;
      }
      // 回退：MSE 播放器（旧浏览器）。
      this._mode = 'mse';
      if (this.canvas) this.canvas.classList.add('hidden');
      if (this.video) this.video.classList.remove('hidden');
      if (window.DesktopViewMse) {
        this._mse = new window.DesktopViewMse();
        this._mse.connect();
      } else {
        this.setStatus('当前浏览器不支持 WebCodecs/MSE', true);
      }
    }

    // ── 拉流：WS 优先（relay WS 下行），失败回退 HTTP fetch ──────
    _startWs() {
      const token = sessionStorage.getItem('shell-remote-token');
      if (!token || typeof WebSocket === 'undefined') {
        this._startFetch();
        return;
      }
      const self = this;
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = proto + '//' + location.host + '/agent/desktop/ws?token=' + encodeURIComponent(token);
      let ws;
      try { ws = new WebSocket(url); } catch (e) {
        this._startFetch();
        return;
      }
      const sessionTimeout = setTimeout(function() {
        try { ws.close(); } catch (e) {}
        self._onWsFailed();
      }, 4000);
      this._ws = ws;
      ws.binaryType = 'arraybuffer';
      ws.onopen = function() {
        clearTimeout(sessionTimeout);
        self._streamRetries = 0;
        self.connected = true;
        self._bindInput();
        self._startMetrics();
        self.setStatus('桌面已连接 (WS)', false);
        self._buf = new Uint8Array(0);
        // 接入快路径：1.5s 内没等到首帧（首 IDR 被 6s 活跃周期延迟）→
        // reqkey 立即出关键帧，避免"接入黑屏等 IDR"。
        self._firstFrameTimer = setTimeout(function() {
          if (self.connected && !self._gotFirstFrame) self._requestKey();
        }, 1500);
      };
      ws.onmessage = function(ev) {
        if (typeof ev.data === 'string') return; // 控制帧（如 ping 文本）
        const v = new Uint8Array(ev.data);
        if (v.byteLength) {
          self._trackBandwidth(v.byteLength);
          self._feed(v);
        }
      };
      ws.onerror = function() { self._onWsFailed(); };
      ws.onclose = function() {
        if (self._ws === ws) self._ws = null;
        if (self.connected && self._streamRetries < 10) {
          self._streamRetries += 1;
          self.setStatus('桌面流重启… (' + self._streamRetries + ')', false);
          self.disconnect(false);
          const delay = Math.min(700 * Math.pow(1.5, self._streamRetries - 1), 5000);
          setTimeout(function() { self.connect(); }, delay);
        } else if (self.connected) {
          self.setStatus('桌面流已结束', true);
          self.connected = false;
        }
      };
    }

    _onWsFailed() {
      if (this._ws) { try { this._ws.onclose = null; this._ws.close(); } catch (e) {} this._ws = null; }
      // WS 不可用 → HTTP fetch 兜底
      this._startFetch();
    }

    // ── 拉流（两种模式共用入口）──────────────────────────────
    _startFetch() {
      const token = sessionStorage.getItem('shell-remote-token');
      if (!token) {
        this.setStatus('缺少会话密钥', true);
        return;
      }
      const controller = new AbortController();
      this.controller = controller;
      const self = this;

      fetch('/agent/desktop/stream', {
        headers: { 'Authorization': 'Bearer ' + token }
      }).then(function(resp) {
        if (controller.signal.aborted) return null;
        if (!resp.ok || !resp.body) {
          if (resp.status === 404 && self._retryDesktopStream()) return null;
          self.setStatus('桌面流不可用 (HTTP ' + resp.status + ')，请重试', true);
          self.disconnect();
          return null;
        }
        self._streamRetries = 0;
        self.connected = true;
        self._bindInput();
        self._startMetrics();
        self.setStatus('桌面已连接', false);
        const reader = resp.body.getReader();
        self.reader = reader;
        self._buf = new Uint8Array(0); // demux 缓冲

        function pump() {
          if (controller.signal.aborted) return Promise.resolve();
          return reader.read().then(function(result) {
            if (result.done) {
              if (self.connected && self._streamRetries < 10) {
                self._streamRetries += 1;
                self.setStatus('桌面流重启… (' + self._streamRetries + ')', false);
                self.disconnect(false);
                const delay = Math.min(700 * Math.pow(1.5, self._streamRetries - 1), 5000);
                setTimeout(function() { self.connect(); }, delay);
              } else {
                self.setStatus('桌面流已结束', true);
              }
              return;
            }
            const v = result.value;
            if (v && v.byteLength) {
              self._trackBandwidth(v.byteLength);
              self._feed(v);
            }
            return pump();
          }).catch(function(e) {
            if (!controller.signal.aborted) {
              self.setStatus('桌面流中断: ' + e.message, true);
            }
          });
        }
        return pump();
      }).catch(function(e) {
        if (!controller.signal.aborted) {
          self.setStatus('无法连接桌面: ' + e.message, true);
        }
      });
    }

    // ── fMP4 demux ─────────────────────────────────────────
    // 字节流 → (init 的 avcC) + 逐 moof 的 {ptsUs, isKey, captureMs, avcc}
    _feed(chunk) {
      // R4 乙30s 判死：任何视频数据到达（init/moof/mdat 都在此进入）都刷新
      // 到达时刻——网络存活证据。静止安全：agent 静止时每 4s 一个 IDR 心跳
      // 仍会到达 moof，不会误判；30s 完全无任何数据才是连接黑洞。
      if (chunk && chunk.byteLength > 0) this._lastDataAt = Date.now();
      // 追加到缓冲
      const merged = new Uint8Array(this._buf.length + chunk.byteLength);
      merged.set(this._buf, 0);
      merged.set(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength), this._buf.length);
      this._buf = merged;

      let guard = 0;
      while (guard++ < 512) {
        const box = this._parseNextBox();
        if (!box) break;
        if (box.type === 'moov' || (box.type === 'ftyp' && !this._codecKind)) {
          // init：ftyp 直接跳过，moov 内找 avcC
          if (box.type === 'moov') this._handleMoov(box.body);
        } else if (box.type === 'moof') {
          this._handleMoof(box.body);
        } else if (box.type === 'mdat') {
          // mdat 紧跟其 moof 之后；_handleMoof 记录了 pending sample 期望
          // 长度，这里按需截取 AVCC 数据。
          this._handleMdat(box.body);
        }
        // 其它 box（free 等）跳过
      }
    }

    // 解析缓冲中首个完整 box；不完整时返回 null（等更多字节）。
    _parseNextBox() {
      const b = this._buf;
      if (b.length < 8) return null;
      const size = (b[0] << 24) | (b[1] << 16) | (b[2] << 8) | b[3];
      if (size < 8 || size > 32 * 1024 * 1024) {
        // 流损坏：丢掉缓冲重新同步（丢帧由下一个关键帧恢复）。
        this._buf = new Uint8Array(0);
        // 连续 3 次损坏 → 参考链已不可信，重发 init 让 agent 重出参数集
        //（对齐 R3 丁150：demux 抗损坏的升级动作——静默重同步只治标，
        //  反复损坏说明流本身坏了）。
        this._demuxCorrupt += 1;
        if (this._demuxCorrupt >= 3) {
          this._demuxCorrupt = 0;
          if (!this._codecKind) return null; // 尚无 init，无 decode 上下文
          if (!this._lastInitReq) this._lastInitReq = 0;
          const now = Date.now();
          // 3s 限频，防 reqkey/init 风暴与损坏帧共振。
          if (now - this._lastInitReq > 3000) {
            this._lastInitReq = now;
            window.shellRemote.send('desktop:reqkey', { reinit: true });
            this._reqKeyCount += 1;
          }
        }
        return null;
      }
      if (b.length < size) return null;
      const type = String.fromCharCode(b[4], b[5], b[6], b[7]);
      const body = b.subarray(8, size);
      this._buf = b.slice(size);
      return { type: type, body: body };
    }

    _hasBoxType(buf, type) {
      const t0 = type.charCodeAt(0) & 0xff, t1 = type.charCodeAt(1) & 0xff;
      const t2 = type.charCodeAt(2) & 0xff, t3 = type.charCodeAt(3) & 0xff;
      for (let j = 0; j + 4 <= buf.length; j++) {
        if (buf[j] === t0 && buf[j+1] === t1 && buf[j+2] === t2 && buf[j+3] === t3) return true;
      }
      return false;
    }

    _handleMoov(body) {
      // 找 codec 配置 box：先扫 avcC（H.264），再扫 vpcC（VP9），再扫 av1C（AV1）。
      //   avcC payload: [i+4]=1(version) [i+5..i+7]=profile/compat/level
      //   vpcC payload: [i+4]=1(version) [i+5]=profile [i+6]=level [i+7]=bitDepth4|chroma3|range1
      //   av1C payload: [i+4]=0x81(marker+version) [i+5]=profile(3)|level(5)
      //     [i+6]=tier/high/twelve/mono/chroma_x/chroma_y/position [i+7]=delay
      // 之前误把 i（标签起点）当 payload 起算，codec 串取到 'v','c','C'
      // 变成 avc1.766343 非法——WebCodecs configure 报 Unknown codec name。
      for (let i = 0; i + 8 <= body.length; i++) {
        if (body[i] === 0x61 && body[i+1] === 0x76 && body[i+2] === 0x63 && body[i+3] === 0x43) {
          const spsLen = (body[i+10] << 8) | body[i+11];
          const numPpsOff = i + 12 + spsLen;
          const ppsLen = (body[numPpsOff+1] << 8) | body[numPpsOff+2];
          // description 取 AVCDecoderConfigurationRecord（不含 box 标签）。
          this._desc = body.slice(i + 4, numPpsOff + 3 + ppsLen);
          this._codecKind = 'h264';
          this._initDecoder();
          return;
        }
        if (body[i] === 0x76 && body[i+1] === 0x70 && body[i+2] === 0x63 && body[i+3] === 0x43) {
          // vpcC 是 FullBox：version/flags(4B) 后才是 profile/level。
          // 位置: [i+4]=version [i+5..i+7]=flags [i+8]=profile [i+9]=level
          // VP8 与 VP9 共用 vpcC config record，靠 sample entry box 名区分
          // （vp08 vs vp09）——扫 moov body 里的 vp08 判定。
          this._desc = null; // VP9/VP8 无 description
          this._vpcProfile = body[i+8];
          this._vpcLevel = body[i+9];
          this._codecKind = this._hasBoxType(body, 'vp08') ? 'vp8' : 'vp9';
          this._initDecoder();
          return;
        }
        if (body[i] === 0x61 && body[i+1] === 0x76 && body[i+2] === 0x31 && body[i+3] === 0x43) {
          // av1C 是普通 box（非 FullBox），payload 从标签后 4 字节起。
          // [i+5]=seq_profile(3)|seq_level_idx_0(5)；codec 串 LL 直接用
          // 该 5 位索引的十进制两位（3.0→idx=2→"02"，4.0→idx=4→"04"）。
          this._desc = null; // AV1 无 description
          this._av1Profile = (body[i+5] >> 5) & 0x7; // seq_profile(3)
          this._av1Level = body[i+5] & 0x1f;         // seq_level_idx_0(5)
          this._av1Tier = (body[i+6] >> 7) & 0x1;    // seq_tier_0(1)
          this._codecKind = 'av1';
          this._initDecoder();
          return;
        }
      }
    }

    _initDecoder() {
      const self = this;
      if (this._dec) {
        try { this._dec.close(); } catch (e) {}
        this._dec = null;
      }
      const hex = (b) => b.toString(16).padStart(2, '0').toUpperCase();
      if (this._codecKind === 'av1') {
        // AV1 codec 串: av01.P.LLT.DD；P=profile, LL=seq_level_idx 的十进制
        // 两位(3.0→02、4.0→04)，不是 level 号本身——Chrome 按 5 位 idx
        // (0-31) 校验, 写 "40" 会被拒。T=tier(M/H), DD=bit depth(08)。
        // 无 description。实测 ffmpeg：1080p30 的 av1C idx=8 → av01.*.08M.08。
        const tier = this._av1Tier ? 'H' : 'M';
        const codec = 'av01.' + this._av1Profile + '.' +
          String(this._av1Level).padStart(2, '0') + tier + '.08';
        this._dec = new VideoDecoder({
          output: function(frame) { self._onDecoded(frame); },
          error: function(e) {
            self._decErr = true; // 下个关键帧自愈重建（见 _handleMdat）
            self.setStatus('解码错误: ' + e.message, true);
            self._onDecodeError(e);
            self._scheduleDecodeRecover();
          }
        });
        this._dec.configure({
          codec: codec,
          optimizeForLatency: true
        });
        this._codecStr = codec;
        return;
      }
      if (this._codecKind === 'vp8') {
        // VP8: WebCodecs 注册名为裸 "vp8"（vp08.PP.LL 是 ISOBMFF sample entry
        // 名，VideoDecoder 不认 → Unknown codec name）。无 profile/level 组件。
        const codec = 'vp8';
        this._dec = new VideoDecoder({
          output: function(frame) { self._onDecoded(frame); },
          error: function(e) {
            self._decErr = true; // 下个关键帧自愈重建（见 _handleMdat）
            self.setStatus('解码错误: ' + e.message, true);
            self._onDecodeError(e);
            self._scheduleDecodeRecover();
          }
        });
        this._dec.configure({
          codec: codec,
          optimizeForLatency: true
        });
        this._codecStr = codec;
        return;
      }
      if (this._codecKind === 'vp9') {
        // VP9: codec 串 vp09.profile.level.bitdepth，无 description。
        const codec = 'vp09.' + String(this._vpcProfile).padStart(2, '0') +
          '.' + String(this._vpcLevel).padStart(2, '0') + '.08';
        this._dec = new VideoDecoder({
          output: function(frame) { self._onDecoded(frame); },
          error: function(e) {
            self._decErr = true; // 下个关键帧自愈重建（见 _handleMdat）
            self.setStatus('解码错误: ' + e.message, true);
            self._onDecodeError(e);
            self._scheduleDecodeRecover();
          }
        });
        this._dec.configure({
          codec: codec,
          optimizeForLatency: true
        });
        this._codecStr = codec;
        return;
      }
      // H.264: codec 串取 avcC 的 profile/compat/level（与码流一致才被接受）。
      const codec = 'avc1.' + hex(this._desc[1]) + hex(this._desc[2]) + hex(this._desc[3]);
      this._dec = new VideoDecoder({
        output: function(frame) { self._onDecoded(frame); },
        error: function(e) {
          self._decErr = true; // 下个关键帧自愈重建（见 _handleMdat）
          self.setStatus('解码错误: ' + e.message, true);
          self._onDecodeError(e);
          self._scheduleDecodeRecover();
        }
      });
      this._dec.configure({
        codec: codec,
        description: this._desc,
        optimizeForLatency: true
      });
      this._codecStr = codec;
    }

    // moof: 提取 tfdt(pts)、trun(size+flags)、srtc(captureMs)、seqn(帧号)
    _handleMoof(body) {
      if (!this._dec) return; // 尚无 init
      // 遍历 traf 子 box
      let pos = 0;
      let ptsUs = 0, sampleSize = 0, isKey = false, captureMs = 0, frameSeq = 0;
      let sawTrun = false;
      while (pos + 8 <= body.length) {
        const size = (body[pos] << 24) | (body[pos+1] << 16) | (body[pos+2] << 8) | body[pos+3];
        if (size < 8) break;
        const type = String.fromCharCode(body[pos+4], body[pos+5], body[pos+6], body[pos+7]);
        const inner = body.subarray(pos + 8, pos + size);
        if (type === 'traf') {
          let p = 0;
          while (p + 8 <= inner.length) {
            const s2 = (inner[p] << 24) | (inner[p+1] << 16) | (inner[p+2] << 8) | inner[p+3];
            if (s2 < 8) break;
            const t2 = String.fromCharCode(inner[p+4], inner[p+5], inner[p+6], inner[p+7]);
            const d2 = inner.subarray(p + 8, p + s2);
            if (t2 === 'tfdt' && d2.length >= 12) {
              ptsUs = Number((BigInt(d2[4]) << 56n) | (BigInt(d2[5]) << 48n) |
                (BigInt(d2[6]) << 40n) | (BigInt(d2[7]) << 32n) |
                (BigInt(d2[8]) << 24n) | (BigInt(d2[9]) << 16n) |
                (BigInt(d2[10]) << 8n) | BigInt(d2[11]));
            } else if (t2 === 'trun') {
              // flags: data-offset(0x1)|first-sample-flags(0x4)|dur(0x100)|size(0x200)
              // 布局: ver/flags(4) count(4) dataOffset(4) firstFlags(4) dur(4) size(4)
              sawTrun = true;
              // first_sample_flags 高字节低 2 位 = sample_depends_on; 2=key, 1=delta。
              // 不能只看整字节非零(0x01=delta 的整字节也是非零 → 误判全 key,
              // 每帧重建解码器+perf 崩)。见 verify_vp9_browser.js 的同样修正。
              const sampleFlagsByte = d2[12];
              isKey = ((sampleFlagsByte & 0x03) === 0x02);
              const szPos = d2.length - 4;
              sampleSize = (d2[szPos] << 24) | (d2[szPos+1] << 16) | (d2[szPos+2] << 8) | d2[szPos+3];
            } else if (t2 === 'srtc' && d2.length >= 8) {
              captureMs = Number((BigInt(d2[0]) << 56n) | (BigInt(d2[1]) << 48n) |
                (BigInt(d2[2]) << 40n) | (BigInt(d2[3]) << 32n) |
                (BigInt(d2[4]) << 24n) | (BigInt(d2[5]) << 16n) |
                (BigInt(d2[6]) << 8n) | BigInt(d2[7]));
            } else if (t2 === 'seqn' && d2.length >= 8) {
              // 帧号：TCP 有序，只可能从上游丢帧 → seq gap = 真实上行丢帧数
              //（对齐 rustdesk 控制端丢帧口径，替代"解码丢弃"估算）。
              frameSeq = Number((BigInt(d2[0]) << 56n) | (BigInt(d2[1]) << 48n) |
                (BigInt(d2[2]) << 40n) | (BigInt(d2[3]) << 32n) |
                (BigInt(d2[4]) << 24n) | (BigInt(d2[5]) << 16n) |
                (BigInt(d2[6]) << 8n) | BigInt(d2[7]));
            }
            p += s2;
          }
        }
        pos += size;
      }
      if (!sawTrun || sampleSize === 0) return;
      // 真实上行丢帧统计：TCP 有序，seq gap 只能来自上游丢弃（上传队列丢旧/
      // relay 丢旧）。对齐 rustdesk 控制端丢帧口径。
      if (frameSeq > 0) {
        if (this._lastSeq > 0 && frameSeq > this._lastSeq + 1) {
          this._seqDrop += frameSeq - this._lastSeq - 1;
        }
        this._lastSeq = Math.max(this._lastSeq, frameSeq);
      }
      this._pending = { ptsUs: ptsUs, isKey: isKey, captureMs: captureMs, size: sampleSize };
    }

    _handleMdat(body) {
      const p = this._pending;
      this._pending = null;
      if (!p || !this._dec || this._dec.state === 'closed') return;
      // 帧到达 jitter：相邻帧到达时刻差（R2 乙58/R4 丁157 面板项）
      const now = Date.now();
      if (this._lastArrival) {
        this._arrivals.push(now - this._lastArrival);
        if (this._arrivals.length > 32) this._arrivals.shift();
      }
      this._lastArrival = now;
      const sample = body.subarray(body.length - p.size); // mdat 尾部即本帧
      const chunk = new EncodedVideoChunk({
        type: p.isKey ? 'key' : 'delta',
        timestamp: p.ptsUs,
        data: sample
      });
      // 解码器报错（参考链断裂/丢帧等）：在下一个关键帧重建解码器自愈。
      // 不重建的话 WebCodecs 报错后 decode() 全部失效，画面永久卡死。
      if (p.isKey && this._decErr) {
        this._decErr = false;
        this._initDecoder();
      }
      // 积压保护：解码队列过深时丢旧的非关键帧，并请求关键帧修复参考链
      //（丢弃即撕参考链，reqkey 让 agent 立即 force_idr 补上，对齐 rustdesk
      //  控制端"队列满顶出旧帧 → refresh_video"）。
      if (this._dec.decodeQueueSize > MAX_DECODE_QUEUE && !p.isKey) {
        this._droppedFrames += 1;
        this._requestKey();
        return;
      }
      // captureMs 由 timestamp 索引，输出帧时取回（VideoFrame 无自定义元数据）。
      if (!this._captureByPts) this._captureByPts = new Map();
      this._captureByPts.set(p.ptsUs, p.captureMs);
      if (this._captureByPts.size > 64) {
        // 删除最旧的键（Map 保插入序）
        const firstKey = this._captureByPts.keys().next().value;
        this._captureByPts.delete(firstKey);
      }
      try {
        this._dec.decode(chunk);
      } catch (e) {
        // config 变化（分辨率重配）等：丢弃该帧, 等下一个 IDR 重建解码器。
        // 丢 decode 异常即请求刷新，不等 6s 周期 IDR。
        this._requestKey();
        if (p.isKey && this._codecKind) this._initDecoder();
      }
    }

    _onDecoded(frame) {
      // 从 timestamp 索引取回采集时间，供 e2e 延时计算。
      const capMs = this._captureByPts ? this._captureByPts.get(frame.timestamp) : null;
      if (this._captureByPts) this._captureByPts.delete(frame.timestamp);
      this._lastNewFrameAt = Date.now();
      this._gotFirstFrame = true;
      this._hideLoading();
      this._decodeCount += 1;
      // TTFV：从 connect() 到首帧解码完成的毫秒（对齐 R2 乙60，面板打点）。
      if (this._ttfvMs === null && this._ttfvStart) {
        this._ttfvMs = Date.now() - this._ttfvStart;
      }
      if (capMs) {
        this._lastCaptureMs = capMs;
        // e2e 在解码**到达时刻**测定（即时管线延时 = 本地now(relay时基) − 采集
        // epoch），不沿用"指标tick再算 now−旧采集"——那是帧陈旧度（≈1/fps，
        // fps=1 时高达数百 ms）而非真实延迟。fps 越低保真度越高，会反过来把
        // QoS 压进低帧率自锁（MYS-886 卡顿死锁的源头之一）。
        this._lastE2eMs = Math.max(0, this._lastNewFrameAt + this._clockOffset - capMs);
        // 帧超龄丢弃（对齐 R4 乙88）：解码链路若积压到帧已是 2s 前拍的旧画面，
        // 它没有渲染价值（直播间追新语义）——直接丢掉，并请求关键帧让 agent
        // 立即重出最新画面，而不是把这个陈旧帧画上去制造"慢半拍"观感。
        // 阈值 2s 远高于正常 e2e（<150ms），只命中真正的积压崩溃；静态心跳
        // IDR（≈10ms）与低 fps 正常帧（陈旧度≈1/fps ≤1s）都到不了这里。
        if (this._lastE2eMs > 2000) {
          this._staleDropped += 1;
          try { frame.close(); } catch (e) {}
          this._requestKey();
          return; // 不渲染此帧
        }
      }
      if (this._frames.length > 2) {
        // 渲染管线积压：丢最旧帧（保留最新），追新跳旧减陈旧度。
        this._frames.shift().close();
      }
      this._frames.push(frame);
      this._scheduleRender();
    }

    _scheduleRender() {
      if (this._renderPending) return;
      this._renderPending = true;
      const self = this;
      requestAnimationFrame(function() {
        self._renderPending = false;
        self._render();
      });
    }

    _render() {
      if (!this.connected || !this.canvas) return;
      while (this._frames.length > 1) {
        // 只渲染最新一帧（实时流：旧帧直接弃）
        this._frames.shift().close();
      }
      const frame = this._frames[0];
      if (!frame) return;
      const c = this.canvas;
      if (c.width !== frame.displayWidth || c.height !== frame.displayHeight) {
        c.width = frame.displayWidth;
        c.height = frame.displayHeight;
      }
      const ctx = c.getContext('2d');
      ctx.drawImage(frame, 0, 0);
      // 渲染帧率 = 实际画到 canvas 的新内容帧数（对齐远程桌面帧率），
      // 而非本地显示器刷新率（MYS-886：之前指标是 rAF 计数恒 60）。
      this._rafCount += 1;
      frame.close();
      this._frames = [];
    }

    _trackBandwidth(bytes) {
      const now = Date.now();
      if (!this._bpsTs) {
        this._bpsTs = now;
        this._bpsBytes = 0;
        this._peakKbps = 0;
      }
      this._bpsBytes += bytes;
      const dt = (now - this._bpsTs) / 1000;
      if (dt >= 1.0) {
        const kbps = Math.round(this._bpsBytes * 8 / dt / 1000);
        // 指标面板显示 1s 窗口的"实测均值"（对齐 rustdesk Target Bitrate 的
        // 稳态语义，避免缓存灌入/I 帧瞬时 burst 把读数顶到几千 kbps 造成
        // 误读，MYS-886）。
        this._avgKbps = kbps;
        // 峰值估计仍用于给 agent 评估上行能力（弱网降码率），不上面板。
        if (kbps > this._peakKbps) this._peakKbps = kbps;
        this._bpsTs = now;
        this._bpsBytes = 0;
        if (this._peakKbps > 0 && window.shellRemote && window.shellRemote.send) {
          window.shellRemote.send('desktop:bitrate', { kbps: this._peakKbps });
        }
      }
    }

    disconnect(resetRetries) {
      if (this._mse) {
        this._mse.disconnect(resetRetries);
        this._mse = null;
      }
      if (this.video && this._mseFirstFrame) {
        try { this.video.removeEventListener('loadeddata', this._mseFirstFrame); } catch (e) {}
        this._mseFirstFrame = null;
      }
      if (this.controller) { this.controller.abort(); this.controller = null; }
      if (this.reader) { this.reader.cancel().catch(function() {}); this.reader = null; }
      this.connected = false;
      if (resetRetries !== false) this._streamRetries = 0;
      this._bpsBytes = 0;
      this._bpsTs = 0;
      this._buf = new Uint8Array(0);
      this._pending = null;
      for (const f of this._frames) { try { f.close(); } catch (e) {} }
      this._frames = [];
      if (this._dec) { try { this._dec.close(); } catch (e) {} this._dec = null; }
      // 光标 overlay 清理（R5#64 通道断开后移除，避免元素/样式残留）。
      if (this._cursorEl) { try { this._cursorEl.remove(); } catch (e) {} this._cursorEl = null; }
      // loading 覆盖：断开即隐藏（下次 connect 再显示）。
      this._hideLoading();
      this._desc = null;
      this._lastCaptureMs = 0;
      this._lastE2eMs = undefined;
      this._lastNewFrameAt = 0;
      this._e2eMs = undefined;
      this._gotFirstFrame = false;
      this._decodeCount = 0;
      this._reqKeyAt = 0;
      this._reqKeyCount = 0;
      this._reqKeyWindowStart = 0;
      this._lastSeq = 0;
      this._seqDrop = 0;
      this._arrivals = [];
      this._lastArrival = 0;
      this._ackFps = null;
      this._ackScale = null;
      this._ttfvStart = 0;
      this._ttfvMs = null;
      this._weakNetStrikes = 0;
      this._weakNet = false;
      this._staleDropped = 0;
      this._throttleCounter = 0;
      this._qosDfps = 0;
      if (this._moveTimer) { clearTimeout(this._moveTimer); this._moveTimer = null; }
      this._movePending = null;
      this._unbindInput();
      this._stopMetrics();
      const panel = document.getElementById('desktop-metrics');
      if (panel) panel.classList.add('hidden');
      this._captureBackend = null;
      this._uplinkMode = null;
      if (this.canvas) {
        const ctx = this.canvas.getContext('2d');
        if (ctx && this.canvas.width) ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
      }
      this.setStatus('', false);
    }

    // ── 性能指标面板 ─────────────────────────────────────────
    // 端到端延时 = 本地时钟 - 帧的采集 epoch（agent 与浏览器时钟需大致
    // 同步；局域网 NTP 下误差 <10ms，公网下仅作参考趋势）。渲染帧率由
    // rAF 计数。捕获方式/链路方式由 agent 的 desktop:started /
    // desktop:uplink 广播提供；解码方案是本播放器自己选的（webcodecs）。

    // agent 经 relay 回传的 QoS 状态（desktop:qos-ack → desktop:qos 的
    // 应答）：当前目标帧率与码率档。面板"目标帧率/活动"行据此展示 agent
    // 眼中的目标 vs 本地实际渲染帧率（对齐 rustdesk TestDelay 携带目标
    // bitrate 的口径；diff >0 时面板可区分"agent 在降帧"还是"解码跟不上"）。
    receiveQosAck(ack) {
      if (!ack) return;
      if (typeof ack.fps === 'number' || typeof ack.fps === 'string') {
        this._ackFps = Number(ack.fps);
      }
      if (typeof ack.qos_scale === 'number' || typeof ack.qos_scale === 'string') {
        this._ackScale = Number(ack.qos_scale);
      }
      // QoS 五态质量状态（R4 甲A0/A2，agent qos-ack 回传）→ 面板状态行。
      if (ack.qos_state) this._qosState = String(ack.qos_state);
      // R5#25 空闲回收可见性：agent 回传 active（最近真实新帧 ≤1.5s）——
      // 静止时 agent 已回收编码资源，面板"目标帧率/活动"行显示"静止"。
      this._ackActive = (typeof ack.active === 'boolean') ? ack.active : undefined;
    }
    // TestDelay 探针回包：agent 原样 echo t0，本地单调时钟算纯网络 RTT。
    receiveTestDelayAck(ack) {
      if (!ack || typeof ack.t0 !== 'number') return;
      const rtt = performance.now() - ack.t0;
      if (!(rtt >= 0) || rtt > 5000) return; // 异常样本丢弃（时钟重置/断连）
      this._probeRttMs = Math.round(rtt);
    }
    _startTestDelay() {
      if (this._testDelayTimer) return;
      const self = this;
      this._testDelayTimer = setInterval(function() {
        if (!self.connected) return;
        self._probeSeq += 1;
        if (window.shellRemote && window.shellRemote.send) {
          window.shellRemote.send('desktop:test-delay', {
            seq: self._probeSeq,
            t0: performance.now()
          });
        }
      }, 1000);
    }
    // 远端光标 overlay（R5#64 光标独立通道）：agent 独立查询位置（X11
    // GetImage 不含光标层）经 desktop:cursor 下发，此处把光标渲染到视频
    // 上方。坐标从 agent 捕获分辨率映射到实际显示尺寸（容器缩放后仍准）。
    updateCursor(x, y, shown) {
      const v = this._targetEl();
      if (!v) return;
      if (!shown) {
        if (this._cursorEl) this._cursorEl.style.display = 'none';
        return;
      }
      const srcW = this._videoW(), srcH = this._videoH();
      if (!srcW || !srcH) return;
      const rect = v.getBoundingClientRect();
      const dispW = rect.width, dispH = rect.height;
      if (!dispW || !dispH) return;
      let el = this._cursorEl;
      if (!el) {
        el = document.createElement('div');
        el.className = 'sr-cursor-overlay';
        el.style.cssText = 'position:absolute;pointer-events:none;z-index:10;width:20px;height:20px;' +
          'background-image:url(\'data:image/svg+xml;utf8,<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><path d="M2 1 L2 13 L5.5 10.2 L8 14.5 L10 13.5 L7.5 9.3 L11 9.3 Z" fill="rgba(0,0,0,0.85)" stroke="white" stroke-width="1.2"/></svg>\');' +
          'background-size:contain;background-repeat:no-repeat;';
        const container = v.parentElement || document.body;
        if (container) {
          if (getComputedStyle(container).position === 'static') {
            container.style.position = 'relative';
          }
          container.appendChild(el);
        }
        this._cursorEl = el;
      }
      const cont = el.parentElement;
      const contRect = cont ? cont.getBoundingClientRect() : { left: 0, top: 0 };
      const left = (rect.left - contRect.left) + (x / srcW) * dispW;
      const top = (rect.top - contRect.top) + (y / srcH) * dispH;
      el.style.left = Math.max(0, Math.round(left)) + 'px';
      el.style.top = Math.max(0, Math.round(top)) + 'px';
      el.style.display = 'block';
    }
    _startMetrics() {
      if (this._metricsTimer) return;
      const panel = document.getElementById('desktop-metrics');
      if (!panel) return;
      const self = this;
      this._startTestDelay();
      // 长会话时钟漂移对抗（R3 丙135）：连接期间每 15min 重校一次 relay 时基，
      // e2e 读数不因双机时钟 drift 缓慢失真。校准失败静默（保持原偏移）。
      if (!this._clockRecheckTimer) {
        this._clockRecheckTimer = setInterval(function() {
          if (!self.connected) return;
          self._calibrateClock();
        }, 15 * 60 * 1000);
      }
      // QoS 反馈 250ms 独立上报（R5#72）：agent 的 fps/码率决策需要比
      // 面板刷新（1s）更密的端到端与解码背压信号——弱网变化越快响应。
      // dfps 按窗口 ×4 折算回每秒（agent 背压阈值语义不变）。
      if (!this._qosTimer) {
        this._qosTimer = setInterval(function() {
          if (!self.connected) return;
          const dfps = self._decodeCount;
          self._decodeCount = 0;
          self._qosDfps = dfps * 4; // 250ms 窗口 → 每秒
          if (self._e2eMs !== undefined && window.shellRemote && window.shellRemote.send) {
            window.shellRemote.send('desktop:qos', {
              delay_ms: Math.round(self._e2eMs),
              probe_ms: self._probeRttMs || 0,
              dfps: self._qosDfps,
              dq: self._dec ? self._dec.decodeQueueSize : 0,
              lseq: self._lastSeq || 0
            });
          }
          // 停滞检测（曾收到帧但现在 500ms 无新帧）也移到这里——比 1s
          // 面板 tick 更早发现关键帧断裂。
          if (self.connected && self._gotFirstFrame && self._lastNewFrameAt &&
            (Date.now() - self._lastNewFrameAt) > 500) {
            self._requestKey();
          }
          // R4 乙30s 判死：曾连上（收到过首帧）但 30s 无任何视频数据到达
          // （WS/SSE 半开黑洞、relay 静默卡死）→ 判定连接死亡，触发重连。
          // 与静止区分：静止有 4s IDR 心跳数据；"_lastDataAt" 超 30s 未刷新
          // 才是真黑洞。触发后置位防重复（重连失败由 SSE 退避/join 看门狗兜底）。
          if (self.connected && self._gotFirstFrame && self._lastDataAt &&
            (Date.now() - self._lastDataAt) > 30000) {
            self._lastDataAt = Date.now();
            if (window.shellRemote && window.shellRemote.reconnect) {
              window.shellRemote.reconnect();
            }
          }
        }, 250);
      }

      // 显式按钮开关（session.html #desktop-metrics-btn）；旧的左上角
      // 隐藏点击区不可发现，已废弃。
      this._onMetricsBtn = function(e) {
        panel.classList.toggle('hidden');
        e.preventDefault();
        e.stopPropagation();
      };
      const btn = document.getElementById('desktop-metrics-btn');
      if (btn) btn.addEventListener('pointerdown', this._onMetricsBtn, true);

      this._rafCount = 0;
      this._metricsTimer = setInterval(function() {
        if (!self.connected) return;
        const lag = document.getElementById('metric-lag');
        const br = document.getElementById('metric-bitrate');
        const res = document.getElementById('metric-res');
        const fps = document.getElementById('metric-fps');
        const gofps = document.getElementById('metric-gofps');
        const buf = document.getElementById('metric-buffer');
        const drop = document.getElementById('metric-dropped');
        const jitterEl = document.getElementById('metric-jitter');
        const weaknet = document.getElementById('metric-weaknet');
        const reqkeyEl = document.getElementById('metric-reqkey');
        const backend = document.getElementById('metric-backend');
        const uplink = document.getElementById('metric-uplink');
        const decoder = document.getElementById('metric-decoder');
        const encoder = document.getElementById('metric-encoder');
        if (!lag) return;
        try {
          // e2e: 采集→解码。数值在 _onDecoded 解码到达时刻测定（即时管线延时，
          // 不含"距上一帧多久"的陈旧度——陈旧度随 fps 升高而膨胀，会喂给 QoS
          // 形成低帧率自锁，MYS-886 卡顿死锁根因）。这里只负责"新鲜窗口内
          // 转发最近样本"：静止（1.5s 无新帧）时 srtc 陈旧不更新不误报。
          const fresh = self._lastNewFrameAt &&
            self._lastE2eMs !== undefined &&
            (Date.now() - self._lastNewFrameAt) <= 1500;
          if (fresh) {
            const e2e = self._lastE2eMs;
            self._e2eMs = e2e;
            lag.textContent = e2e + ' ms';
          } else {
            // 静止/无新帧：不更新 e2e、不向 agent 上报（静止延迟无意义且虚高）。
            self._e2eMs = undefined;
            lag.textContent = '-';
          }
          res.textContent = self.canvas ? self.canvas.width + 'x' + self.canvas.height : '-';
          const actualFps = self._rafCount;
          fps.textContent = actualFps;
          self._rafCount = 0;
          // 解码队列帧数 + 排队时延估算（R2 己175 / R5#60：e2e 归因分流——
          // e2e 高时查这行是"解码积压"还是"上游慢"）。dq/解码fps ≈ 秒，
          // 画面要等多久才轮到当前帧。
          const dq = self._dec ? self._dec.decodeQueueSize : 0;
          if (dq > 0 && self._qosDfps > 0) {
            const estMs = Math.round(dq / self._qosDfps * 1000);
            buf.textContent = dq + ' 帧 (~' + estMs + 'ms)';
          } else {
            buf.textContent = dq;
          }
          drop.textContent = self._droppedFrames + ' 解码 / ' + self._staleDropped + ' 超龄 / ' + self._seqDrop + ' 上行(seq)';
          // 帧到达 jitter（stddev）：稳定流应 ≪ 帧间隔（top 场景目标 <8ms）
          if (jitterEl) {
            const a = self._arrivals;
            self._arrivals = [];
            self._lastArrival = 0;
            if (a.length >= 2) {
              const m = a.reduce(function(x, y) { return x + y; }, 0) / a.length;
              const v = a.reduce(function(x, y) { var d = y - m; return x + d * d; }, 0) / a.length;
              jitterEl.textContent = Math.sqrt(v).toFixed(1) + ' ms';
            } else {
              jitterEl.textContent = '-';
            }
          }
          // TestDelay 网络层 RTT（R5#148）：与端到端延时对照——网络 RTT 正常
          // 而 e2e 高 → 延迟在管线/解码侧而非网络。
          const probeEl = document.getElementById('metric-probe');
          if (probeEl) probeEl.textContent = self._probeRttMs ? self._probeRttMs + ' ms' : '-';
          // QoS 五态状态行（R4 甲A0/A2）：agent qos-ack 回传，颜色分级。
          const qosStateEl = document.getElementById('metric-qos-state');
          if (qosStateEl) {
            const s = self._qosState;
            if (s) {
              qosStateEl.textContent = s;
              const colors = { Good: '#3fbf6f', Medium: '#d9b13f', Degraded: '#e08a2e', Critical: '#e04f4f', Unknown: '#999' };
              qosStateEl.style.color = colors[s] || '#999';
            } else {
              qosStateEl.textContent = '-';
              qosStateEl.style.color = '';
            }
          }
          br.textContent = self._avgKbps ? self._avgKbps + ' kbps' : '-';
          if (backend) backend.textContent = self._captureBackend || '-';
          if (uplink) uplink.textContent = self._uplinkMode || '-';
          if (decoder) decoder.textContent = self._decoderLabel();
          if (encoder) encoder.textContent = self._encoderLabel();
          // JS 内存曲线（R2 丁131 / R5#61）：长会话泄漏观测——Chrome 暴露
          // performance.memory（非标准但实际可用），used 显示当前 + 峰值。
          // 持续上升不回落 = 泄漏信号。
          const jsmem = document.getElementById('metric-jsmem');
          if (jsmem) {
            if (performance.memory && performance.memory.usedJSHeapSize) {
              const used = Math.round(performance.memory.usedJSHeapSize / 1024 / 1024);
              if (used > (self._jsMemPeak || 0)) self._jsMemPeak = used;
              jsmem.textContent = used + ' MB' + (self._jsMemPeak ? ' / 峰值 ' + self._jsMemPeak + ' MB' : '');
            } else {
              jsmem.textContent = '不支持';
            }
          }
          // 目标帧率 vs 内容活动：agent 回传的目标 fps 与活跃度标记
          // （desktop:qos-ack），与本地实际渲染对比。fps=1 → 静态；≥15 →
          // 动态满帧；中间为解码背压阶梯。活跃度优先用 agent 实测 active
          // （R5#25：静止时 agent 已回收编码资源仍保持画面→显示"静止"而非
          // 误以为卡死）；未回传时回退到 ack>=15 的档位推断。
          if (gofps) {
            const ack = self._ackFps;
            if (ack !== null && ack !== undefined) {
              let state;
              if (self._ackActive === false) state = '静止';
              else if (self._ackActive === true || ack >= 15) state = '活跃';
              else state = '静态';
              gofps.textContent = '目标 ' + ack + ' / 实际 ' + actualFps + ' (' + state + ')';
            } else {
              gofps.textContent = '实际 ' + actualFps;
            }
          }
          // reqkey 计数：面板可见的刷新风暴观测（对齐 R2 己110/R4 丁75）。
          if (reqkeyEl) {
            if (self._reqKeyCount > 0) {
              reqkeyEl.textContent = self._reqKeyCount + ' 次/10s';
            } else {
              reqkeyEl.textContent = self._reqKeyHistory ? self._reqKeyHistory + ' 次' : '-';
            }
          }
          // 弱网模式标记：e2e 连续 ≥2 个观测窗口命中阈值（>500ms）判弱网；
          // 恢复（e2e<阈值）后清零标记（对齐 R3 丁110 弱网模式 UI）。
          if (weaknet) {
            if (self._e2eMs !== undefined && self._e2eMs > 500) {
              self._weakNetStrikes += 1;
              if (self._weakNetStrikes >= 2) self._weakNet = true;
            } else {
              self._weakNetStrikes = 0;
              self._weakNet = false;
            }
            let wl = self._weakNet ? '弱网 ON' : '正常';
            if (self._ttfvMs !== null) wl += ' · 首帧 ' + self._ttfvMs + 'ms';
            weaknet.textContent = wl;
            weaknet.style.color = self._weakNet ? '#f0a020' : '';
          }
          // QoS 反馈已移至 250ms 独立定时器（R5#72），此处不再重复上报。
          // 停滞检测同理（250ms 定时器内）。
        } catch (e) { /* 面板只是展示 */ }
      }, 1000);
    }

    _stopMetrics() {
      if (this._metricsTimer) {
        clearInterval(this._metricsTimer);
        this._metricsTimer = null;
      }
      if (this._qosTimer) {
        clearInterval(this._qosTimer);
        this._qosTimer = null;
      }
      if (this._clockRecheckTimer) {
        clearInterval(this._clockRecheckTimer);
        this._clockRecheckTimer = null;
      }
      if (this._testDelayTimer) {
        clearInterval(this._testDelayTimer);
        this._testDelayTimer = null;
      }
      const btn = document.getElementById('desktop-metrics-btn');
      if (btn && this._onMetricsBtn) btn.removeEventListener('pointerdown', this._onMetricsBtn, true);
    }

    // ── 键鼠输入注入（与 MSE 版相同）────────────────────────
    _bindInput() {
      if (this._inputBound) return;
      this._inputBound = true;
      const v = this._targetEl();
      const self = this;

      const send = function(type, payload) {
        if (!self.connected) return;
        if (window.shellRemote && window.shellRemote.send) {
          window.shellRemote.send(type, payload);
        }
      };

      this._toDesktopXY = function(e) {
        const vw = self._videoW(), vh = self._videoH();
        if (!vw || !vh) return null;
        const rect = v.getBoundingClientRect();
        // CSS object-fit: contain —— 完整画面可见, 等比缩放铺满一条边,
        // 另一条边留最多两条黑边。像素→桌面坐标取 min(scale): 渲染区
        // 恰好是元素 box 内的 contain 矩形(黑边区不产生输入映射)。
        // getBoundingClientRect 每次取实时容器尺寸, 窗口缩放/浏览器缩放
        // 时坐标映射随之动态调整(MYS-886)。
        const scale = Math.min(rect.width / vw, rect.height / vh);
        const drawW = vw * scale, drawH = vh * scale;
        const offX = (rect.width - drawW) / 2, offY = (rect.height - drawH) / 2;
        const x = (e.clientX - rect.left - offX) / scale;
        const y = (e.clientY - rect.top - offY) / scale;
        if (x < 0 || y < 0 || x >= vw || y >= vh) return null;
        return { x: Math.round(x), y: Math.round(y) };
      };

            // 弱网输入降采样（对齐 R2 丙104/109）：e2e>300ms 时鼠标 move 按 2:1
      // 节流（只发奇数次），>800ms 时再减半（每 4 次发 1 次）。输入事件与
      // 视频共享弱网上行带宽——主动降输入频率比让它拥塞造成视频/控制双向
      // 卡顿更优（rustdesk 弱网输入节流语义）。点击/滚轮/按键是离散动作，
      // 不做采样（一次点击漏发会错意），只节流连续 move。
      this._throttleCounter = 0;
      // 10ms 输入合并（R3 己188 / R5#33）：浏览器 mousemove 可达 ~120Hz，
      // 远超 agent 注入/远端消费速率。合并到 10ms 窗口发**最后一个坐标**
      // （追新语义），把上行控制频率压到 ~100Hz 且不丢移动轨迹——与弱网
      // 降采样（按 e2e 进一步减）叠加。
      this._movePending = null;   // {x, y}
      this._moveTimer = null;
      this._onPointerMove = function(e) {
        const p = self._toDesktopXY(e);
        if (!p) return;
        // 弱网降采样：e2e 高时跳过部分事件（离散点按计数取）。
        const e2e = self._e2eMs;
        if (e2e !== undefined) {
          self._throttleCounter += 1;
          if (e2e > 300) {
            if (e2e > 800) {
              if (self._throttleCounter % 4 !== 0) return;
            } else if (self._throttleCounter % 2 !== 0) {
              return;
            }
          }
        }
        // 10ms 合并：缓存最后坐标，定时器到点只发最新一个。
        self._movePending = p;
        if (!self._moveTimer) {
          self._moveTimer = setTimeout(function() {
            self._moveTimer = null;
            const last = self._movePending;
            self._movePending = null;
            if (last) send('desktop:mouse', { type: 'move', x: last.x, y: last.y });
          }, 10);
        }
      };
      this._onPointerDown = function(e) {
        const p = self._toDesktopXY(e);
        if (!p) return;
        send('desktop:mouse', { type: 'move', x: p.x, y: p.y });
        send('desktop:mouse', { type: 'down', button: e.button });
        v.setPointerCapture(e.pointerId);
        // preventDefault 会阻止 mousedown 的默认聚焦 → canvas 拿不到焦点
        // → keydown 永远不触发（键盘完全无反应的根因）。显式 focus 补回。
        if (v.focus) v.focus();
        e.preventDefault();
      };
      this._onPointerUp = function(e) {
        send('desktop:mouse', { type: 'up', button: e.button });
        e.preventDefault();
      };
      this._onWheel = function(e) {
        var unit = 100;
        if (e.deltaMode === 1) unit = 33;
        else if (e.deltaMode === 2) unit = 100;
        var clicks = Math.round(Math.abs(e.deltaY) / unit) || 1;
        var dy = e.deltaY > 0 ? clicks : -clicks;
        var dx = 0;
        if (e.deltaX) {
          var c2 = Math.round(Math.abs(e.deltaX) / unit) || 1;
          dx = e.deltaX > 0 ? c2 : -c2;
        }
        send('desktop:mouse', { type: 'wheel', dx: dx, dy: dy });
        e.preventDefault();
      };
      this._onContextMenu = function(e) { e.preventDefault(); };
      // 键盘挂 window 而非 canvas：焦点管理在浏览器里很脆弱（点工具栏、
      // Alt+Tab 回来后焦点在 body），canvas 上的 keydown 依赖焦点正确落位。
      // 桌面视图激活期间（_inputBound）全局转发，终端此时是隐藏的，无冲突。
      // 输入框聚焦时（文件抽屉重命名等）不拦截。
      this._onKeyWin = function(down) {
        return function(e) {
          if (!self._inputBound) return;
          const t = e.target;
          if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
          send('desktop:key', { code: e.code, down: down });
          if (['F5', 'F12'].indexOf(e.code) < 0) e.preventDefault();
        };
      };
      this._onKeyDown = this._onKeyWin(true);
      this._onKeyUp = this._onKeyWin(false);

      v.addEventListener('pointermove', this._onPointerMove);
      v.addEventListener('pointerdown', this._onPointerDown);
      v.addEventListener('pointerup', this._onPointerUp);
      v.addEventListener('wheel', this._onWheel, { passive: false });
      v.addEventListener('contextmenu', this._onContextMenu);
      window.addEventListener('keydown', this._onKeyDown);
      window.addEventListener('keyup', this._onKeyUp);
      v.tabIndex = 0;
    }

    _targetEl() { return this.canvas || this.video; }
    _videoW() { return this.canvas ? this.canvas.width : this.video.videoWidth; }
    _videoH() { return this.canvas ? this.canvas.height : this.video.videoHeight; }

    _unbindInput() {
      if (!this._inputBound) return;
      this._inputBound = false;
      const v = this._targetEl();
      v.removeEventListener('pointermove', this._onPointerMove);
      v.removeEventListener('pointerdown', this._onPointerDown);
      v.removeEventListener('pointerup', this._onPointerUp);
      v.removeEventListener('wheel', this._onWheel);
      v.removeEventListener('contextmenu', this._onContextMenu);
      window.removeEventListener('keydown', this._onKeyDown);
      window.removeEventListener('keyup', this._onKeyUp);
    }
  };
})();
