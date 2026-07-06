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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The requested custom id is already held by a different live session.
    IdTaken,
    /// The requested custom id failed validation (must be 5-20 alphanumeric).
    InvalidId,
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
        desired_id: Option<String>,
    ) -> Result<(String, Vec<(String, Permission)>), RegisterError> {
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

        let session_id = match desired_id {
            Some(ref id) if crate::proto::is_valid_custom_session_id(id) => id.clone(),
            Some(_) => return Err(RegisterError::InvalidId),
            None => generate_session_id(),
        };
        let is_temporary = fixed_key.is_none();

        {
            let mut sessions = self.sessions.write().await;
            if sessions.contains_key(&session_id) {
                // Reclaim across a process restart: if any of the tokens we're
                // about to install already maps to the existing session (i.e.
                // a fixed key being re-used by the same agent restarting with
                // the same --key + --session-id), evict the stale incarnation
                // and re-register instead of rejecting with 409. Without this
                // an agent restarted with a fixed key could never reclaim its
                // id — its ghost session lingers in the registry until idle-
                // reaped, so every fresh `register` hits IdTaken. A random-
                // token agent that lost its tokens on restart can't prove
                // ownership (its new random token doesn't map to the existing
                // session) and still gets IdTaken, which is the safe behavior.
                let same_agent = {
                    let tmap = self.token_map.read().await;
                    tokens.iter().any(|(t, _)| {
                        tmap.get(t)
                            .map(|(sid, _)| sid == &session_id)
                            .unwrap_or(false)
                    })
                };
                if !same_agent {
                    return Err(RegisterError::IdTaken);
                }
                // Evict the stale prior incarnation: drop the old session
                // entry and clear its tokens from the token_map so the new
                // minted set replaces them cleanly.
                if let Some(old_info) = sessions.remove(&session_id) {
                    let mut tmap = self.token_map.write().await;
                    for (t, _) in &old_info.tokens {
                        tmap.remove(t);
                    }
                }
            }
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

        Ok((session_id, tokens))
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
        desired_id: Option<String>,
    ) -> Result<(String, Vec<(String, Permission)>), RegisterError> {
        let session_id = match desired_id {
            Some(ref id) if crate::proto::is_valid_custom_session_id(id) => id.clone(),
            Some(_) => return Err(RegisterError::InvalidId),
            None => generate_session_id(),
        };

        {
            let mut sessions = self.sessions.write().await;
            if sessions.contains_key(&session_id) {
                // Evict only if this is the same logical session resuming
                // (one of our cached tokens already maps to that session).
                let same_session = {
                    let tmap = self.token_map.read().await;
                    tokens.iter().any(|(t, _)| {
                        tmap.get(t).map(|(sid, _)| sid == &session_id).unwrap_or(false)
                    })
                };
                if !same_session {
                    return Err(RegisterError::IdTaken);
                }
                // Evict the stale prior incarnation: remove the old session
                // entry and clear its tokens from the token_map.
                if let Some(old_info) = sessions.remove(&session_id) {
                    let mut tmap = self.token_map.write().await;
                    for (t, _) in &old_info.tokens {
                        tmap.remove(t);
                    }
                }
            }
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

        Ok((session_id, tokens))
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
        let (_session_id, tokens) = registry.register(None, "rw", None).await.unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].1, Permission::ReadWrite);
        assert!(registry.is_temporary(&_session_id).await);
    }

    #[tokio::test]
    async fn test_register_both_token_types() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "both", None).await.unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].1, Permission::ReadWrite);
        assert_eq!(tokens[1].1, Permission::ReadOnly);
        assert_ne!(tokens[0].0, tokens[1].0);
    }

    #[tokio::test]
    async fn test_register_ro_only() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "ro", None).await.unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].1, Permission::ReadOnly);
    }

    #[tokio::test]
    async fn test_register_fixed_key() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry
            .register(Some("my-secret-key".to_string()), "rw", None)
            .await
            .unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].0, "my-secret-key");
        assert_eq!(tokens[0].1, Permission::ReadWrite);
    }

    #[tokio::test]
    async fn test_register_fixed_key_both() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry
            .register(Some("my-secret-key".to_string()), "both", None)
            .await
            .unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].0, "my-secret-key");
        assert_eq!(tokens[0].1, Permission::ReadWrite);
        assert_eq!(tokens[1].1, Permission::ReadOnly);
        assert_ne!(tokens[1].0, "my-secret-key");
    }

    #[tokio::test]
    async fn test_register_with_custom_id() {
        let registry = SessionRegistry::new();
        let (sid, _t) = registry.register(None, "rw", Some("mydev01".to_string())).await.unwrap();
        assert_eq!(sid, "mydev01");
    }

    #[tokio::test]
    async fn test_register_custom_id_taken() {
        let registry = SessionRegistry::new();
        let _ = registry.register(None, "rw", Some("mydev01".to_string())).await.unwrap();
        let err = registry.register(None, "rw", Some("mydev01".to_string())).await.unwrap_err();
        assert!(matches!(err, RegisterError::IdTaken));
    }

    #[tokio::test]
    async fn test_register_fixed_key_reclaims_id_after_restart() {
        // An agent restarted with the same --key + --session-id must reclaim
        // its id (evict the ghost from the prior incarnation), not 409. This
        // is the user-reported scenario: fixed key + fixed session id still
        // hit "session_id already in use" across a process restart.
        let registry = SessionRegistry::new();
        let (sid1, t1) = registry
            .register(Some("fixed-key-X".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(sid1, "dev01");
        assert_eq!(t1[0].0, "fixed-key-X");

        // A fresh process re-registering with the same key + id: must succeed
        // (evicts the stale prior incarnation) instead of IdTaken.
        let (sid2, t2) = registry
            .register(Some("fixed-key-X".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(sid2, "dev01");
        assert_eq!(t2[0].0, "fixed-key-X");
        // The fixed key still authenticates to the (reclaimed) session.
        let (resolved, _) = registry.authenticate("fixed-key-X").await.unwrap();
        assert_eq!(resolved, "dev01");
    }

    #[tokio::test]
    async fn test_register_different_fixed_key_conflict() {
        // A different agent (different fixed key) trying to claim an in-use id
        // is still rejected — only the same key can reclaim.
        let registry = SessionRegistry::new();
        let _ = registry
            .register(Some("key-A".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap();
        let err = registry
            .register(Some("key-B".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap_err();
        assert!(matches!(err, RegisterError::IdTaken));
        // Original key still owns the session.
        let (resolved, _) = registry.authenticate("key-A").await.unwrap();
        assert_eq!(resolved, "dev01");
    }

    #[tokio::test]
    async fn test_register_fixed_key_both_reclaims_and_rotates_ro() {
        // token_type=both: the fixed rw key reclaims the id; the random ro
        // token is rotated (old one invalidated, new one minted).
        let registry = SessionRegistry::new();
        let (_sid, t1) = registry
            .register(Some("fixed".to_string()), "both", Some("dev01".to_string()))
            .await
            .unwrap();
        let old_ro = t1.iter().find(|(_, p)| *p == Permission::ReadOnly).unwrap().0.clone();
        assert!(registry.authenticate(&old_ro).await.is_some());

        let (_sid, t2) = registry
            .register(Some("fixed".to_string()), "both", Some("dev01".to_string()))
            .await
            .unwrap();
        let new_ro = t2.iter().find(|(_, p)| *p == Permission::ReadOnly).unwrap().0.clone();
        assert_ne!(old_ro, new_ro);
        // Old ro token invalidated by the eviction.
        assert!(registry.authenticate(&old_ro).await.is_none());
        // Fixed rw key + new ro both authenticate to the reclaimed session.
        let (r1, _) = registry.authenticate("fixed").await.unwrap();
        let (r2, _) = registry.authenticate(&new_ro).await.unwrap();
        assert_eq!(r1, "dev01");
        assert_eq!(r2, "dev01");
    }

    #[tokio::test]
    async fn test_register_invalid_id_rejected() {
        let registry = SessionRegistry::new();
        let err = registry.register(None, "rw", Some("ab!".to_string())).await.unwrap_err();
        assert!(matches!(err, RegisterError::InvalidId));
        // None still works (random id)
        let (_sid, _t) = registry.register(None, "rw", None).await.unwrap();
    }

    #[tokio::test]
    async fn test_register_existing_evicts_same_tokens() {
        let registry = SessionRegistry::new();
        let (sid, tokens) = registry.register(None, "rw", Some("dev01".to_string())).await.unwrap();
        assert_eq!(sid, "dev01");
        // reconnect with the same cached tokens + same id: evicts the stale
        // prior incarnation, returns the same id, tokens re-map to new session.
        let (sid2, _t2) = registry
            .register_existing(tokens.clone(), Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(sid2, "dev01");
        // cached token now authenticates to the (new) dev1 session
        let (resolved, _) = registry.authenticate(&tokens[0].0).await.unwrap();
        assert_eq!(resolved, "dev01");
    }

    #[tokio::test]
    async fn test_register_existing_conflict_different_tokens() {
        let registry = SessionRegistry::new();
        let _ = registry.register(None, "rw", Some("dev01".to_string())).await.unwrap();
        // a different device (different tokens) tries to claim the same id
        let other = vec![("other-token-xx".to_string(), Permission::ReadWrite)];
        let err = registry.register_existing(other, Some("dev01".to_string())).await.unwrap_err();
        assert!(matches!(err, RegisterError::IdTaken));
    }

    #[tokio::test]
    async fn test_authenticate_valid_token() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "rw", None).await.unwrap();
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
        let (_session_id, tokens) = registry.register(None, "both", None).await.unwrap();
        let result = registry.authenticate(&tokens[1].0).await;
        assert!(result.is_some());
        let (_sid, perm) = result.unwrap();
        assert_eq!(perm, Permission::ReadOnly);
    }

    #[tokio::test]
    async fn test_remove_session() {
        let registry = SessionRegistry::new();
        let (session_id, tokens) = registry.register(None, "rw", None).await.unwrap();
        registry.remove(&session_id).await;
        let result = registry.authenticate(&tokens[0].0).await;
        assert!(result.is_none());
        assert!(!registry.is_temporary(&session_id).await);
    }

    #[tokio::test]
    async fn test_is_temporary_false_for_fixed_key() {
        let registry = SessionRegistry::new();
        let (session_id, _tokens) = registry.register(Some("key".to_string()), "rw", None).await.unwrap();
        assert!(!registry.is_temporary(&session_id).await);
    }

    #[tokio::test]
    async fn test_token_hex_format() {
        let registry = SessionRegistry::new();
        let (_session_id, tokens) = registry.register(None, "rw", None).await.unwrap();
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
        let (sid, tokens) = registry.register_existing(reused.clone(), None).await.unwrap();
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
            .register_existing(vec![("shared-token".to_string(), Permission::ReadWrite)], None)
            .await
            .unwrap();
        // Re-register same token: a new session wins the mapping
        let (new_sid, _t) = registry
            .register_existing(vec![("shared-token".to_string(), Permission::ReadWrite)], None)
            .await
            .unwrap();
        assert_ne!(old_sid, new_sid);
        let (resolved, _) = registry.authenticate("shared-token").await.unwrap();
        assert_eq!(resolved, new_sid);
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let registry = SessionRegistry::new();
        let (sid, _t) = registry.register(None, "both", None).await.unwrap();
        let list = registry.list_sessions().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, sid);
        assert_eq!(list[0].1.tokens.len(), 2);
    }

    #[tokio::test]
    async fn test_revoke_token() {
        let registry = SessionRegistry::new();
        let (_sid, tokens) = registry.register(None, "both", None).await.unwrap();
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
        let (sid, tokens) = registry.register(None, "rw", None).await.unwrap();
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
        let (_sid, tokens) = registry.register(None, "rw", None).await.unwrap();
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
}
