// web/sr-web-sw.js — 网页映射 v2 的 Service Worker（MYS-2383-G）。
//
// 作用范围 `/`，但**只接管映射页面的请求**：shell-remote 自身的页面、SSE、
// 上传等请求必须在判定为"不属于映射"时直接 return（不调用 respondWith），
// 让浏览器按原样处理。判定顺序见 onFetch。
//
// 映射页面在 iframe 中以目标站点的**原始路径**运行（地址栏/`location` 里没有
// `/agent/web/m/<map_id>` 前缀），因此根路径资源、history 路由、`<a href="/x">`
// 整页跳转都按目标站点的写法工作。到 relay 反代的转换全部在下面完成：
//
//   1. 首次进入（会话页把 iframe.src 设成 `/agent/web/m/<id>/path`）→ 登记
//      pendingNav[去掉前缀的绝对 URL] 后 302 到该 URL；浏览器重新发起一次
//      iframe 导航，这次走第 2 步。
//   2. iframe 导航：按 pendingNav → clientId/replacesClientId → lastActive 的
//      顺序确定 map_id，向 `/agent/web/m/<id><path>` 发 fetch。3xx 的
//      opaqueredirect 原样交回浏览器（浏览器跟随 relay 已改写过的 Location，
//      再次进入第 1 步）。
//   3. 子资源 / fetch / WebSocket 之外的 XHR：按 clientId 找到 map_id 后改写
//      同源或目标主机的 URL。
//
// SW 随时可能被浏览器回收，因此 maps / clients / pendingNav / lastActive 全部
// 写 IndexedDB（内存里另有一份缓存）。
//
// 本文件同时导出纯函数（`module.exports`）供 `tools/test_sr_web.js` 在 node
// 里直接测试——URL 改写与 `<head>` 字节注入都不需要浏览器环境。
'use strict';

(function () {
  // ── 常量 ──────────────────────────────────────────────────────────
  var PREFIX = '/agent/web/m';
  // 这两个路径永远放行：一个是 SW 自己，一个是注入到映射页面的 shim。
  var RESERVED = { '/sr-web-sw.js': true, '/sr-web-shim.js': true };
  var PENDING_TTL_MS = 10000;              // 首次进入的 302 往返窗口
  var LAST_ACTIVE_TTL_MS = 30 * 60 * 1000; // lastActive 归属的有效期
  var ENTRY_TTL_MS = 24 * 60 * 60 * 1000;  // 条目总保质期
  var HEAD_WINDOW = 64 * 1024;             // 只在文档前 64 KiB 内找 <head>
  var CLEAN_INTERVAL_MS = 60 * 1000;
  var DB_NAME = 'sr-web';
  var DB_VERSION = 1;
  var SHIM_PATH = '/sr-web-shim.js';

  // ══════════════════════ 纯函数：URL 路由 ══════════════════════════

  function isReservedPath(pathname) {
    return RESERVED[pathname] === true;
  }

  /// `/agent/web/m/<id>/…` → `<id>`；不带前缀返回 null。
  function mapIdFromPath(pathname) {
    var s = String(pathname == null ? '' : pathname);
    if (s.indexOf(PREFIX + '/') !== 0) return null;
    var rest = s.slice(PREFIX.length + 1);
    if (!rest) return null;
    var slash = rest.indexOf('/');
    var id = slash < 0 ? rest : rest.slice(0, slash);
    return id || null;
  }

  function mapPrefix(mapId) {
    return PREFIX + '/' + mapId;
  }

  /// 去掉映射前缀。`/agent/web/m/<id>` → `/`，`/agent/web/m/<id>/a` → `/a`。
  function stripPrefix(pathname, mapId) {
    var p = mapPrefix(mapId);
    var s = String(pathname == null ? '' : pathname);
    if (s === p) return '/';
    if (s.indexOf(p + '/') === 0) return s.slice(p.length);
    return s;
  }

  function hasPrefix(pathname, mapId) {
    var p = mapPrefix(mapId);
    var s = String(pathname == null ? '' : pathname);
    return s === p || s.indexOf(p + '/') === 0 || s.indexOf(p + '?') === 0;
  }

  function tryUrl(raw, base) {
    try {
      return base === undefined ? new URL(raw) : new URL(raw, base);
    } catch (e) {
      return null;
    }
  }

  /// 非导航请求的去向：`'pass'` 放行、`'as-is'` 已经是反代地址、`'prefix'` 加前缀。
  ///
  ///   - 保留路径 → pass；
  ///   - 同源（relay 本站）→ 未带前缀就加前缀；
  ///   - host 等于 targetOrigin 的 host（兼容 http→https 自动升级）→ 同上；
  ///   - 其他跨源 → pass（他源 CDN 由浏览器直连）。
  function decideNonNav(opts) {
    var u = tryUrl(opts.url);
    if (!u) return 'pass';
    if (isReservedPath(u.pathname)) return 'pass';
    if (hasPrefix(u.pathname, opts.mapId)) return 'as-is';
    if (u.origin === opts.origin) return 'prefix';
    var t = tryUrl(opts.targetOrigin);
    if (t && u.host === t.host) return 'prefix';
    return 'pass';
  }

  /// 加前缀后的反代 URL（保留 query，主机固定为 SW 自己的源）。
  function prefixUrl(url, mapId, origin) {
    var u = tryUrl(url);
    if (!u) return null;
    var base = origin || (typeof self !== 'undefined' ? self.location.origin : '');
    return base + mapPrefix(mapId) + u.pathname + u.search;
  }

  /// 首次进入时的 302 目标：去掉前缀的同源绝对 URL（保留 query）。
  function stripNavTarget(url, mapId, origin) {
    var u = tryUrl(url);
    if (!u) return null;
    var base = origin || (typeof self !== 'undefined' ? self.location.origin : '');
    return base + stripPrefix(u.pathname, mapId) + u.search;
  }

  // ══════════════════════ 纯函数：<head> 字节注入 ══════════════════════

  function toBytes(s) {
    if (typeof s === 'string') {
      var out = new Uint8Array(s.length);
      for (var i = 0; i < s.length; i++) {
        var c = s.charCodeAt(i);
        // 注入串只可能是 ASCII（URL + 路径）；非 ASCII 一律按 UTF-8 兜底。
        if (c < 0x80) {
          out[i] = c;
        } else {
          return utf8Bytes(s);
        }
      }
      return out;
    }
    if (s instanceof Uint8Array) return s;
    if (s && s.buffer) return new Uint8Array(s.buffer, s.byteOffset || 0, s.byteLength);
    return new Uint8Array(0);
  }

  function utf8Bytes(s) {
    if (typeof TextEncoder !== 'undefined') return new TextEncoder().encode(s);
    var out = [];
    for (var i = 0; i < s.length; i++) {
      var c = s.charCodeAt(i);
      if (c < 0x80) out.push(c);
      else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f));
      else out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f));
    }
    return new Uint8Array(out);
  }

  function lower(b) {
    return (b >= 0x41 && b <= 0x5a) ? b + 0x20 : b;
  }

  /// 找到第一个 `<head …>` 的 `>` 之后的字节偏移；找不到返回 -1。
  ///
  /// 全部按字节比较，**绝不先解码**：内网设备常见 GBK 页面，先 TextDecoder
  /// 解码再编码会把中文损坏。`<header>` 这类同前缀标签不算（后一个字节必须
  /// 是分隔符）。
  function findHeadInsertIndex(bytes, limit) {
    var n = bytes.length;
    var end = limit == null ? n : Math.min(n, limit);
    for (var i = 0; i + 5 <= end; i++) {
      if (bytes[i] !== 0x3c) continue;                             // '<'
      if (lower(bytes[i + 1]) !== 0x68) continue;                  // h
      if (lower(bytes[i + 2]) !== 0x65) continue;                  // e
      if (lower(bytes[i + 3]) !== 0x61) continue;                  // a
      if (lower(bytes[i + 4]) !== 0x64) continue;                  // d
      var c = bytes[i + 5];
      if (c !== 0x3e && c !== 0x2f && c !== 0x20 && c !== 0x09 &&
          c !== 0x0a && c !== 0x0d) {
        continue;
      }
      for (var k = i + 5; k < n; k++) {
        if (bytes[k] === 0x3e) return k + 1;                        // '>'
      }
      return -1;   // 标签本身还没接收到 '>'：当作"暂时找不到"
    }
    return -1;
  }

  /// 整段缓冲版本：把 `insert` 插到第一个 `<head>` 的 `>` 之后；没有 `<head>`
  /// 就插在最前面。原始字节顺序保持不变（GBK 页面不受影响）。
  function injectHtml(bytes, insert) {
    var src = toBytes(bytes);
    var ins = toBytes(insert);
    var idx = findHeadInsertIndex(src, src.length);
    var at = idx < 0 ? 0 : idx;
    var out = new Uint8Array(src.length + ins.length);
    out.set(src.subarray(0, at), 0);
    out.set(ins, at);
    out.set(src.subarray(at), at + ins.length);
    return out;
  }

  /// 流式版本：只在决定插入点之前缓冲（最多 HEAD_WINDOW 字节），之后原样透传。
  function createInjector(insert, windowSize) {
    var ins = toBytes(insert);
    var win = windowSize || HEAD_WINDOW;
    var pending = [];
    var pendingLen = 0;
    var done = false;

    function merge() {
      var out = new Uint8Array(pendingLen);
      var off = 0;
      for (var i = 0; i < pending.length; i++) {
        out.set(pending[i], off);
        off += pending[i].length;
      }
      pending = [];
      pendingLen = 0;
      return out;
    }

    function emit(buf, ctrl) {
      var idx = findHeadInsertIndex(buf, buf.length);
      var at = idx < 0 ? 0 : idx;
      if (at > 0) ctrl.enqueue(buf.subarray(0, at));
      ctrl.enqueue(ins);
      if (at < buf.length) ctrl.enqueue(buf.subarray(at));
    }

    return new TransformStream({
      transform: function (chunk, ctrl) {
        if (done) { ctrl.enqueue(chunk); return; }
        var data = toBytes(chunk);
        pending.push(data);
        pendingLen += data.length;
        var buf = merge();
        var idx = findHeadInsertIndex(buf, buf.length);
        // head 还没出现、且还没到窗口上限：继续攒。
        if (idx < 0 && buf.length < win) {
          pending.push(buf);
          pendingLen = buf.length;
          return;
        }
        done = true;
        emit(buf, ctrl);
      },
      flush: function (ctrl) {
        if (done) return;
        done = true;
        emit(merge(), ctrl);
      }
    });
  }

  /// 注入到映射页面的脚本标签（必须是页面里第一个执行的脚本，见 shim 注释）。
  function shimTag(mapId, targetOrigin) {
    return '<script src="' + SHIM_PATH +
      '?m=' + encodeURIComponent(mapId) +
      '&o=' + encodeURIComponent(targetOrigin || '') + '"></script>';
  }

  // ══════════════════════ 状态（IndexedDB + 内存缓存） ══════════════════════

  var state = {
    loaded: false,
    maps: new Map(),      // mapId    → {targetOrigin, lastUsed}
    clients: new Map(),   // clientId → {mapId, at}
    pending: new Map(),   // absUrl   → {mapId, expires}
    lastActive: null,     // {mapId, at}
    loadedAt: 0
  };
  var loadPromise = null;
  var dbPromise = null;
  var lastClean = 0;

  function openDb() {
    if (dbPromise) return dbPromise;
    dbPromise = new Promise(function (resolve, reject) {
      var req;
      try {
        req = indexedDB.open(DB_NAME, DB_VERSION);
      } catch (e) {
        reject(e);
        return;
      }
      req.onupgradeneeded = function () {
        var db = req.result;
        ['maps', 'clients', 'pendingNav', 'meta'].forEach(function (name) {
          if (!db.objectStoreNames.contains(name)) db.createObjectStore(name);
        });
      };
      req.onsuccess = function () { resolve(req.result); };
      req.onerror = function () { reject(req.error); };
    });
    return dbPromise;
  }

  function idbOp(store, mode, fn) {
    return openDb().then(function (db) {
      return new Promise(function (resolve, reject) {
        var tx = db.transaction(store, mode);
        var req = fn(tx.objectStore(store));
        tx.oncomplete = function () { resolve(req ? req.result : undefined); };
        tx.onerror = function () { reject(tx.error); };
        tx.onabort = function () { reject(tx.error); };
      });
    });
  }

  /// 读整个 store → [[key, value], …]；任何异常（隐私模式禁用 IDB、schema
  /// 变更）都退化成空数组，功能降级为"只在内存里工作"。
  function readStore(store) {
    return openDb().then(function (db) {
      return new Promise(function (resolve, reject) {
        var out = [];
        var tx = db.transaction(store, 'readonly');
        var req = tx.objectStore(store).openCursor();
        req.onsuccess = function () {
          var cur = req.result;
          if (!cur) { resolve(out); return; }
          out.push([cur.key, cur.value]);
          cur.continue();
        };
        req.onerror = function () { reject(req.error); };
      });
    }).catch(function () { return []; });
  }

  function writeStore(store, key, value) {
    idbOp(store, 'readwrite', function (os) { return os.put(value, key); })
      .catch(function () { /* 内存里已经是唯一真相，落盘失败不影响本次会话 */ });
  }

  function dropStore(store, key) {
    idbOp(store, 'readwrite', function (os) { return os.delete(key); })
      .catch(function () {});
  }

  function ensureLoaded() {
    if (loadPromise) return loadPromise;
    loadPromise = Promise.all([
      readStore('maps'), readStore('clients'), readStore('pendingNav'), readStore('meta')
    ]).then(function (res) {
      res[0].forEach(function (kv) { state.maps.set(kv[0], kv[1] || {}); });
      res[1].forEach(function (kv) { state.clients.set(kv[0], kv[1] || {}); });
      res[2].forEach(function (kv) { state.pending.set(kv[0], kv[1] || {}); });
      res[3].forEach(function (kv) {
        if (kv[0] === 'lastActive') state.lastActive = kv[1] || null;
      });
      state.loaded = true;
      state.loadedAt = Date.now();
      return state;
    });
    return loadPromise;
  }

  function putMap(mapId, rec) {
    state.maps.set(mapId, rec);
    writeStore('maps', mapId, rec);
  }

  function getMap(mapId) {
    var rec = state.maps.get(mapId);
    return rec || null;
  }

  function putClient(clientId, mapId) {
    if (!clientId) return;
    var rec = { mapId: mapId, at: Date.now() };
    state.clients.set(clientId, rec);
    writeStore('clients', clientId, rec);
  }

  function mapIdOfClient(clientId) {
    if (!clientId) return null;
    var rec = state.clients.get(clientId);
    return rec && rec.mapId ? rec.mapId : null;
  }

  function putPending(url, rec) {
    state.pending.set(url, rec);
    writeStore('pendingNav', url, rec);
  }

  function takePending(url) {
    var rec = state.pending.get(url);
    if (!rec) return null;
    state.pending.delete(url);
    dropStore('pendingNav', url);
    if (!(rec.expires > Date.now())) return null;
    return rec.mapId || null;
  }

  function touch(mapId) {
    var now = Date.now();
    var rec = state.maps.get(mapId);
    if (rec) {
      rec.lastUsed = now;
      writeStore('maps', mapId, rec);
    }
    state.lastActive = { mapId: mapId, at: now };
    writeStore('meta', 'lastActive', state.lastActive);
  }

  function lastActiveMapId() {
    var la = state.lastActive;
    if (!la || !la.mapId) return null;
    if (!(Date.now() - (la.at || 0) <= LAST_ACTIVE_TTL_MS)) return null;
    if (!state.maps.has(la.mapId)) return null;
    return la.mapId;
  }

  /// 清理：过期 pendingNav、已关闭页面的 clientId、超过 24h 的 maps。
  /// 每分钟最多跑一次。`matchAll` 不可用时只清理时间过期的条目。
  function maybeClean() {
    if (!state.loaded) return;
    var now = Date.now();
    if (now - lastClean < CLEAN_INTERVAL_MS) return;
    lastClean = now;

    state.pending.forEach(function (rec, url) {
      if (!(rec && rec.expires > now)) {
        state.pending.delete(url);
        dropStore('pendingNav', url);
      }
    });
    state.maps.forEach(function (rec, mapId) {
      if (now - (rec.lastUsed || 0) > ENTRY_TTL_MS) {
        state.maps.delete(mapId);
        dropStore('maps', mapId);
      }
    });
    if (state.lastActive && now - (state.lastActive.at || 0) > ENTRY_TTL_MS) {
      state.lastActive = null;
      dropStore('meta', 'lastActive');
    }
    var alive = new Set();
    var canMatch = typeof self !== 'undefined' && self.clients &&
      typeof self.clients.matchAll === 'function';
    if (!canMatch) return;
    self.clients.matchAll({ includeUncontrolled: true, type: 'all' })
      .then(function (list) {
        (list || []).forEach(function (c) { alive.add(c.id); });
        state.clients.forEach(function (rec, clientId) {
          // 刚登记的条目留给下一轮：导航刚结束时 matchAll 可能还没看到它。
          if (alive.has(clientId)) return;
          if (now - (rec && rec.at || 0) < CLEAN_INTERVAL_MS) return;
          state.clients.delete(clientId);
          dropStore('clients', clientId);
        });
      })
      .catch(function () {});
  }

  // ══════════════════════ fetch 事件 ══════════════════════

  /// `event.clientId` 是否属于某个映射页面。state 尚未加载完时无法判定，
  /// 返回 true 让请求走 handleSubresource（内部再 await 状态）。
  function maybeMapped(clientId) {
    if (!state.loaded) return true;
    return !!mapIdOfClient(clientId);
  }

  function onFetch(event) {
    var req = event.request;
    var url = tryUrl(req.url);
    if (!url) return;
    // 保留路径永远原样交给浏览器（含我们自己的 SW 与 shim）。
    if (isReservedPath(url.pathname)) return;

    if (req.mode === 'navigate') {
      // 只处理 iframe 内的导航：顶层 document 导航一律放行，绝不能劫持会话页。
      if (req.destination !== 'iframe' && req.destination !== 'frame') return;
      event.respondWith(handleNavigation(event, url));
      return;
    }

    // 非导航：来自映射页面才处理。稳态下这里是同步判定，shell-remote
    // 自身的 SSE / 上传 / 静态资源完全不经 SW（不调用 respondWith）。
    // SW 冷启动、状态还没从 IndexedDB 读完时无法按 clientId 判定：shell-remote
    // 自身的 API（`/agent/…`，映射前缀除外）直接放行，长连接 SSE 绝不经 SW。
    if (!state.loaded && url.origin === self.location.origin &&
        url.pathname.indexOf('/agent/') === 0 && url.pathname.indexOf(PREFIX + '/') !== 0) {
      return;
    }
    if (!maybeMapped(event.clientId)) return;
    event.respondWith(handleSubresource(event, url));
  }

  function rewrittenRequest(req, targetUrl, body) {
    var init = {
      method: req.method,
      headers: req.headers,
      credentials: 'same-origin',
      redirect: req.redirect,
      cache: req.cache,
      signal: req.signal
    };
    if (body !== undefined) init.body = body;
    return new Request(targetUrl, init);
  }

  function handleSubresource(event, url) {
    var req = event.request;
    return ensureLoaded().then(function () {
      maybeClean();
      var mapId = mapIdOfClient(event.clientId);
      if (!mapId) return fetch(req);
      var map = getMap(mapId);
      if (!map || !map.targetOrigin) return fetch(req);
      var action = decideNonNav({
        url: req.url,
        origin: self.location.origin,
        mapId: mapId,
        targetOrigin: map.targetOrigin
      });
      if (action === 'pass') return fetch(req);
      if (action === 'as-is') {
        touch(mapId);
        return fetch(req);
      }
      var target = prefixUrl(req.url, mapId, self.location.origin);
      if (!target) return fetch(req);
      touch(mapId);
      var bodyPromise = (req.method === 'GET' || req.method === 'HEAD')
        ? Promise.resolve(undefined)
        : req.arrayBuffer();
      return bodyPromise.then(function (body) {
        return fetch(rewrittenRequest(req, target, body));
      });
    }).catch(function () {
      return fetch(req);
    });
  }

  function handleNavigation(event, url) {
    var req = event.request;
    var origin = self.location.origin;
    // ① 首次进入 / relay 改写后的 3xx Location：剥掉前缀，302 到目标站原始
    //    路径。iframe 的地址栏从此显示原始路径，SPA 的 history 路由与根路径
    //    资源都按目标站点的写法工作。
    var prefixed = mapIdFromPath(url.pathname);
    if (prefixed) {
      var target = stripNavTarget(url.href, prefixed, origin);
      if (!target) return Promise.resolve(fetch(req));
      putPending(target, { mapId: prefixed, expires: Date.now() + PENDING_TTL_MS });
      touch(prefixed);
      return Promise.resolve(Response.redirect(target, 302));
    }

    return ensureLoaded().then(function () {
      maybeClean();
      var mapId = takePending(url.href) ||
        mapIdOfClient(event.clientId) ||
        mapIdOfClient(event.replacesClientId) ||
        lastActiveMapId();
      if (!mapId) return fetch(req);
      var map = getMap(mapId);
      if (!map || !map.targetOrigin) return fetch(req);

      var target = origin + mapPrefix(mapId) + url.pathname + url.search;
      var init = {
        method: req.method,
        headers: req.headers,
        redirect: 'manual',
        credentials: 'same-origin'
      };
      var bodyPromise = (req.method === 'GET' || req.method === 'HEAD')
        ? Promise.resolve(undefined)
        : req.arrayBuffer();
      return bodyPromise.then(function (body) {
        if (body !== undefined) init.body = body;
        return fetch(target, init);
      }).then(function (resp) {
        touch(mapId);
        putClient(event.resultingClientId, mapId);
        // 3xx：opaqueredirect 原样交回浏览器（status 0、头为空），浏览器会跟随
        // relay 改写过的 Location（带映射前缀）→ 再次进入本函数第一段。
        if (resp.type === 'opaqueredirect') return resp;
        if (resp.status >= 300 && resp.status < 400) return resp;
        if (resp.status === 204 || resp.status === 304) return resp;
        var ctype = String(resp.headers.get('content-type') || '').toLowerCase();
        if (ctype.indexOf('text/html') < 0) return resp;
        return injectResponse(resp, shimTag(mapId, map.targetOrigin));
      });
    }).catch(function () {
      return fetch(req);
    });
  }

  /// 注入 shim 并重建 Response：保留 status / statusText / headers，删掉
  /// `content-length`（长度变了）与 `content-encoding`（body 已被浏览器解压，
  /// 留着会让浏览器再解一次）。
  function injectResponse(resp, insert) {
    var headers = new Headers(resp.headers);
    headers.delete('content-length');
    headers.delete('content-encoding');
    headers.delete('transfer-encoding');
    var body = resp.body ? resp.body.pipeThrough(createInjector(insert, HEAD_WINDOW)) : null;
    return new Response(body, {
      status: resp.status,
      statusText: resp.statusText,
      headers: headers
    });
  }

  // ══════════════════════ 会话页 ↔ SW 消息 ══════════════════════

  function reply(source, msg) {
    try {
      if (source && typeof source.postMessage === 'function') source.postMessage(msg);
    } catch (e) {}
  }

  function onMessage(event) {
    var data = event.data || {};
    if (data.type === 'sr-web-map') {
      var mapId = String(data.mapId || '');
      var targetOrigin = String(data.targetOrigin || '');
      if (!mapId || !targetOrigin) {
        reply(event.source, { type: 'sr-web-map-ok', mapId: mapId, ok: false });
        return;
      }
      var run = state.loaded ? Promise.resolve() : ensureLoaded();
      run.then(function () {
        var rec = getMap(mapId) || {};
        rec.targetOrigin = targetOrigin;
        rec.lastUsed = Date.now();
        putMap(mapId, rec);
        touch(mapId);
        reply(event.source, { type: 'sr-web-map-ok', mapId: mapId, ok: true });
      }).catch(function () {
        reply(event.source, { type: 'sr-web-map-ok', mapId: mapId, ok: false });
      });
      return;
    }
    // 切回静态模式 / 断线时清掉该会话的 client 归属：避免后续 iframe 导航
    // 仍然被 lastActive 拉进映射。
    if (data.type === 'sr-web-clear') {
      ensureLoaded().then(function () {
        var keep = String(data.keepMapId || '');
        state.clients.forEach(function (rec, clientId) {
          if (!keep || rec.mapId !== keep) {
            state.clients.delete(clientId);
            dropStore('clients', clientId);
          }
        });
        state.pending.clear();
        if (!keep) state.lastActive = null;
      }).catch(function () {});
    }
  }

  // ══════════════════════ 生命周期 ══════════════════════

  var isServiceWorker = typeof self !== 'undefined' &&
    typeof self.skipWaiting === 'function' &&
    typeof self.registration !== 'undefined';

  if (isServiceWorker) {
    self.addEventListener('install', function () {
      self.skipWaiting();
    });
    self.addEventListener('activate', function (event) {
      event.waitUntil((function () {
        var chain = ensureLoaded().then(function () {
          // 换版本后重新清理一次：旧条目不应把新代码带进错误的映射。
          lastClean = 0;
          maybeClean();
        }).catch(function () {});
        return chain.then(function () { return self.clients.claim(); });
      })());
    });
    self.addEventListener('fetch', onFetch);
    self.addEventListener('message', onMessage);
    // SW 脚本一被求值就预热状态：fetch 事件可能紧接着到来。
    ensureLoaded().catch(function () {});
  }

  // ── node 单测入口（tools/test_sr_web.js）────────────────────────────
  var api = {
    PATH_PREFIX: PREFIX,
    RESERVED_PATHS: Object.keys(RESERVED),
    SHIM_PATH: SHIM_PATH,
    PENDING_TTL_MS: PENDING_TTL_MS,
    LAST_ACTIVE_TTL_MS: LAST_ACTIVE_TTL_MS,
    HEAD_WINDOW: HEAD_WINDOW,
    isReservedPath: isReservedPath,
    mapIdFromPath: mapIdFromPath,
    mapPrefix: mapPrefix,
    stripPrefix: stripPrefix,
    hasPrefix: hasPrefix,
    decideNonNav: decideNonNav,
    prefixUrl: prefixUrl,
    stripNavTarget: stripNavTarget,
    findHeadInsertIndex: findHeadInsertIndex,
    injectHtml: injectHtml,
    shimTag: shimTag
  };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})();
