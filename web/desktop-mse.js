// desktop.js - MSE fMP4 player for the shell-remote desktop view.
//
// The agent streams a fragmented MP4 (init segment + per-frame fragments)
// over `GET /agent/desktop/stream` (token via Authorization header). We feed
// those bytes straight into a SourceBuffer. The relay replays the cached init
// segment to every new stream request, so joining mid-stream works.

(function() {
  'use strict';

  // 低延迟追帧余量（秒）：播放头被钳到 缓冲尾部-该值。60fps 下 0.15s
  // （≈9 帧）平滑且延时最低; 若卡顿明显可回调（MYS-886 延时优化）。
  const LIVE_EDGE_LAG = 0.15;

  window.DesktopViewMse = class {
    constructor() {
      this.video = document.getElementById('desktop-video');
      this.statusEl = document.getElementById('desktop-status');
      this.mediaSource = null;
      this.sourceBuffer = null;
      this.controller = null; // AbortController for the fetch
      this.reader = null;
      this.queue = [];
      this.codec = 'avc1.42E01E'; // OpenH264 baseline profile default
      this.connected = false;
      this._streamRetries = 0;
      this._bpsBytes = 0;
      this._bpsTs = 0;
      this._inputBound = false;
    }

    // relay 预建桌面流的时机略晚于 desktop:started 广播; 遇到 404 时重试
    // (指数退避, 上限 10 次约 3.5 分钟)等待流就绪, 返回 true 表示已接管重试。
    // 之前上限 5 次太紧: agent 静止超时被 relay 收流后再重建的窗口里,
    // 5 次 700ms 重试耗尽后视图永久停住, 用户必须刷新页面。
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

    _codecSupported(codec) {
      return typeof MediaSource !== 'undefined' &&
        MediaSource.isTypeSupported('video/mp4; codecs="' + codec + '"');
    }

    // 弱网自适应：用拉流速度估计可用带宽，每 ~2.5s 上报给 agent，
    // agent 把编码码率天花板 clamp 到该值（桌面帧允许丢旧保新）。
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
        this._lastKbps = kbps; // 指标面板展示当前值
        this._bpsTs = now;
        this._bpsBytes = 0;
        if (kbps > 0 && window.shellRemote && window.shellRemote.send) {
          window.shellRemote.send('desktop:bitrate', { kbps: kbps });
        }
      }
    }

    connect() {
      this.disconnect(false); // 重试保留 _streamRetries 计数; 主动断开才清零
      if (typeof MediaSource === 'undefined') {
        this.setStatus('当前浏览器不支持 MediaSource', true);
        return;
      }
      const ms = new MediaSource();
      this.mediaSource = ms;
      this.video.src = URL.createObjectURL(ms);
      const self = this;
      ms.addEventListener('sourceopen', function onOpen() {
        ms.removeEventListener('sourceopen', onOpen);
        // 不在这里用预设 codec 建 SourceBuffer：必须先拿到 init 段、解析
        // SPS 的真实 profile/level 再建。用 avc1.42E01E(level 3.0) 建而实际
        // 1080p 码流是 level 4.0 时, 严格浏览器会拒绝解码(黑屏)。
        self._startFetch();
      });
    }

    _createSourceBuffer(codec) {
      const self = this;
      try {
        this.sourceBuffer = this.mediaSource.addSourceBuffer('video/mp4; codecs="' + codec + '"');
        this.sourceBuffer.addEventListener('updateend', function() {
          self._drain();
          self._syncPlayhead();
        });
        this.sourceBuffer.addEventListener('error', function() {
          // 附上浏览器标识与当前 UA，便于远程定位是哪类内核拒绝解码。
          let ua = '';
          try { ua = ' UA=' + navigator.userAgent; } catch (e) {}
          self.setStatus('MSE 解码错误: 浏览器无法解码该视频流 (codec=' + codec + ')' + ua, true);
        });
      } catch (e) {
        this.setStatus('播放器初始化失败: ' + e.message, true);
        this.disconnect();
        return;
      }
      // video 元素错误（MEDIA_ERR_* code）比 SourceBuffer 事件更有信息量
      this.video.addEventListener('error', function onVErr() {
        const m = self.video.error;
        if (m) {
          let ua = '';
          try { ua = ' UA=' + navigator.userAgent; } catch (e) {}
          self.setStatus('播放错误 code=' + m.code + ' (' + m.message + ')' + ua, true);
        }
        self.video.removeEventListener('error', onVErr);
      });
      this._drain();
    }

    // 实时流对齐：agent 端时间戳从本会话累计，浏览器是"中途加入"——首帧
    // pts 通常远大于 0（如 10s）。MSE 播放器默认停在 currentTime=0，该处
    // 无缓冲 → 黑屏。这里把播放头对齐到缓冲窗口起点（实时流标准做法）。
    _syncPlayhead() {
      const sb = this.sourceBuffer;
      const v = this.video;
      if (!sb || !v || v.seeking || v.readyState === 0) return;
      let bl = 0;
      try { bl = sb.buffered ? sb.buffered.length : 0; } catch (e) { return; }
      if (bl === 0) return;
      let start = 0;
      try { start = sb.buffered.start(0); } catch (e) { return; }
      if (v.currentTime < start - 0.05) {
        v.currentTime = start;
      }
      // 低延迟追帧：实时桌面流不允许缓冲堆积。播放头若落后缓冲尾部超过
      // LIVE_EDGE_LAG 秒，直接跳到 尾部-LIVE_EDGE_LAG 处，防止延时单调
      // 增长到秒级（之前 7s 延时的根因：只在落后起点时 seek，从不追尾）。
      // 参考 RustDesk frame_controller 的持续追帧策略。
      let end = 0;
      try { end = sb.buffered.end(bl - 1); } catch (e) { return; }
      if (v.currentTime < end - LIVE_EDGE_LAG) {
        // 落后太多(>2s)时说明 seek 到了空洞（跳帧策略在 30fps 高码率下
        // 产生的缓冲碎片）或长期停滞——直接跳到尾部最新处重新开始,
        // 避免停留在旧 range 的缝里越拖越远。
        v.currentTime = v.currentTime < end - 2.0 ? end - 0.05 : end - LIVE_EDGE_LAG;
      }
      // 缓冲修剪：实时流不需要回看。播放头之后的旧缓冲持续移除,
      // 限制 SourceBuffer 内存并减少 range 碎片（碎片会让 seek 落进空洞）。
      try {
        const keep = Math.max(start, v.currentTime - 2);
        if (keep > start + 1 && !sb.updating) {
          sb.remove(start, keep);
        }
      } catch (e) { /* remove 中断无害 */ }
    }

    // 从 init 段 (ftyp/moov 内含 avcC) 解析浏览器实际需要的 codec 串。
    // OpenH264 输出的 SPS profile/level 可能与预设不同；解析到真实值后
    // 用 true codec 建 SourceBuffer，避免严格 MSE 因 codec 串与实际流不
    // 匹配而拒播（1080p 实际是 level 4.0，avc1.42E01E 是 level 3.0）。
    _codecFromInit(buf) {
      const u8 = new Uint8Array(buf);
      for (let i = 0; i + 8 <= u8.length; i++) {
        if (u8[i] === 0x61 && u8[i + 1] === 0x76 && u8[i + 2] === 0x63 && u8[i + 3] === 0x43) {
          const hex = (b) => b.toString(16).padStart(2, '0').toUpperCase();
          return 'avc1.' + hex(u8[i + 5]) + hex(u8[i + 6]) + hex(u8[i + 7]);
        }
      }
      return null;
    }

    _resolveCodec(initBuf) {
      const actual = this._codecFromInit(initBuf);
      if (actual && this._codecSupported(actual)) return actual;
      // fallback：默认串 / 高 profile 串
      if (this._codecSupported(this.codec)) return this.codec;
      if (this._codecSupported('avc1.64001E')) return 'avc1.64001E';
      if (this._codecSupported('avc1.42C028')) return 'avc1.42C028'; // 1080p level 4.0
      return 'avc1.640028';
    }

    _startFetch() {
      const token = sessionStorage.getItem('shell-remote-token');
      if (!token) {
        this.setStatus('缺少会话密钥', true);
        return;
      }
      const controller = new AbortController();
      this.controller = controller;

      this.pendingChunks = [];

      const self = this;
      fetch('/agent/desktop/stream', {
        headers: { 'Authorization': 'Bearer ' + token }
      }).then(function(resp) {
        if (controller.signal.aborted) return null;
        if (!resp.ok || !resp.body) {
          // relay 在 desktop:started 预建流、首个 init 分片随后到达;
          // 若本请求赶在 init 之前打到 relay, 会得到 404。短暂重试等待
          // 流就绪, 而不是直接判失败。
          if (resp.status === 404 && self._retryDesktopStream()) {
            return null;
          }
          self.setStatus('桌面流不可用 (HTTP ' + resp.status + ')，请重试', true);
          self.disconnect();
          return null;
        }
        self._streamRetries = 0;
        self.connected = true;
        self._bindInput(); // 流就绪后开始采集键鼠事件
        self._startMetrics(); // 性能指标面板
        self.setStatus('桌面已连接', false);
        const reader = resp.body.getReader();
        self.reader = reader;

        function pump() {
          if (controller.signal.aborted) return Promise.resolve();
          return reader.read().then(function(result) {
            if (result.done) {
              // relay 可能在 init 长时间未收到/流空闲超时后主动结束本
              // viewer 流; 视图仍激活则自动重连(MSE 重新从 init 建流),
              // 而不是停在黑屏。上限 10 次 + 退避, 覆盖 agent 重建窗口。
              if (self.mediaSource && self._streamRetries < 10) {
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
              const buf = v.buffer.slice(v.byteOffset, v.byteOffset + v.byteLength);
              // 用 init 段解析真实 codec 后再建 SourceBuffer(首个 chunk)。
              if (!self.sourceBuffer) {
                self.pendingChunks.push(buf);
                self.codec = self._resolveCodec(buf);
                self._createSourceBuffer(self.codec);
                self._trackBandwidth(v.byteLength);
                return pump();
              }
              self.pendingChunks.push(buf);
              self._drain();
              self._trackBandwidth(v.byteLength);
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

    _drain() {
      const sb = this.sourceBuffer;
      if (!sb || sb.updating) return;
      if (this.pendingChunks.length === 0) return;

      const next = this.pendingChunks.shift();
      try {
        sb.appendBuffer(next);
      } catch (e) {
        // SourceBuffer 被移除/更换(如 codec 重建)时: 丢弃该队列并下次重连重建。
        if (e && e.name === 'InvalidStateError') {
          this.pendingChunks = [];
        } else {
          console.warn('appendBuffer failed:', e);
        }
      }
    }

    disconnect(resetRetries) {
      if (this.controller) { this.controller.abort(); this.controller = null; }
      if (this.reader) { this.reader.cancel().catch(function() {}); this.reader = null; }
      this.connected = false;
      if (resetRetries !== false) this._streamRetries = 0;
      this._bpsBytes = 0;
      this._bpsTs = 0;
      this.queue = [];
      this.pendingChunks = [];
      this._unbindInput();
      this._stopMetrics();
      const panel = document.getElementById('desktop-metrics');
      if (panel) panel.classList.add('hidden');
      if (this.mediaSource) {
        if (this.mediaSource.readyState === 'open') {
          try { this.mediaSource.endOfStream(); } catch (e) { /* ignore */ }
        }
        try { URL.revokeObjectURL(this.video.src); } catch (e) { /* ignore */ }
        this.mediaSource = null;
        this.sourceBuffer = null;
      }
      this.video.pause();
      this.video.removeAttribute('src');
      this.setStatus('', false);
    }

    // ── 性能指标面板 ─────────────────────────────────────────────
    // 点击视频切换显隐; 显示端到端延时(缓冲尾部-播放头)、码率、分辨率、
    // 渲染 fps、缓冲长度、丢帧。数据源全部本地可得, 无需 agent 配合。
    _startMetrics() {
      if (this._metricsTimer) return;
      const v = this.video;
      const panel = document.getElementById('desktop-metrics');
      if (!panel) return;
      const self = this;

      // 双击/长按会与鼠标注入冲突——用"单击视频"切换面板; 但单击也会
      // 发送一个 left click 注入。约定: 点击画面左上角 64x32 安全区切换
      // (该区域面板本身挡住, 不会误触远端)。
      this._onToggleMetrics = function(e) {
        const rect = v.getBoundingClientRect();
        if (e.clientX - rect.left < 64 && e.clientY - rect.top < 32) {
          panel.classList.toggle('hidden');
          e.stopPropagation();
          e.preventDefault();
        }
      };
      v.addEventListener('pointerdown', this._onToggleMetrics, true); // capture 先于注入

      let frames = 0;
      // requestVideoFrameCallback 计渲染帧率(可用时)
      if (v.requestVideoFrameCallback) {
        const cb = function() {
          frames += 1;
          v.requestVideoFrameCallback(cb);
        };
        v.requestVideoFrameCallback(cb);
      }

      this._metricsTimer = setInterval(function() {
        if (!self.connected) return;
        const lag = document.getElementById('metric-lag');
        const br = document.getElementById('metric-bitrate');
        const res = document.getElementById('metric-res');
        const fps = document.getElementById('metric-fps');
        const buf = document.getElementById('metric-buffer');
        const drop = document.getElementById('metric-dropped');
        if (!lag) return;

        try {
          const b = v.buffered;
          if (b && b.length) {
            const end = b.end(b.length - 1);
            const ms = Math.round((end - v.currentTime) * 1000);
            lag.textContent = ms + ' ms';
            const total = (b.end(b.length - 1) - b.start(0));
            buf.textContent = total.toFixed(1) + ' s';
          } else {
            lag.textContent = '-'; buf.textContent = '-';
          }
          res.textContent = v.videoWidth + 'x' + v.videoHeight;
          fps.textContent = frames; // 1s 间隔, 数值即 fps
          frames = 0;
          if (v.getVideoPlaybackQuality) {
            drop.textContent = v.getVideoPlaybackQuality().droppedVideoFrames;
          } else { drop.textContent = '-'; }
          // 码率: 复用带宽跟踪的计数窗口(2.5s), 折算当前值
          br.textContent = self._lastKbps ? self._lastKbps + ' kbps' : '-';
        } catch (e) { /* 面板只是展示, 不因异常中断播放 */ }
      }, 1000);
    }

    _stopMetrics() {
      if (this._metricsTimer) {
        clearInterval(this._metricsTimer);
        this._metricsTimer = null;
      }
    }

    // ── 键鼠输入注入 ─────────────────────────────────────────────
    // 在 <video> 上采集 pointer/键盘事件, 缩放到桌面坐标后发给 agent
    // (desktop:mouse / desktop:key)。指针坐标换算参考 RustDesk 的
    // canvas→remote 缩放: video 实际渲染尺寸 → 视频原始分辨率。
    _bindInput() {
      if (this._inputBound) return;
      this._inputBound = true;
      const v = this.video;
      const self = this;

      const send = function(type, payload) {
        if (!self.connected) return;
        if (window.shellRemote && window.shellRemote.send) {
          window.shellRemote.send(type, payload);
        }
      };

      // 视频渲染坐标 → 桌面像素坐标（object-fit: contain 的信箱区域剔除）
      this._toDesktopXY = function(e) {
        const vw = v.videoWidth, vh = v.videoHeight;
        if (!vw || !vh) return null;
        const rect = v.getBoundingClientRect();
        const scale = Math.min(rect.width / vw, rect.height / vh);
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
        e.preventDefault();
      };
      this._onPointerUp = function(e) {
        send('desktop:mouse', { type: 'up', button: e.button });
        e.preventDefault();
      };
      this._onWheel = function(e) {
        // 浏览器 deltaY 正=向下。enigo scroll 单位是"格"(1 格 = Windows
        // WHEEL_DELTA=120 / X11 一次 button click)；浏览器一次滚轮的
        // deltaY ~100px(deltaMode==0)或 1~3 行(deltaMode==1)，都映射为
        // 1~3 格。dy 语义=正向下（与 enigo win/x11 一致——之前取了负
        // 号导致方向相反）。
        var unit = 100;
        if (e.deltaMode === 1) unit = 33; // 行模式: 3 行/格
        else if (e.deltaMode === 2) unit = 100; // 页模式按 1 格处理
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
      this._onKey = function(down) {
        return function(e) {
          // 过滤浏览器自身快捷键（F5 刷新/Ctrl+W 关标签等由浏览器处理）
          send('desktop:key', { code: e.code, down: down });
          if (['F5', 'F12'].indexOf(e.code) < 0) e.preventDefault();
        };
      };

      v.addEventListener('pointermove', this._onPointerMove);
      v.addEventListener('pointerdown', this._onPointerDown);
      v.addEventListener('pointerup', this._onPointerUp);
      v.addEventListener('wheel', this._onWheel, { passive: false });
      v.addEventListener('contextmenu', this._onContextMenu);
      v.addEventListener('keydown', this._onKey(true));
      v.addEventListener('keyup', this._onKey(false));
      // 键盘焦点: video 不是天然可聚焦元素, 点击后手动聚焦才能收 key 事件
      v.tabIndex = 0;
    }

    _unbindInput() {
      if (!this._inputBound) return;
      this._inputBound = false;
      const v = this.video;
      v.removeEventListener('pointermove', this._onPointerMove);
      v.removeEventListener('pointerdown', this._onPointerDown);
      v.removeEventListener('pointerup', this._onPointerUp);
      v.removeEventListener('wheel', this._onWheel);
      v.removeEventListener('contextmenu', this._onContextMenu);
      v.removeEventListener('keydown', this._onKey(true));
      v.removeEventListener('keyup', this._onKey(false));
    }
  };
})();