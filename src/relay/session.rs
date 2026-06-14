use std::collections::HashMap;
use tokio::sync::RwLock;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum Permission {
    ReadWrite,
    ReadOnly,
}

#[allow(dead_code)]
pub struct SessionInfo {
    pub tokens: Vec<(String, Permission)>,
    pub fixed_key: Option<String>,
    pub is_temporary: bool,
}

#[allow(dead_code)]
pub struct SessionRegistry {
    sessions: RwLock<HashMap<String, SessionInfo>>,
    token_map: RwLock<HashMap<String, (String, Permission)>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            token_map: RwLock::new(HashMap::new()),
        }
    }
}
