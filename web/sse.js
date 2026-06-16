// sse.js - SSE + POST browser client for shell-remote

(function() {
  var params = new URLSearchParams(window.location.search);
  var sessionId = params.get('session');
  var token = params.get('token');

  if (!sessionId || !token) {
    document.body.innerHTML = '<div style="padding:2em;color:red">Missing session or token in URL</div>';
    return;
  }

  var userId = null;
  var permission = 'ro';
  var es = null;
  var handlers = {};

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
          session_id: sessionId,
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
    getPermission: function() { return permission; },
    getSessionId: function() { return sessionId; }
  };

  function connectSSE() {
    if (es) es.close();
    es = new EventSource('/agent/session/sse?session=' + encodeURIComponent(sessionId) + '&token=' + encodeURIComponent(token));

    es.addEventListener('connected', function(e) {
      try {
        var data = JSON.parse(e.data);
        userId = data.payload.user_id;
        permission = data.payload.permission;
      } catch(err) {
        console.warn('Failed to parse connected event:', err);
      }
    });

    es.onmessage = function(e) {
      try {
        var msg = JSON.parse(e.data);
        var type = msg.type;
        if (handlers[type]) {
          handlers[type].forEach(function(fn) { fn(msg); });
        }
        if (handlers['*']) {
          handlers['*'].forEach(function(fn) { fn(msg); });
        }
      } catch(err) {
        console.warn('Failed to parse SSE message:', err);
      }
    };

    es.onerror = function() {
      // EventSource auto-reconnects
    };
  }

  connectSSE();
})();
