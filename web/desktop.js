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

    connect() {
      this.disconnect();
      if (!this._codecSupported(this.codec)) {
        // 部分浏览器对高 profile 的字符串更宽松；再试一个通用串。
        if (!this._codecSupported('avc1.64001E')) {
          this.setStatus('当前浏览器不支持 H.264 MSE 播放', true);
          return;
        }
        this.codec = 'avc1.64001E';
      }
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
        self._createSourceBuffer(self.codec);
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
    // OpenH264 输出的 SPS profile/level 可能与本文件预设不同
    // (avc1.42E01E 是保守默认)；解析到真实值后按需重建 SourceBuffer，
    // 避免严格 MSE (如 Safari) 因 codec 串与实际流不匹配而拒播。
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

    _startFetch() {
      const token = sessionStorage.getItem('shell-remote-token');
      if (!token) {
        this.setStatus('缺少会话密钥', true);
        return;
      }
      const controller = new AbortController();
      this.controller = controller;

      this.pendingChunks = [];
      this.codecResolved = false;

      const self = this;
      fetch('/agent/desktop/stream', {
        headers: { 'Authorization': 'Bearer ' + token }
      }).then(function(resp) {
        if (controller.signal.aborted) return null;
        if (!resp.ok || !resp.body) {
          self.setStatus('桌面流不可用 (HTTP ' + resp.status + ')，请重试', true);
          self.disconnect();
          return null;
        }
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
              self.pendingChunks.push(buf);
              self._drain();
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

      // 首个 chunk 是 init 段；先在 append 前解析真实 codec，按需换 SourceBuffer
      if (!this.codecResolved && this.pendingChunks.length > 0) {
        const actual = this._codecFromInit(this.pendingChunks[0]);
        if (actual) {
          this.codecResolved = true;
          if (actual !== this.codec && this._codecSupported(actual)) {
            this.codec = actual;
            try {
              this.mediaSource.removeSourceBuffer(this.sourceBuffer);
            } catch (e) { /* ignore */ }
            this._createSourceBuffer(actual);
            return;
          }
        }
      }

      const next = this.pendingChunks.shift();
      try {
        sb.appendBuffer(next);
      } catch (e) {
        console.warn('appendBuffer failed:', e);
      }
    }

    disconnect() {
      if (this.controller) { this.controller.abort(); this.controller = null; }
      if (this.reader) { this.reader.cancel().catch(function() {}); this.reader = null; }
      this.connected = false;
      this.queue = [];
      this.pendingChunks = [];
      this.codecResolved = false;
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