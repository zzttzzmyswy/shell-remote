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
    /// Admin-assigned labels on a session (e.g. "prod", "db"). In-memory only;
    /// cleared on relay restart. Duplicates are not stored.
    pub tags: Vec<String>,
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
                    tags: Vec::new(),
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
                    tags: Vec::new(),
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

    /// Snapshot of all sessions for the admin overview. Clones session info
    /// (tokens, permissions, fixed_key, temporary flag).
    pub async fn list_sessions(&self) -> Vec<(String, SessionInfo)> {
        self.sessions
            .read()
            .await
            .iter()
            .map(|(id, info)| (id.clone(), info.clone()))
            .collect()
    }

    /// Remove a single token from both the token map and its session's token
    /// list. Returns false if the token was unknown. Browsers/MCP clients
    /// using it will fail to authenticate on their next request.
    pub async fn revoke_token(&self, token: &str) -> bool {
        let sid = self
            .token_map
            .read()
            .await
            .get(token)
            .map(|(sid, _)| sid.clone());
        let Some(sid) = sid else { return false };
        self.token_map.write().await.remove(token);
        if let Some(info) = self.sessions.write().await.get_mut(&sid) {
            info.tokens.retain(|(t, _)| t != token);
        }
        true
    }

    /// Mint a fresh set of tokens for a session (preserving each existing
    /// permission slot), invalidate the old tokens, and return the new set.
    /// Returns None if the session is unknown or has no tokens. For fixed-key
    /// sessions this also replaces the fixed key — the agent must be restarted
    /// / reconnected with the new credentials.
    pub async fn regenerate_session(
        &self,
        session_id: &str,
    ) -> Option<Vec<(String, Permission)>> {
        let perms: Vec<Permission> = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_id)?
                .tokens
                .iter()
                .map(|(_, p)| p.clone())
                .collect()
        };
        if perms.is_empty() {
            return None;
        }
        let new_tokens: Vec<(String, Permission)> = perms
            .iter()
            .map(|p| (generate_token(), p.clone()))
            .collect();

        // token_map: drop old tokens for this session, insert new ones.
        {
            let old: Vec<String> = {
                let sessions = self.sessions.read().await;
                sessions
                    .get(session_id)
                    .map(|i| i.tokens.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>())
                    .unwrap_or_default()
            };
            let mut tmap = self.token_map.write().await;
            for t in &old {
                tmap.remove(t);
            }
            for (t, p) in &new_tokens {
                tmap.insert(t.clone(), (session_id.to_string(), p.clone()));
            }
        }

        // session: replace the token list.
        if let Some(info) = self.sessions.write().await.get_mut(session_id) {
            info.tokens = new_tokens.clone();
        }
        Some(new_tokens)
    }

    /// Flip a single token's permission (rw <-> ro) in both the token map and
    /// its session entry. Returns false if the token was unknown.
    pub async fn set_token_permission(&self, token: &str, perm: Permission) -> bool {
        let sid = self
            .token_map
            .read()
            .await
            .get(token)
            .map(|(sid, _)| sid.clone());
        let Some(sid) = sid else { return false };
        if let Some(entry) = self.token_map.write().await.get_mut(token) {
            entry.1 = perm.clone();
        }
        if let Some(info) = self.sessions.write().await.get_mut(&sid) {
            if let Some(e) = info.tokens.iter_mut().find(|(t, _)| t == token) {
                e.1 = perm;
            }
        }
        true
    }

    /// Add a label to a session. The tag is trimmed; empty tags are ignored.
    /// Returns false if the session is unknown. Duplicate tags are not added.
    pub async fn add_tag(&self, session_id: &str, tag: &str) -> bool {
        let tag = tag.trim();
        if tag.is_empty() {
            return false;
        }
        let mut sessions = self.sessions.write().await;
        let Some(info) = sessions.get_mut(session_id) else {
            return false;
        };
        if !info.tags.iter().any(|t| t == tag) {
            info.tags.push(tag.to_string());
        }
        true
    }

    /// Remove a label from a session. Returns false if the session is unknown.
    pub async fn remove_tag(&self, session_id: &str, tag: &str) -> bool {
        let tag = tag.trim();
        let mut sessions = self.sessions.write().await;
        let Some(info) = sessions.get_mut(session_id) else {
            return false;
        };
        info.tags.retain(|t| t != tag);
        true
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

    #[tokio::test]
    async fn test_list_sessions() {
        let registry = SessionRegistry::new();
        let (sid, _t) = registry.register(None, "both").await;
        let list = registry.list_sessions().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, sid);
        assert_eq!(list[0].1.tokens.len(), 2);
    }

    #[tokio::test]
    async fn test_revoke_token() {
        let registry = SessionRegistry::new();
        let (_sid, tokens) = registry.register(None, "both").await;
        assert!(registry.revoke_token(&tokens[0].0).await);
        assert!(registry.authenticate(&tokens[0].0).await.is_none());
        // the other token still works
        assert!(registry.authenticate(&tokens[1].0).await.is_some());
        // unknown token
        assert!(!registry.revoke_token("nope").await);
    }

    #[tokio::test]
    async fn test_regenerate_session() {
        let registry = SessionRegistry::new();
        let (sid, tokens) = registry.register(None, "rw").await;
        let new_tokens = registry.regenerate_session(&sid).await.unwrap();
        assert_eq!(new_tokens.len(), 1);
        assert_ne!(new_tokens[0].0, tokens[0].0);
        // old token invalidated
        assert!(registry.authenticate(&tokens[0].0).await.is_none());
        // new token authenticates to same session
        let (resolved, perm) = registry.authenticate(&new_tokens[0].0).await.unwrap();
        assert_eq!(resolved, sid);
        assert_eq!(perm, Permission::ReadWrite);
        // unknown session
        assert!(registry.regenerate_session("deadbeef").await.is_none());
    }

    #[tokio::test]
    async fn test_set_token_permission() {
        let registry = SessionRegistry::new();
        let (_sid, tokens) = registry.register(None, "rw").await;
        assert!(registry
            .set_token_permission(&tokens[0].0, Permission::ReadOnly)
            .await);
        let (_sid, perm) = registry.authenticate(&tokens[0].0).await.unwrap();
        assert_eq!(perm, Permission::ReadOnly);
        // flip back
        registry
            .set_token_permission(&tokens[0].0, Permission::ReadWrite)
            .await;
        let (_, perm) = registry.authenticate(&tokens[0].0).await.unwrap();
        assert_eq!(perm, Permission::ReadWrite);
        // unknown token
        assert!(!registry.set_token_permission("nope", Permission::ReadOnly).await);
    }

    #[tokio::test]
    async fn test_add_and_remove_tag() {
        let registry = SessionRegistry::new();
        let (sid, _t) = registry.register(None, "rw").await;
        assert!(registry.add_tag(&sid, "prod").await);
        assert!(registry.add_tag(&sid, "  db  ").await); // trimmed
        // duplicate is a no-op (still one "prod")
        registry.add_tag(&sid, "prod").await;
        let list = registry.list_sessions().await;
        let info = list.iter().find(|(s, _)| s == &sid).unwrap();
        assert_eq!(info.1.tags, vec!["prod".to_string(), "db".to_string()]);

        // empty tag ignored
        assert!(!registry.add_tag(&sid, "   ").await);

        // remove
        assert!(registry.remove_tag(&sid, "prod").await);
        let list = registry.list_sessions().await;
        let info = list.iter().find(|(s, _)| s == &sid).unwrap();
        assert_eq!(info.1.tags, vec!["db".to_string()]);

        // unknown session
        assert!(!registry.add_tag("deadbeef", "x").await);
        assert!(!registry.remove_tag("deadbeef", "x").await);
    }
}
