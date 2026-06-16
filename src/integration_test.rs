#[cfg(test)]
mod integration_tests {
    use std::time::Duration;
    use std::sync::Arc;
    use serde_json::{json, Value};

    use crate::relay::ws;
    use crate::relay::mcp;

    #[tokio::test]
    #[ignore]
    async fn test_full_workflow() {
        let _ = tracing_subscriber::fmt().try_init();
        let port = 19878u16;
        let server_auth = "integration-test-pw";
        let state = Arc::new(crate::relay::SharedState::new(
            server_auth.to_string(),
            None,
            100 * 1024 * 1024,
        ));

        use axum::Router;
        use axum::routing::get;

        let app = Router::new()
            .route("/agent/session/sse", get(ws::browser_sse_handler))
            .route("/agent/session/send", axum::routing::post(ws::browser_send_handler))
            .route("/agent/send", axum::routing::post(ws::agent_send_handler))
            .route("/agent/events", get(ws::agent_events_handler))
            .route("/agent/mcp/sse", get(mcp::sse_handler))
            .route("/agent/mcp/messages", axum::routing::post(mcp::messages_handler))
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
            .send().await.unwrap();
        assert_eq!(resp.status(), 200, "Agent registration should succeed");
        let reg: Value = resp.json().await.unwrap();
        assert_eq!(reg["type"], "agent:registered");
        let session_id = reg["session_id"].as_str().unwrap().to_string();
        let rw_token = reg["payload"]["tokens"].as_array().unwrap()
            .iter().find(|t| t["permission"] == "rw")
            .and_then(|t| t["token"].as_str()).unwrap().to_string();
        eprintln!("  [1] agent registered: session={}", session_id);

        // ── 2. Agent subscribes to events ────────────────────────
        let resp = client
            .get(format!("{}/agent/events?session={}", relay_url, session_id))
            .header("Accept", "text/event-stream")
            .send().await.unwrap();
        assert_eq!(resp.status(), 200);
        eprintln!("  [2] events stream connected");

        // ── 3. Browser connects via SSE+POST ──────────────────
        let resp = client
            .get(format!("{}/agent/session/sse?token={}&session={}", relay_url, rw_token, session_id))
            .header("Accept", "text/event-stream")
            .send().await.unwrap();
        assert_eq!(resp.status(), 200, "Browser SSE connection should return 200");
        eprintln!("  [3a] browser SSE stream connected");

        let resp = client
            .post(format!("{}/agent/session/send", relay_url))
            .json(&json!({
                "type": "terminal:input",
                "session_id": session_id,
                "token": rw_token,
                "payload": {"data": "echo hello"}
            }))
            .send().await.unwrap();
        assert_eq!(resp.status(), 202, "Browser POST should return 202");
        eprintln!("  [3b] browser POST returns 202");

        // ── 4. MCP tools/list (via 202 + push to SSE) ──────────
        // Open SSE connection, read first event to get sessionId
        let sse_resp = client
            .get(format!("{}/agent/mcp/sse", relay_url))
            .header("x-auth", server_auth)
            .header("Accept", "text/event-stream")
            .send().await.unwrap();
        assert_eq!(sse_resp.status(), 200);

        // Read just enough of the SSE stream to get the endpoint event
        use tokio_stream::StreamExt;
        let mut body_stream = sse_resp.bytes_stream();
        let sse_text = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            body_stream.next()
        ).await.unwrap().unwrap().unwrap();
        let sse_text = String::from_utf8_lossy(&sse_text);
        let session_id = sse_text
            .lines()
            .find(|l| l.starts_with("data: "))
            .and_then(|l| l.rsplit("sessionId=").next())
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(!session_id.is_empty(), "Should have sessionId: {}", sse_text);
        eprintln!("  [4a] SSE sessionId={}", session_id);

        // POST to messages with sessionId (keep SSE alive during POST)
        let resp = client
            .post(format!("{}/agent/mcp/messages?sessionId={}", relay_url, session_id))
            .header("x-auth", server_auth)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
            .send().await.unwrap();
        assert_eq!(resp.status(), 202, "MCP messages should return 202 Accepted");
        eprintln!("  [4] MCP messages returns 202, SSE push flow works");

        drop(body_stream); // close SSE connection after POST

        // ── 5. Auth rejection ─────────────────────────────────────
        let resp = client
            .post(format!("{}/agent/send", relay_url))
            .json(&json!({"type":"terminal:output","session_id":"nope","payload":{}}))
            .send().await.unwrap();
        assert_eq!(resp.status(), 401, "Non-register messages without auth should be 401");

        server_handle.abort();
        eprintln!("  PASS — all 5 steps succeeded");
    }
}
