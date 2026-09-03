// desktop.js - MSE fMP4 player for the shell-remote desktop view.
//
// The agent streams a fragmented MP4 (init segment + per-frame fragments)
// over `GET /agent/desktop/stream` (token via Authorization header). We feed
// those bytes straight into a SourceBuffer. The relay replays the cached init
// segment to every new stream request, so joining mid-stream works.

(function() {
  'use strict';

  window.DesktopView = class {
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
    }

    // relay 预建桌面流的时机略晚于 desktop:started 广播; 遇到 404 时短暂
    // 重试(最多 5 次)等待流就绪, 返回 true 表示已接管重试。
    _retryDesktopStream() {
      if (this._streamRetries >= 5) return false;
      this._streamRetries += 1;
      const self = this;
      this.setStatus('等待桌面流就绪… (' + this._streamRetries + ')', false);
      setTimeout(function() { self.connect(); }, 700);
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
        });
        this.sourceBuffer.addEventListener('error', function() {
          self.setStatus('MSE 解码错误: 浏览器无法解码该视频流 (codec=' + codec + ')', true);
        });
      } catch (e) {
        this.setStatus('播放器初始化失败: ' + e.message, true);
        this.disconnect();
        return;
      }
      this._drain();
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
        self.setStatus('桌面已连接', false);
        const reader = resp.body.getReader();
        self.reader = reader;

        function pump() {
          if (controller.signal.aborted) return Promise.resolve();
          return reader.read().then(function(result) {
            if (result.done) {
              self.setStatus('桌面流已结束', true);
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
  };
})();