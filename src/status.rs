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
    /// 最近一次需要用户看见的连接错误（MYS-2806 阶段4）。
    ///
    /// TUI 只画状态，不需要它；GUI 配置窗口必须能把"为什么连不上"留在窗口里
    /// （任务卡：错误留在窗口中，不闪退）。只存一条，新错误覆盖旧错误，连接
    /// 成功即由 [`AgentStatus::clear_error`] 清掉。
    last_error: Mutex<Option<String>>,
    /// 本次连接的**活跃配置快照**（MYS-2819）。
    ///
    /// 只由 GUI 控制层在 `connect()` 时写入一次，且写的是**当次连接实际使用的
    /// 那份配置**——界面之后怎么改草稿都与它无关。自升级交接用它，而不是去读
    /// 磁盘上的普通 `ui-config.json`：那份文件可能是用户后来改过的草稿，也可能是
    /// 上个世纪的内容。
    ///
    /// 放在这里而不是某个进程级静态量：一个 `AgentStatus` 对应一个连接实例，
    /// 多个控制器（测试里并行跑的那些）互不干扰。
    gui_snapshot: Mutex<Option<crate::agent::update_apply::GuiHandoffConfig>>,
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
        // 活跃配置快照里的 session id 跟着**实际** id 走（MYS-2819）。
        //
        // `connect()` 时写入的是用户那份草稿：GUI 留空时它没有 id，真正的 id
        // 由 relay 分配、只有这里才知道。不在这里同步，快照就会带着空 id 进入
        // 升级交接，新进程恢复时便无处可回——那正是"静默变成另一台设备"。
        if let Some(snapshot) = self
            .inner
            .gui_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            snapshot.session_id = Some(session_id.to_string());
        }
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

    /// 记下一条要展示给用户的连接错误（只留最后一条）。
    pub fn set_error(&self, message: impl Into<String>) {
        *self.inner.last_error.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(message.into());
    }

    /// 最近一次连接错误（GUI 每帧读它渲染到窗口里）。
    pub fn last_error(&self) -> Option<String> {
        self.inner
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 连接成功 / 重新开始连接时清掉旧错误（旧原因已不再成立）。
    pub fn clear_error(&self) {
        *self.inner.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 记下**本次连接实际使用的**配置快照（MYS-2819）。
    ///
    /// GUI 控制层在 `connect()` 里写入一次；之后界面怎么改草稿都不会影响它。
    /// 自升级交接读的就是这份，而不是磁盘上的 `ui-config.json`。
    ///
    /// 快照里没有固定密钥：密钥继续走既有受保护通道（交接文件的 `fixed_key`），
    /// 不放进这个可以被随手 `{:?}` 打印的结构。
    pub fn set_gui_snapshot(&self, snapshot: crate::agent::update_apply::GuiHandoffConfig) {
        *self
            .inner
            .gui_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(snapshot);
    }

    /// 本次连接的活动配置快照；非 GUI 连接恒为 `None`。
    pub fn gui_snapshot(&self) -> Option<crate::agent::update_apply::GuiHandoffConfig> {
        self.inner
            .gui_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

    #[test]
    fn test_error_is_last_wins_and_clearable() {
        // GUI 只展示一条原因：后到的覆盖先到的，连接成功就清空。
        let st = AgentStatus::new(String::new());
        assert_eq!(st.last_error(), None);
        st.set_error("第一次失败");
        assert_eq!(st.last_error().as_deref(), Some("第一次失败"));
        st.set_error("第二次失败");
        assert_eq!(st.last_error().as_deref(), Some("第二次失败"));
        st.clear_error();
        assert_eq!(st.last_error(), None);
        // 清空之后再写的错误仍可见（不是一次性 latch）。
        st.set_error("第三次失败");
        assert_eq!(st.last_error().as_deref(), Some("第三次失败"));
    }
}