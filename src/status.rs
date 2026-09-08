//! 连接状态共享：agent 核心写入（连接阶段/会话 id/token/延迟），TUI 界面
//! 轮询渲染。token 手动刷新信号也经由这里传递（TUI `r` → request_refresh →
//! 会话重建 → 同 session id 重新注册拿到新 token）。
//!
//! 全部内容仅在 `tui` feature 下编译（lean CLI 不引入）。ratatui 只承担渲染，
//! 状态模型本身零额外依赖。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// 连接阶段（TUI 状态行展示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 启动中（尚未建立会话）
    Starting,
    /// 正在注册/连 relay
    Connecting,
    /// 会话已建立（正常转发）
    Connected,
    /// 断线/重连/刷新中
    Reconnecting,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Starting => "启动中",
            Phase::Connecting => "连接中",
            Phase::Connected => "已连接",
            Phase::Reconnecting => "重连中",
        }
    }
}

impl Default for Phase {
    fn default() -> Self {
        Phase::Starting
    }
}

#[derive(Default)]
struct StatusInner {
    phase: Mutex<Phase>,
    session_id: Mutex<String>,
    tokens: Mutex<Vec<(String, String)>>,
    relay_url: String,
    latency_ms: Mutex<Option<u64>>,
    connected_at: Mutex<Option<Instant>>,
    /// 手动刷新 token 请求（TUI r 键置位；run_session/start 消费）。
    refresh_requested: AtomicBool,
    /// 手动停机请求（TUI q/Ctrl-C 置位；run_session 退出 + start 返回）。
    shutdown_requested: AtomicBool,
}

/// 与 agent 核心共享的只读状态句柄（Arc 克隆廉价，Send+Sync）。
#[derive(Clone)]
pub struct AgentStatus {
    inner: std::sync::Arc<StatusInner>,
}

impl AgentStatus {
    pub fn new(relay_url: String) -> Self {
        Self {
            inner: std::sync::Arc::new(StatusInner {
                relay_url,
                ..Default::default()
            }),
        }
    }

    pub fn set_phase(&self, phase: Phase) {
        *self.inner.phase.lock().unwrap() = phase;
    }

    pub fn phase(&self) -> Phase {
        *self.inner.phase.lock().unwrap()
    }

    /// 会话建立成功：记录实际 session id 与 token（刷新后新 token 在此更新）。
    pub fn set_connected(&self, session_id: &str, tokens: Vec<(String, String)>) {
        *self.inner.session_id.lock().unwrap() = session_id.to_string();
        *self.inner.tokens.lock().unwrap() = tokens;
        *self.inner.connected_at.lock().unwrap() = Some(Instant::now());
        *self.inner.phase.lock().unwrap() = Phase::Connected;
    }

    pub fn session_id(&self) -> String {
        self.inner.session_id.lock().unwrap().clone()
    }

    pub fn tokens(&self) -> Vec<(String, String)> {
        self.inner.tokens.lock().unwrap().clone()
    }

    pub fn relay_url(&self) -> &str {
        &self.inner.relay_url
    }

    /// 由 sender_loop 心跳 ping 往返测得（约每个心跳周期更新一次）。
    pub fn set_latency_ms(&self, ms: u64) {
        *self.inner.latency_ms.lock().unwrap() = Some(ms);
    }

    pub fn latency_ms(&self) -> Option<u64> {
        *self.inner.latency_ms.lock().unwrap()
    }

    /// 本次连接已持续秒数（未连接 → None）。
    pub fn connected_secs(&self) -> Option<u64> {
        self.inner
            .connected_at
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs())
    }

    /// 请求刷新 token（手动，会话 id 保持不变）。幂等：重复置位无害。
    pub fn request_refresh(&self) {
        self.inner.refresh_requested.store(true, Ordering::Relaxed);
    }

    /// 是否收到刷新请求（run_session 消费用，只读不消费）。
    pub fn refresh_pending(&self) -> bool {
        self.inner.refresh_requested.load(Ordering::Relaxed)
    }

    /// 消费刷新请求（start 用：返回是否本次退出由刷新触发）。
    pub fn take_refresh(&self) -> bool {
        self.inner.refresh_requested.swap(false, Ordering::Relaxed)
    }

    /// 请求干净停机（TUI q / Ctrl-C；agent 正常退出回收 PTY 子进程）。
    pub fn request_shutdown(&self) {
        self.inner.shutdown_requested.store(true, Ordering::Relaxed);
    }

    /// 是否收到停机请求（run_session 消费用，只读不消费）。
    pub fn shutdown_pending(&self) -> bool {
        self.inner.shutdown_requested.load(Ordering::Relaxed)
    }

    /// 消费停机请求（start 用：为 true 则结束重连循环，进程正常退出）。
    pub fn take_shutdown(&self) -> bool {
        self.inner.shutdown_requested.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connected_state_and_tokens() {
        let st = AgentStatus::new("https://relay.example.com".into());
        assert_eq!(st.phase(), Phase::Starting);
        st.set_connected("sess1", vec![("rw".into(), "abc".into())]);
        assert_eq!(st.phase(), Phase::Connected);
        assert_eq!(st.session_id(), "sess1");
        assert_eq!(st.tokens(), vec![("rw".into(), "abc".into())]);
        assert_eq!(st.relay_url(), "https://relay.example.com");
        assert!(st.connected_secs().is_some());
    }

    #[test]
    fn test_refresh_signal_is_latched_and_consumed() {
        let st = AgentStatus::new(String::new());
        assert!(!st.refresh_pending());
        assert!(!st.take_refresh());

        st.request_refresh();
        assert!(st.refresh_pending(), "refresh flag must latch");
        // 未消费前仍保持
        assert!(st.refresh_pending());
        // start 消费后复位
        assert!(st.take_refresh());
        assert!(!st.take_refresh(), "second take must be false");
        assert!(!st.refresh_pending());
    }

    #[test]
    fn test_latency_roundtrip() {
        let st = AgentStatus::new(String::new());
        assert_eq!(st.latency_ms(), None);
        st.set_latency_ms(23);
        assert_eq!(st.latency_ms(), Some(23));
    }

    #[test]
    fn test_shutdown_signal_latched_and_consumed() {
        let st = AgentStatus::new(String::new());
        assert!(!st.shutdown_pending());
        st.request_shutdown();
        assert!(st.shutdown_pending());
        assert!(st.take_shutdown());
        assert!(!st.take_shutdown());
    }
}