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

  var api = {
    DEFAULT_URL: DEFAULT_URL,
    PROXY_PATH: PROXY_PATH,
    TICKET_PATH: TICKET_PATH,
    Rewrite: Rewrite,
    WebView: WebView,
    normalizeInput: normalizeInput
  };

  if (typeof window !== 'undefined') {
    window.WebView = WebView;
    window.WebViewRewrite = Rewrite;
    window.WebViewDEFAULT_URL = DEFAULT_URL;
  }
  // node 单测入口（tools/test_webview.js）；浏览器里 module 未定义，不受影响。
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
})();
