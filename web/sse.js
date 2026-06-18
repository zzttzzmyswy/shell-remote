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

  var controller = null;          // AbortController for the active fetch
  var intentionalClose = false;   // true when we deliberately stop the stream
  var reconnectTimer = null;

  window.shellRemote = {
    on: function(type, fn) {
      if (!handlers[type]) handlers[type] = [];
      handlers[type].push(fn);
    },
    off: function(type, fn) {
      if (handlers[type]) handlers[type] = handlers[type].filter(function(f) { return f !== fn; });
    },
    send: function(type, payload) {
      fetch('/agent/session/send', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          token: token,
          type: type,
          payload: payload || {}
        })
      }).then(function(resp) {
        if (resp.status === 401 || resp.status === 403) {
          window.location.href = '/';
        }
      }).catch(function(e) {
        console.warn('POST failed:', e.message);
      });
    },
    getUserId: function() { return userId; },
    getPermission: function() { return permission; }
  };

  function scheduleReconnect() {
    if (intentionalClose) return;
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(function() {
      reconnectTimer = null;
      connectSSE();
    }, 3000);
  }

  // Parse one SSE block (lines separated by \n) and dispatch to handlers.
  function handleBlock(block) {
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
      try {
        userId = parsed.payload.user_id;
        permission = parsed.payload.permission;
      } catch (err) {
        console.warn('Failed to parse connected event:', err);
      }
      if (handlers['connected']) {
        handlers['connected'].forEach(function(fn) { fn(parsed); });
      }
      return;
    }

    var type = parsed.type;
    if (handlers[type]) {
      handlers[type].forEach(function(fn) { fn(parsed); });
    }
    if (handlers['*']) {
      handlers['*'].forEach(function(fn) { fn(parsed); });
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
      headers: {
        'Authorization': 'Bearer ' + token,
        'Accept': 'text/event-stream',
        'Cache-Control': 'no-cache'
      },
      signal: localController.signal
    }).then(function(resp) {
      if (!resp.ok || !resp.body) {
        if (resp.status === 401 || resp.status === 403) {
          window.location.href = '/';
        }
        throw new Error('SSE HTTP ' + resp.status);
      }
      var reader = resp.body.getReader();
      var decoder = new TextDecoder();

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

  connectSSE();
})();
