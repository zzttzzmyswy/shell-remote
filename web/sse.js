// sse.js - SSE + POST browser client for shell-remote
//
// Uses a fetch-based streaming reader instead of native EventSource so the
// session token travels in an Authorization header rather than the URL query
// string (which would otherwise be written to reverse-proxy access logs).

(function() {
  var token = sessionStorage.getItem('shell-remote-token');
  var permission = sessionStorage.getItem('shell-remote-permission') || 'ro';

  if (!token) {
    document.body.innerHTML = '<div style="padding:2em;color:red">Missing token — please go back and enter your session token</div>';
    return;
  }

  var userId = null;
  var handlers = {};

  // 服务器密码（relay --auth），登录页写入 sessionStorage。所有浏览器请求都带
  // X-Auth 头；与会话密钥（Authorization: Bearer）是两个独立的凭据。
  function authHeaders(extra) {
    var h = extra || {};
    var pw = sessionStorage.getItem('shell-remote-auth');
    if (pw) h['X-Auth'] = pw;
    return h;
  }
  window.shellRemoteAuthHeaders = authHeaders;
  // 服务器密码被拒：清掉并回登录页提示（不再重连）。
  function rejectPassword() {
    intentionalClose = true;
    sessionStorage.removeItem('shell-remote-auth');
    window.location.href = '/?err=password';
  }
  function isPasswordRejection(resp) {
    return resp.status === 401 && resp.clone().json()
      .then(function(d) { return d && d.error === 'AUTH_INVALID_PASSWORD'; })
      .catch(function() { return false; });
  }

  var controller = null;          // AbortController for the active fetch
  var intentionalClose = false;   // true when we deliberately stop the stream
  var reconnectTimer = null;
  var reconnectDelay = 1000;      // grows on failure, resets on success
  var lastSessionError = null;    // dedup identical consecutive error toasts
  // 401/403 容忍窗口：relay 重启后 agent 尚未重新注册时，旧 token 会短暂
  // 被判无效。页面曾经连上过则先重试 AUTH_GRACE_MS 再退回登录页；从未连上
  // （密钥本身填错）则立即退回。
  var everConnected = false;
  var authFailSince = 0;
  var AUTH_GRACE_MS = 90000;
  var lastPermWarnAt = 0;
  // R5#8 SSE 空闲看门狗：agent 下行 SSE 心跳 15s、relay 半开超时 60s。浏览器
  // 侧显式空闲计数（对齐 #8）：30s 无任何 SSE 事件即判死主动重连（relay 60s
  // 兜底的一半），弱网事件稀疏时更快检出半开连接。
  var lastSseAt = 0;              // 最近一次收到 SSE 块的墙钟；0 = 当前流未就绪
  var sseIdleTimer = null;        // setInterval id（惰性创建，连接成功后启动）
  var SSE_IDLE_MS = 30000;
  var SSE_IDLE_CHECK_MS = 5000;

  function checkSseIdle() {
    if (lastSseAt === 0 || intentionalClose) return;
    var idle = Date.now() - lastSseAt;
    if (idle > SSE_IDLE_MS) {
      console.warn('SSE idle ' + idle + 'ms（半开检测 #8），主动重连');
      lastSseAt = 0;
      if (controller) {
        intentionalClose = true;
        controller.abort();
        intentionalClose = false;
      }
      scheduleReconnect();
    }
  }

  function emit(type, obj) {
    // 单个 handler 抛错不能跳过其余 handler，也不能杀掉 SSE pump
    var call = function(fn) {
      try { fn(obj); } catch (e) { console.error('SSE handler error (' + type + '):', e); }
    };
    var hs = handlers[type];
    if (hs) hs.slice().forEach(call);
    if (handlers['*']) handlers['*'].slice().forEach(call);
  }

  window.shellRemote = {
    on: function(type, fn) {
      if (!handlers[type]) handlers[type] = [];
      handlers[type].push(fn);
    },
    off: function(type, fn) {
      if (handlers[type]) handlers[type] = handlers[type].filter(function(f) { return f !== fn; });
    },
    // Promise<boolean>：true 当且仅当 resp.ok；网络错误/超时 -> false，不抛。
    // timeoutMs 可选：超时即 abort，避免半开连接上的 POST 永久挂起。
    sendAsync: function(type, payload, timeoutMs) {
      var ac = timeoutMs ? new AbortController() : null;
      var timer = ac ? setTimeout(function() { ac.abort(); }, timeoutMs) : null;
      return fetch('/agent/session/send', {
        method: 'POST',
        signal: ac ? ac.signal : undefined,
        headers: authHeaders({ 'Content-Type': 'application/json' }),
        body: JSON.stringify({
          token: token,
          type: type,
          payload: payload || {}
        })
      }).then(function(resp) {
        if (resp.status === 401) {
          return Promise.resolve(isPasswordRejection(resp)).then(function(bad) {
            if (bad) rejectPassword();
            return false;
          });
        }
        // 401 的会话密钥失效由 SSE 通道统一处理（含 relay 重启容忍窗口）；
        // 403 = 只读用户发了写操作，提示即可（此前会被直接踢回登录页）。
        if (resp.status === 403) {
          var now = Date.now();
          if (now - lastPermWarnAt > 3000) {
            lastPermWarnAt = now;
            emit('error', { type: 'error', payload: { code: 'PERMISSION_DENIED' } });
          }
        }
        return resp.ok;
      }).catch(function(e) {
        console.warn('POST failed:', e.message);
        return false;
      }).then(function(ok) {
        if (timer) clearTimeout(timer);
        return ok;
      });
    },
    send: function(type, payload) {
      window.shellRemote.sendAsync(type, payload); // fire-and-forget
    },
    getUserId: function() { return userId; },
    getPermission: function() { return permission; },
    // Programmatic reconnect (used by the UI's join-ack watchdog / overlays).
    reconnect: function() { scheduleReconnect(); }
  };

  function scheduleReconnect() {
    if (intentionalClose) return;
    if (reconnectTimer) return;
    var wait = Math.max(500, reconnectDelay * (0.5 + Math.random() * 0.5)); // jitter
    reconnectTimer = setTimeout(function() {
      reconnectTimer = null;
      connectSSE();
    }, wait);
    reconnectDelay = Math.min(reconnectDelay * 2, 10000);
  }

  // Parse one SSE block (lines separated by \n) and dispatch to handlers.
  function handleBlock(block) {
    lastSseAt = Date.now(); // 任何块到达 = 流活着（R5#8 空闲看门狗刷新）
    var eventName = 'message';
    var dataLines = [];
    var lines = block.split('\n');
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      if (line.charAt(0) === ':') continue;            // comment / keep-alive
      var colon = line.indexOf(':');
      var field = colon === -1 ? line : line.slice(0, colon);
      var value = colon === -1 ? '' : line.slice(colon + 1);
      if (value.charAt(0) === ' ') value = value.slice(1); // leading space per spec
      if (field === 'event') {
        eventName = value;
      } else if (field === 'data') {
        dataLines.push(value);
      }
    }
    if (dataLines.length === 0) return;
    var data = dataLines.join('\n');

    var parsed;
    try {
      parsed = JSON.parse(data);
    } catch (err) {
      console.warn('Failed to parse SSE message:', err);
      return;
    }

    if (eventName === 'connected') {
      reconnectDelay = 1000; // a live connection resets the backoff
      try {
        userId = parsed.payload.user_id;
        permission = parsed.payload.permission;
      } catch (err) {
        console.warn('Failed to parse connected event:', err);
      }
      emit('connected', parsed);
      return;
    }

    var type = parsed.type;
    emit(type, parsed);

    // The agent half of the session is gone (relay informed us). The browser
    // SSE is still healthy; reconnect so we re-join once the agent is back —
    // until then the relay answers with 503 and we keep retrying.
    if (type === 'session:agent_disconnect') {
      scheduleReconnect();
    } else if (type === 'session:error' &&
               parsed.payload && parsed.payload.code === 'AGENT_NOT_CONNECTED') {
      // The relay tried to deliver our join but the agent channel was stale
      // (closed/full). Retry — a rejoin will land once the agent link is real.
      scheduleReconnect();
    }
  }

  function connectSSE() {
    if (controller) {
      intentionalClose = true;
      controller.abort();
      intentionalClose = false;
    }
    controller = new AbortController();
    var localController = controller;
    var buffer = '';

    fetch('/agent/session/sse', {
      method: 'GET',
      headers: authHeaders({
        'Authorization': 'Bearer ' + token,
        'Accept': 'text/event-stream',
        'Cache-Control': 'no-cache'
      }),
      signal: localController.signal
    }).then(function(resp) {
      if (!resp.ok || !resp.body) {
        if (resp.status === 401 && !everConnected) {
          // 先区分是服务器密码错还是会话密钥错
          return Promise.resolve(isPasswordRejection(resp)).then(function(bad) {
            intentionalClose = true;
            if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
            if (bad) { rejectPassword(); return; }
            window.location.href = '/?err=token';
          });
        }
        if (resp.status === 401 || resp.status === 403) {
          var now = Date.now();
          if (!authFailSince) authFailSince = now;
          if (!everConnected || now - authFailSince > AUTH_GRACE_MS) {
            intentionalClose = true; // 不再重连
            if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
            window.location.href = '/';
            return;
          }
          if (lastSessionError !== 'AUTH_PENDING') {
            lastSessionError = 'AUTH_PENDING';
            emit('session:error', { payload: { code: 'AUTH_PENDING' } });
          }
          throw new Error('SSE HTTP ' + resp.status + ' (auth pending)');
        }
        return resp.json().catch(function() { return {}; }).then(function(data) {
          // Registered-but-unreachable agent: tell the UI (toast, dedup'd)
          // and let the generic catch below keep retrying.
          if (data.error === 'AGENT_NOT_CONNECTED') {
            if (lastSessionError !== 'AGENT_NOT_CONNECTED') {
              lastSessionError = 'AGENT_NOT_CONNECTED';
              emit('session:error', { payload: { code: 'AGENT_NOT_CONNECTED' } });
            }
            throw new Error('AGENT_NOT_CONNECTED');
          }
          lastSessionError = null;
          throw new Error('SSE HTTP ' + resp.status);
        });
      }
      lastSessionError = null;
      everConnected = true;
      authFailSince = 0;
      lastSseAt = Date.now(); // 流打开即计时，只开流不发数据也会被空闲看门狗捕获
      var reader = resp.body.getReader();
      var decoder = new TextDecoder();
      // R5#8：SSE 流建立即启动空闲看门狗（惰性，仅一次）——30s 无任何
      // 块判定半开（relay 60s 兜底的一半），主动 abort + 重连。
      if (!sseIdleTimer) {
        sseIdleTimer = setInterval(checkSseIdle, SSE_IDLE_CHECK_MS);
      }

      function pump() {
        return reader.read().then(function(result) {
          if (localController.signal.aborted) return;
          if (result.done) {
            scheduleReconnect();
            return;
          }
          buffer += decoder.decode(result.value, { stream: true });
          var idx;
          while ((idx = buffer.indexOf('\n\n')) !== -1) {
            var block = buffer.slice(0, idx);
            buffer = buffer.slice(idx + 2);
            handleBlock(block);
          }
          return pump();
        });
      }
      return pump();
    }).catch(function(err) {
      if (localController.signal.aborted) return;  // deliberate stop
      console.warn('SSE stream error:', err.message);
      scheduleReconnect();
    });
  }

  // 网络恢复 / 页面回到前台：重置退避并尽快重连，避免双连
  function kickReconnect() {
    if (intentionalClose) return;
    reconnectDelay = 1000;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
      connectSSE();
      return;
    }
    if (lastSseAt === 0) {
      if (!controller) connectSSE();
      return;
    }
    if (Date.now() - lastSseAt > SSE_IDLE_MS) checkSseIdle();
  }
  window.addEventListener('online', kickReconnect);
  document.addEventListener('visibilitychange', function() {
    if (document.visibilityState === 'visible') kickReconnect();
  });

  connectSSE();
})();
