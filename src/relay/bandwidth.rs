//! Relay 整体带宽限制（MYS-2287）。
//!
//! 一条 relay 进程只有一份 [`BandwidthLimiter`]，里面是**两个方向各自独立**的
//! 令牌桶：
//!
//! - `down`：relay 出站（relay → 浏览器 / agent / 客户端）。
//! - `up`：relay 入站（agent 上行影片、浏览器与 MCP 上传、控制消息…）。
//!
//! 速率 0 表示不限制。不同流量的处理方式（见各接入点）：
//!
//! | 流量 | 方法 | 原因 |
//! |---|---|---|
//! | 文件/升级包/录像等可靠流 | [`TokenBucket::acquire`]（等待＝背压） | 只降速，不丢数据 |
//! | 桌面视频 | [`TokenBucket::try_acquire`]（不够即丢帧） | 视频宁可丢帧，靠关键帧恢复 |
//! | 终端/控制等交互消息 | [`TokenBucket::consume`]（只记账不等待） | 保交互响应 |
//!
//! 令牌桶容量 `burst = max(rate × 0.2s, 64 KiB)`：允许 200ms 的突发，避免把
//! 每一帧都拖成"等一块发一块"（那样会同时抬高时延和 CPU）。
//!
//! 公平性：`acquire` 用一把 tokio `Mutex` 串行化等待者（tokio 的锁是 FIFO），
//! 并在**持锁**状态下 sleep，因此排队顺序不会被后来者插队。持锁 sleep 期间
//! 只持队列锁，不持账本锁（账本用 `std::sync::Mutex` 做极短的算术），所以
//! [`TokenBucket::consume`] / [`TokenBucket::try_acquire`] 永远不会被等待者堵住。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::Instant;

/// Mbps → bytes/s。1 Mbit/s = 1_000_000 bit/s = 125_000 byte/s。
pub const MBPS_TO_BYTES_PER_SEC: f64 = 125_000.0;

/// admin 可配置的上限（Mbit/s）。
pub const MAX_MBPS: f64 = 100_000.0;

/// 桶容量下限：即使限速很低，也至少攒得下 64 KiB，否则小包会被拆得极碎。
const MIN_BURST_BYTES: f64 = 64.0 * 1024.0;

/// 桶容量按"多少秒的流量"计算。
const BURST_SECONDS: f64 = 0.2;

/// 单次 sleep 的上限。等待中若 `set_rate` 把速率改小/改成 0，等待者必须在
/// 有限时间内重新读速率，不能抱着一个算好的长 sleep 睡到底。
const MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);

/// 速率来源（admin GET/POST 的 `source` 字段）。启动优先级：
/// settings 文件 > CLI > 不限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateSource {
    File,
    Cli,
    Default,
}

impl RateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RateSource::File => "file",
            RateSource::Cli => "cli",
            RateSource::Default => "default",
        }
    }
}

/// `--settings-file` 的持久化结构。字段为 `Option`：文件里没写该字段时
/// 不得覆盖 CLI 值（优先级：文件字段 > CLI > 0），写回时两个字段都会写。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BandwidthSettingsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_down_mbps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_up_mbps: Option<f64>,
}

impl BandwidthSettingsFile {
    /// 读 settings 文件。文件不存在/不可读/JSON 非法一律当作"没有配置"
    /// （返回 `Default`）——一份坏文件不该让 relay 起不来。
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "bandwidth settings file is not valid JSON — ignored"
                );
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "cannot read bandwidth settings file — ignored"
                );
                Self::default()
            }
        }
    }

    /// 原子写入：先写同目录临时文件再 `rename` 覆盖，避免半个文件被读到。
    /// 目录不存在时自动创建。临时文件与本文件同目录，保证 rename 不跨设备。
    pub fn save(path: &Path, down_mbps: f64, up_mbps: f64) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let body = serde_json::to_vec_pretty(&Self {
            bandwidth_down_mbps: Some(down_mbps),
            bandwidth_up_mbps: Some(up_mbps),
        })?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, &body)?;
        // Windows 上 rename 覆盖已存在文件会失败，先尽力删目标。
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(path);
        }
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

/// 令牌桶账本。只在极短的临界区里用，不跨 await 持有。
struct State {
    tokens: f64,
    last: Instant,
}

/// 方向性限速器（一个方向一个桶）。
pub struct TokenBucket {
    /// bytes/s，0 = 不限。
    rate: AtomicU64,
    state: std::sync::Mutex<State>,
    /// 串行化 `acquire` 等待者（tokio Mutex = FIFO），持锁 sleep 保排队顺序。
    queue: Mutex<()>,
    /// `set_rate` 用它把正在 sleep 的等待者立刻叫醒重算。
    notify: Notify,
    /// 当前排队/正在等待的 acquire 数（>0 时后来者不再走快路径，避免插队
    /// 把排队者饿死）。
    waiters: AtomicUsize,
    /// 本方向累计记账字节（acquire / try_acquire / consume 全部计入）。
    total_bytes: AtomicU64,
    /// `acquire` 真正进入过等待的次数（同一个 acquire 可能 sleep 多轮，只计一次）。
    throttled_waits: AtomicU64,
}

impl TokenBucket {
    pub fn new(rate_bytes_per_sec: u64) -> Self {
        let rate = rate_bytes_per_sec;
        Self {
            rate: AtomicU64::new(rate),
            state: std::sync::Mutex::new(State {
                tokens: burst_for(rate),
                last: Instant::now(),
            }),
            queue: Mutex::new(()),
            notify: Notify::new(),
            waiters: AtomicUsize::new(0),
            total_bytes: AtomicU64::new(0),
            throttled_waits: AtomicU64::new(0),
        }
    }

    /// 即时生效地改速率（tokens 钳到新容量）。正在等待的 `acquire` 会被
    /// 唤醒并在 ≤[`MAX_SLEEP`] 内按新速率重算。
    ///
    /// 钳制是双向的：正额度截到新 burst，透支的欠账也截到 `-burst`。改速率是
    /// 一次"重新配置"，不该让上一次限速期间攒下的欠账继续压着新配置（否则
    /// admin 把限速从 8 调到 2 之后，前者期间 `consume` 记的账会凭空多压一段
    /// 等待）。`consume` 的即时欠账语义不受影响（2.6 测的是同一次配置内）。
    pub fn set_rate(&self, rate_bytes_per_sec: u64) {
        let old = self.rate.swap(rate_bytes_per_sec, Ordering::SeqCst);
        let now = Instant::now();
        if let Ok(mut st) = self.state.lock() {
            // 先用旧速率把已攒的额度结算掉（旧速率为 0 时不结算——那时 tokens
            // 不随时间增长）。
            if old > 0 {
                refill(&mut st, old, now, burst_for(old));
            }
            st.last = now;
            let cap = burst_for(rate_bytes_per_sec.max(1));
            st.tokens = st.tokens.clamp(-cap, cap);
        }
        self.notify.notify_waiters();
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::SeqCst)
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    pub fn throttled_waits(&self) -> u64 {
        self.throttled_waits.load(Ordering::Relaxed)
    }

    /// 只记账（不扣令牌、不等待）。
    fn account(&self, n: usize) {
        self.total_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// 等待式获取：令牌不足则 sleep 到够为止。
    ///
    /// 允许单次 `n` 大于桶容量（借贷）：容量按 `max(burst, n)` 临时提高，
    /// 这样"一次要 5MB 而桶只有 200KB"不会死等。
    pub async fn acquire(&self, n: usize) {
        if n == 0 {
            return;
        }
        if self.rate() == 0 {
            self.account(n);
            return;
        }
        // 快路径：没人在排队时才走，避免插队。
        if self.waiters.load(Ordering::SeqCst) == 0 && self.try_acquire(n) {
            return;
        }
        self.acquire_queued(n).await;
    }

    async fn acquire_queued(&self, n: usize) {
        /// 等待者计数在 future 被取消（超时/abort）时也要减回去，否则
        /// `waiters` 永久 >0，快路径会一直被关掉。
        struct WaiterGuard<'a>(&'a AtomicUsize);
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        self.waiters.fetch_add(1, Ordering::SeqCst);
        let _waiter = WaiterGuard(&self.waiters);
        let _gate = self.queue.lock().await;
        let mut counted = false;
        loop {
            let rate = self.rate();
            if rate == 0 {
                // 等待期间被改成不限：立即放行。
                self.account(n);
                break;
            }
            let deficit = {
                let mut st = match self.state.lock() {
                    Ok(st) => st,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let now = Instant::now();
                // 单次请求超过桶容量时按借贷处理：容量临时抬到 n。
                refill(&mut st, rate, now, burst_for(rate).max(n as f64));
                if st.tokens >= n as f64 {
                    st.tokens -= n as f64;
                    None
                } else {
                    Some(n as f64 - st.tokens)
                }
            };
            let Some(deficit) = deficit else {
                self.account(n);
                break;
            };
            if !counted {
                counted = true;
                self.throttled_waits.fetch_add(1, Ordering::Relaxed);
            }
            // 等 deficit 这么长（上限 MAX_SLEEP）。每轮新建 Notified：复用同一个
            // future 会在它已完成后立刻 Ready，变成空转的忙循环。上限保证即使
            // 通知丢失（notify_waiters 不存 permit）也最多 100ms 就重算速率。
            let notified = self.notify.notified();
            let secs = (deficit / rate as f64).min(MAX_SLEEP.as_secs_f64());
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs_f64(secs)) => {}
                _ = notified => {}
            }
        }
    }

    /// 非等待获取：够则扣减并返回 true；不够返回 false 且**不扣减**。
    pub fn try_acquire(&self, n: usize) -> bool {
        if n == 0 {
            return true;
        }
        let rate = self.rate();
        if rate == 0 {
            self.account(n);
            return true;
        }
        let mut st = match self.state.lock() {
            Ok(st) => st,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        refill(&mut st, rate, now, burst_for(rate));
        if st.tokens >= n as f64 {
            st.tokens -= n as f64;
            drop(st);
            self.account(n);
            true
        } else {
            false
        }
    }

    /// 只记账：直接扣减（可为负，形成"欠账"，后续 acquire 需等待还清），不等待。
    /// 用于交互消息——它们必须立刻放行，但占用过的带宽仍要计入用量。
    pub fn consume(&self, n: usize) {
        if n == 0 {
            return;
        }
        let rate = self.rate();
        if rate == 0 {
            self.account(n);
            return;
        }
        {
            let mut st = match self.state.lock() {
                Ok(st) => st,
                Err(poisoned) => poisoned.into_inner(),
            };
            refill(&mut st, rate, Instant::now(), burst_for(rate));
            st.tokens -= n as f64;
        }
        self.account(n);
    }
}

fn burst_for(rate: u64) -> f64 {
    (rate as f64 * BURST_SECONDS).max(MIN_BURST_BYTES)
}

/// 按速率把时间折算成令牌，并把总量钳到 `cap`（`cap` 一般就是桶容量；
/// 单次 acquire 超过容量时调用方会传入 `max(burst, n)`）。
fn refill(st: &mut State, rate: u64, now: Instant, cap: f64) {
    let elapsed = now.saturating_duration_since(st.last).as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }
    st.tokens = (st.tokens + elapsed * rate as f64).min(cap);
    st.last = now;
}

/// 上下行实时速率采样（1s / 10s 均值），由 1s ticker 维护。
#[derive(Default)]
struct Sampler {
    down_1s: AtomicU64,
    up_1s: AtomicU64,
    down_10s: AtomicU64,
    up_10s: AtomicU64,
}

/// 一次采样的只读快照（admin 展示 + overview 卡片）。
#[derive(Clone, Debug, Default)]
pub struct BandwidthStats {
    pub down_bps_1s: u64,
    pub up_bps_1s: u64,
    pub down_bps_10s: u64,
    pub up_bps_10s: u64,
    pub down_total_bytes: u64,
    pub up_total_bytes: u64,
    pub throttled_waits: u64,
    pub dropped_frames: u64,
}

impl BandwidthStats {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "down_bps_1s": self.down_bps_1s,
            "up_bps_1s": self.up_bps_1s,
            "down_bps_10s": self.down_bps_10s,
            "up_bps_10s": self.up_bps_10s,
            "down_total_bytes": self.down_total_bytes,
            "up_total_bytes": self.up_total_bytes,
            "throttled_waits": self.throttled_waits,
            "dropped_frames": self.dropped_frames,
        })
    }
}

/// relay 全局带宽限制器：上下行两个桶 + 统计 + 设置持久化。
pub struct BandwidthLimiter {
    pub down: TokenBucket,
    pub up: TokenBucket,
    sampler: Sampler,
    /// 上下行速率的来源（`file` / `cli` / `default`）。
    sources: RwLock<(RateSource, RateSource)>,
    /// `--settings-file` 路径；`None` 时 admin 的修改无法持久化（只留内存）。
    settings_path: RwLock<Option<PathBuf>>,
    /// 因带宽不足被丢弃的视频帧数（上下行合计）。
    dropped_frames: AtomicU64,
}

impl Default for BandwidthLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl BandwidthLimiter {
    /// 全不限。启动时的真实值由 [`apply_startup_rates`] 写入。
    pub fn new() -> Self {
        Self {
            down: TokenBucket::new(0),
            up: TokenBucket::new(0),
            sampler: Sampler::default(),
            sources: RwLock::new((RateSource::Default, RateSource::Default)),
            settings_path: RwLock::new(None),
            dropped_frames: AtomicU64::new(0),
        }
    }

    /// 直接指定初始速率（测试与启动期用）。单位 Mbit/s。
    pub fn with_mbps(down_mbps: f64, up_mbps: f64) -> Self {
        let me = Self::new();
        me.set_mbps(down_mbps, up_mbps);
        me
    }

    /// 即时生效地设置双向速率（Mbit/s）。
    pub fn set_mbps(&self, down_mbps: f64, up_mbps: f64) {
        self.down.set_rate(mbps_to_bps(down_mbps));
        self.up.set_rate(mbps_to_bps(up_mbps));
    }

    pub fn down_mbps(&self) -> f64 {
        self.down.rate() as f64 / MBPS_TO_BYTES_PER_SEC
    }

    pub fn up_mbps(&self) -> f64 {
        self.up.rate() as f64 / MBPS_TO_BYTES_PER_SEC
    }

    pub async fn set_settings_path(&self, path: PathBuf) {
        *self.settings_path.write().await = Some(path);
    }

    pub async fn settings_path(&self) -> Option<PathBuf> {
        self.settings_path.read().await.clone()
    }

    pub async fn sources(&self) -> (RateSource, RateSource) {
        *self.sources.read().await
    }

    pub async fn set_sources(&self, down: RateSource, up: RateSource) {
        *self.sources.write().await = (down, up);
    }

    /// 记一次视频丢帧（带宽不足，非"viewer 跟不上"）。
    pub fn count_dropped_frame(&self, n: u64) {
        self.dropped_frames.fetch_add(n, Ordering::Relaxed);
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> BandwidthStats {
        BandwidthStats {
            down_bps_1s: self.sampler.down_1s.load(Ordering::Relaxed),
            up_bps_1s: self.sampler.up_1s.load(Ordering::Relaxed),
            down_bps_10s: self.sampler.down_10s.load(Ordering::Relaxed),
            up_bps_10s: self.sampler.up_10s.load(Ordering::Relaxed),
            down_total_bytes: self.down.total_bytes(),
            up_total_bytes: self.up.total_bytes(),
            throttled_waits: self.down.throttled_waits() + self.up.throttled_waits(),
            dropped_frames: self.dropped_frames(),
        }
    }

    /// `GET/POST {ap}/api/bandwidth` 的响应体。
    pub async fn to_json(&self) -> serde_json::Value {
        let (down_src, up_src) = self.sources().await;
        serde_json::json!({
            "down_mbps": self.down_mbps(),
            "up_mbps": self.up_mbps(),
            "source": { "down": down_src.as_str(), "up": up_src.as_str() },
            "stats": self.stats().to_json(),
        })
    }

    /// 原子持久化当前速率。写失败时返回错误，**内存值保持生效**（调用方
    /// 据此回 500 + warning）。
    pub async fn persist(&self) -> std::io::Result<()> {
        let Some(path) = self.settings_path().await else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no settings file configured",
            ));
        };
        let (d, u) = (self.down_mbps(), self.up_mbps());
        // 文件 IO 放阻塞池：std::fs 在 async 上下文里同步调用会阻塞 worker。
        match tokio::task::spawn_blocking(move || BandwidthSettingsFile::save(&path, d, u)).await {
            Ok(r) => r,
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    }

    /// 启动 1s ticker：用累计字节的差分算 1s / 10s 平均速率。
    /// 每隔 1s 采样一次，10s 值取最近 10 个样本的平均。
    pub fn spawn_sampler(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut last = (me.down.total_bytes(), me.up.total_bytes());
            let mut down_hist: VecDeque<u64> = VecDeque::with_capacity(10);
            let mut up_hist: VecDeque<u64> = VecDeque::with_capacity(10);
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let now = (me.down.total_bytes(), me.up.total_bytes());
                let down_delta = now.0.saturating_sub(last.0);
                let up_delta = now.1.saturating_sub(last.1);
                last = now;
                down_hist.push_back(down_delta);
                up_hist.push_back(up_delta);
                if down_hist.len() > 10 {
                    down_hist.pop_front();
                }
                if up_hist.len() > 10 {
                    up_hist.pop_front();
                }
                me.sampler.down_1s.store(down_delta, Ordering::Relaxed);
                me.sampler.up_1s.store(up_delta, Ordering::Relaxed);
                let avg = |h: &VecDeque<u64>| -> u64 {
                    if h.is_empty() {
                        0
                    } else {
                        h.iter().sum::<u64>() / h.len() as u64
                    }
                };
                me.sampler
                    .down_10s
                    .store(avg(&down_hist), Ordering::Relaxed);
                me.sampler.up_10s.store(avg(&up_hist), Ordering::Relaxed);
            }
        });
    }
}

/// Mbit/s → bytes/s（四舍五入，负数按 0）。
pub fn mbps_to_bps(mbps: f64) -> u64 {
    if !mbps.is_finite() || mbps <= 0.0 {
        return 0;
    }
    (mbps * MBPS_TO_BYTES_PER_SEC).round() as u64
}

/// 启动期优先级解析：settings 文件里的字段 > CLI > 0（不限）。纯函数，便于单测。
pub fn resolve_rate(file: Option<f64>, cli: Option<f64>) -> (f64, RateSource) {
    match (file, cli) {
        (Some(v), _) => (v, RateSource::File),
        (None, Some(v)) => (v, RateSource::Cli),
        (None, None) => (0.0, RateSource::Default),
    }
}

/// 启动期初始化：读 settings 文件 → 按优先级与 CLI 合成 → 写入限速器并打日志。
/// `relay::start` 与测试共用这一条路径，保证"重启后值保持"的语义一致。
pub async fn apply_startup_rates(
    limiter: &Arc<BandwidthLimiter>,
    settings_path: PathBuf,
    cli_down: Option<f64>,
    cli_up: Option<f64>,
) {
    let file = BandwidthSettingsFile::load(&settings_path);
    let (down, down_src) = resolve_rate(file.bandwidth_down_mbps, cli_down);
    let (up, up_src) = resolve_rate(file.bandwidth_up_mbps, cli_up);
    limiter.set_sources(down_src, up_src).await;
    limiter.set_settings_path(settings_path.clone()).await;
    limiter.set_mbps(down, up);
    tracing::info!(
        down_mbps = down,
        down_source = down_src.as_str(),
        up_mbps = up,
        up_source = up_src.as_str(),
        settings = %settings_path.display(),
        "bandwidth limit in effect (0 = unlimited)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(rate: u64) -> TokenBucket {
        TokenBucket::new(rate)
    }

    /// 验收 2.1：rate=1MB/s 连续 acquire 共 5MB，虚拟时间应在 4.8s（5MB 减去
    /// 初始 200KB 突发额度）附近。
    #[tokio::test(start_paused = true)]
    async fn acquire_five_megabytes_at_one_megabyte_per_second() {
        let b = bucket(1_000_000);
        let start = Instant::now();
        let chunk = 256 * 1024;
        for _ in 0..20 {
            b.acquire(chunk).await;
        }
        let elapsed = start.elapsed().as_secs_f64();
        assert!(
            (4.7..=5.3).contains(&elapsed),
            "5MB @1MB/s 应耗时 ≈4.8s，实际 {elapsed}s"
        );
        assert_eq!(b.total_bytes(), 20 * chunk as u64);
    }

    /// 验收 2.2：rate=0 时不等待，try_acquire 恒真。
    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let b = bucket(0);
        let start = Instant::now();
        b.acquire(10 * 1024 * 1024).await;
        for _ in 0..100 {
            assert!(b.try_acquire(1024 * 1024));
        }
        assert_eq!(start.elapsed().as_secs_f64(), 0.0);
        assert_eq!(b.throttled_waits(), 0);
        // rate=0 也要记账（admin 的实时吞吐显示依赖它）。
        assert_eq!(b.total_bytes(), 10 * 1024 * 1024 + 100 * 1024 * 1024);
    }

    /// 验收 2.3：try_acquire 不足时返回 false，且不扣减令牌。
    #[tokio::test(start_paused = true)]
    async fn try_acquire_does_not_deduct_on_failure() {
        let b = bucket(1000); // burst = 64KiB
        // 一次性耗掉全部额度：失败的那次不扣减。
        let burst = MIN_BURST_BYTES as usize;
        assert!(b.try_acquire(burst / 2));
        assert!(!b.try_acquire(burst), "额度不足应返回 false");
        // 失败不扣减：剩下一半额度仍可用。
        assert!(b.try_acquire(burst / 2));
        assert_eq!(b.total_bytes(), burst as u64, "失败的 try_acquire 不计入流量");
    }

    /// 验收 2.4：单次 acquire 超过 burst 不能死锁。
    #[tokio::test(start_paused = true)]
    async fn acquire_larger_than_burst_completes() {
        let b = bucket(1_000_000); // burst = 200KB
        let start = Instant::now();
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            b.acquire(5 * 1024 * 1024),
        )
        .await;
        assert!(done.is_ok(), "单次 acquire 超过 burst 不应死锁");
        let elapsed = start.elapsed().as_secs_f64();
        assert!(
            (4.7..=5.3).contains(&elapsed),
            "借贷式 acquire 仍应按时完成，实际 {elapsed}s"
        );
    }

    /// 验收 2.5：等待中把 rate 改成 0，等待者应在 ≤1 个 tick（这里断言
    /// ≤200ms 虚拟时间）内完成。实际实现是"唤醒 + 每次 sleep 最大值 100ms"，
    /// 即使通知早于 select 注册也不会超过 100ms 才重算。
    #[tokio::test(start_paused = true)]
    async fn set_rate_zero_releases_waiters() {
        let b = std::sync::Arc::new(bucket(1000)); // 1KB/s：等 1MB 要 1000s
        let waiter = {
            let b = b.clone();
            tokio::spawn(async move { b.acquire(1024 * 1024).await })
        };
        // 让等待者真正进入 sleep。
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let start = Instant::now();
        b.set_rate(0);
        tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("改成不限后等待者应立即完成")
            .unwrap();
        assert!(
            start.elapsed() <= std::time::Duration::from_millis(200),
            "改 rate=0 后释放耗时应 ≤1 tick"
        );
    }

    /// 验收 2.6：consume 可以把 tokens 打成负数，后续 acquire 需要等待还清。
    #[tokio::test(start_paused = true)]
    async fn consume_debt_delays_next_acquire() {
        let b = bucket(1_000_000); // burst = 200KB
        b.consume(300 * 1024); // tokens = -100KB
        let start = Instant::now();
        b.acquire(100 * 1024).await; // 先还 100KB 欠账、再攒 100KB = 0.2s
        let elapsed = start.elapsed().as_secs_f64();
        assert!(
            (0.19..=0.25).contains(&elapsed),
            "欠账应让下一次 acquire 等待 ≈0.2s，实际 {elapsed}s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn set_rate_clamps_tokens_to_new_burst() {
        let b = bucket(1_000_000); // burst 200KB，初始满
        b.set_rate(1_000); // burst 降到 64KiB
        assert!(b.try_acquire(64 * 1024), "新容量内的额度应可用");
        assert!(!b.try_acquire(1024), "超出新容量应失败");
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_respects_fifo_order() {
        // 三个等待者按发起顺序拿额度：tokio Mutex 是 FIFO，且 sleep 期间持锁。
        let b = std::sync::Arc::new(bucket(100_000)); // 100KB/s, burst 64KiB
        b.consume(64 * 1024); // 清空突发额度（tokens=0）
        let order = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..3u32 {
            let b = b.clone();
            let order = order.clone();
            handles.push(tokio::spawn(async move {
                b.acquire(32 * 1024).await; // 每个要 0.32s
                order.lock().await.push(i);
            }));
            // 依次发起，保证入队顺序确定。
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*order.lock().await, vec![0, 1, 2], "等待者应按 FIFO 顺序完成");
    }

    #[test]
    fn resolve_rate_priority() {
        assert_eq!(resolve_rate(Some(8.0), Some(4.0)), (8.0, RateSource::File));
        assert_eq!(resolve_rate(None, Some(4.0)), (4.0, RateSource::Cli));
        assert_eq!(resolve_rate(None, None), (0.0, RateSource::Default));
    }

    #[test]
    fn mbps_conversion() {
        assert_eq!(mbps_to_bps(8.0), 1_000_000);
        assert_eq!(mbps_to_bps(0.0), 0);
        assert_eq!(mbps_to_bps(-1.0), 0);
        assert_eq!(mbps_to_bps(f64::NAN), 0);
    }

    #[test]
    fn settings_file_roundtrip_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/relay-settings.json");
        BandwidthSettingsFile::save(&path, 8.0, 2.0).unwrap();
        let loaded = BandwidthSettingsFile::load(&path);
        assert_eq!(loaded.bandwidth_down_mbps, Some(8.0));
        assert_eq!(loaded.bandwidth_up_mbps, Some(2.0));
        // JSON 字段名固定（admin/运维直接读文件）。
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"bandwidth_down_mbps\""));
        assert!(text.contains("\"bandwidth_up_mbps\""));
        // 同目录不留临时文件。
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写不应残留临时文件");
    }

    #[test]
    fn settings_file_missing_or_broken_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        let s = BandwidthSettingsFile::load(&missing);
        assert!(s.bandwidth_down_mbps.is_none() && s.bandwidth_up_mbps.is_none());
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{not json").unwrap();
        let s = BandwidthSettingsFile::load(&broken);
        assert!(s.bandwidth_down_mbps.is_none());
    }

    #[tokio::test]
    async fn apply_startup_rates_prefers_file_over_cli() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-settings.json");
        BandwidthSettingsFile::save(&path, 8.0, 4.0).unwrap();
        let limiter = std::sync::Arc::new(BandwidthLimiter::new());
        apply_startup_rates(&limiter, path, Some(2.0), Some(1.0)).await;
        assert_eq!(limiter.down_mbps(), 8.0);
        assert_eq!(limiter.up_mbps(), 4.0);
        assert_eq!(
            limiter.sources().await,
            (RateSource::File, RateSource::File)
        );
    }

    #[tokio::test]
    async fn apply_startup_rates_falls_back_to_cli_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay-settings.json");
        let limiter = std::sync::Arc::new(BandwidthLimiter::new());
        // 文件不存在 → CLI
        apply_startup_rates(&limiter, path.clone(), Some(3.0), None).await;
        assert_eq!(limiter.down_mbps(), 3.0);
        assert_eq!(limiter.up_mbps(), 0.0);
        assert_eq!(
            limiter.sources().await,
            (RateSource::Cli, RateSource::Default)
        );
        // 文件只写了下行 → 上行仍取 CLI
        BandwidthSettingsFile::save(&path, 5.0, 5.0).unwrap();
        std::fs::write(&path, r#"{"bandwidth_down_mbps": 7.0}"#).unwrap();
        let limiter = std::sync::Arc::new(BandwidthLimiter::new());
        apply_startup_rates(&limiter, path, None, Some(2.0)).await;
        assert_eq!(limiter.down_mbps(), 7.0);
        assert_eq!(limiter.up_mbps(), 2.0);
        assert_eq!(
            limiter.sources().await,
            (RateSource::File, RateSource::Cli)
        );
    }
}
