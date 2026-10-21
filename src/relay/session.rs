use rand::Rng;
use std::collections::HashMap;
use tokio::sync::RwLock;

use crate::proto::{DeviceInfo, Permission};

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub tokens: Vec<(String, Permission)>,
    #[allow(dead_code)]
    pub fixed_key: Option<String>,
    pub is_temporary: bool,
    /// Best-effort host probe reported by the agent at registration. `None`
    /// for older agents / failed probes.
    pub device: Option<DeviceInfo>,
    /// Agent binary version reported at registration (`CARGO_PKG_VERSION`).
    /// `None` for older agents that predate version reporting.
    pub agent_version: Option<String>,
    /// Agent 能力声明（R5#44 capability 协商最小子集）：注册时携带的
    /// 能力字符串列表（`codec:av1` / `backend:x11` / `desktop:gray` 等）。
    /// 老版本 agent 不报 → 空。admin overview 展示，浏览器可据此协商。
    pub capabilities: Vec<String>,
    /// 阶段2 LAN 直连地址（"ip:port"）：agent 注册消息带 `lan_addr` 后存此，
    /// 浏览器经 agent 的 `desktop:capabilities.lan_addrs` 拿到做同网段探测
    /// 直连（不经过 relay）。老版本 / 未开启 `--desktop-lan-port` → None。
    pub lan_addr: Option<String>,
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
        //
        // 两把锁一次性按 `sessions → token_map` 顺序取写锁并持有到 token_map
        // 更新完毕：此前在每个被驱逐会话上反复 `token_map.read()` 再单独取
        // 写锁，中间窗口能读到「会话已换、token 还指向旧 sid」的撕裂状态。
        // 全仓库统一这个加锁顺序（反序会与这里互锁）。
        let mut evicted = false;
        {
            let mut sessions = self.sessions.write().await;
            let mut tmap = self.token_map.write().await;
            // If any of our tokens already maps to a *different* session, that
            // session is displaced too (token reuse → newest wins).
            for (t, _) in &tokens {
                let old_sid = tmap.get(t).map(|(sid, _)| sid.clone());
                if let Some(old_sid) = old_sid {
                    if old_sid != session_id {
                        if let Some(old_info) = sessions.remove(&old_sid) {
                            for (ot, _) in &old_info.tokens {
                                tmap.remove(ot);
                            }
                            evicted = true;
                        }
                    }
                }
            }
            if let Some(old_info) = sessions.remove(&session_id) {
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
                    device: None,
                    agent_version: None,
                    capabilities: Vec::new(),
                    lan_addr: None,
                },
            );
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

    /// 只查 token 属于哪个会话（不关心权限）。用于凭证变更后定位需要断开
    /// 的浏览器流。
    pub async fn session_of_token(&self, token: &str) -> Option<String> {
        self.token_map
            .read()
            .await
            .get(token)
            .map(|(sid, _)| sid.clone())
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
            let mut tmap = self.token_map.write().await;
            // Displace any session that currently holds one of our tokens.
            for (t, _) in &tokens {
                let old_sid = tmap.get(t).map(|(sid, _)| sid.clone());
                if let Some(old_sid) = old_sid {
                    if old_sid != session_id {
                        if let Some(old_info) = sessions.remove(&old_sid) {
                            for (ot, _) in &old_info.tokens {
                                tmap.remove(ot);
                            }
                            evicted = true;
                        }
                    }
                }
            }
            if let Some(old_info) = sessions.remove(&session_id) {
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
                    device: None,
                    agent_version: None,
                    capabilities: Vec::new(),
                    lan_addr: None,
                },
            );
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

    /// R5#44 capability 协商最小子集：注册后更新会话能力声明（agent 注册
    /// 消息带 `capabilities` 数组，如 `codec:av1` / `backend:x11` /
    /// `desktop:gray`）。会话不存在时静默。admin overview 展示，浏览器可
    /// 据此协商能力。
    pub async fn update_capabilities(&self, session_id: &str, capabilities: Vec<String>) {
        let mut sessions = self.sessions.write().await;
        if let Some(info) = sessions.get_mut(session_id) {
            info.capabilities = capabilities;
        }
    }

    /// 读会话能力声明（无会话/未声明 → 空）。
    pub async fn get_capabilities(&self, session_id: &str) -> Vec<String> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .map(|i| i.capabilities.clone())
            .unwrap_or_default()
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

    /// Store/replace the device probe reported by an agent at registration.
    /// Registered separately from `register` so the registry's token flow stays
    /// untouched — the agent sends `device` alongside `agent:register` and the
    /// relay wires it in right after the session is minted.
    pub async fn set_device(&self, session_id: &str, device: Option<DeviceInfo>) {
        if let Some(info) = self.sessions.write().await.get_mut(session_id) {
            info.device = device;
        }
    }

    /// Store/replace the agent version reported at registration (drives the
    /// admin device panel's version column and upgrade completion display).
    pub async fn set_agent_version(&self, session_id: &str, version: Option<String>) {
        if let Some(info) = self.sessions.write().await.get_mut(session_id) {
            info.agent_version = version;
        }
    }

    /// 阶段2 LAN 直连：存/换注册消息携带的 `lan_addr`（"ip:port"，浏览器
    /// 同网段探测直连地址）。与 capabilities 一样在 registration 后单独
    /// 录入，避免扰动 register 的 token 流程。会话不存在时静默。
    pub async fn set_lan_addr(&self, session_id: &str, lan_addr: Option<String>) {
        if let Some(info) = self.sessions.write().await.get_mut(session_id) {
            info.lan_addr = lan_addr;
        }
    }

    /// Look up one session's info (used by the admin upgrade trigger to learn
    /// the device arch / platform and pick the matching artifact).
    pub async fn get(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions.read().await.get(session_id).cloned()
    }

    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// 注册抢占的归属证明（`agent:register` 在同一 sid 已有**在线** agent 时
    /// 使用）：请求必须能证明自己就是该会话的主人，否则是 id 抢占。
    ///
    /// 认可两种证明：
    /// - 请求的 fixed key（`--key`）与该会话登记的 `fixed_key` 相同；
    /// - 请求携带的缓存 tokens 中至少一个仍属于该会话。
    ///
    /// 会话不存在 → 返回 true（没有可抢占的对象，正常新建）。
    pub async fn can_reclaim(
        &self,
        session_id: &str,
        fixed_key: Option<&str>,
        cached_tokens: Option<&[(String, Permission)]>,
    ) -> bool {
        let sessions = self.sessions.read().await;
        let Some(info) = sessions.get(session_id) else {
            return true;
        };
        if let (Some(req_key), Some(cur_key)) = (fixed_key, info.fixed_key.as_deref()) {
            if req_key == cur_key {
                return true;
            }
        }
        if let Some(tokens) = cached_tokens {
            return tokens.iter().any(|(t, _)| {
                info.tokens
                    .iter()
                    .any(|(existing, _)| crate::relay::auth::constant_time_eq(existing, t))
            });
        }
        false
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
    ///
    /// 两把锁按 `sessions → token_map` 顺序同时持有（与 `register` 一致），
    /// 避免中途被读到「token 失效但会话仍列着它」的撕裂状态。
    pub async fn revoke_token(&self, token: &str) -> bool {
        let mut sessions = self.sessions.write().await;
        let mut tmap = self.token_map.write().await;
        let Some((sid, _)) = tmap.remove(token) else {
            return false;
        };
        if let Some(info) = sessions.get_mut(&sid) {
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
        // 两把锁一次性按 `sessions → token_map` 顺序取得（与 `register`
        // 一致），旧 token 的移除与新 token 的安装在同一个临界区内完成。
        let mut sessions = self.sessions.write().await;
        let mut tmap = self.token_map.write().await;
        let old_tokens: Vec<String> = sessions
            .get(session_id)?
            .tokens
            .iter()
            .map(|(t, _)| t.clone())
            .collect();
        if old_tokens.is_empty() {
            return None;
        }
        let new_tokens: Vec<(String, Permission)> = sessions
            .get(session_id)?
            .tokens
            .iter()
            .map(|(_, p)| (generate_token(), p.clone()))
            .collect();

        for t in &old_tokens {
            tmap.remove(t);
        }
        for (t, p) in &new_tokens {
            tmap.insert(t.clone(), (session_id.to_string(), p.clone()));
        }
        if let Some(info) = sessions.get_mut(session_id) {
            info.tokens = new_tokens.clone();
        }
        Some(new_tokens)
    }

    /// Flip a single token's permission (rw <-> ro) in both the token map and
    /// its session entry. Returns false if the token was unknown.
    pub async fn set_token_permission(&self, token: &str, perm: Permission) -> bool {
        let mut sessions = self.sessions.write().await;
        let mut tmap = self.token_map.write().await;
        let Some((sid, _)) = tmap.get(token).cloned() else {
            return false;
        };
        if let Some(entry) = tmap.get_mut(token) {
            entry.1 = perm.clone();
        }
        if let Some(info) = sessions.get_mut(&sid) {
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
        assert!(list[0].1.device.is_none(), "no device reported by default");
    }

    /// 阶段2 LAN 直连：agent 注册消息带 `lan_addr`（"ip:port"）后，relay 存进
    /// session 表可读回；未上报保持 None（老版本 agent / 未开启
    /// --desktop-lan-port 时无此字段 → None）。浏览器经 desktop:capabilities
    /// 拿到该地址做同网段探测直连。
    #[tokio::test]
    async fn test_register_stores_lan_addr() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", None).await.unwrap();
        let sid = r.session_id.clone();
        assert!(
            registry.get(&sid).await.unwrap().lan_addr.is_none(),
            "未上报 lan_addr 应为 None（老版本/未开启 LAN 直连）"
        );
        registry
            .set_lan_addr(&sid, Some("192.168.1.5:43210".to_string()))
            .await;
        assert_eq!(
            registry.get(&sid).await.unwrap().lan_addr.as_deref(),
            Some("192.168.1.5:43210")
        );
        // 覆盖（agent 重启换端口后再次上报）→ 新值生效。
        registry
            .set_lan_addr(&sid, Some("10.0.0.8:51234".to_string()))
            .await;
        assert_eq!(
            registry.get(&sid).await.unwrap().lan_addr.as_deref(),
            Some("10.0.0.8:51234")
        );
        // 清除（关闭 LAN 直连）→ None。
        registry.set_lan_addr(&sid, None).await;
        assert!(registry.get(&sid).await.unwrap().lan_addr.is_none());
    }

    #[tokio::test]
    async fn test_set_device_stores_and_replaces() {
        let registry = SessionRegistry::new();
        let r = registry.register(None, "rw", Some("dev01".to_string())).await.unwrap();
        let sid = r.session_id.clone();
        // Unknown session is a silent no-op.
        registry.set_device("nope", Some(DeviceInfo::default())).await;

        let di = DeviceInfo {
            hostname: Some("box1".to_string()),
            platform: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
            os: Some("Linux".to_string()),
            kernel: Some("6.1.0".to_string()),
            cpu_model: Some("Intel(R) Xeon(R)".to_string()),
        };
        registry.set_device(&sid, Some(di.clone())).await;
        let list = registry.list_sessions().await;
        assert_eq!(list[0].1.device, Some(di));

        // Clear it.
        registry.set_device(&sid, None).await;
        let list = registry.list_sessions().await;
        assert!(list[0].1.device.is_none());
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

    // ── A5 原子性 / 加锁顺序 ──────────────────────────────────────

    /// `register` 在同一临界区内完成「驱逐旧会话 + 安装新 token」：并发读
    /// 的观察者永远看不到「会话已换、token 还指向旧 sid」的撕裂状态。
    #[tokio::test]
    async fn test_register_is_atomic_under_concurrent_reads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let registry = std::sync::Arc::new(SessionRegistry::new());
        let first = registry
            .register(Some("stable-key".to_string()), "rw", Some("atomic1".to_string()))
            .await
            .unwrap();
        let old_token = first.tokens[0].0.clone();

        let done = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let reg = registry.clone();
            let done = done.clone();
            let token = old_token.clone();
            tokio::spawn(async move {
                let mut torn = 0;
                while !done.load(Ordering::Relaxed) {
                    // 每次读：token 要么还在（指向已有会话），要么已消失。
                    // 「token 在但会话不在」就是撕裂。
                    if let Some((sid, _)) = reg.authenticate(&token).await {
                        if reg.get(&sid).await.is_none() {
                            torn += 1;
                        }
                    }
                    tokio::task::yield_now().await;
                }
                torn
            })
        };
        // 并发地反复重注册同一 id / 同一 key（每次都会驱逐重建）。
        for _ in 0..40 {
            registry
                .register(Some("stable-key".to_string()), "rw", Some("atomic1".to_string()))
                .await
                .unwrap();
        }
        done.store(true, Ordering::Relaxed);
        let torn = reader.await.unwrap();
        assert_eq!(torn, 0, "token→session must never be observed in a torn state");
    }

    /// 全仓库统一「sessions → token_map」加锁顺序：反序调用会在高并发下
    /// 互相死锁。这里用超时兜住——若发生反序，注册与凭证操作并发会卡住。
    #[tokio::test]
    async fn test_no_lock_order_inversion_under_concurrency() {
        let registry = std::sync::Arc::new(SessionRegistry::new());
        let r = registry
            .register(None, "both", Some("lockord1".to_string()))
            .await
            .unwrap();
        let token = r.tokens[0].0.clone();
        let sid = r.session_id.clone();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let reg = registry.clone();
                let token = token.clone();
                let sid = sid.clone();
                tokio::spawn(async move {
                    for _ in 0..20 {
                        match i % 4 {
                            0 => {
                                let _ = reg
                                    .register(None, "rw", Some(format!("lo{:05}", i)))
                                    .await;
                            }
                            1 => {
                                let _ = reg.register_existing(
                                    vec![(format!("tok-{i}"), Permission::ReadWrite)],
                                    Some(sid.clone()),
                                )
                                .await;
                            }
                            2 => {
                                let _ = reg.revoke_token(&format!("tok-{i}")).await;
                                let _ = reg.session_of_token(&token).await;
                            }
                            _ => {
                                let _ = reg
                                    .set_token_permission(&token, Permission::ReadWrite)
                                    .await;
                                let _ = reg.list_sessions().await;
                            }
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            tokio::time::timeout(std::time::Duration::from_secs(20), h)
                .await
                .expect("lock ordering must not deadlock")
                .unwrap();
        }
    }

    // ── A4 归属证明 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_can_reclaim_accepts_fixed_key_or_cached_token() {
        let registry = SessionRegistry::new();
        let r = registry
            .register(Some("my-key".to_string()), "rw", Some("claim01".to_string()))
            .await
            .unwrap();
        let token = r.tokens[0].0.clone();

        // 同 fixed key → 可接管
        assert!(registry.can_reclaim("claim01", Some("my-key"), None).await);
        // 错的 key、无 token → 不可接管
        assert!(!registry.can_reclaim("claim01", Some("other"), None).await);
        assert!(!registry.can_reclaim("claim01", None, None).await);
        // 缓存 token 命中 → 可接管
        assert!(
            registry
                .can_reclaim(
                    "claim01",
                    None,
                    Some(&[(token.clone(), Permission::ReadWrite)])
                )
                .await
        );
        // 不属于该会话的 token → 不可接管
        assert!(
            !registry
                .can_reclaim(
                    "claim01",
                    None,
                    Some(&[("someone-elses".to_string(), Permission::ReadWrite)])
                )
                .await
        );
        // 会话根本不存在 → 无需证明（正常新建）
        assert!(registry.can_reclaim("brand-new", None, None).await);
    }

    #[tokio::test]
    async fn test_can_reclaim_ignores_key_when_session_has_none() {
        // 临时会话（无 fixed key）不能被「随便报个 key」认领。
        let registry = SessionRegistry::new();
        registry
            .register(None, "rw", Some("nokey01".to_string()))
            .await
            .unwrap();
        assert!(!registry.can_reclaim("nokey01", Some("anything"), None).await);
    }
}
