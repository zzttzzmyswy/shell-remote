// web/sr-web-shim.js — 注入到映射页面的脚本（MYS-2383-G）。
//
// Service Worker 能接管 `fetch` / `XMLHttpRequest` / 静态资源，但**接管不到**
// `new WebSocket(...)`：WebSocket 握手不是 fetch，SW 的 fetch 事件永远收不到。
// 因此由本脚本在页面里把 WebSocket URL 改写成反代路径，握手就会走
// `/agent/web/m/<map_id>/…`，relay 再按 Upgrade 透传到目标设备。
//
// 其余职责：
//   - `fetch` / `XHR.open` / `EventSource` 里**写死目标源绝对地址**的 URL
//     （`http://192.168.0.1/api/x`）改成同源相对路径，之后由 SW 接手。相对的
//     和 `location.host` 的写法本来就走 SW，不需要动。
//   - 屏蔽页面里的 Service Worker API：远端页面若能 `register()` 或
//     `getRegistrations()+unregister()`，就能抢走/注销整个 shell-remote 源的
//     SW，等于接管会话页本身的全部请求。这里让它看不到、也注销不掉。
//
// 约束：自包含 IIFE，不依赖 shell-remote 的任何全局变量，不读写
// sessionStorage/localStorage（映射页面的脚本与本页同源，能读到本页凭据；
// 少给它一条捷径）。必须在页面任何脚本之前同步执行——SW 按字节把它插在第一个
// `<head>` 标签之后。
'use strict';

(function () {
  var inBrowser = typeof window !== 'undefined' && typeof document !== 'undefined';
  var current = inBrowser ? document.currentScript : null;
  var src = (current && current.src) || '';

  // ── 从自身 URL 解析 m（map_id）/ o（targetOrigin）────────────────────
  var params = null;
  if (src) {
    try {
      params = new URL(src, location.href).searchParams;
    } catch (e) {
      params = null;
    }
  }
  var MAP_ID = (params && params.get('m')) || '';
  var TARGET_ORIGIN = (params && params.get('o')) || '';

  // 浏览器里没有 map_id 说明这个脚本被误加载，什么都不做。
  if (inBrowser && !MAP_ID) return;

  var PREFIX = '/agent/web/m';

  function tryUrl(raw, base) {
    try {
      return base === undefined ? new URL(raw) : new URL(raw, base);
    } catch (e) {
      return null;
    }
  }

  function ownLocation() {
    return inBrowser ? location : { href: 'https://relay.invalid/', host: '', protocol: 'https:', origin: 'https://relay.invalid' };
  }

  function targetHostOf(targetOrigin) {
    var t = targetOrigin ? tryUrl(targetOrigin) : null;
    return t ? t.host : '';
  }

  // ══════════════════════ 纯函数（node 单测用） ══════════════════════

  /// WebSocket URL 改写：host 等于本页 host（且未带映射前缀）或等于
  /// targetOrigin 的 host → 走反代；其余（第三方推送服务等）原样返回。
  ///
  /// `ws` / `wss` 由**本页**协议决定：同源方案下浏览器到 relay 的链路是 https
  /// 就得用 wss，与目标设备自己是不是 TLS 无关（TLS 由 relay 在隧道上做）。
  function rewriteWsUrl(raw, opts) {
    opts = opts || {};
    var loc = opts.location || ownLocation();
    var mapId = opts.mapId !== undefined ? opts.mapId : MAP_ID;
    var u = typeof raw === 'string' ? tryUrl(raw, loc.href) : null;
    if (!u) return raw;
    if (u.protocol !== 'ws:' && u.protocol !== 'wss:') {
      // 协议相对写法（`//host/path`）：WebSocket 构造器会按本页协议补成
      // ws/wss，这里按同一规则接受，否则新写法会绕开改写。
      var schemeRelative = typeof raw === 'string' && raw.slice(0, 2) === '//' &&
        (u.protocol === 'http:' || u.protocol === 'https:');
      if (!schemeRelative) return raw;
    }
    if (u.pathname.indexOf(PREFIX + '/') === 0) return raw;
    var th = opts.targetHost !== undefined ? opts.targetHost : targetHostOf(TARGET_ORIGIN);
    var sameHost = !!loc.host && u.host === loc.host;
    var targetMatch = !!th && u.host === th;
    if (!sameHost && !targetMatch) return raw;
    var scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:';
    return scheme + '//' + loc.host + PREFIX + '/' + mapId + u.pathname + u.search;
  }

  /// 指向 targetOrigin 的**绝对** URL → 同源相对路径（`pathname + search`）。
  /// 返回 null 表示不改写（相对地址、他源、已带前缀、非 http/https）。
  function toRelativeIfTarget(raw, opts) {
    if (typeof raw !== 'string') return null;
    opts = opts || {};
    var loc = opts.location || ownLocation();
    // 只处理带 scheme 的绝对地址：相对地址交给 SW 按原路径处理即可。
    if (!/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(raw)) return null;
    var u = tryUrl(raw, loc.href);
    if (!u) return null;
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
    if (u.pathname.indexOf(PREFIX + '/') === 0) return null;
    var th = opts.targetHost !== undefined ? opts.targetHost : targetHostOf(TARGET_ORIGIN);
    if (!th || u.host !== th) return null;
    return u.pathname + u.search;
  }

  /// navigator.serviceWorker 的替身：注册一律失败，枚举永远为空，
  /// 因此页面拿不到我们的 registration，也就注销不掉。
  function fakeSwContainer() {
    function blocked() {
      var err = new Error('service workers are disabled inside shell-remote web mapping');
      err.name = 'SecurityError';
      return Promise.reject(err);
    }
    return {
      register: blocked,
      unregister: blocked,
      update: blocked,
      getRegistrations: function () { return Promise.resolve([]); },
      getRegistration: function () { return Promise.resolve(undefined); },
      addEventListener: function () {},
      removeEventListener: function () {},
      dispatchEvent: function () { return false; },
      ready: new Promise(function () {})
    };
  }

  // ══════════════════════ 安装 ══════════════════════

  function installWebSocket(opts) {
    var NativeWS = window.WebSocket;
    if (typeof NativeWS !== 'function') return;
    var Wrapped = null;
    try {
      // 子类保持 instanceof / 静态常量 / protocols 参数不变。
      Wrapped = class extends NativeWS {
        constructor(url, protocols) {
          var finalUrl = rewriteWsUrl(url, opts);
          if (protocols === undefined) super(finalUrl);
          else super(finalUrl, protocols);
        }
      };
      ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function (k) {
        try {
          Object.defineProperty(Wrapped, k, { value: NativeWS[k], enumerable: true });
        } catch (e) {}
      });
      try {
        Object.defineProperty(Wrapped, 'name', { value: 'WebSocket' });
      } catch (e) {}
    } catch (e) {
      // 少数老 WebView 不允许继承 WebSocket：退化为构造器包装（丢失
      // prototype 上的自定义能力，但 URL 改写仍然生效）。
      Wrapped = function WebSocket(url, protocols) {
        var finalUrl = rewriteWsUrl(url, opts);
        return protocols === undefined
          ? new NativeWS(finalUrl)
          : new NativeWS(finalUrl, protocols);
      };
      Wrapped.prototype = NativeWS.prototype;
      ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED'].forEach(function (k) {
        try { Wrapped[k] = NativeWS[k]; } catch (e) {}
      });
    }
    window.WebSocket = Wrapped;
  }

  function installFetch(opts) {
    if (typeof window.fetch !== 'function') return;
    var nativeFetch = window.fetch;
    window.fetch = function (input, init) {
      try {
        if (typeof input === 'string') {
          var rel = toRelativeIfTarget(input, opts);
          if (rel) input = rel;
        } else if (typeof Request !== 'undefined' && input instanceof Request) {
          // Request 对象的 URL 只能靠新建实例替换；用相对地址构造会把 URL
          // 解析成同源路径，正好等价于我们想要的改写。
          var rel2 = toRelativeIfTarget(input.url, opts);
          if (rel2) input = new Request(rel2, input);
        }
      } catch (e) {}
      return nativeFetch.call(this, input, init);
    };
  }

  function installEventSource(opts) {
    if (typeof window.EventSource !== 'function') return;
    var NativeES = window.EventSource;
    var WrappedES = function EventSource(url, cfg) {
      var rel = toRelativeIfTarget(url, opts);
      return new NativeES(rel || url, cfg);
    };
    WrappedES.prototype = NativeES.prototype;
    ['CONNECTING', 'OPEN', 'CLOSED'].forEach(function (k) {
      try { WrappedES[k] = NativeES[k]; } catch (e) {}
    });
    window.EventSource = WrappedES;
  }

  function installXhr(opts) {
    if (typeof XMLHttpRequest === 'undefined' || !XMLHttpRequest.prototype) return;
    var nativeOpen = XMLHttpRequest.prototype.open;
    if (typeof nativeOpen !== 'function') return;
    XMLHttpRequest.prototype.open = function (method, url) {
      var rest = Array.prototype.slice.call(arguments, 2);
      var rel = toRelativeIfTarget(url, opts);
      if (rel) url = rel;
      return nativeOpen.apply(this, [method, url].concat(rest));
    };
  }

  /// 用 Object.defineProperty 覆盖到 `navigator.serviceWorker` 实例上
  /// （configurable:false）：页面连恢复原生实现的余地都没有。
  ///
  /// **只改实例、不改原型**：同源方案下 iframe 与父页面共享
  /// `ServiceWorkerContainer.prototype`，改原型会连会话页自己的注册一起废掉。
  function installSwGuard() {
    var sw = navigator.serviceWorker;
    if (!sw) return;
    var fake = fakeSwContainer();
    Object.keys(fake).forEach(function (name) {
      try {
        Object.defineProperty(sw, name, {
          value: fake[name],
          configurable: false,
          writable: false,
          enumerable: true
        });
      } catch (e) {
        try { sw[name] = fake[name]; } catch (e2) {}
      }
    });
  }

  function install() {
    var opts = {
      mapId: MAP_ID,
      targetHost: targetHostOf(TARGET_ORIGIN),
      location: location
    };
    // 重复注入（页面里出现第二个 <head> 之类）不套娃。
    if (window.__srWebShimMap === MAP_ID) return opts;
    window.__srWebShimMap = MAP_ID;
    installWebSocket(opts);
    installFetch(opts);
    installEventSource(opts);
    installXhr(opts);
    installSwGuard();
    return opts;
  }

  if (inBrowser) {
    try {
      install();
    } catch (e) {}
  }

  // node 单测入口（tools/test_sr_web.js）；浏览器里 module 未定义。
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = {
      MAP_ID: MAP_ID,
      TARGET_ORIGIN: TARGET_ORIGIN,
      rewriteWsUrl: rewriteWsUrl,
      toRelativeIfTarget: toRelativeIfTarget,
      targetHostOf: targetHostOf,
      fakeSwContainer: fakeSwContainer
    };
  }
})();
