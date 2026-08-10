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
    /// The requested custom id failed validation (must be 5-20 alphanumeric).
    InvalidId,
}

/// Result of a successful registration. `evicted` is true when an existing
/// session with the same `session_id` (or sharing any of the new tokens) was
/// displaced — i.e. a new agent incarnation took over the identity, and the
/// old session's tokens were invalidated.
#[derive(Debug, Clone)]
pub struct RegisterResult {
    pub session_id: String,
    pub tokens: Vec<(String, Permission)>,
    pub evicted: bool,
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
    ) -> Result<RegisterResult, RegisterError> {
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

        // Session id is reusable: a new agent may take over an existing
        // session_id (agent restart, second device reusing the identity).
        // Any existing session under this id — or any session currently
        // holding one of the tokens we're about to install — is evicted so the
        // new incarnation fully owns the identity. No 409 "id in use" anymore.
        let mut evicted = false;
        {
            let mut sessions = self.sessions.write().await;
            // If any of our tokens already maps to a *different* session, that
            // session is displaced too (token reuse → newest wins).
            for (t, _) in &tokens {
                let old_sid = self.token_map.read().await.get(t).map(|(sid, _)| sid.clone());
                if let Some(old_sid) = old_sid {
                    if old_sid != session_id {
                        if let Some(old_info) = sessions.remove(&old_sid) {
                            let mut tmap = self.token_map.write().await;
                            for (ot, _) in &old_info.tokens {
                                tmap.remove(ot);
                            }
                            evicted = true;
                        }
                    }
                }
            }
            if let Some(old_info) = sessions.remove(&session_id) {
                let mut tmap = self.token_map.write().await;
                for (t, _) in &old_info.tokens {
                    tmap.remove(t);
                }
                evicted = true;
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

        Ok(RegisterResult {
            session_id,
            tokens,
            evicted,
        })
    }

    pub async fn authenticate(&self, token: &str) -> Option<(String, Permission)> {
        let tmap = self.token_map.read().await;
        tmap.get(token).cloned()
    }

    /// Re-register an agent that already holds a set of tokens (e.g. on
    /// auto-reconnect). The supplied tokens are reused verbatim so
    /// clients/browsers that cached them keep working. The session is
    /// temporary so idle cleanup can still reap it. Like [`register`], any
    /// existing session under the same id (or holding any of the tokens) is
    /// evicted — newest incarnation wins.
    pub async fn register_existing(
        &self,
        tokens: Vec<(String, Permission)>,
        desired_id: Option<String>,
    ) -> Result<RegisterResult, RegisterError> {
        let session_id = match desired_id {
            Some(ref id) if crate::proto::is_valid_custom_session_id(id) => id.clone(),
            Some(_) => return Err(RegisterError::InvalidId),
            None => generate_session_id(),
        };

        let mut evicted = false;
        {
            let mut sessions = self.sessions.write().await;
            // Displace any session that currently holds one of our tokens.
            for (t, _) in &tokens {
                let old_sid = self.token_map.read().await.get(t).map(|(sid, _)| sid.clone());
                if let Some(old_sid) = old_sid {
                    if old_sid != session_id {
                        if let Some(old_info) = sessions.remove(&old_sid) {
                            let mut tmap = self.token_map.write().await;
                            for (ot, _) in &old_info.tokens {
                                tmap.remove(ot);
                            }
                            evicted = true;
                        }
                    }
                }
            }
            if let Some(old_info) = sessions.remove(&session_id) {
                let mut tmap = self.token_map.write().await;
                for (t, _) in &old_info.tokens {
                    tmap.remove(t);
                }
                evicted = true;
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

        Ok(RegisterResult {
            session_id,
            tokens,
            evicted,
        })
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
        let r = registry.register(None, "rw", None).await.unwrap();
        assert_eq!(r.tokens.len(), 1);
        assert_eq!(r.tokens[0].1, Permission::ReadWrite);
        assert!(registry.is_temporary(&r.session_id).await);
    }

    #[tokio::test]
    async fn test_register_both_token_types() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "both", None).await.unwrap();
        assert_eq!(r.tokens.len(), 2);
        assert_eq!(r.tokens[0].1, Permission::ReadWrite);
        assert_eq!(r.tokens[1].1, Permission::ReadOnly);
        assert_ne!(r.tokens[0].0, r.tokens[1].0);
    }

    #[tokio::test]
    async fn test_register_ro_only() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "ro", None).await.unwrap();
        assert_eq!(r.tokens.len(), 1);
        assert_eq!(r.tokens[0].1, Permission::ReadOnly);
    }

    #[tokio::test]
    async fn test_register_fixed_key() {
        let registry = SessionRegistry::new();
        let r = registry
            .register(Some("my-secret-key".to_string()), "rw", None)
            .await
            .unwrap();
        assert_eq!(r.tokens.len(), 1);
        assert_eq!(r.tokens[0].0, "my-secret-key");
        assert_eq!(r.tokens[0].1, Permission::ReadWrite);
    }

    #[tokio::test]
    async fn test_register_fixed_key_both() {
        let registry = SessionRegistry::new();
        let r = registry
            .register(Some("my-secret-key".to_string()), "both", None)
            .await
            .unwrap();
        assert_eq!(r.tokens.len(), 2);
        assert_eq!(r.tokens[0].0, "my-secret-key");
        assert_eq!(r.tokens[0].1, Permission::ReadWrite);
        assert_eq!(r.tokens[1].1, Permission::ReadOnly);
        assert_ne!(r.tokens[1].0, "my-secret-key");
    }

    #[tokio::test]
    async fn test_register_with_custom_id() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", Some("mydev01".to_string())).await.unwrap();
        assert_eq!(r.session_id, "mydev01");
        assert!(!r.evicted, "first registration evicts nothing");
    }

    #[tokio::test]
    async fn test_register_same_id_evicts_previous() {
        // Session ids are reusable: a second agent claiming the same id
        // takes over (evicts) the previous session instead of failing.
        let registry = SessionRegistry::new();
        let first = registry.register(None, "rw", Some("mydev01".to_string())).await.unwrap();
        let old_token = first.tokens[0].0.clone();

        let second = registry.register(None, "rw", Some("mydev01".to_string())).await.unwrap();
        assert_eq!(second.session_id, "mydev01");
        assert!(second.evicted, "re-registering an in-use id must evict the old session");
        // Old tokens invalidated
        assert!(registry.authenticate(&old_token).await.is_none());
    }

    #[tokio::test]
    async fn test_register_same_token_evicts_old_session() {
        // Token reuse: a new session using an already-registered token
        // displaces the session that owned it (newest wins).
        let registry = SessionRegistry::new();
        let first = registry.register(Some("shared-key".to_string()), "rw", None).await.unwrap();
        let sid1 = first.session_id.clone();

        let second = registry.register(Some("shared-key".to_string()), "rw", None).await.unwrap();
        assert_ne!(second.session_id, sid1);
        assert!(second.evicted, "reusing a token must evict the old session");
        // Old session gone
        assert!(registry.authenticate("shared-key").await.is_some());
        assert!(!registry.sessions.read().await.contains_key(&sid1));
    }

    #[tokio::test]
    async fn test_register_fixed_key_reclaims_id_after_restart() {
        // An agent restarted with the same --key + --session-id reclaims its
        // id, evicting the prior incarnation.
        let registry = SessionRegistry::new();
        let r1 = registry
            .register(Some("fixed-key-X".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(r1.session_id, "dev01");
        assert_eq!(r1.tokens[0].0, "fixed-key-X");

        let r2 = registry
            .register(Some("fixed-key-X".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(r2.session_id, "dev01");
        assert_eq!(r2.tokens[0].0, "fixed-key-X");
        assert!(r2.evicted, "restart reclaims id by evicting the stale incarnation");
        let (resolved, _) = registry.authenticate("fixed-key-X").await.unwrap();
        assert_eq!(resolved, "dev01");
    }

    #[tokio::test]
    async fn test_register_different_key_evicts_and_takes_over() {
        // A *different* fixed key claiming an in-use id now takes over rather
        // than failing — the id is reusable across devices/keys.
        let registry = SessionRegistry::new();
        let _r1 = registry
            .register(Some("key-A".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap();
        let r2 = registry
            .register(Some("key-B".to_string()), "rw", Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(r2.session_id, "dev01");
        assert!(r2.evicted, "different key reusing id must evict old session");
        // key-A no longer authenticates; key-B does
        assert!(registry.authenticate("key-A").await.is_none());
        let (resolved, _) = registry.authenticate("key-B").await.unwrap();
        assert_eq!(resolved, "dev01");
    }

    #[tokio::test]
    async fn test_register_fixed_key_both_reclaims_and_rotates_ro() {
        // token_type=both: the fixed rw key reclaims the id; the random ro
        // token is rotated (old one invalidated, new one minted).
        let registry = SessionRegistry::new();
        let r1 = registry
            .register(Some("fixed".to_string()), "both", Some("dev01".to_string()))
            .await
            .unwrap();
        let old_ro = r1.tokens.iter().find(|(_, p)| *p == Permission::ReadOnly).unwrap().0.clone();
        assert!(registry.authenticate(&old_ro).await.is_some());

        let r2 = registry
            .register(Some("fixed".to_string()), "both", Some("dev01".to_string()))
            .await
            .unwrap();
        let new_ro = r2.tokens.iter().find(|(_, p)| *p == Permission::ReadOnly).unwrap().0.clone();
        assert_ne!(old_ro, new_ro);
        assert!(registry.authenticate(&old_ro).await.is_none());
        let (r1_, _) = registry.authenticate("fixed").await.unwrap();
        let (r2_, _) = registry.authenticate(&new_ro).await.unwrap();
        assert_eq!(r1_, "dev01");
        assert_eq!(r2_, "dev01");
    }

    #[tokio::test]
    async fn test_register_invalid_id_rejected() {
        let registry = SessionRegistry::new();
        let err = registry.register(None, "rw", Some("ab!".to_string())).await.unwrap_err();
        assert!(matches!(err, RegisterError::InvalidId));
        let r = registry.register(None, "rw", None).await.unwrap();
        assert!(!r.session_id.is_empty());
    }

    #[tokio::test]
    async fn test_register_existing_evicts_same_tokens() {
        let registry = SessionRegistry::new();
        let r1 = registry.register(None, "rw", Some("dev01".to_string())).await.unwrap();
        assert_eq!(r1.session_id, "dev01");
        let cached = r1.tokens.clone();
        let r2 = registry
            .register_existing(cached.clone(), Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(r2.session_id, "dev01");
        assert!(r2.evicted);
        let (resolved, _) = registry.authenticate(&cached[0].0).await.unwrap();
        assert_eq!(resolved, "dev01");
    }

    #[tokio::test]
    async fn test_register_existing_different_tokens_evicts() {
        // A different device (different tokens) claiming the same id now
        // takes over (evicts) the old session — no 409.
        let registry = SessionRegistry::new();
        let r1 = registry.register(None, "rw", Some("dev01".to_string())).await.unwrap();
        let other = vec![("other-token-xx".to_string(), Permission::ReadWrite)];
        let r2 = registry
            .register_existing(other.clone(), Some("dev01".to_string()))
            .await
            .unwrap();
        assert_eq!(r2.session_id, "dev01");
        assert!(r2.evicted);
        // Old session's token invalidated
        assert!(registry.authenticate(&r1.tokens[0].0).await.is_none());
        assert!(registry.authenticate("other-token-xx").await.is_some());
    }

    #[tokio::test]
    async fn test_authenticate_valid_token() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", None).await.unwrap();
        let result = registry.authenticate(&r.tokens[0].0).await;
        assert!(result.is_some());
        let (sid, perm) = result.unwrap();
        assert_eq!(sid, r.session_id);
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
        let r = registry.register(None, "both", None).await.unwrap();
        let result = registry.authenticate(&r.tokens[1].0).await;
        assert!(result.is_some());
        let (_sid, perm) = result.unwrap();
        assert_eq!(perm, Permission::ReadOnly);
    }

    #[tokio::test]
    async fn test_remove_session() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", None).await.unwrap();
        registry.remove(&r.session_id).await;
        let result = registry.authenticate(&r.tokens[0].0).await;
        assert!(result.is_none());
        assert!(!registry.is_temporary(&r.session_id).await);
    }

    #[tokio::test]
    async fn test_is_temporary_false_for_fixed_key() {
        let registry = SessionRegistry::new();
        let r = registry.register(Some("key".to_string()), "rw", None).await.unwrap();
        assert!(!registry.is_temporary(&r.session_id).await);
    }

    #[tokio::test]
    async fn test_token_hex_format() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", None).await.unwrap();
        let token = &r.tokens[0].0;
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
        let r = registry.register_existing(reused.clone(), None).await.unwrap();
        assert_eq!(r.tokens.len(), 2);
        assert_eq!(r.tokens[0].0, "cached-rw-token");
        assert_eq!(r.tokens[1].0, "cached-ro-token");
        let (s1, p1) = registry.authenticate("cached-rw-token").await.unwrap();
        let (s2, _p2) = registry.authenticate("cached-ro-token").await.unwrap();
        assert_eq!(s1, r.session_id);
        assert_eq!(s2, r.session_id);
        assert_eq!(p1, Permission::ReadWrite);
        assert!(registry.is_temporary(&r.session_id).await);
    }

    #[tokio::test]
    async fn test_register_existing_overwrites_old_mapping() {
        let registry = SessionRegistry::new();
        let r1 = registry
            .register_existing(vec![("shared-token".to_string(), Permission::ReadWrite)], None)
            .await
            .unwrap();
        let r2 = registry
            .register_existing(vec![("shared-token".to_string(), Permission::ReadWrite)], None)
            .await
            .unwrap();
        assert_ne!(r1.session_id, r2.session_id);
        assert!(r2.evicted, "re-registering a reused token evicts the prior session");
        let (resolved, _) = registry.authenticate("shared-token").await.unwrap();
        assert_eq!(resolved, r2.session_id);
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "both", None).await.unwrap();
        let list = registry.list_sessions().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, r.session_id);
        assert_eq!(list[0].1.tokens.len(), 2);
    }

    #[tokio::test]
    async fn test_revoke_token() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "both", None).await.unwrap();
        assert!(registry.revoke_token(&r.tokens[0].0).await);
        assert!(registry.authenticate(&r.tokens[0].0).await.is_none());
        assert!(registry.authenticate(&r.tokens[1].0).await.is_some());
        assert!(!registry.revoke_token("nope").await);
    }

    #[tokio::test]
    async fn test_regenerate_session() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", None).await.unwrap();
        let new_tokens = registry.regenerate_session(&r.session_id).await.unwrap();
        assert_eq!(new_tokens.len(), 1);
        assert_ne!(new_tokens[0].0, r.tokens[0].0);
        assert!(registry.authenticate(&r.tokens[0].0).await.is_none());
        let (resolved, perm) = registry.authenticate(&new_tokens[0].0).await.unwrap();
        assert_eq!(resolved, r.session_id);
        assert_eq!(perm, Permission::ReadWrite);
        assert!(registry.regenerate_session("deadbeef").await.is_none());
    }

    #[tokio::test]
    async fn test_set_token_permission() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", None).await.unwrap();
        assert!(registry
            .set_token_permission(&r.tokens[0].0, Permission::ReadOnly)
            .await);
        let (_sid, perm) = registry.authenticate(&r.tokens[0].0).await.unwrap();
        assert_eq!(perm, Permission::ReadOnly);
        registry
            .set_token_permission(&r.tokens[0].0, Permission::ReadWrite)
            .await;
        let (_, perm) = registry.authenticate(&r.tokens[0].0).await.unwrap();
        assert_eq!(perm, Permission::ReadWrite);
        assert!(!registry.set_token_permission("nope", Permission::ReadOnly).await);
    }
}
