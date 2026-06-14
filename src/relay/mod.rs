#![allow(unused_imports)]

pub mod auth;
pub mod mcp;
pub mod session;
pub mod ws;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};

#[allow(dead_code)]
use crate::relay::session::SessionRegistry;

#[allow(dead_code)]
pub struct ChannelMap {
    pub senders: Vec<mpsc::UnboundedSender<String>>,
}

#[allow(dead_code)]
impl ChannelMap {
    pub fn new() -> Self {
        Self { senders: vec![] }
    }
}

#[allow(dead_code)]
pub struct SharedState {
    pub sessions: SessionRegistry,
    pub agent_broadcast: RwLock<HashMap<String, ChannelMap>>,
    pub pending_mcp: RwLock<HashMap<String, oneshot::Sender<String>>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            sessions: SessionRegistry::new(),
            agent_broadcast: RwLock::new(HashMap::new()),
            pending_mcp: RwLock::new(HashMap::new()),
        }
    }
}

pub async fn start(
    bind: String,
    _tls_cert: Option<String>,
    _tls_key: Option<String>,
    _dev: bool,
) -> anyhow::Result<()> {
    use axum::Router;
    use tower_http::cors::{Any, CorsLayer};

    let state = Arc::new(SharedState::new());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // .route("/ws", get(ws::ws_handler))
        // .route("/mcp/sse", get(mcp::sse_handler))
        // .route("/mcp/messages", axum::routing::post(mcp::messages_handler))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("Relay server listening on {}", bind);

    axum::serve(listener, app).await?;

    Ok(())
}
