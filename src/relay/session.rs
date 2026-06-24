use rand::Rng;
use std::collections::HashMap;
use tokio::sync::RwLock;

use crate::proto::Permission;

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub tokens: Vec<(String, Permission)>,
    #[allow(dead_code)]
    pub fixed_key: Option<String>,
    pub is_temporary: bool,
}

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

    pub async fn register(
        &self,
        fixed_key: Option<String>,
        token_type: &str,
    ) -> (String, Vec<(String, Permission)>) {
        let tokens: Vec<(String, Permission)> = if let Some(ref key) = fixed_key {
            let mut result = vec![(key.clone(), Permission::ReadWrite)];
            if token_type == "both" {
                let ro_token = generate_token();
                result.push((ro_token, Permission::ReadOnly));
            } else if token_type == "ro" {
                result = vec![(key.clone(), Permission::ReadOnly)];
            }
            result
        } else {
            let rw_token = generate_token();
            let mut result = vec![(rw_token.clone(), Permission::ReadWrite)];
            if token_type == "both" {
                let ro_token = generate_token();
                result.push((ro_token.clone(), Permission::ReadOnly));
            } else if token_type == "ro" {
                result = vec![(rw_token, Permission::ReadOnly)];
            }
            result
        };

        let session_id = generate_session_id();
        let is_temporary = fixed_key.is_none();

        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionInfo {
                    tokens: tokens.clone(),
                    fixed_key,
                    is_temporary,
                },
            );
        }

        {
            let mut tmap = self.token_map.write().await;
            for (token, perm) in &tokens {
                tmap.insert(token.clone(), (session_id.clone(), perm.clone()));
            }
        }

        (session_id, tokens)
    }

    pub async fn authenticate(&self, token: &str) -> Option<(String, Permission)> {
        let tmap = self.token_map.read().await;
        tmap.get(token).cloned()
    }

    /// Re-register an agent that already holds a set of tokens (e.g. on
    /// auto-reconnect). A fresh session_id is issued, but the supplied tokens
    /// are reused verbatim so clients/browsers that cached them keep working.
    /// The session is temporary so idle cleanup can still reap it.
    pub async fn register_existing(
        &self,
        tokens: Vec<(String, Permission)>,
    ) -> (String, Vec<(String, Permission)>) {
        let session_id = generate_session_id();

        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(
                session_id.clone(),
                SessionInfo {
                    tokens: tokens.clone(),
                    fixed_key: None,
                    is_temporary: true,
                },
            );
        }
        {
            let mut tmap = self.token_map.write().await;
            for (token, perm) in &tokens {
                tmap.insert(token.clone(), (session_id.clone(), perm.clone()));
            }
        }

        (session_id, tokens)
    }

    pub async fn remove(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(info) = sessions.remove(session_id) {
            let mut tmap = self.token_map.write().await;
            for (token, _) in &info.tokens {
                tmap.remove(token);
            }
        }
    }

    pub async fn is_temporary(&self, session_id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .map(|s| s.is_temporary)
            .unwrap_or(false)
    }

    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 32] = rng.gen();
    hex::encode(bytes)
}

fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 4] = rng.gen();
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_temporary() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "rw").await;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].1, Permission::ReadWrite);
        assert!(registry.is_temporary(&_session_id).await);
    }

    #[tokio::test]
    async fn test_register_both_token_types() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "both").await;
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].1, Permission::ReadWrite);
        assert_eq!(tokens[1].1, Permission::ReadOnly);
        assert_ne!(tokens[0].0, tokens[1].0);
    }

    #[tokio::test]
    async fn test_register_ro_only() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "ro").await;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].1, Permission::ReadOnly);
    }

    #[tokio::test]
    async fn test_register_fixed_key() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry
            .register(Some("my-secret-key".to_string()), "rw")
            .await;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].0, "my-secret-key");
        assert_eq!(tokens[0].1, Permission::ReadWrite);
    }

    #[tokio::test]
    async fn test_register_fixed_key_both() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry
            .register(Some("my-secret-key".to_string()), "both")
            .await;
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].0, "my-secret-key");
        assert_eq!(tokens[0].1, Permission::ReadWrite);
        assert_eq!(tokens[1].1, Permission::ReadOnly);
        assert_ne!(tokens[1].0, "my-secret-key");
    }

    #[tokio::test]
    async fn test_authenticate_valid_token() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "rw").await;
        let result = registry.authenticate(&tokens[0].0).await;
        assert!(result.is_some());
        let (sid, perm) = result.unwrap();
        assert_eq!(sid, _session_id);
        assert_eq!(perm, Permission::ReadWrite);
    }

    #[tokio::test]
    async fn test_authenticate_invalid_token() {
        let registry = SessionRegistry::new();
        let result = registry.authenticate("nonexistent").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_authenticate_ro_token() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "both").await;
        let result = registry.authenticate(&tokens[1].0).await;
        assert!(result.is_some());
        let (_sid, perm) = result.unwrap();
        assert_eq!(perm, Permission::ReadOnly);
    }

    #[tokio::test]
    async fn test_remove_session() {
        let registry = SessionRegistry::new();
        let (session_id, tokens) = registry.register(None, "rw").await;
        registry.remove(&session_id).await;
        let result = registry.authenticate(&tokens[0].0).await;
        assert!(result.is_none());
        assert!(!registry.is_temporary(&session_id).await);
    }

    #[tokio::test]
    async fn test_is_temporary_false_for_fixed_key() {
        let registry = SessionRegistry::new();
        let (session_id, _tokens) = registry.register(Some("key".to_string()), "rw").await;
        assert!(!registry.is_temporary(&session_id).await);
    }

    #[tokio::test]
    async fn test_token_hex_format() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "rw").await;
        let token = &tokens[0].0;
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn test_register_existing_reuses_tokens() {
        let registry = SessionRegistry::new();
        let reused = vec![
            ("cached-rw-token".to_string(), Permission::ReadWrite),
            ("cached-ro-token".to_string(), Permission::ReadOnly),
        ];
        let (sid, tokens) = registry.register_existing(reused.clone()).await;
        // Tokens come back unchanged
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].0, "cached-rw-token");
        assert_eq!(tokens[1].0, "cached-ro-token");
        // Both authenticate to the new session
        let (s1, p1) = registry.authenticate("cached-rw-token").await.unwrap();
        let (s2, _p2) = registry.authenticate("cached-ro-token").await.unwrap();
        assert_eq!(s1, sid);
        assert_eq!(s2, sid);
        assert_eq!(p1, Permission::ReadWrite);
        assert!(registry.is_temporary(&sid).await);
    }

    #[tokio::test]
    async fn test_register_existing_overwrites_old_mapping() {
        let registry = SessionRegistry::new();
        let (old_sid, _t) = registry
            .register_existing(vec![("shared-token".to_string(), Permission::ReadWrite)])
            .await;
        // Re-register same token: a new session wins the mapping
        let (new_sid, _t) = registry
            .register_existing(vec![("shared-token".to_string(), Permission::ReadWrite)])
            .await;
        assert_ne!(old_sid, new_sid);
        let (resolved, _) = registry.authenticate("shared-token").await.unwrap();
        assert_eq!(resolved, new_sid);
    }
}
