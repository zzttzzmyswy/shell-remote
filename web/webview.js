// webview.js — 会话页「网页」视图：经 relay/agent 隧道浏览远端设备网络中的页面（MYS-2383-C）。
//
// 数据路径（阶段1/阶段2 已合并的接口，本文件只消费）：
//   浏览器 --POST /agent/web/ticket--（X-SR-Token + 可选 X-Auth）→ {ticket, expires_in}
//   浏览器 --GET  /agent/web/proxy?ticket=…&url=…---------------→ 远端响应 + X-SR-Final-URL
//
// relay **不重写** HTML/CSS（阶段2 的实现约定），所以相对资源必须由这里处理：
//   1. 用同一个代理 fetch 主文档，把响应头 `X-SR-Final-URL`（重定向后的最终地址）
//      和文档内的 `<base href>` 一起当作相对 URL 的 base；
//   2. 把 src/href/poster/srcset 与 CSS `url()` 改写成**同源代理 URL**（只带
//      ticket，绝不带会话 token / 服务器口令），`<script>` 一律删除；
//   3. 外部样式表取回后就地内联成 `<style>` —— 否则浏览器直接经代理取到的 CSS 里，
//      相对 `url()` 会以代理 URL 为 base 解析，图片必然 404；
//   4. 结果塞进 `iframe[srcdoc]`，sandbox 只用 `allow-same-origin`（**不给
//      allow-scripts**）：远端 JS 不执行，父页面仍能读 iframe DOM 来拦截站内链接。
//
// 已知限制（不承诺全站兼容）：复杂 SPA（依赖远端 JS 拼 URL）、WebSocket、
// 文件下载、非 GET 表单、`<script>` 驱动的动态内容都不支持。
(function () {
  'use strict';

  var DEFAULT_URL = 'http://127.0.0.1/';
  var PROXY_PATH = '/agent/web/proxy';
  var TICKET_PATH = '/agent/web/ticket';
  // v2（MYS-2383-G）：映射入口前缀与建映射接口，与 src/relay/web_map.rs 对齐。
  var MAP_PATH = '/agent/web/map';
  var MAP_PREFIX = '/agent/web/m';
  // ticket 提前刷新余量：子资源是浏览器自己发的，我们只能保证「渲染时」的
  // ticket 还有足够寿命。
  var TICKET_MARGIN_MS = 15000;

  // 这些 scheme 绝不改写成代理：javascript:/data:/vbscript: 会执行脚本或内联数据，
  // 其余非 HTTP(S) scheme 代理侧也会 400。保持原样交给浏览器（iframe 内无脚本，
  // 因此 javascript: 链接不会执行）。
  var DENY_SCHEME =
    /^(javascript|data|vbscript|blob|filesystem|about|mailto|tel|sms|magnet|ftp|file):/i;

  // ────────────────────────── 纯函数：URL / HTML / CSS 重写 ──────────────────────────
  // 刻意写成字符串级重写（不依赖 DOMParser），这样 `tools/test_webview.js` 能在
  // node 里直接跑同一套代码，不需要浏览器环境。
  var Rewrite = (function () {
    // 需要改写的属性；href 只在链接类标签上改写（<form action> 是非 GET 表单，
    // 属已知限制，不代理）。
    var URL_ATTRS = {
      src: true, href: true, poster: true, 'xlink:href': true,
      data: true, background: true
    };
    var SRCSET_ATTRS = { srcset: true, imagesrcset: true };
    var HREF_TAGS = { a: true, area: true, link: true };

    function attrValue(tag, name) {
      var re = new RegExp('\\b' + name + '\\s*=\\s*(?:"([^"]*)"|\'([^\']*)\'|([^\\s>]+))', 'i');
      var m = re.exec(tag);
      if (!m) return null;
      return m[1] != null ? m[1] : (m[2] != null ? m[2] : m[3]);
    }

    /// 把引用解析成绝对的 http/https URL；不能/不该代理的返回 null。
    function resolveUrl(raw, base) {
      var s = raw == null ? '' : String(raw).trim();
      if (!s) return null;
      if (s.charAt(0) === '#') return null;          // 页内锚点：交给浏览器
      if (DENY_SCHEME.test(s)) return null;
      var u;
      try {
        u = new URL(s, base || DEFAULT_URL);
      } catch (e) {
        return null;
      }
      if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
      return u.href;
    }

    /// 同源代理 URL。**只放 ticket 与目标地址**：会话 token / 服务器口令永不出现在 URL。
    function proxyUrl(absUrl, ticket, origin) {
      return (origin || '') + PROXY_PATH +
        '?ticket=' + encodeURIComponent(ticket || '') +
        '&url=' + encodeURIComponent(absUrl);
    }

    /// 从已重写的 href 里取回目标地址（iframe 内链接点击拦截用）。
    function extractTarget(href) {
      if (!href) return null;
      var s = String(href);
      var i = s.indexOf('url=');
      if (i < 0) return null;
      try {
        return new URL(s, 'http://placeholder.invalid').searchParams.get('url');
      } catch (e) {
        return null;
      }
    }

    /// srcset → [{url, descriptor}]。整段是 data: 时返回空数组（原样保留）。
    function parseSrcset(value) {
      var s = String(value == null ? '' : value).trim();
      if (!s || /^data:/i.test(s)) return [];
      var out = [];
      var re = /([^\s,]+)(\s+[^,]+)?/g;
      var m;
      while ((m = re.exec(s)) !== null) {
        out.push({ url: m[1], descriptor: (m[2] || '').trim() });
      }
      return out;
    }

    /// 返回改写后的 srcset；没有任何可改写项时返回 null（调用方保留原值）。
    function rewriteSrcset(value, base, ticket, origin) {
      if (/^\s*data:/i.test(String(value == null ? '' : value))) return null;
      var items = parseSrcset(value);
      if (!items.length) return null;
      var changed = false;
      var parts = items.map(function (it) {
        var abs = resolveUrl(it.url, base);
        if (!abs) return it.descriptor ? it.url + ' ' + it.descriptor : it.url;
        changed = true;
        var p = proxyUrl(abs, ticket, origin);
        return it.descriptor ? p + ' ' + it.descriptor : p;
      });
      if (!changed) return null;
      return parts.join(', ');
    }

    /// CSS 里的 `url(...)` 与 `@import "…"`：按 CSS 文件自身地址解析后改写。
    /// 同一代理能同时服务这两类子资源，只是外层 CSS 内部的 url() 仍会相对
    /// 代理 URL 解析——所以外部样式表由 WebView 取回后内联（见 _buildDocument）。
    function rewriteCss(css, base, ticket, origin) {
      var s = String(css == null ? '' : css);
      s = s.replace(/url\(\s*(['"]?)([\s\S]*?)\1\s*\)/gi, function (whole, quote, ref) {
        var abs = resolveUrl(ref, base);
        if (!abs) return whole;
        return 'url(' + quote + proxyUrl(abs, ticket, origin) + quote + ')';
      });
      s = s.replace(/@import\s+(['"])([^'"]*)\1/gi, function (whole, quote, ref) {
        var abs = resolveUrl(ref, base);
        if (!abs) return whole;
        return '@import ' + quote + proxyUrl(abs, ticket, origin) + quote;
      });
      return s;
    }

    /// 文档的解析基准：`<base href>` 优先（相对 fallback 再解析一次），否则 fallback。
    function documentBase(html, fallback) {
      var baseTag = /<base\b[^>]*>/i.exec(String(html == null ? '' : html));
      if (baseTag) {
        var hv = attrValue(baseTag[0], 'href');
        var abs = hv ? resolveUrl(hv, fallback) : null;
        if (abs) return abs;
      }
      return fallback;
    }

    /// 收集 `<link rel="stylesheet">`（内联用）。
    function findStylesheets(html, base) {
      var out = [];
      var re = /<link\b[^>]*>/gi;
      var m;
      var s = String(html == null ? '' : html);
      while ((m = re.exec(s)) !== null) {
        var tag = m[0];
        var rel = attrValue(tag, 'rel');
        if (!rel || !/\bstylesheet\b/i.test(rel)) continue;
        if (/\bdisabled\b/i.test(tag)) continue;
        var abs = resolveUrl(attrValue(tag, 'href'), base);
        if (!abs) continue;
        out.push({ tag: tag, url: abs });
      }
      return out;
    }

    /// 属性内的 `&` 转义成 `&amp;`：代理 URL 带 `?ticket=…&url=…`，直接放进
    /// HTML 属性虽然多数浏览器也能解析，但规范写法是转义。
    function escapeAttr(v) {
      return String(v).replace(/&/g, '&amp;').replace(/"/g, '&quot;');
    }

    function rewriteAttrs(tag, attrs, base, ticket, origin) {
      return String(attrs).replace(
        /(\s)([a-zA-Z_:][-a-zA-Z0-9_:.]*)(\s*=\s*)("[^"]*"|'[^']*'|[^\s"'>]+)/g,
        function (whole, sp, name, eq, quoted) {
          var attr = String(name).toLowerCase();
          var q = quoted.charAt(0);
          var hasQ = (q === '"' || q === "'");
          var value = hasQ ? quoted.slice(1, -1) : quoted;
          if (attr === 'style') {
            var css = rewriteCss(value, base, ticket, origin);
            if (css === value) return whole;
            return sp + name + eq + q + css + (hasQ ? q : '');
          }
          if (SRCSET_ATTRS[attr]) {
            var ss = rewriteSrcset(value, base, ticket, origin);
            if (ss === null) return whole;
            return sp + name + eq + q + escapeAttr(ss) + (hasQ ? q : '');
          }
          if (!URL_ATTRS[attr]) return whole;
          if (attr === 'href' && !HREF_TAGS[tag]) return whole;
          var abs = resolveUrl(value, base);
          if (!abs) return whole;
          var pu = proxyUrl(abs, ticket, origin);
          return sp + name + eq + q + (hasQ ? escapeAttr(pu) : pu) + (hasQ ? q : '');
        });
    }

    /// HTML 重写主入口。返回 `{html, base}`。
    ///   - 删除 `<script>`（无 allow-scripts 也不会执行，显式删掉避免误导）；
    ///   - 删除远端 CSP / refresh `<meta>`（前者会挡住代理 URL，后者会导航到未重写地址）；
    ///   - 删除 `<base>`（srcdoc 里它指向父页面，留着会让相对资源绕过代理）；
    ///   - `<style>` 与 `style=` 里的 url() 按文档 base 改写。
    function rewriteHtml(html, opts) {
      opts = opts || {};
      var ticket = opts.ticket || '';
      var origin = opts.origin || '';
      var src = String(html == null ? '' : html);
      var base = documentBase(src, opts.base || DEFAULT_URL);

      src = src.replace(/<base\b[^>]*>/gi, '');
      src = src.replace(/<meta\b[^>]*>/gi, function (tag) {
        return /http-equiv\s*=\s*(?:"|')?\s*(content-security-policy|refresh)/i.test(tag) ? '' : tag;
      });
      src = src.replace(/<script\b[^>]*>[\s\S]*?<\/script\s*>/gi, '');
      src = src.replace(/<script\b[^>]*\/?>/gi, '');
      src = src.replace(/(<style\b[^>]*>)([\s\S]*?)(<\/style\s*>)/gi, function (m, open, css, close) {
        // 已经由 WebView 内联（自己带 base 改写过）的样式表不再二次处理，
        // 否则代理 URL 会被当成相对地址再套一层代理。
        if (/data-sr-inline/i.test(open)) return m;
        return open + rewriteCss(css, base, ticket, origin) + close;
      });
      src = src.replace(/<([a-zA-Z][^\s/>]*)((?:[^>"']|"[^"]*"|'[^']*')*)>/g,
        function (whole, name, attrs) {
          var tag = String(name).toLowerCase();
          var out = rewriteAttrs(tag, attrs, base, ticket, origin);
          if (out === attrs) return whole;
          return '<' + name + out + '>';
        });

      return { html: src, base: base };
    }

    return {
      PROXY_PATH: PROXY_PATH,
      resolveUrl: resolveUrl,
      proxyUrl: proxyUrl,
      extractTarget: extractTarget,
      parseSrcset: parseSrcset,
      rewriteSrcset: rewriteSrcset,
      rewriteCss: rewriteCss,
      documentBase: documentBase,
      findStylesheets: findStylesheets,
      rewriteHtml: rewriteHtml
    };
  })();

  // ────────────────────────── 输入规范化 ──────────────────────────

  /// 用户输入的地址 → 绝对 http/https URL（非法返回 null）。
  /// 允许省略 scheme（`192.168.1.10:8080` → `http://192.168.1.10:8080`）：
  /// 地址最终在**远端设备侧**解析，所以这里不做任何可达性判断。
  function normalizeInput(raw) {
    var s = String(raw == null ? '' : raw).trim();
    if (!s) return null;
    if (DENY_SCHEME.test(s)) return null;
    if (!/^https?:\/\//i.test(s)) {
      if (/^https?:/i.test(s)) return null;               // 形如 `http:foo` 的非法写法
      s = 'http://' + s.replace(/^\/+/, '');
    }
    return Rewrite.resolveUrl(s, DEFAULT_URL);
  }

  function cancelledError() {
    var e = new Error('cancelled');
    e.cancelled = true;
    return e;
  }

  function describeError(err) {
    var code = (err && err.code) || '';
    if (err && err.cancelled) return '已取消';
    if (code === 'AUTH') return '鉴权失败：会话密钥或服务器口令无效';
    if (code === 'UNSUPPORTED') return '该设备的 agent 不支持网页映射';
    if (code === 'NETWORK') return '请求失败：网络不稳定或设备已离线';
    if (code === 'TOO_LARGE') return '响应过大，已放弃渲染';
    if (code === 'HTTP_401' || code.indexOf('HTTP_403') === 0) return '代理拒绝访问（凭据已失效）';
    if (code === 'HTTP_404') return '目标地址在设备侧不存在（404）';
    if (code === 'HTTP_503') return '设备 agent 离线，暂不可访问';
    if (/^HTTP_/.test(code)) return '目标页面返回 ' + code.slice(5) + '，无法渲染';
    return (err && err.message) || '加载失败';
  }

  function fail(code, message) {
    var e = new Error(message || code);
    e.code = code;
    return e;
  }

  // ────────────────────────── WebView ──────────────────────────

  /// 网页视图控制器。DOM 元素通过 opts 注入，便于单测用桩件驱动。
  function WebView(opts) {
    opts = opts || {};
    this.frame = opts.frame || null;
    this.statusEl = opts.statusEl || null;
    this.inputEl = opts.inputEl || null;
    this.token = opts.token || '';
    this.getAuthHeaders = opts.getAuthHeaders || function () { return {}; };
    this.fetchImpl = opts.fetch || null;
    this.origin = opts.origin || '';
    this.onStatus = opts.onStatus || null;
    // 上限：一次导航最多内联多少外部样式表、单个资源多大。都只是防呆，
    // 真正的流量边界在 relay/agent 侧（并发 8、单请求 5min）。
    this.maxStylesheets = opts.maxStylesheets || 8;
    this.maxStylesheetBytes = opts.maxStylesheetBytes || 512 * 1024;
    this.maxDocumentBytes = opts.maxDocumentBytes || 8 * 1024 * 1024;

    this.ticket = '';
    this.ticketExpiresAt = 0;
    this.url = '';
    this.generation = 0;
    this.controller = null;
    this.loading = false;
    this.lastError = '';
    this.statusText = '';
    this._boundFrame = false;
  }

  WebView.prototype._fetch = function () {
    if (this.fetchImpl) return this.fetchImpl;
    if (typeof window !== 'undefined' && window.fetch) return window.fetch.bind(window);
    throw fail('NETWORK', 'fetch unavailable');
  };

  WebView.prototype._setStatus = function (text, kind) {
    this.statusText = text || '';
    if (this.statusEl) {
      this.statusEl.textContent = this.statusText;
      this.statusEl.className = 'web-status' + (kind ? ' ' + kind : '');
    }
    if (this.onStatus) {
      try { this.onStatus(this.statusText, kind || ''); } catch (e) {}
    }
  };

  /// 取一张 ticket：内存里有效就复用，过期/强制则重新 POST。
  /// ticket 只存在于内存与代理 URL 的 query 里，绝不写进 sessionStorage。
  WebView.prototype.ensureTicket = function (force) {
    var self = this;
    var now = Date.now();
    if (!force && this.ticket && now < this.ticketExpiresAt - TICKET_MARGIN_MS) {
      return Promise.resolve(this.ticket);
    }
    if (!this.token) return Promise.reject(fail('AUTH', 'missing session token'));
    var headers = { 'Content-Type': 'application/json', 'X-SR-Token': this.token };
    var extra = {};
    try { extra = this.getAuthHeaders() || {}; } catch (e) { extra = {}; }
    for (var k in extra) {
      if (Object.prototype.hasOwnProperty.call(extra, k)) headers[k] = extra[k];
    }
    return Promise.resolve()
      .then(function () {
        return self._fetch()((self.origin || '') + TICKET_PATH, {
          method: 'POST',
          headers: headers,
          body: '{}',
          credentials: 'omit'
        });
      })
      .catch(function (e) {
        if (e && e.code) throw e;
        throw fail('NETWORK', (e && e.message) || 'ticket request failed');
      })
      .then(function (resp) {
        if (resp.status === 401) throw fail('AUTH', 'ticket rejected');
        // 旧 agent：relay 直接 404（不读正文）。
        if (resp.status === 404) throw fail('UNSUPPORTED', 'agent lacks web:proxy-v1');
        if (!resp.ok) throw fail('TICKET_' + resp.status, 'ticket HTTP ' + resp.status);
        return resp.json();
      })
      .then(function (data) {
        var t = data && data.ticket;
        if (!t) throw fail('TICKET_EMPTY', 'empty ticket');
        self.ticket = String(t);
        var ttl = Number(data && data.expires_in);
        if (!(ttl > 0)) ttl = 600;
        self.ticketExpiresAt = Date.now() + ttl * 1000;
        return self.ticket;
      });
  };

  /// 经代理 GET 一个目标地址。ticket 失效（403）时重新签发**一次**再试。
  WebView.prototype._fetchProxied = function (target, signal) {
    var self = this;
    function attempt(ticket, mayRetry) {
      var url = Rewrite.proxyUrl(target, ticket, self.origin);
      return Promise.resolve()
        .then(function () {
          return self._fetch()(url, { method: 'GET', signal: signal, credentials: 'omit' });
        })
        .catch(function (e) {
          if (e && e.code) throw e;
          if (signal && signal.aborted) throw cancelledError();
          throw fail('NETWORK', (e && e.message) || 'proxy request failed');
        })
        .then(function (resp) {
          if (resp.status === 403 && mayRetry) {
            // 过期 / 被撤销（agent 断线会撤销该会话全部 ticket）：换一张再来一次。
            self.ticket = '';
            self.ticketExpiresAt = 0;
            return self.ensureTicket(true).then(function (t) { return attempt(t, false); });
          }
          return resp;
        });
    }
    return this.ensureTicket(false).then(function (t) { return attempt(t, true); });
  };

  WebView.prototype.refresh = function () {
    var target = this.url || (this.inputEl && this.inputEl.value) || DEFAULT_URL;
    return this.navigate(target);
  };

  /// 打开一个地址。旧导航会被 abort，其响应因 generation 不匹配而被丢弃。
  WebView.prototype.navigate = function (raw) {
    var self = this;
    var target = normalizeInput(raw);
    if (!target) {
      this._setStatus('地址无效：请填写远端设备网络中的 http:// 或 https:// 绝对地址', 'error');
      return Promise.resolve(false);
    }
    this.url = target;
    this.lastError = '';
    this.generation += 1;
    var gen = this.generation;
    if (this.controller) {
      try { this.controller.abort(); } catch (e) {}
    }
    this.controller = (typeof AbortController !== 'undefined') ? new AbortController() : null;
    var signal = this.controller ? this.controller.signal : undefined;
    this.loading = true;
    if (this.inputEl) this.inputEl.value = target;
    this._setStatus('正在经隧道加载 ' + target + ' …', 'loading');

    return this._loadDocument(target, gen, signal)
      .then(function (doc) {
        if (gen !== self.generation) return false;
        self._render(doc);
        self.loading = false;
        self._setStatus('已加载 ' + self.url, 'ok');
        return true;
      })
      .catch(function (err) {
        if (gen !== self.generation) return false;
        self.loading = false;
        if (err && err.cancelled) return false;
        var msg = describeError(err);
        self.lastError = msg;
        self._setStatus(msg, 'error');
        self._renderMessage(msg);
        return false;
      });
  };

  /// 取主文档并按内容类型决定是「重写成 srcdoc」还是「直接把代理 URL 交给 iframe」。
  WebView.prototype._loadDocument = function (target, gen, signal) {
    var self = this;
    return this._fetchProxied(target, signal).then(function (resp) {
      if (gen !== self.generation) throw cancelledError();
      if (!resp.ok) throw fail('HTTP_' + resp.status, 'proxy HTTP ' + resp.status);
      var ctype = String((resp.headers && resp.headers.get && resp.headers.get('content-type')) || '')
        .toLowerCase();
      // 重定向后的最终地址：相对资源必须按它解析，否则跨目录/换主机的跳转会 404。
      var finalUrl = (resp.headers && resp.headers.get && resp.headers.get('x-sr-final-url')) || target;
      if (finalUrl && finalUrl !== target) self.url = finalUrl;
      if (ctype.indexOf('text/html') < 0 && ctype.indexOf('application/xhtml') < 0 &&
          ctype.indexOf('text/css') < 0) {
        // 非 HTML（图片/纯文本/PDF…）：让 iframe 自己加载代理 URL，浏览器原生渲染。
        return { kind: 'raw', url: finalUrl };
      }
      return readBody(resp, self.maxDocumentBytes).then(function (buf) {
        var text = decodeUtf8(buf);
        if (ctype.indexOf('text/css') >= 0) {
          return { kind: 'html', html: wrapCss(Rewrite.rewriteCss(text, finalUrl, self.ticket, self.origin)),
                   base: finalUrl };
        }
        return self._buildDocument(text, finalUrl, gen, signal);
      });
    });
  };

  /// 外部样式表内联 → 再整体重写 HTML。
  WebView.prototype._buildDocument = function (html, finalUrl, gen, signal) {
    var self = this;
    var links = Rewrite.findStylesheets(html, finalUrl).slice(0, this.maxStylesheets);
    var chain = Promise.resolve(html);
    links.forEach(function (link) {
      chain = chain.then(function (cur) {
        if (gen !== self.generation) return cur;
        return self._fetchProxied(link.url, signal)
          .then(function (resp) {
            if (!resp.ok) return cur;
            return readBody(resp, self.maxStylesheetBytes).then(function (buf) {
              var sheetBase = (resp.headers && resp.headers.get && resp.headers.get('x-sr-final-url')) ||
                link.url;
              var css = Rewrite.rewriteCss(decodeUtf8(buf), sheetBase, self.ticket, self.origin);
              // CSS 里万一带 `</style`，内联会撕开标签。
              css = css.replace(/<\/style/gi, '');
              var styleTag = '<style data-sr-inline="1">' + css + '</style>';
              return cur.split(link.tag).join(styleTag);
            });
          })
          // 单个样式表失败不影响整页：其余内容照常显示（限制而非错误）。
          .catch(function () { return cur; });
      });
    });
    return chain.then(function (patched) {
      var out = Rewrite.rewriteHtml(patched, {
        base: finalUrl, ticket: self.ticket, origin: self.origin
      });
      return { kind: 'html', html: out.html, base: out.base };
    });
  };

  WebView.prototype._render = function (doc) {
    var frame = this.frame;
    if (!frame) return;
    if (doc.kind === 'raw') {
      setAttr(frame, 'srcdoc', null);
      setAttr(frame, 'src', Rewrite.proxyUrl(doc.url, this.ticket, this.origin));
    } else {
      setAttr(frame, 'src', null);
      setAttr(frame, 'srcdoc', doc.html);
    }
    this._bindFrame();
  };

  /// 渲染一条本地提示页（不经过代理）：加载失败时 iframe 里不再留着上一页。
  WebView.prototype._renderMessage = function (msg) {
    var frame = this.frame;
    if (!frame) return;
    var esc = String(msg).replace(/&/g, '&amp;').replace(/</g, '&lt;');
    var html = '<!DOCTYPE html><html><head><meta charset="utf-8">' +
      '<style>body{margin:0;padding:24px;font:14px/1.6 system-ui,sans-serif;' +
      'color:#9aa6bd;background:#121826}h3{color:#e6ebf5;font-size:15px;margin:0 0 8px}' +
      'p{margin:0}</style></head><body><h3>网页映射</h3><p>' + esc + '</p></body></html>';
    setAttr(frame, 'srcdoc', html);
    this._bindFrame();
  };

  /// iframe 装了 sandbox=allow-same-origin（无 allow-scripts），所以远端脚本不会
  /// 执行，但父页面能读 iframe 文档：用它拦截站内链接点击与非 GET 表单。
  WebView.prototype._bindFrame = function () {
    if (this._boundFrame || !this.frame || !this.frame.addEventListener) return;
    this._boundFrame = true;
    var self = this;
    this.frame.addEventListener('load', function () {
      try {
        self._instrument(self.frame.contentDocument);
      } catch (e) {
        // 跨源理论上不会发生（srcdoc + allow-same-origin）；真跨源就放弃拦截。
      }
    });
  };

  WebView.prototype._instrument = function (doc) {
    if (!doc || doc.__srWebInstrumented) return;
    doc.__srWebInstrumented = true;
    var self = this;
    doc.addEventListener('click', function (ev) {
      var node = ev.target;
      var a = node && node.closest ? node.closest('a[href]') : null;
      if (!a) return;
      var target = Rewrite.extractTarget(a.getAttribute('href'));
      if (!target) return;              // 页内锚点 / 未重写的外链：交给浏览器
      ev.preventDefault();
      self.navigate(target);
    }, true);
    doc.addEventListener('submit', function (ev) {
      // 只支持 GET 浏览：表单提交一律拦下并告知（已知限制）。
      ev.preventDefault();
      self._setStatus('表单提交不经网页代理（仅支持 GET 页面浏览）', 'warn');
    }, true);
  };

  /// 清理：abort 在途请求、丢弃 ticket、清空 iframe。切视图/断线/退出时调用。
  WebView.prototype.reset = function () {
    this.generation += 1;
    if (this.controller) {
      try { this.controller.abort(); } catch (e) {}
      this.controller = null;
    }
    this.ticket = '';
    this.ticketExpiresAt = 0;
    this.url = '';
    this.loading = false;
    this.lastError = '';
    this.statusText = '';
    if (this.statusEl) {
      this.statusEl.textContent = '';
      this.statusEl.className = 'web-status';
    }
    if (this.frame) {
      setAttr(this.frame, 'srcdoc', null);
      setAttr(this.frame, 'src', null);
      if (this.frame.contentDocument) {
        try { this.frame.contentDocument.__srWebInstrumented = false; } catch (e) {}
      }
    }
  };

  // ────────────────────────── 小工具 ──────────────────────────

  function setAttr(el, name, value) {
    try {
      if (value === null || value === undefined) {
        if (el.removeAttribute) el.removeAttribute(name);
        else el[name] = '';
        return;
      }
      if (el.setAttribute) el.setAttribute(name, value);
      else el[name] = value;
    } catch (e) {}
  }

  function readBody(resp, cap) {
    var len = Number((resp.headers && resp.headers.get && resp.headers.get('content-length')) || 0);
    if (cap && len > cap) return Promise.reject(fail('TOO_LARGE', 'response too large'));
    var p = resp.arrayBuffer ? resp.arrayBuffer() : Promise.resolve(new ArrayBuffer(0));
    return p.then(function (buf) {
      if (cap && buf && buf.byteLength > cap) throw fail('TOO_LARGE', 'response too large');
      return buf;
    });
  }

  function decodeUtf8(buf) {
    if (!buf) return '';
    if (typeof buf === 'string') return buf;
    try {
      return new TextDecoder('utf-8').decode(buf);
    } catch (e) {
      // 极老的 WebView 没有 TextDecoder：退化为逐字节 latin1（仅影响非 ASCII）。
      var out = '';
      var bytes = new Uint8Array(buf);
      for (var i = 0; i < bytes.length; i++) out += String.fromCharCode(bytes[i]);
      return out;
    }
  }

  function wrapCss(css) {
    return '<!DOCTYPE html><html><head><meta charset="utf-8">' +
      '<style>body{margin:0;padding:16px;background:#fff;color:#222}' +
      'pre{white-space:pre-wrap;word-break:break-all;font:12px/1.5 monospace}</style>' +
      '</head><body><pre>' + String(css).replace(/&/g, '&amp;').replace(/</g, '&lt;') +
      '</pre></body></html>';
  }

  // ══════════════════════ 网页映射 v2（MYS-2383-G） ══════════════════════
  //
  // v2 不再由本文件重写 HTML：浏览器里的 Service Worker（`/sr-web-sw.js`）把
  // 映射页面的请求转到 relay 反代 `/agent/web/m/<map_id>/…`，映射页面在 iframe
  // 中以**目标站点的原始路径**运行。因此脚本、CSS、Cookie、POST、WebSocket 全部
  // 由浏览器原生处理，本文件只负责建映射、注册 SW、把 iframe 指到入口。
  //
  // 模式判定与降级原因都在 detectMode 里，纯函数、可单测。

  var SW_PATH = '/sr-web-sw.js';
  var SW_SCOPE = '/';
  var MAP_PATH = '/agent/web/map';
  // 给 SW 写 IndexedDB 的时间窗口：写完才回 ack，超时就用默认放行策略继续。
  var SW_ACK_TIMEOUT_MS = 2000;

  /// 模式判定。三档：
  ///   - `v2`：agent 支持 TCP 管道 + rw token + 安全上下文有 SW；
  ///   - `v1`：agent 只支持 `web:proxy-v1`（v0.58 静态模式），或 v2 条件不满足
  ///     但 v1 可用；
  ///   - `none`：两者都不行（旧 agent，入口隐藏）。
  /// 返回 `{mode, reason}`，reason 用于状态区说明降级原因。
  function detectMode(opts) {
    opts = opts || {};
    var caps = Array.isArray(opts.caps) ? opts.caps : [];
    var permission = opts.permission || '';
    var secure = !!opts.isSecureContext;
    var hasSw = !!opts.hasServiceWorker;
    var hasTcp = caps.indexOf('net:tcp-v1') >= 0;
    var hasV1 = caps.indexOf('web:proxy-v1') >= 0;

    if (hasTcp && permission === 'rw' && secure && hasSw) {
      return { mode: 'v2', reason: '' };
    }
    if (!hasV1) return { mode: 'none', reason: '' };
    // v1 可用：说明为什么没走 v2（只读 token / 非安全上下文 / agent 版本旧）。
    if (!hasTcp) return { mode: 'v1', reason: 'agent-version' };
    if (permission !== 'rw') return { mode: 'v1', reason: 'read-only' };
    if (!secure) return { mode: 'v1', reason: 'insecure-context' };
    return { mode: 'v1', reason: 'no-service-worker' };
  }

  var MODE_REASON_TEXT = {
    'agent-version': '当前设备的 agent 不支持完整映射（需要新版 agent），已回退静态模式',
    'read-only': '只读 token 仅支持静态模式（完整映射可修改设备配置）',
    'insecure-context': '当前页面不是安全上下文（需要受信证书的 https 或 localhost），已回退静态模式',
    'no-service-worker': '当前浏览器不支持 Service Worker，已回退静态模式'
  };

  /// v2 控制器。
  ///
  /// 生命周期：ensureServiceWorker() → open(url) → iframe 里由 SW 接管。
  /// 打开失败按错误码回退到 v1（由调用方决定是否回退）。
  function WebMapV2(opts) {
    opts = opts || {};
    this.frame = opts.frame || null;
    this.statusEl = opts.statusEl || null;
    this.inputEl = opts.inputEl || null;
    this.token = opts.token || '';
    this.origin = opts.origin || '';
    this.getAuthHeaders = opts.getAuthHeaders || function () { return {}; };
    this.fetchImpl = opts.fetch || null;
    // 单测可注入：默认用浏览器的 navigator.serviceWorker。
    this.swContainer = opts.swContainer || null;
    this.onStatus = opts.onStatus || null;

    this.registration = null;
    this.ready = false;
    this.mapId = '';
    this.targetOrigin = '';
    this.url = '';            // 目标站点的原始地址（地址栏显示的形态）
    this.lastError = '';
    this.loading = false;
    this.generation = 0;
    this._ack = null;
    this._onSwMessage = null;
  }

  WebMapV2.prototype._sw = function () {
    if (this.swContainer) return this.swContainer;
    if (typeof navigator !== 'undefined' && navigator.serviceWorker) return navigator.serviceWorker;
    return null;
  };

  WebMapV2.prototype._fetch = function () {
    if (this.fetchImpl) return this.fetchImpl;
    if (typeof window !== 'undefined' && window.fetch) return window.fetch.bind(window);
    throw fail('NETWORK', 'fetch unavailable');
  };

  WebMapV2.prototype._setStatus = function (text, kind) {
    if (this.statusEl) {
      this.statusEl.textContent = text || '';
      this.statusEl.className = 'web-status' + (kind ? ' ' + kind : '');
    }
    if (this.onStatus) {
      try { this.onStatus(text || '', kind || ''); } catch (e) {}
    }
  };

  /// 注册（或复用）我们的 SW 并等它 activate。scope `/` 覆盖整个源，但 fetch
  /// 分流的放行逻辑保证 shell-remote 自身的请求零影响（见 sr-web-sw.js）。
  WebMapV2.prototype.ensureServiceWorker = function () {
    var self = this;
    if (this.registration && this.ready) return Promise.resolve(this.registration);
    var sw = this._sw();
    if (!sw) return Promise.reject(fail('NO_SW', 'service worker unavailable'));
    return Promise.resolve()
      .then(function () {
        return sw.register(SW_PATH, { scope: SW_SCOPE, updateViaCache: 'none' });
      })
      .then(function (reg) {
        self.registration = reg;
        self._listen();
        // `ready` 只在该 scope 已有 active worker 时 resolve。
        return sw.ready.then(function (r) { return r || reg; });
      })
      .then(function (reg) {
        if (reg) self.registration = reg;
        self.ready = true;
        return self.registration;
      })
      .catch(function (e) {
        self.ready = false;
        throw fail('NO_SW', (e && e.message) || 'service worker registration failed');
      });
  };

  /// 监听 SW 的 ack。只保留一个监听器，避免多次打开后回调堆积。
  WebMapV2.prototype._listen = function () {
    var self = this;
    var sw = this._sw();
    if (!sw || this._onSwMessage) return;
    this._onSwMessage = function (ev) {
      var data = (ev && ev.data) || {};
      if (data.type !== 'sr-web-map-ok') return;
      if (self._ack && self._ack.mapId === data.mapId) {
        var ack = self._ack;
        self._ack = null;
        ack.resolve(data);
      }
    };
    try { sw.addEventListener('message', this._onSwMessage); } catch (e) {}
  };

  /// 把映射登记到 SW，最多等 2s。超时不算失败：SW 的 lastActive 兜底策略
  /// 仍然能把 iframe 的导航归到这个映射，只是多一次整页跳转。
  WebMapV2.prototype._registerMap = function (mapId, targetOrigin) {
    var self = this;
    var sw = this._sw();
    var worker = this.registration && (this.registration.active || this.registration.waiting ||
      this.registration.installing);
    if (!sw || !worker || typeof worker.postMessage !== 'function') return Promise.resolve(false);
    return new Promise(function (resolve) {
      var timer = null;
      var done = function (v) {
        if (timer) clearTimeout(timer);
        timer = null;
        if (self._ack && self._ack.mapId === mapId) self._ack = null;
        resolve(v);
      };
      self._ack = { mapId: mapId, resolve: function () { done(true); } };
      timer = setTimeout(function () { done(false); }, SW_ACK_TIMEOUT_MS);
      try {
        worker.postMessage({ type: 'sr-web-map', mapId: mapId, targetOrigin: targetOrigin });
      } catch (e) {
        done(false);
      }
    });
  };

  /// 切回静态模式 / 断线时通知 SW 丢掉 client 归属。
  WebMapV2.prototype.clearSwState = function () {
    var sw = this._sw();
    var worker = this.registration && this.registration.active;
    if (!sw || !worker || typeof worker.postMessage !== 'function') return;
    try { worker.postMessage({ type: 'sr-web-clear' }); } catch (e) {}
  };

  /// 建映射（同一会话同一目标复用）。错误码：
  ///   403 → 只读 token；404/405 → 旧 relay 或旧 agent；503 → agent 离线。
  WebMapV2.prototype.createMap = function (target) {
    var self = this;
    var headers = { 'Content-Type': 'application/json', 'X-SR-Token': this.token };
    var extra = {};
    try { extra = this.getAuthHeaders() || {}; } catch (e) { extra = {}; }
    for (var k in extra) {
      if (Object.prototype.hasOwnProperty.call(extra, k)) headers[k] = extra[k];
    }
    return Promise.resolve()
      .then(function () {
        return self._fetch()((self.origin || '') + MAP_PATH, {
          method: 'POST',
          headers: headers,
          body: JSON.stringify({ url: target }),
          credentials: 'omit'
        });
      })
      .catch(function (e) {
        if (e && e.code) throw e;
        throw fail('NETWORK', (e && e.message) || 'map request failed');
      })
      .then(function (resp) {
        if (resp.status === 200 || resp.ok) return resp.json();
        if (resp.status === 401) throw fail('AUTH', 'map rejected');
        if (resp.status === 403) throw fail('V2_READONLY', 'read-only token');
        if (resp.status === 404 || resp.status === 405) {
          throw fail('V2_UNSUPPORTED', 'web mapping v2 unavailable');
        }
        if (resp.status === 503) throw fail('V2_OFFLINE', 'agent offline');
        if (resp.status === 400) throw fail('V2_BAD_URL', 'target url rejected');
        throw fail('V2_HTTP_' + resp.status, 'map HTTP ' + resp.status);
      })
      .then(function (data) {
        var mapId = data && data.map_id;
        if (!mapId) throw fail('V2_EMPTY', 'empty map_id');
        return {
          mapId: String(mapId),
          base: String(data.base || (MAP_PREFIX + '/' + mapId + '/')),
          targetOrigin: String(data.target_origin || '')
        };
      });
  };

  /// 打开一个目标地址：建映射 → 通知 SW → iframe 指到入口。
  ///
  /// iframe.src 用的是**带前缀的入口**：SW 会把它 302 到目标站原始路径，之后
  /// 页面里的相对资源、history 路由、整页跳转都由 SW 在后台改写。
  WebMapV2.prototype.open = function (raw) {
    var self = this;
    var target = normalizeInput(raw);
    if (!target) {
      this._setStatus('地址无效：请填写远端设备网络中的 http:// 或 https:// 绝对地址', 'error');
      return Promise.resolve({ ok: false, code: 'V2_BAD_URL' });
    }
    this.generation += 1;
    var gen = this.generation;
    this.loading = true;
    this.lastError = '';
    this._setStatus('正在建立映射 ' + target + ' …', 'loading');

    return this.ensureServiceWorker()
      .then(function () { return self.createMap(target); })
      .then(function (map) {
        if (gen !== self.generation) return { ok: false, code: 'CANCELLED' };
        self.mapId = map.mapId;
        self.targetOrigin = map.targetOrigin;
        return self._registerMap(map.mapId, map.targetOrigin).then(function (acked) {
          if (gen !== self.generation) return { ok: false, code: 'CANCELLED' };
          self.url = target;
          if (self.inputEl) self.inputEl.value = target;
          self._setFrameSrc(entryUrl(map.base, target));
          self._setStatus(acked
            ? '映射已就绪，正在加载 ' + target + ' …'
            : '映射已就绪（SW 未应答，使用兜底归属），正在加载 ' + target + ' …', 'loading');
          return { ok: true, mapId: map.mapId, targetOrigin: map.targetOrigin };
        });
      })
      .catch(function (err) {
        if (gen !== self.generation) return { ok: false, code: 'CANCELLED' };
        self.loading = false;
        var code = (err && err.code) || 'NETWORK';
        self.lastError = code;
        self._setStatus(describeMapError(code), 'error');
        return { ok: false, code: code };
      });
  };

  WebMapV2.prototype._setFrameSrc = function (src) {
    var frame = this.frame;
    if (!frame) return;
    // v2：不加 sandbox（需要执行脚本、提交表单、使用同源存储），也不加
    // no-referrer（目标站的防盗链/防 CSRF 往往依赖 Referer）。
    setAttr(frame, 'sandbox', null);
    setAttr(frame, 'referrerpolicy', null);
    setAttr(frame, 'srcdoc', null);
    setAttr(frame, 'src', src);
  };

  /// iframe load：地址栏同步成目标站原始地址（同源可读）。
  WebMapV2.prototype.onFrameLoad = function () {
    var frame = this.frame;
    if (!frame) return null;
    var loc = null;
    try {
      var win = frame.contentWindow;
      loc = win ? win.location : null;
      if (loc && loc.href === 'about:blank') return null;
      var p = loc.pathname || '/';
      var s = loc.search || '';
      if (p.indexOf(MAP_PREFIX + '/') === 0) {
        // 还在入口（SW 未接管）：不覆盖地址栏，等下一次 load。
        return null;
      }
      this.loading = false;
      var shown = (this.targetOrigin || '') + p + s;
      this.url = shown;
      if (this.inputEl) this.inputEl.value = shown;
      this._setStatus('已加载 ' + shown, 'ok');
      return shown;
    } catch (e) {
      // 跨源（目标站自己跳到别的源）：无法读取，保持上一次的地址。
      this.loading = false;
      return null;
    }
  };

  /// 「刷新」= iframe 内部真实刷新（保留目标站的当前路由与 POST 结果）。
  WebMapV2.prototype.reload = function () {
    var frame = this.frame;
    try {
      if (frame && frame.contentWindow) {
        frame.contentWindow.location.reload();
        return true;
      }
    } catch (e) {}
    if (this.url) return this.open(this.url).then(function (r) { return r.ok; });
    return Promise.resolve(false);
  };

  WebMapV2.prototype.reset = function () {
    this.generation += 1;
    this.mapId = '';
    this.targetOrigin = '';
    this.url = '';
    this.loading = false;
    this.lastError = '';
    this._ack = null;
    if (this.statusEl) {
      this.statusEl.textContent = '';
      this.statusEl.className = 'web-status';
    }
    if (this.frame) {
      setAttr(this.frame, 'srcdoc', null);
      setAttr(this.frame, 'src', 'about:blank');
    }
  };

  /// 入口路径：`base` 已含尾斜杠，拼去掉首斜杠的 pathname 与 query。
  function entryUrl(base, target) {
    var u = null;
    try { u = new URL(target); } catch (e) { return base; }
    return base + u.pathname.replace(/^\/+/, '') + u.search;
  }

  function describeMapError(code) {
    if (code === 'V2_READONLY') return MODE_REASON_TEXT['read-only'];
    if (code === 'V2_UNSUPPORTED') return '当前设备不支持网页映射 v2，已回退静态模式';
    if (code === 'V2_OFFLINE') return '设备 agent 离线，暂不可访问';
    if (code === 'V2_BAD_URL') return '地址无效（需要 http:// 或 https:// 绝对地址）';
    if (code === 'AUTH') return '鉴权失败：会话密钥或服务器口令无效';
    if (code === 'NO_SW') return '当前环境无法注册 Service Worker，已回退静态模式';
    if (code === 'NETWORK') return '请求失败：网络不稳定或设备已离线';
    return '加载失败（' + code + '）';
  }

  var api = {
    DEFAULT_URL: DEFAULT_URL,
    PROXY_PATH: PROXY_PATH,
    TICKET_PATH: TICKET_PATH,
    MAP_PATH: MAP_PATH,
    MAP_PREFIX: MAP_PREFIX,
    SW_PATH: SW_PATH,
    Rewrite: Rewrite,
    WebView: WebView,
    WebMapV2: WebMapV2,
    detectMode: detectMode,
    describeMapError: describeMapError,
    entryUrl: entryUrl,
    MODE_REASON_TEXT: MODE_REASON_TEXT,
    normalizeInput: normalizeInput
  };

  if (typeof window !== 'undefined') {
    window.WebView = WebView;
    window.WebMapV2 = WebMapV2;
    window.WebViewDetectMode = detectMode;
    window.WebViewRewrite = Rewrite;
    window.WebViewDEFAULT_URL = DEFAULT_URL;
    window.WebViewModeReasons = MODE_REASON_TEXT;
  }
  // node 单测入口（tools/test_webview.js）；浏览器里 module 未定义，不受影响。
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
})();
