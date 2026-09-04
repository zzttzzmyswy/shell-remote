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
  const MAX_DECODE_QUEUE = 16;

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
      this._frames = [];          // decoded VideoFrames pending render
      this._lastCaptureMs = 0;    // 最新已渲染帧的采集时间（e2e 延时）
      this._e2eMs = null;
      this._renderPending = false;
      this._droppedFrames = 0;
      // 浏览器与 relay 的时钟偏移（relay_epoch - 本地_epoch）。srtc 在 relay
      // 转发时被改写为 relay 墙钟，e2e 用 本地now+偏移 与 srtc 对齐，彻底摆脱
      // 双机系统时间差（MYS-886 指标失真根因）。
      this._clockOffset = 0;
    }

    // 向 relay /api/clock 做 NTP 式往返采样，求得 (relay_epoch - 本地_epoch)。
    // 采样 3 次取中值，消除单向网络延迟造成的偏差。
    _calibrateClock() {
      const self = this;
      const samples = [];
      let pending = 3;
      return new Promise(function(resolve) {
        const done = function() {
          if (samples.length === 0) { resolve(); return; }
          samples.sort(function(a, b) { return a.offset - b.offset; });
          self._clockOffset = samples[Math.floor(samples.length / 2)].offset;
          resolve();
        };
        for (let i = 0; i < 3; i++) {
          const t0 = Date.now();
          fetch('/api/clock', { cache: 'no-store' }).then(function(r) { return r.json(); })
            .then(function(j) {
              const t1 = Date.now();
              const rtt = t1 - t0;
              // 单程 ≈ rtt/2：relay 时钟等于 t0 时刻的 (j.epoch_ms - rtt/2)
              const relayAtT0 = j.epoch_ms - rtt / 2;
              samples.push({ offset: relayAtT0 - t0 });
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

    connect() {
      this.disconnect(false);
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
      // 追加到缓冲
      const merged = new Uint8Array(this._buf.length + chunk.byteLength);
      merged.set(this._buf, 0);
      merged.set(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength), this._buf.length);
      this._buf = merged;

      let guard = 0;
      while (guard++ < 512) {
        const box = this._parseNextBox();
        if (!box) break;
        if (box.type === 'moov' || (box.type === 'ftyp' && !this._desc)) {
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
        return null;
      }
      if (b.length < size) return null;
      const type = String.fromCharCode(b[4], b[5], b[6], b[7]);
      const body = b.subarray(8, size);
      this._buf = b.slice(size);
      return { type: type, body: body };
    }

    _handleMoov(body) {
      // 找 avcC box。扫描到的 i 指向 4 字节标签 "avcC"，payload 从 i+4 起：
      //   [i+4]=1(version) [i+5..i+7]=profile/compat/level [i+8]=0xff
      //   [i+9]=numSPS [i+10..i+11]=spsLen sps... [..]=numPPS [..]=ppsLen pps...
      // 之前误把 i（标签起点）当 payload 起算，codec 串取到 'v','c','C'
      // 变成 avc1.766343 非法——WebCodecs configure 报 Unknown codec name。
      for (let i = 0; i + 8 <= body.length; i++) {
        if (body[i] === 0x61 && body[i+1] === 0x76 && body[i+2] === 0x63 && body[i+3] === 0x43) {
          const spsLen = (body[i+10] << 8) | body[i+11];
          const numPpsOff = i + 12 + spsLen;
          const ppsLen = (body[numPpsOff+1] << 8) | body[numPpsOff+2];
          // description 取 AVCDecoderConfigurationRecord（不含 box 标签）。
          this._desc = body.slice(i + 4, numPpsOff + 3 + ppsLen);
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
      // codec 串取 avcC 的 profile/compat/level（与码流一致才被接受）。
      const hex = (b) => b.toString(16).padStart(2, '0').toUpperCase();
      const codec = 'avc1.' + hex(this._desc[1]) + hex(this._desc[2]) + hex(this._desc[3]);
      this._dec = new VideoDecoder({
        output: function(frame) { self._onDecoded(frame); },
        error: function(e) {
          self.setStatus('解码错误: ' + e.message, true);
        }
      });
      this._dec.configure({
        codec: codec,
        description: this._desc,
        optimizeForLatency: true
      });
      this._codecStr = codec;
    }

    // moof: 提取 tfdt(pts)、trun(size+flags)、srtc(captureMs)
    _handleMoof(body) {
      if (!this._dec) return; // 尚无 init
      // 遍历 traf 子 box
      let pos = 0;
      let ptsUs = 0, sampleSize = 0, isKey = false, captureMs = 0;
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
              const firstFlags = (d2[12] << 16) | (d2[13] << 8) | d2[14];
              isKey = (firstFlags & 0x00ff0000) !== 0; // sample_is_non_sync_sample 位为 0 → key
              const szPos = d2.length - 4;
              sampleSize = (d2[szPos] << 24) | (d2[szPos+1] << 16) | (d2[szPos+2] << 8) | d2[szPos+3];
            } else if (t2 === 'srtc' && d2.length >= 8) {
              captureMs = Number((BigInt(d2[0]) << 56n) | (BigInt(d2[1]) << 48n) |
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
      this._pending = { ptsUs: ptsUs, isKey: isKey, captureMs: captureMs, size: sampleSize };
    }

    _handleMdat(body) {
      const p = this._pending;
      this._pending = null;
      if (!p || !this._dec || this._dec.state === 'closed') return;
      const sample = body.subarray(body.length - p.size); // mdat 尾部即本帧
      const chunk = new EncodedVideoChunk({
        type: p.isKey ? 'key' : 'delta',
        timestamp: p.ptsUs,
        data: sample
      });
      // 积压保护：解码队列过深时丢旧的非关键帧。
      if (this._dec.decodeQueueSize > MAX_DECODE_QUEUE && !p.isKey) {
        this._droppedFrames += 1;
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
        if (p.isKey && this._desc) this._initDecoder();
      }
    }

    _onDecoded(frame) {
      // 从 timestamp 索引取回采集时间，供 e2e 延时计算。
      const capMs = this._captureByPts ? this._captureByPts.get(frame.timestamp) : null;
      if (this._captureByPts) this._captureByPts.delete(frame.timestamp);
      if (capMs) this._lastCaptureMs = capMs;
      if (this._frames.length > 4) {
        // 渲染管线积压：丢最旧帧（保留最新）。
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
      frame.close();
      this._frames = [];
    }

    _trackBandwidth(bytes) {
      const now = Date.now();
      if (!this._bpsTs) {
        this._bpsTs = now;
        this._bpsBytes = 0;
      }
      this._bpsBytes += bytes;
      const dt = (now - this._bpsTs) / 1000;
      if (dt >= 2.5) {
        const kbps = Math.round(this._bpsBytes * 8 / dt / 1000);
        this._lastKbps = kbps;
        this._bpsTs = now;
        this._bpsBytes = 0;
        if (kbps > 0 && window.shellRemote && window.shellRemote.send) {
          window.shellRemote.send('desktop:bitrate', { kbps: kbps });
        }
      }
    }

    disconnect(resetRetries) {
      if (this._mse) {
        this._mse.disconnect(resetRetries);
        this._mse = null;
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
      this._desc = null;
      this._lastCaptureMs = 0;
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
    _startMetrics() {
      if (this._metricsTimer) return;
      const panel = document.getElementById('desktop-metrics');
      if (!panel) return;
      const self = this;

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
        const buf = document.getElementById('metric-buffer');
        const drop = document.getElementById('metric-dropped');
        const backend = document.getElementById('metric-backend');
        const uplink = document.getElementById('metric-uplink');
        const decoder = document.getElementById('metric-decoder');
        if (!lag) return;
        try {
          // e2e: 采集→渲染。srtc 已由 agent 校准到 relay 时基；本地 now 也加
          // _clockOffset 换算到 relay 时基 —— 两个值都在同一时间轴上，彻底
          // 摆脱 agent/浏览器两机系统时钟差（MYS-886 指标失真根因）。
          if (self._lastCaptureMs) {
            const e2e = Math.max(0, (Date.now() + self._clockOffset) - self._lastCaptureMs);
            self._e2eMs = e2e;
            lag.textContent = e2e + ' ms';
          } else {
            lag.textContent = '-';
          }
          res.textContent = self.canvas ? self.canvas.width + 'x' + self.canvas.height : '-';
          fps.textContent = self._rafCount;
          self._rafCount = 0;
          buf.textContent = self._dec ? self._dec.decodeQueueSize : '-';
          drop.textContent = self._droppedFrames;
          br.textContent = self._lastKbps ? self._lastKbps + ' kbps' : '-';
          if (backend) backend.textContent = self._captureBackend || '-';
          if (uplink) uplink.textContent = self._uplinkMode || '-';
          if (decoder) decoder.textContent = self._decoderLabel();
        } catch (e) { /* 面板只是展示 */ }
      }, 1000);

      const raf = function() {
        if (!self._metricsTimer) return;
        self._rafCount += 1;
        requestAnimationFrame(raf);
      };
      requestAnimationFrame(raf);
    }

    _stopMetrics() {
      if (this._metricsTimer) {
        clearInterval(this._metricsTimer);
        this._metricsTimer = null;
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
        // CSS object-fit: cover —— 画面铺满容器, 比例不一致时裁切超出部分。
        // 像素→桌面坐标取 max(scale): 显示区恰好覆盖整个元素 box。
        const scale = Math.max(rect.width / vw, rect.height / vh);
        const drawW = vw * scale, drawH = vh * scale;
        const offX = (rect.width - drawW) / 2, offY = (rect.height - drawH) / 2;
        const x = (e.clientX - rect.left - offX) / scale;
        const y = (e.clientY - rect.top - offY) / scale;
        if (x < 0 || y < 0 || x >= vw || y >= vh) return null;
        return { x: Math.round(x), y: Math.round(y) };
      };

      this._onPointerMove = function(e) {
        const p = self._toDesktopXY(e);
        if (p) send('desktop:mouse', { type: 'move', x: p.x, y: p.y });
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
