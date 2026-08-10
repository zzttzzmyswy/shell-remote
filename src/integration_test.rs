#[cfg(test)]
mod integration_tests {
    use serde_json::{json, Value};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::relay::mcp;
    use crate::relay::ws;

    /// Build a minimal relay router over a fresh shared state, for tests that
    /// only need the agent POST endpoints (no recording, no admin).
    fn relay_app() -> Arc<crate::relay::SharedState> {
        Arc::new(crate::relay::SharedState::new(
            String::new(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            None,
        ))
    }

    /// Session ids are reusable: a fresh agent re-registering under an id that
    /// a previous incarnation (possibly dead) already holds takes over the
    /// identity — the old session is evicted and its tokens invalidated. No
    /// more 409 "id in use"; the response flags `evicted: true`.
    #[tokio::test]
    async fn test_ghost_session_reclaimed_by_cached_token_reconnect() {
        let state = relay_app();
        use axum::routing::get;
        use axum::Router;
        let app = Router::new()
            .route("/agent/send", axum::routing::post(ws::agent_send_handler))
            .route("/agent/events", get(ws::agent_events_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move { axum::serve(listener, app).await });
        tokio::time::sleep(Duration::from_millis(150)).await;

        let relay_url = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();

        // 1. First registration with a custom session id — succeeds, mints T.
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"agent:register","token_type":"rw","session_id":"seSupportBot"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let reg: Value = resp.json().await.unwrap();
        assert_eq!(reg["session_id"], "seSupportBot");
        assert_eq!(reg["evicted"], false, "first registration evicts nothing");
        let token = reg["payload"]["tokens"][0]["token"].as_str().unwrap().to_string();

        // 2. A *fresh* re-registration for the same id (no cached tokens) now
        //    succeeds and evicts the prior incarnation (no 409).
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"agent:register","token_type":"rw","session_id":"seSupportBot"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "fresh re-register must succeed, not 409");
        let reg2: Value = resp.json().await.unwrap();
        assert_eq!(reg2["evicted"], true, "re-registering an in-use id must evict");
        assert_eq!(reg2["session_id"], "seSupportBot");
        // The old token is invalidated by the eviction.
        assert!(state.sessions.authenticate(&token).await.is_none());

        // 3. Re-registration replaying a cached token routes through
        //    register_existing and succeeds (token reuse).
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({
                "type":"agent:register",
                "tokens":[{"token":"cached-tok","permission":"rw"}],
                "session_id":"seSupportBot"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "cached-token reconnect must succeed");
        let reg3: Value = resp.json().await.unwrap();
        assert_eq!(reg3["session_id"], "seSupportBot");
        assert_eq!(reg3["payload"]["tokens"][0]["token"], "cached-tok");
    }

    /// An agent started with `--key <fixed> --session-id <id>` that restarts
    /// as a fresh process (no cached tokens) reclaims its id via the fixed key.
    /// And a *different* key claiming the same id also succeeds — ids are fully
    /// reusable; the newest incarnation evicts the old.
    #[tokio::test]
    async fn test_fixed_key_register_reclaims_id_across_process_restart() {
        let state = relay_app();
        use axum::Router;
        let app = Router::new()
            .route("/agent/send", axum::routing::post(ws::agent_send_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move { axum::serve(listener, app).await });
        tokio::time::sleep(Duration::from_millis(150)).await;

        let relay_url = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();

        // Process 1: register with a fixed key + custom id. Succeeds.
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"agent:register","key":"fixed-key-Z","token_type":"rw","session_id":"seSupportBot"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let reg: Value = resp.json().await.unwrap();
        assert_eq!(reg["session_id"], "seSupportBot");
        assert_eq!(reg["payload"]["tokens"][0]["token"], "fixed-key-Z");

        // Process 2 starts fresh: NO cached tokens, same key + id. Reclaims.
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"agent:register","key":"fixed-key-Z","token_type":"rw","session_id":"seSupportBot"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "fixed-key register across a process restart must reclaim the id"
        );
        let reg: Value = resp.json().await.unwrap();
        assert_eq!(reg["session_id"], "seSupportBot");
        assert_eq!(reg["payload"]["tokens"][0]["token"], "fixed-key-Z");

        // A *different* fixed key claiming the same in-use id now succeeds and
        // evicts the previous session — ids are reusable across devices/keys.
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"agent:register","key":"intruder-key","token_type":"rw","session_id":"seSupportBot"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "a different key reusing an id must take over");
        let reg: Value = resp.json().await.unwrap();
        assert_eq!(reg["evicted"], true);
        assert_eq!(reg["session_id"], "seSupportBot");
        assert_eq!(reg["payload"]["tokens"][0]["token"], "intruder-key");
        // The previous key is no longer valid.
        assert!(state.sessions.authenticate("fixed-key-Z").await.is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn test_full_workflow() {
        let _ = tracing_subscriber::fmt().try_init();
        let port = 19878u16;
        let server_auth = "integration-test-pw";
        let state = Arc::new(crate::relay::SharedState::new(
            server_auth.to_string(),
            100 * 1024 * 1024,
            None,
            String::new(),
            String::new(),
            None,
        ));

        use axum::routing::get;
        use axum::Router;

        let app = Router::new()
            .route("/agent/session/sse", get(ws::browser_sse_handler))
            .route(
                "/agent/session/send",
                axum::routing::post(ws::browser_send_handler),
            )
            .route("/agent/send", axum::routing::post(ws::agent_send_handler))
            .route("/agent/events", get(ws::agent_events_handler))
            .route("/agent/mcp/sse", get(mcp::sse_handler))
            .route(
                "/agent/mcp/messages",
                axum::routing::post(mcp::messages_handler),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        let relay_url = format!("http://127.0.0.1:{}", port);
        let client = reqwest::Client::new();

        // ── 1. Agent registers ────────────────────────────────────
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"agent:register","key":"itest","token_type":"rw"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "Agent registration should succeed");
        let reg: Value = resp.json().await.unwrap();
        assert_eq!(reg["type"], "agent:registered");
        let session_id = reg["session_id"].as_str().unwrap().to_string();
        let rw_token = reg["payload"]["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["permission"] == "rw")
            .and_then(|t| t["token"].as_str())
            .unwrap()
            .to_string();
        eprintln!("  [1] agent registered: session={}", session_id);

        // ── 2. Agent subscribes to events ────────────────────────
        let resp = client
            .get(format!("{}/agent/events?session={}", relay_url, session_id))
            .header("Accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        eprintln!("  [2] events stream connected");

        // ── 3. Browser connects via SSE+POST ──────────────────
        // Token travels in the Authorization header (not the query string),
        // matching the browser client in web/sse.js.
        let resp = client
            .get(format!("{}/agent/session/sse", relay_url))
            .header("Authorization", format!("Bearer {}", rw_token))
            .header("Accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "Browser SSE connection should return 200"
        );
        eprintln!("  [3a] browser SSE stream connected");

        let resp = client
            .post(format!("{}/agent/session/send", relay_url))
            .json(&json!({
                "type": "terminal:input",
                "session_id": session_id,
                "token": rw_token,
                "payload": {"data": "echo hello"}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "Browser POST should return 202");
        eprintln!("  [3b] browser POST returns 202");

        // ── 4. MCP tools/list (via 202 + push to SSE) ──────────
        // Open SSE connection, read first event to get sessionId
        let sse_resp = client
            .get(format!("{}/agent/mcp/sse", relay_url))
            .header("x-auth", server_auth)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(sse_resp.status(), 200);

        // Read just enough of the SSE stream to get the endpoint event
        use tokio_stream::StreamExt;
        let mut body_stream = sse_resp.bytes_stream();
        let sse_text = tokio::time::timeout(std::time::Duration::from_secs(3), body_stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let sse_text = String::from_utf8_lossy(&sse_text);
        let session_id = sse_text
            .lines()
            .find(|l| l.starts_with("data: "))
            .and_then(|l| l.rsplit("sessionId=").next())
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(
            !session_id.is_empty(),
            "Should have sessionId: {}",
            sse_text
        );
        eprintln!("  [4a] SSE sessionId={}", session_id);

        // POST to messages with sessionId (keep SSE alive during POST)
        let resp = client
            .post(format!(
                "{}/agent/mcp/messages?sessionId={}",
                relay_url, session_id
            ))
            .header("x-auth", server_auth)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            202,
            "MCP messages should return 202 Accepted"
        );
        eprintln!("  [4] MCP messages returns 202, SSE push flow works");

        drop(body_stream); // close SSE connection after POST

        // ── 5. Auth rejection ─────────────────────────────────────
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"terminal:output","session_id":"nope","payload":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            401,
            "Non-register messages without auth should be 401"
        );

        server_handle.abort();
        eprintln!("  PASS — all 5 steps succeeded");
    }
}
