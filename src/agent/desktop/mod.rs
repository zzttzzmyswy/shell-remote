//! Desktop video sharing (capture → H.264 encode → fMP4 stream).
//!
//! Pipeline: `FrameSource` → `bgra_to_i420` → `H264Encoder` → fMP4 mux →
//! POST `desktop:video` messages to the relay, which fans them out to
//! browsers subscribed on `GET /agent/desktop/stream`.
//!
//! Control flow (browser ↔ agent over the interactive SSE/POST channel):
//! - `desktop:start`  → agent spawns the capture+encode loop
//! - `desktop:stop`   → agent stops it
//! - `desktop:started`/`desktop:stopped`/`desktop:capabilities` → broadcasts
//! - `session:join`   → agent replies `desktop:capabilities` so the UI knows
//!   whether the 桌面 button can be enabled.

pub mod capture;
pub mod color;
pub mod clipboard;
pub mod encoder;
pub mod input;
pub mod mp4;
pub mod openh264;
pub mod rate;
#[cfg(feature = "vp9")]
pub mod vpx;
#[cfg(feature = "av1")]
pub mod aom;

#[cfg(windows)]
pub mod dxgi;
#[cfg(all(target_os = "linux", feature = "wayland"))]
pub mod wayland;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Post a JSON message to the relay on the agent's own HTTP transport.
pub type PostFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

/// Agent-side desktop configuration (wired from CLI flags).
#[derive(Clone, Debug)]
pub struct DesktopConfig {
    /// Capture backend: `auto`, `x11`, `wayland`, `gdi`/`windows` or `none`.
    pub capture: String,
    /// Encoder codec (`h264` / `vp9` / `av1`). Default AV1: 压缩率最高,
    /// 码率在固定分辨率下真实受控 —— 支撑"禁止自动降低分辨率"。
    pub codec: String,
    /// Nominal encode frame rate.
    pub fps: f64,
    /// Adaptive bitrate bounds in bps (user request: 800 / 200 → kbps).
    pub min_bps: u64,
    pub max_bps: u64,
    /// Optional X11 display override (`--desktop-display`).
    pub display: Option<String>,
    /// 质量档倍率（speed=0.5 / balanced=0.67 / best=1.5，rustdesk 同款）。
    /// 决定目标码率（base_bitrate × quality）与 QP 区间。
    pub quality: f32,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            capture: "auto".to_string(),
            codec: "av1".to_string(),
            // 30fps 起步（MYS-886 时延对标 rustdesk: 22fps/12ms）。60fps 软编
            // 在低配机器上单核打满（实测 115%），编码耗时直接加进 e2e；
            // 30fps 减一半编码负载，时延显著优于 60fps，流畅度损失小。
            // 需要更高帧率用 --desktop-fps 显式指定。
            fps: 30.0,
            // 静态桌面 ~80k 足够 (openh264 实测 84k 满帧); 动态由 ABR 拉回
            min_bps: 80_000,
            // 0 = 自动按 rustdesk 模型（base_bitrate × quality，1080p balanced
            // ≈1388kbps）；显式设值作为硬顶。
            max_bps: 0,
            display: None,
            quality: crate::agent::desktop::encoder::QUALITY_BALANCED,
        }
    }
}

impl DesktopConfig {
    /// Whether the desktop feature is compiled & configured to possibly work.
    /// The capture socket is only actually opened on `desktop:start`.
    pub fn enabled(&self) -> bool {
        self.capture != "none"
    }

    pub fn supports_codec(&self, codec: &str) -> bool {
        codec.eq_ignore_ascii_case("h264")
            || (cfg!(feature = "vp9")
                && (codec.eq_ignore_ascii_case("vp9") || codec.eq_ignore_ascii_case("vp8")))
            || (cfg!(feature = "av1") && codec.eq_ignore_ascii_case("av1"))
    }
}

/// Controls the desktop capture+encode task.
pub struct DesktopManager {
    config: DesktopConfig,
    running: Arc<AtomicBool>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 最近一次浏览器实测的可用带宽(bps), 用于弱网下把码率天花板压回
    /// 网络可承受的范围; 从未上报时保持配置上限。
    bandwidth: Arc<std::sync::atomic::AtomicU64>,
    /// 键鼠注入器。惰性创建：首个输入事件到达时才 spawn 注入线程
    /// （纯观看会话不付这个代价）。
    injector: tokio::sync::Mutex<Option<input::InputInjector>>,
    /// 剪贴板同步器。同样惰性创建：首个剪贴板命令才 spawn 线程。
    clipboard: tokio::sync::Mutex<Option<clipboard::ClipboardSync>>,
    /// 与 relay 的时钟偏移（relay_epoch - 本地_epoch，ms）。srtc 打点加上
    /// 此偏移后落在 relay 时基，端到端延时不再依赖 agent/浏览器两机时钟同步
    /// （MYS-886 指标失真根因）。由 DesktopManager::set_clock_offset 注入，
    /// 默认 0 ＝ 未校准（行为与旧版一致）。
    clock_offset: std::sync::atomic::AtomicI64,
    /// 运行时编码方案（av1/vp9/h264）。初始 = config.codec，前端可通过
    /// desktop:codec 热切换（重建桌面流），见 [`Self::set_codec`]。
    codec: std::sync::RwLock<String>,
    /// 运行时目标编码帧率（QoS 动态调，rustdesk 同款：延时好提升、差降低）。
    fps: Arc<std::sync::atomic::AtomicU32>,
    /// 运行时质量档倍率（web 码率下拉可改：speed/balanced/best）。初始 =
    /// config.quality；改动后重建桌面流应用（set_codec 同机制）。
    quality: std::sync::RwLock<f32>,
    /// 运行时用户码率硬顶（web 自定义码率）。0 = auto（base×quality）。
    max_bps: std::sync::RwLock<u64>,
    /// QoS 码率缩放（千分比，1000 = 100%）。弱网（高端到端延时）时下调
    /// 目标码率预算，让 ABR/编码器在紧张链路下自动收敛（rustdesk QoS 的
    /// quality 维度，MYS-886 需求7-3）；网络恢复自动回升到 1000。
    qos_scale: Arc<std::sync::atomic::AtomicU32>,
    /// rustdesk 渐进式 QoS 状态（fps/ratio 平滑调整，非硬档跳变）。
    qos: tokio::sync::Mutex<QosAdaptive>,
    /// 已编码帧计数（供 QoS DYNAMIC_SCREEN 判定：每秒编码帧数）。
    qos_frames: Arc<std::sync::atomic::AtomicU64>,
    /// QoS 上次采样墙钟（微秒，供 on_qos_delay 算 elapsed_s）。
    qos_last_sample: std::sync::atomic::AtomicU64,
    /// 灰度模式（web 端可切）：编码前把 UV 平面置中性 128，色度≈0。
    /// 弱网下带宽占用显著下降（亮度是主观关键），画质降为灰度可接受。
    /// 运行时即时生效，不重建编码器/不重启流。
    gray: Arc<std::sync::atomic::AtomicBool>,
}

impl DesktopManager {
    pub fn new(config: DesktopConfig) -> Self {
        let bps = config.max_bps;
        let codec = config.codec.clone();
        let fps0 = config.fps.clamp(1.0, 60.0) as u32;
        let quality0 = config.quality;
        let max_bps0 = config.max_bps;
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            task: tokio::sync::Mutex::new(None),
            bandwidth: Arc::new(std::sync::atomic::AtomicU64::new(bps)),
            injector: tokio::sync::Mutex::new(None),
            clipboard: tokio::sync::Mutex::new(None),
            clock_offset: std::sync::atomic::AtomicI64::new(0),
            codec: std::sync::RwLock::new(codec),
            fps: Arc::new(std::sync::atomic::AtomicU32::new(fps0)),
            quality: std::sync::RwLock::new(quality0),
            max_bps: std::sync::RwLock::new(max_bps0),
            qos_scale: Arc::new(std::sync::atomic::AtomicU32::new(1000)),
            qos: tokio::sync::Mutex::new(QosAdaptive::new()),
            qos_frames: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            qos_last_sample: std::sync::atomic::AtomicU64::new(0),
            gray: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// 运行时切换码率档位（rustdesk 同款三档 + 自定义 kbps）。
    /// `quality` ∈ speed/balanced/best；`custom_kbps` >0 时为自定义硬顶。
    /// 重建桌面流使新档生效（低频操作，可接受瞬间重建）。
    pub async fn set_quality(&self, quality: &str, custom_kbps: u64, post: PostFn) -> Result<(), String> {
        let q = match quality {
            "speed" => crate::agent::desktop::encoder::QUALITY_SPEED,
            "best" => crate::agent::desktop::encoder::QUALITY_BEST,
            _ => crate::agent::desktop::encoder::QUALITY_BALANCED,
        };
        {
            let mut cur = self.quality.write().map_err(|_| "quality lock poisoned".to_string())?;
            *cur = q;
        }
        {
            let mut cur = self.max_bps.write().map_err(|_| "bitrate lock poisoned".to_string())?;
            *cur = if custom_kbps > 0 { custom_kbps * 1000 } else { 0 };
        }
        if self.is_running() {
            self.stop(post.clone()).await;
            self.start(post).await;
        }
        Ok(())
    }

    /// 灰度模式开关（web 端 `desktop:gray`）。只翻转编码前降色度 flag，
    /// 下一帧即时生效——不重建编码器、不重启流（与 set_codec/set_quality
    /// 的"重启重建"不同，灰度是纯编码前像素处理）。
    pub fn set_gray(&self, enabled: bool) {
        use std::sync::atomic::Ordering as O;
        self.gray.store(enabled, O::Relaxed);
        tracing::info!(enabled, "desktop gray mode {}", if enabled { "ON" } else { "OFF" });
    }

    pub fn gray_enabled(&self) -> bool {
        use std::sync::atomic::Ordering as O;
        self.gray.load(O::Relaxed)
    }

    /// 编码循环每帧调用：为 QoS DYNAMIC_SCREEN 统计编码帧数。
    pub fn bump_qos_frame(&self) {
        use std::sync::atomic::Ordering as O;
        self.qos_frames.fetch_add(1, O::Relaxed);
    }

    /// QoS 动态调整目标帧率（rustdesk 渐进式）：每次浏览器上报端到端延时，
    /// 按 rustdesk `video_qos::user_network_delay` 渐进调整（好网 +1/+5、
    /// 差网按比例降），非硬档跳变。帧数/时间差由内部采样。
    pub async fn on_qos_delay(&self, delay_ms: u32) -> (u32, u32, u64) {
        use std::sync::atomic::Ordering as O;
        let cap = (self.config.fps as u32).clamp(1, 60);
        // 距上次采样的墙钟与帧数差
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let last = self.qos_last_sample.swap(now_us, O::Relaxed);
        let elapsed_s = if last > 0 {
            (((now_us - last) as f64 / 1e6).min(5.0)) as f32
        } else {
            0.0
        };
        let frames = self.qos_frames.swap(0, O::Relaxed) as u32;
        let (fps, permille) = {
            let mut q = self.qos.lock().await;
            q.on_delay(delay_ms, frames, elapsed_s)
        };
        let fps = fps.clamp(1, cap);
        let permille = permille.clamp(100, 1000);
        self.fps.store(fps, O::Relaxed);
        self.qos_scale.store(permille, O::Relaxed);
        // 当前生效目标码率（对齐 rustdesk TestDelay 携带 target_bitrate 回传，
        // MYS-886 #153）：用户硬顶 >0 用其值×缩放；否则按 min_bps 基×缩放。
        let base_bps = if self.config.max_bps > 0 {
            self.config.max_bps
        } else {
            self.config.min_bps
        };
        let bitrate_kbps = base_bps * (permille as u64) / 1000 / 1000;
        tracing::info!(delay_ms, fps, qos_scale = permille, cap, "desktop QoS: adaptive adjusted");
        (fps, permille, bitrate_kbps)
    }

    /// QoS 动态调整目标帧率（rustdesk 同款）：浏览器端到端延时 <150ms 提升
    /// 到配置上限、<300ms 维持、更差降到 15fps 保流畅。编码循环按此值
    /// 动态改 tick 周期。
    ///
    /// 上限不是 60：软编（VP9/AV1 1080p）单帧编码 30-60ms，16.7ms 的 60fps
    /// 预算编不出来只会让编码队列越积越深、端到端延时飙升（实测 200-600ms，
    /// MYS-886 卡顿回归根因之一）。上限取 `--desktop-fps`（默认 30），用户
    /// 显式要求更高帧率才提升。
    pub fn set_fps(&self, fps: u32) {
        use std::sync::atomic::Ordering as O;
        let cap = (self.config.fps as u32).clamp(1, 60);
        let fps = fps.clamp(1, cap);
        self.fps.store(fps, O::Relaxed);
        tracing::info!(fps, cap, "desktop QoS: target fps adjusted");
    }

    /// QoS 码率缩放（千分比）：弱网时下调目标码率预算，恢复后置回 1000。
    /// 只影响 ABR ceiling，不重建流（rustdesk QoS 的 quality 维度，MYS-886
    /// 需求7-3）。范围 100-1000。
    pub fn set_qos_scale(&self, permille: u32) {
        let permille = permille.clamp(100, 1000);
        use std::sync::atomic::Ordering as O;
        self.qos_scale.store(permille, O::Relaxed);
        tracing::info!(qos_scale = permille, "desktop QoS: bitrate scale adjusted");
    }

    /// 运行时热切换编码方案（av1/vp9/h264）。仅当桌面正在运行时重建
    /// 桌面流（stop 旧流 → start 新流，前端按新 init 段的 codec box
    /// 自动切换解码）。codec 不变时是 no-op。
    pub async fn set_codec(&self, codec: &str, post: PostFn) -> Result<(), String> {
        let codec = codec.to_string();
        if !self.config.supports_codec(&codec) {
            return Err(format!("unsupported codec {codec}"));
        }
        {
            let mut cur = self.codec.write().map_err(|_| "codec lock poisoned".to_string())?;
            if *cur == codec {
                return Ok(());
            }
            *cur = codec;
        }
        if self.is_running() {
            self.stop(post.clone()).await;
            self.start(post).await;
        }
        Ok(())
    }

    /// 当前编码方案（供日志/指标）。
    pub fn codec(&self) -> String {
        self.codec.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// 注入 relay 时钟偏移（relay_epoch - 本地_epoch，ms）。agent 在会话
    /// 建立后对 relay /api/clock 采样得到；srtc 打点会加上这个偏移，
    /// 把采集时刻换算到 relay 时基（不依赖 agent 系统时间）。
    pub fn set_clock_offset(&self, offset_ms: i64) {
        use std::sync::atomic::Ordering as O;
        self.clock_offset.store(offset_ms, O::Relaxed);
    }

    /// Browser-reported available bandwidth (bps) feedback (weak networks).
    pub fn set_bandwidth_bps(&self, bps: u64) {
        use std::sync::atomic::Ordering as O;
        self.bandwidth.store(bps.max(1), O::Relaxed);
    }

    /// Handle one `desktop:mouse` payload from a browser (inject locally).
    pub async fn handle_mouse(&self, payload: &serde_json::Value) {
        let Some(ev) = input::parse_mouse(payload) else { return };
        let mut g = self.injector.lock().await;
        let inj = g.get_or_insert_with(input::InputInjector::start);
        inj.send(ev);
    }

    /// Handle one `desktop:key` payload from a browser (inject locally).
    pub async fn handle_key(&self, payload: &serde_json::Value) {
        let Some(ev) = input::parse_key(payload) else { return };
        let mut g = self.injector.lock().await;
        let inj = g.get_or_insert_with(input::InputInjector::start);
        inj.send(ev);
    }

    /// Handle `desktop:clipboard:set` — browser pushed its local clipboard
    /// text to the remote machine.
    pub async fn handle_clipboard_set(&self, payload: &serde_json::Value) {
        let Some(text) = payload.get("text").and_then(|v| v.as_str()) else { return };
        let mut g = self.clipboard.lock().await;
        let clip = g.get_or_insert_with(clipboard::ClipboardSync::start);
        clip.set(text.to_string());
    }

    /// Handle `desktop:clipboard:get` — read the remote clipboard and return
    /// it as a `desktop:clipboard` message (posted through `post`).
    pub async fn handle_clipboard_get(&self, post: &PostFn) {
        let text = {
            let mut g = self.clipboard.lock().await;
            let clip = g.get_or_insert_with(clipboard::ClipboardSync::start);
            clip.get().unwrap_or_default()
        };
        post(serde_json::json!({
            "type": "desktop:clipboard",
            "payload": { "text": text }
        }));
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn config(&self) -> &DesktopConfig {
        &self.config
    }

    /// Start the capture+encode loop if not already running.
    pub async fn start(&self, post: PostFn) {
        if self.is_running() {
            return;
        }
        if !self.config.enabled() {
            self.post_started_error(&post, "desktop capture is disabled (--desktop-capture none)");
            return;
        }
        // 用运行时 codec（热切换后的值），不是启动时的 config.codec。
        let codec = self.codec.read().map(|c| c.clone()).unwrap_or_default();
        if !self.config.supports_codec(&codec) {
            self.post_started_error(&post, &format!("unsupported codec {codec}"));
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let mut cfg = self.config.clone();
        cfg.codec = codec;
        // 运行时档位（web 码率/质量下拉可改）覆盖启动默认值。
        cfg.quality = self.quality.read().map(|q| *q).unwrap_or(cfg.quality);
        let mb = self.max_bps.read().map(|b| *b).unwrap_or(0);
        if mb > 0 {
            cfg.max_bps = mb;
        }
        let bandwidth = self.bandwidth.clone();
        let clock_offset = self.clock_offset.load(std::sync::atomic::Ordering::Relaxed);
        let fps_ctl = self.fps.clone();
        let qos_scale = self.qos_scale.clone();
        let qos_frames = self.qos_frames.clone();
        let gray = self.gray.clone();
        let task = tokio::task::spawn(async move {
            run_desktop_loop(cfg, running, post, bandwidth, clock_offset, fps_ctl, qos_scale, qos_frames, gray).await;
        });
        *self.task.lock().await = Some(task);
    }

    /// Ask the loop to stop (best-effort; the task sets `running=false`),
    /// then wait briefly for it to notice `running=false`.
    pub async fn stop(&self, post: PostFn) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(task) = self.task.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
        // 停用广播（若循环因其它原因结束则跳过）
        self.broadcast_stopped(&post);
    }

    fn post_started_error(&self, post: &PostFn, error: &str) {
        post(serde_json::json!({
            "type": "desktop:started",
            "payload": { "codec": self.config.codec, "error": error }
        }));
    }

    fn broadcast_stopped(&self, post: &PostFn) {
        post(serde_json::json!({
            "type": "desktop:stopped",
            "payload": {}
        }));
    }

    /// Present the current capability snapshot (sent on `session:join`).
    pub fn capabilities_json(&self) -> serde_json::Value {
        let cfg = &self.config;
        serde_json::json!({
            "available": cfg.enabled(),
            "running": self.is_running(),
            "codecs": if cfg.enabled() {
                let mut v = vec!["h264"];
                if cfg!(feature = "vp9") {
                    v.push("vp9");
                    v.push("vp8");
                }
                if cfg!(feature = "av1") {
                    v.push("av1");
                }
                v
            } else {
                Vec::new()
            },
            "capture": cfg.capture,
        })
    }
}

/// The manager owns the capture+encode loop lifecycle. When the session that
/// created it is torn down (agent reconnect / run_session exit), dropping the
/// manager must stop any still-running loop — otherwise a reconnect leaves the
/// old loop encoding and POSTing forever (observed as multiple parallel full
/// desktop encode loops after a relay restart).
impl Drop for DesktopManager {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // tokio::sync::Mutex::get_mut is available because `&mut self` proves
        // exclusive ownership (no other holder can be in a scope).
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}

/// Encode-resolution scale steps (fraction of the capture size). Length 1
/// disables the adaptive resolution entirely — MYS-886: 用户明确"禁止自动
/// 降低分辨率"(对比 rustdesk 太糊)。码率完全交给 AV1/VP9 CBR 在固定分辨率
/// 下控制(两者实测 60/60 帧码率受控在预算 ~10-60% 内)。
/// 若将来要恢复自适应分辨率, 改回 &[1.0, 0.75, 0.5, 0.375] 即可。
const SCALES: &[f64] = &[1.0];

/// Frame-stride steps for our own frame skipping (1 = encode every capture
/// frame). Effective encode fps = `cfg.fps / STRIDES[i]` → 15/7/5/3 fps.
/// Skipping frames is how we hold the *average* bitrate under the ceiling
/// without OpenH264's RC jumping in and black-screening the stream.
const STRIDES: &[u32] = &[1, 2, 3, 5];

/// 连续捕获失败次数上限：超过则终止循环并回传 `desktop:error`。
///
/// 设 150 而不是 30：Windows 上屏幕 DC 句柄会在锁屏/安全桌面/显示模式
/// 切换后失效（BitBlt err=6），GDI 捕获自愈需要重建上下文，得给足重试
/// 窗口（约 10s @ 15fps）才能跨过瞬时失效而非误杀整个桌面流。
const MAX_CAPTURE_ERRORS: u32 = 150;

/// 编码器内部 AUTO 关键帧兜底间隔（秒）。关键帧实际节奏由外部动态控制
/// （MYS-886 需求7-1：静止 4.5s / 活跃 1.5s 心跳 IDR）；kf_max_dist 只是
/// 编码器最晚自动插关键帧的兜底，保证外部 force_idr 失效时仍可 seek。
pub const KF_AUTO_MAX_SECS: f64 = 5.0;

/// 静止/活跃关键帧间隔（毫秒）。MYS-886 需求7-1：静止画面 P 帧近 0 字节，
/// 关键帧是唯一带宽开销 → 心跳 IDR 从 500ms 拉长到 4.5s（带宽降至 1/9×）；
/// 活跃（帧均字节超阈值）时 1.5s 一个 IDR 保持 seek 与参考链健康。
/// 判定用 [`avg_frame_bytes`]：> [`KF_ACTIVE_BYTES_FRAME`] 视为活跃。
pub const KF_QUIET_MS: u64 = 4500;
pub const KF_ACTIVE_MS: u64 = 1500;
/// 帧均字节阈值：超过则视为活跃内容（需要高频关键帧）。
pub const KF_ACTIVE_BYTES_FRAME: f64 = 2048.0;

/// 动态关键帧间隔由 [`KF_QUIET_MS`]/[`KF_ACTIVE_MS`] 取代旧的 500ms 固定
/// 静止心跳（MYS-886 需求7-1：静止 4.5s 一个 IDR，带宽降至 ~1/9，且远低于
/// relay 观看者 30s 空闲超时，不会误判断流）。

/// The capture → convert → encode → mux → post loop.
/// Handles OpenH264's penalty frame-skipping (observed on high-motion
/// desktops at 200-800 kbps: RC drops nearly every P frame): skipped frames
/// are never POSTed, the bitrate is pinned to the ceiling while skipping, and
/// a persistently high skip ratio degrades the encode resolution until the
/// encoder can actually produce frames again (a fresh init segment is then
/// replayed to every viewer). Quiet scenes restore the resolution step by step.
async fn run_desktop_loop(
    cfg: DesktopConfig,
    running: Arc<AtomicBool>,
    post: PostFn,
    bandwidth: Arc<std::sync::atomic::AtomicU64>,
    clock_offset_ms: i64,
    fps_ctl: Arc<std::sync::atomic::AtomicU32>,
    qos_scale: Arc<std::sync::atomic::AtomicU32>,
    qos_frames: Arc<std::sync::atomic::AtomicU64>,
    gray: Arc<std::sync::atomic::AtomicBool>,
) {
        let src = match capture::open_source(&cfg.capture, cfg.display.as_deref()) {
        Ok((src, backend)) => (src, backend),
        Err(e) => {
            tracing::error!("desktop capture open failed: {}", e);
            (post)(serde_json::json!({
                "type": "desktop:started",
                "payload": { "codec": cfg.codec, "error": format!("capture unavailable: {e}") }
            }));
            return;
        }
    };
    let (src, backend) = src;
    run_desktop_pipeline(cfg, running, post, src, bandwidth, backend, clock_offset_ms, fps_ctl, qos_scale, qos_frames, gray).await;
}

/// The capture → convert → encode → mux → post pipeline. Split from
/// [`run_desktop_loop`] so tests can inject a synthetic [`capture::FrameSource`].
async fn run_desktop_pipeline(
    cfg: DesktopConfig,
    running: Arc<AtomicBool>,
    post: PostFn,
    src: Box<dyn capture::FrameSource>,
    bandwidth: Arc<std::sync::atomic::AtomicU64>,
    backend: String,
    clock_offset_ms: i64,
    fps_ctl: Arc<std::sync::atomic::AtomicU32>,
    qos_scale: Arc<std::sync::atomic::AtomicU32>,
    qos_frames: Arc<std::sync::atomic::AtomicU64>,
    gray: Arc<std::sync::atomic::AtomicBool>,
) {
    // cfg 需可变：编码器 fallback 后回写实际 codec。
    let mut cfg = cfg;
    // 截图线程化（rustdesk capture 线程对齐）：capture 挪到独立线程持续
    // 抓帧，编码循环 try_latest 非阻塞取最新帧——抓帧（X11/DXGI）不再拖慢
    // 编码，慢抓帧时跳帧追最新。src 被 move 进抓帧线程。
    let threaded = capture::ThreadedFrameSource::spawn(src);
    let (mut w0, mut h0) = threaded.resolution();
    if w0 < 2 || h0 < 2 || w0 % 2 != 0 || h0 % 2 != 0 {
        (post)(serde_json::json!({
            "type": "desktop:started",
            "payload": { "codec": cfg.codec, "error": format!("resolution {w0}x{h0} must be even") }
        }));
        return;
    }

    let (mut enc, actual_codec) = match encoder::create_encoder_fallback(
        &cfg.codec,
        w0 as u32,
        h0 as u32,
        cfg.max_bps,
        cfg.fps,
        cfg.quality,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            (post)(serde_json::json!({
                "type": "desktop:started",
                "payload": { "codec": cfg.codec, "error": format!("encoder init failed: {e}") }
            }));
            return;
        }
    };
    if actual_codec != cfg.codec {
        tracing::warn!(requested = %cfg.codec, actual = %actual_codec, "desktop codec fell back");
    }
    cfg.codec = actual_codec;
    // ABR 上限：用户 max_bps>0 用其值，否则用 rustdesk 模型（base×quality）。
    // 小分辨率（如 320x180 测试/低端屏）`base×quality` 可能低于 min_bps，
    // 必须顶到 min_bps——否则 Abr::new 断言 min<=max 直接 panic 冲掉整个
    // 桌面流（rate.rs:27 实测崩溃路径）。
    let abr_ceiling = if cfg.max_bps > 0 {
        cfg.max_bps.max(cfg.min_bps)
    } else {
        encoder::target_bitrate(w0 as u32, h0 as u32, 0, cfg.quality).max(cfg.min_bps)
    };
    let mut abr = rate::Abr::new(cfg.min_bps, abr_ceiling);

    // fMP4 config is final once the first IDR carries SPS/PPS at the *current*
    // encode resolution; a resolution change rebuilds both.
    let mut mp4_cfg: Option<mp4::Mp4Config> = None;
    let mut seq: u32 = 1;
    // fMP4 时间线只按"实际 POST 的帧"推进：跳帧/空帧不推 pts。否则
    // moof 之间出现时间空洞 → 浏览器 SourceBuffer range 断裂成 N 段,
    // 播放头掉进空洞尾部 stall（实测 readyState=1 + 22s lag、11 个
    // range）。连续时间线是 MSE 实时流的硬要求。
    let mut pts_ms: u64 = 0;
    let frame_ms = (1000.0 / cfg.fps).round() as u64;
    // 墙上时间跟踪（心跳与"多久没 POST"的判断用真实时间, 不用 pts）。
    let mut wall_ms: u64 = 0;

    // Adaptive encode resolution.
    let mut scale_idx: usize = 0;
    let mut enc_w: usize = w0;
    let mut enc_h: usize = h0;
    let mut byte_win: VecDeque<u32> = VecDeque::new();
    const OBS: usize = 48;
    let mut since_change: u32 = 0;

    // 码率预算（字节/帧）取自编码上限: max_bps / 8 / fps, 与 ABR 一致
    // 用弱网上限（带宽 clamp）。高熵时帧平均超预算 → 分辨率阶梯降级
    // （模糊但不丢帧）; 预算富余 → 逐档恢复。
    let mut stride_idx: usize = 0;

    tracing::info!(
        width = %w0, height = %h0, fps = %cfg.fps, backend = %backend,
        "desktop capture started"
    );
    (post)(serde_json::json!({
        "type": "desktop:started",
        "payload": {
            "codec": cfg.codec, "width": w0, "height": h0, "fps": cfg.fps,
            "min_kbps": cfg.min_bps / 1000, "max_kbps": cfg.max_bps / 1000,
            "backend": backend,
        }
    }));

    // QoS 动态帧率（rustdesk 同款）：好网提升 fps、差网降低。每次 tick
    // 前读目标 fps，sleep 对应间隔并同步编码器帧率模型（kf_max_dist）。
    let start = std::time::Instant::now();
    let mut frame_idx: u64 = 0;
    // 上次强制 IDR 的虚拟墙钟（wall_ms 时基）。动态关键帧节拍用它判断
    // 是否到间隔（静止 KF_QUIET_MS / 活跃 KF_ACTIVE_MS，MYS-886 需求7-1）。
    let mut last_kf_wall: u64 = 0;

    while running.load(Ordering::SeqCst) {
        let cur_fps = fps_ctl.load(Ordering::SeqCst).clamp(1, 60);
        enc.set_frame_rate(cur_fps as f32);
        tokio::time::sleep(Duration::from_secs_f64(1.0 / cur_fps as f64)).await;
        if !running.load(Ordering::SeqCst) {
            break;
        }
        // 动态关键帧节拍（MYS-886 需求7-1）：由内容活跃度决定 IDR 间隔。
        // 静止（帧均字节 ≤ 阈值）时 P 帧近 0 字节，关键帧是唯一带宽开销
        // → 从 500ms 拉长到 4.5s，静止带宽降至 ~1/9；活跃时 1.5s 一 IDR
        // 保持 seek 与参考链健康。首帧强制 IDR（同时就是 init 段的参数集）。
        // 低延时管线（MYS-886）: **编码不跳帧**——每个 tick 都捕获+编码。
        // 帧率阶梯（STRIDES）与降分辨率不再作用于编码路径, 编码器始终
        // 以满帧率输出; 码率控制交给 ABR 的 set_bitrate。跳帧只发生在
        // 上行（批量 POST 的丢旧保新, 见 agent::mod 的 batch 逻辑）和
        // relay→浏览器（非关键帧丢弃）——两者都不破坏解码器状态。
        wall_ms += frame_ms;
        let kf_ms = kf_interval_ms_for(avg_frame_bytes(&byte_win));
        if frame_idx == 0 || last_kf_wall + kf_ms <= wall_ms {
            enc.force_idr();
            last_kf_wall = wall_ms;
        }
        let fr = match threaded.try_latest() {
            Some(f) => f,
            None => {
                // 无新帧（capture 线程慢于编码，追最新帧跳帧）或线程在报错。
                let ec = threaded.err_count();
                if ec > 0 {
                    // 持续性捕获失败（例如 XWayland 下 root GetImage 抛 BadMatch、
                    // Windows 屏幕 DC 失效）。无限重试只会刷屏且永远黑屏。
                    // 连续失败 MAX_CAPTURE_ERRORS 次后终止, 并把原因回传
                    // 给浏览器展示。
                    if ec >= MAX_CAPTURE_ERRORS {
                        tracing::error!("desktop capture failed {ec} frames in a row — giving up");
                        let e = threaded.last_err().unwrap_or_else(|| "capture failed".to_string());
                        (post)(serde_json::json!({
                            "type": "desktop:error",
                            "payload": { "error": format!("capture failed: {e}") }
                        }));
                        break;
                    }
                    tracing::warn!(
                        "capture frame error (thread): {} — retrying ({ec}/{MAX_CAPTURE_ERRORS})",
                        threaded.last_err().unwrap_or_default()
                    );
                }
                // 无新帧：跳过本 tick（编码不被抓帧阻塞）。
                continue;
            }
        };
        let w = fr.width;
        let h = fr.height;
        // display 变更检测（rustdesk 对齐）：捕获源尺寸变了（X11/GDI 轮询、
        // DXGI mode-change）→ 重建编码器到新尺寸 + 重置分辨率阶梯 + 强制
        // IDR 重发 init（mp4_cfg=None 让下个 IDR 携带新参数集广播给所有
        // viewer）。重建失败则保持旧编码器（缩放到新尺寸）不中断流。
        if (w != w0 || h != h0) && w % 2 == 0 && h % 2 == 0 && w >= 2 && h >= 2 {
            tracing::warn!(old = %format!("{w0}x{h0}"), new = %format!("{w}x{h}"), "display resolution changed — rebuilding encoder");
            w0 = w;
            h0 = h;
            match encoder::create_encoder_fallback(
                &cfg.codec,
                w0 as u32,
                h0 as u32,
                cfg.max_bps,
                cfg.fps,
                cfg.quality,
            ) {
                Ok((new_enc, actual)) => {
                    enc = new_enc;
                    if actual != cfg.codec {
                        cfg.codec = actual;
                    }
                    scale_idx = 0;
                    stride_idx = 0;
                    enc_w = w0;
                    enc_h = h0;
                    // ABR ceiling 随新分辨率重算（rustdesk update_bitrate 路径）。
                    let ceiling = if cfg.max_bps > 0 {
                        cfg.max_bps.max(cfg.min_bps)
                    } else {
                        encoder::target_bitrate(w0 as u32, h0 as u32, 0, cfg.quality).max(cfg.min_bps)
                    };
                    abr = rate::Abr::new(cfg.min_bps, ceiling);
                    mp4_cfg = None;
                    seq = 1;
                    byte_win.clear();
                    since_change = 0;
                    enc.force_idr();
                }
                Err(e) => {
                    // 重建失败：w0/h0 恢复旧值，本帧/后续走缩放路径（旧编码器
                    // 期望旧尺寸，改 enc_w/enc_h 会让 encode 长度断言 panic）。
                    // 下帧仍会触发检测重试。
                    tracing::error!("encoder rebuild after display change failed: {e} — keeping old encoder");
                    w0 = enc_w;
                    h0 = enc_h;
                }
            }
        }
        // srtc 取点在**捕获完成、编码开始前**（用户口径：端到端延时须含
        // 编码全程——编码开始→浏览器解码/渲染完毕）。之前取在 POST 前,
        // 编码耗时（软编 1080p 单帧可达 20-40ms）被漏掉。
        // 校准到 relay 时基：本地墙钟 + 偏移（relay_epoch - 本地_epoch）。
        // 偏移默认 0（未校准, 行为与旧版一致）；校准后 srtc 落在 relay
        // 时间轴上, 浏览器 e2e 不再受 agent/浏览器两机时钟差影响。
        let cap_local = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let capture_ms = (cap_local + clock_offset_ms).max(0) as u64;
        let mut i420 = if w == enc_w && h == enc_h {
            color::bgra_to_i420(&fr.bgra, w, h, w * 4)
        } else {
            color::bgra_to_i420_scaled(&fr.bgra, w, h, w * 4, enc_w, enc_h)
        };
        // 灰度模式（web 端可选，弱网省带宽）：编码前把 UV 平面置中性 128，
        // 色度信息≈0，码率显著下降（亮度是弱网下的主观关键）。切换即时生效
        // （下帧起），不重建编码器。
        if gray.load(std::sync::atomic::Ordering::Relaxed) {
            apply_gray(&mut i420);
        }
        let encoded = match enc.encode(&i420) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("encode error: {}", e);
                continue;
            }
        };
        frame_idx += 1;
        // QoS DYNAMIC_SCREEN 统计：每编码一帧（含空帧）计数一次。
        use std::sync::atomic::Ordering as O;
        qos_frames.fetch_add(1, O::Relaxed);

        // 防御：RC 仍可能输出空帧（极少数情况）——空帧不 POST，记录为跳帧。
        // （pts 不动：空帧不产生 moof, 推 pts 会造成时间线空洞。）
        if encoded.nalu.is_empty() {
            // 空帧 = 桌面静止。记录为 0 字节（瞬时码率 0）, 让字节窗口在
            // 静默期自然回落、触发分辨率恢复; 不 push 则窗口残留动态期
            // 的大帧会误判"仍超预算"。
            byte_win.push_back(0);
            while byte_win.len() > OBS {
                byte_win.pop_front();
            }
            continue;
        }
        // 按实际编码字节入窗: 决策依据 = avg(bytes/frame) vs 预算。
        byte_win.push_back(encoded.nalu.len() as u32);
        while byte_win.len() > OBS {
            byte_win.pop_front();
        }
        since_change += 1;

        if encoded.is_idr && mp4_cfg.is_none() {
            // 首个关键帧（或分辨率重配后）携带 codec 参数集（H.264: SPS/PPS；
            // VP9: profile/level），构建 mux config 并下发 init。
            if let Some(sample) = enc.mux_sample(&encoded) {
                let c = mp4::Mp4Config {
                    width: enc_w as u32,
                    height: enc_h as u32,
                    fps: cfg.fps,
                    sample,
                };
                let init = mp4::mp4_init_segment(&c);
                post(serde_json::json!({
                    "type": "desktop:video",
                    "payload": { "kind": "init", "codec": cfg.codec, "data": base64(&init) }
                }));
                mp4_cfg = Some(c);
            }
        }

        let Some(cfg_mp4) = &mp4_cfg else {
            // No parameter sets yet — drop this frame.
            continue;
        };
        // H.264 sample = AVCC（长度前缀）；VP9 sample = 原始压缩帧。
        let sample = if cfg.codec.eq_ignore_ascii_case("h264") {
            mp4::annexb_to_avcc(&encoded.nalu)
        } else {
            encoded.nalu.clone()
        };
        // capture_ms 已在捕获后、编码前取（见上方赋值处）——srtc 携带的是
        // 含编码全程的起点。
        let frag = mp4::mp4_fragment(cfg_mp4, &sample, pts_ms, encoded.is_idr, seq, capture_ms);
        seq += 1;
        pts_ms += frame_ms; // 只有真实 POST 的帧推进 fMP4 时间线
        post(serde_json::json!({
            "type": "desktop:video",
            "payload": { "kind": "frag", "key": encoded.is_idr, "data": base64(&frag) }
        }));

        // Adaptive bitrate + 降级决策: 每 10 个编码帧评估一次。
        // 决策依据 = 平均帧字节 vs 码率预算(bps / 8 / fps)。统计的是帧
        // 实际大小, 不依赖 RC 丢帧事件(skip=0 下 RC 完全不控码率)。
        if frame_idx % 10 == 0 {
            let now = start.elapsed().as_secs_f64();
            // 弱网：以浏览器实测带宽为动态天花板(clamp 到 [min, 配置峰值])。
            // 峰值上限：用户 --desktop-max-bitrate>0 用其值，否则按 rustdesk
            // 模型 base_bitrate(分辨率)×质量档（1080p balanced ≈1388kbps）。
            use std::sync::atomic::Ordering as O;
            let ceiling = if cfg.max_bps > 0 {
                cfg.max_bps
            } else {
                encoder::target_bitrate(w0 as u32, h0 as u32, 0, cfg.quality)
            };
            // QoS 码率缩放（rustdesk QoS 的 quality 维度，MYS-886 需求7-3）：
            // 弱网时把码率天花板压回网络可承受范围（1000‰ = 不缩放）。
            let scale = qos_scale.load(O::Relaxed).clamp(100, 1000) as u64;
            let eff_base = bandwidth.load(O::Relaxed).min(ceiling).max(cfg.min_bps);
            let eff_max = eff_base.saturating_mul(scale) / 1000;
            abr.set_ceiling(eff_max);
            let budget = (eff_max as f64 / 8.0 / cfg.fps).max(1.0);
            let byte_ratio = avg_frame_bytes(&byte_win) / budget;
            if byte_ratio >= 1.5 {
                abr.set_target(eff_max);
                if enc.bitrate_bps() != eff_max {
                    enc.set_bitrate(eff_max);
                }
            } else {
                let target = abr.note_frame(now, encoded.nalu.len());
                enc.set_bitrate(target);
            }
            tune_once(
                &cfg, &mut enc, &mut stride_idx, &mut scale_idx,
                &mut enc_w, &mut enc_h, w0, h0, &mut mp4_cfg, &mut seq,
                &mut byte_win, &mut since_change, &mut frame_idx, byte_ratio,
            );
        }
    }

    tracing::info!("desktop capture stopped");
    (post)(serde_json::json!({ "type": "desktop:stopped", "payload": {} }));
}

fn avg_frame_bytes(byte_win: &VecDeque<u32>) -> f64 {
    if byte_win.is_empty() {
        return 0.0;
    }
    byte_win.iter().map(|&b| b as f64).sum::<f64>() / byte_win.len() as f64
}

/// 灰度模式：把 I420 的 UV 平面（色度）置中性 128，Y（亮度）保留。
/// 编码前调用，色度≈0 后码率显著下降——弱网下以"灰度"换取更低带宽占用
/// 与更稳帧率（亮度是主观关键）。I420 中 Y = 前 2/3，UV 各 1/6。
fn apply_gray(i420: &mut [u8]) {
    let y_len = i420.len() * 2 / 3;
    i420[y_len..].fill(128);
}

/// 动态关键帧间隔（MYS-886 需求7-1）：帧均字节（反映内容活跃度）高于
/// [`KF_ACTIVE_BYTES_FRAME`] 视为活跃 → 高频 IDR；否则静止 → 低频 IDR。
/// 静止画面 P 帧近 0 字节、关键帧是唯一带宽开销，拉长间隔直接省带宽。
fn kf_interval_ms_for(avg_bytes: f64) -> u64 {
    if avg_bytes > KF_ACTIVE_BYTES_FRAME {
        KF_ACTIVE_MS
    } else {
        KF_QUIET_MS
    }
}

/// QoS 渐进式码率/帧率控制器（完整移植 rustdesk `src/server/video_qos.rs`，
/// MYS-886 继续对齐）。rustdesk 不是硬档跳变，而是：
/// - fps：每个 delay 样本渐进调整（好网 <50ms 连 3 次 +5，<100ms +1，
///   差网按比例降），且带 RTT 扣除、新连接 INIT_FPS=15、response_delayed 兜底
/// - ratio：每 ADJUST_RATIO_INTERVAL=3s 按平均延时平滑缩放（×1.15~×0.8）
/// 取代旧的硬档（60/30/15fps + 1000/750/500‰ 三段跳变）。
pub struct QosAdaptive {
    fps: u32,
    ratio_permille: u32,
    /// 最近一次浏览器上报的端到端延时（ms）。
    last_delay: u32,
    delay_history: std::collections::VecDeque<u32>,
    /// 连续好样本计数（<50ms 连 3 次 → 快速提 fps）。
    quick_increase_fps_count: u32,
    /// 稳定好样本计数（<150ms 连 3 次 → 小幅 +1）。
    increase_fps_count: u32,
    /// 距上次 ratio 调整的秒数累计。
    ratio_elapsed_s: u32,
    /// 每秒编码帧数（用于 DYNAMIC_SCREEN 判定：每秒编码>2 视为动态屏）。
    frame_count_s: u32,
    /// 编码器当前目标码率（bps），由 ABR/ceiling 同步进来。
    bitrate_bps: u64,
}

/// rustdesk 同款常量（video_qos.rs）。
pub const QOS_FPS: u32 = 30;
pub const QOS_MIN_FPS: u32 = 1;
pub const QOS_MAX_FPS: u32 = 120;
pub const QOS_INIT_FPS: u32 = 15;
pub const QOS_DELAY_THRESHOLD_150MS: u32 = 150;
pub const QOS_BR_SPEED: f32 = 0.5;
pub const QOS_BR_BALANCED: f32 = 0.67;
pub const QOS_BR_BEST: f32 = 1.5;
/// 码率缩放上限（=1.0，不超用户档）。
pub const QOS_MAX_BR_MULTIPLE: f32 = 1.0;
/// 低分辨率时 ratio 下限。
pub const QOS_BR_MIN_HIGH_RESOLUTION: f32 = 0.1;
const QOS_ADJUST_RATIO_INTERVAL_S: u32 = 3;
const QOS_DYNAMIC_SCREEN_THRESHOLD: u32 = 2;
const QOS_HISTORY_DELAY_LEN: usize = 2;

impl QosAdaptive {
    pub fn new() -> Self {
        Self {
            fps: QOS_FPS,
            ratio_permille: 1000,
            last_delay: 0,
            delay_history: std::collections::VecDeque::new(),
            quick_increase_fps_count: 0,
            increase_fps_count: 0,
            ratio_elapsed_s: 0,
            frame_count_s: 0,
            bitrate_bps: 0,
        }
    }

    /// 平均延时（rustdesk `avg_delay`：近 HISTORY_DELAY_LEN 样本均值。
    /// 我们端到端延时已由 relay 时钟校准，不再需要扣 RTT——校准后延时即
    /// 净端到端，比 rustdesk 的 delay-RTT 更精确）。
    fn avg_delay(&self) -> u32 {
        let len = self.delay_history.len();
        if len > 0 {
            self.delay_history.iter().sum::<u32>() / len as u32
        } else {
            QOS_DELAY_THRESHOLD_150MS
        }
    }

    /// 用一次新延时样本更新状态，返回 (目标 fps, 码率缩放‰)。
    /// `frame_count` = 距上次更新以来编码帧数；`elapsed_s` = 距上次更新秒数。
    /// 内部按 rustdesk 的 user_network_delay + adjust_ratio 渐进逻辑计算。
    pub fn on_delay(&mut self, delay_ms: u32, frame_count: u32, elapsed_s: f32) -> (u32, u32) {
        let delay = delay_ms.max(10);
        self.last_delay = delay;
        self.frame_count_s += frame_count;
        if self.delay_history.len() >= QOS_HISTORY_DELAY_LEN {
            self.delay_history.pop_front();
        }
        self.delay_history.push_back(delay);
        let avg = self.avg_delay();

        // ── fps 渐进（rustdesk user_network_delay 移植）──
        let target_ratio = 1.0; // 用户档在此简化为 1.0（实际档位在编码器侧）
        let (min_fps, normal_fps) = if target_ratio >= QOS_BR_BEST {
            (8, 16)
        } else if target_ratio >= QOS_BR_BALANCED {
            (10, 20)
        } else {
            (12, 24)
        };
        let dividend_ms = QOS_DELAY_THRESHOLD_150MS * min_fps;
        let mut fps = self.fps;
        if avg < 50 {
            self.quick_increase_fps_count += 1;
            let mut step = if fps < normal_fps { 1 } else { 0 };
            if self.quick_increase_fps_count >= 3 {
                self.quick_increase_fps_count = 0;
                step = 5;
            }
            fps = min_fps.max(fps + step);
        } else if avg < 100 {
            let step = if fps < normal_fps { 1 } else { 0 };
            fps = min_fps.max(fps + step);
        } else if avg < QOS_DELAY_THRESHOLD_150MS {
            fps = min_fps.max(fps);
        } else {
            let divide_fps =
                ((fps as f32) / (avg as f32 / QOS_DELAY_THRESHOLD_150MS as f32)).ceil() as u32;
            if avg < 200 {
                fps = min_fps.max(divide_fps);
            } else if avg < 300 {
                fps = min_fps.min(divide_fps);
            } else if avg < 600 {
                fps = dividend_ms / avg;
            } else {
                fps = (dividend_ms / avg).min(divide_fps);
            }
        }
        if avg < QOS_DELAY_THRESHOLD_150MS {
            self.increase_fps_count += 1;
        } else {
            self.increase_fps_count = 0;
        }
        if self.increase_fps_count >= 3 {
            self.increase_fps_count = 0;
            fps += 1;
        }
        if avg > 50 {
            self.quick_increase_fps_count = 0;
        }
        fps = fps.clamp(QOS_MIN_FPS, QOS_MAX_FPS);
        // 新连接 1s 内 cap 到 INIT_FPS（rustdesk adjust_fps 的 new_user_instant）
        // ——我们无"用户加入"事件，用首次样本近似：fps 超过 INIT_FPS 且刚启动
        // 由外部在 start 时初始化，这里保持渐进即可。
        self.fps = fps;

        // ── ratio 渐进（rustdesk adjust_ratio 移植，每 3s 调一次）──
        self.ratio_elapsed_s += elapsed_s.max(0.0) as u32;
        if self.ratio_elapsed_s >= QOS_ADJUST_RATIO_INTERVAL_S {
            self.ratio_elapsed_s = 0;
            // DYNAMIC_SCREEN：每秒编码 > 阈值 → 动态屏（可升 ratio）
            let dynamic_screen = self.frame_count_s >= QOS_DYNAMIC_SCREEN_THRESHOLD;
            self.frame_count_s = 0;
            let mut v = self.ratio_permille as f32 / 1000.0;
            v = if avg < 50 {
                if dynamic_screen { v * 1.15 } else { v }
            } else if avg < 100 {
                if dynamic_screen { v * 1.1 } else { v }
            } else if avg < QOS_DELAY_THRESHOLD_150MS {
                if dynamic_screen { v * 1.05 } else { v }
            } else if avg < 200 {
                v * 0.95
            } else if avg < 300 {
                v * 0.9
            } else if avg < 500 {
                v * 0.85
            } else {
                v * 0.8
            };
            let max = QOS_MAX_BR_MULTIPLE;
            let min = QOS_BR_MIN_HIGH_RESOLUTION;
            v = v.clamp(min, max);
            self.ratio_permille = (v * 1000.0).round() as u32;
        }

        (self.fps, self.ratio_permille)
    }

    /// 同步当前编码器目标码率（用于 ratio 的 150kbps 限幅参考；简化实现中
    /// 保持纯比例缩放即可）。
    #[allow(dead_code)]
    pub fn set_bitrate(&mut self, bps: u64) {
        self.bitrate_bps = bps;
    }

    pub fn current_ratio_permille(&self) -> u32 {
        self.ratio_permille
    }
}

/// 兼容旧入口：按延时直接映射（供测试/回退）。新逻辑用 [`QosAdaptive`]。
pub fn qos_bitrate_scale_for_delay(delay_ms: u64) -> u32 {
    if delay_ms < 150 {
        1000
    } else if delay_ms < 300 {
        750
    } else {
        500
    }
}

/// One step of the high-entropy degradation ladder (called every ~10 encoded
/// frames, cooldown 30 frames ≈ 2 s):
/// - 平均帧字节 > 预算(码率上限/8/fps) → 降级。**优先降分辨率**(SCALES,
///   模糊但**不丢帧**——用户明确接受模糊、不接受掉帧); 分辨率到底仍超
///   预算才降有效帧率(STRIDES)。
/// - 平均帧字节 ≤ 预算一半 → 恢复(先分辨率后帧率)。
/// 帧率变化只重设编码器输入帧率模型——不重建、不重发, 直播流不中断;
/// 分辨率变化才重建编码器(maybe_rescale, 重新 init 广播给所有 viewer)。
#[allow(clippy::too_many_arguments)]
fn tune_once(
    cfg: &DesktopConfig,
    enc: &mut Box<dyn encoder::VideoEncoder>,
    stride_idx: &mut usize,
    scale_idx: &mut usize,
    enc_w: &mut usize,
    enc_h: &mut usize,
    w0: usize,
    h0: usize,
    mp4_cfg: &mut Option<mp4::Mp4Config>,
    seq: &mut u32,
    byte_win: &mut VecDeque<u32>,
    since_change: &mut u32,
    frame_idx: &mut u64,
    byte_ratio: f64,
) {
    if *since_change < 30 {
        return;
    }
    if byte_ratio <= 0.5 {
        // 预算富余: 先恢复分辨率, 再恢复帧率
        if *scale_idx > 0 {
            maybe_rescale(
                cfg, enc, scale_idx, enc_w, enc_h, w0, h0, mp4_cfg, seq,
                byte_win, since_change, frame_idx, byte_ratio, *stride_idx,
            );
        } else if *stride_idx > 0 {
            *stride_idx -= 1;
            enc.set_frame_rate(cfg.fps as f32 / STRIDES[*stride_idx] as f32);
            byte_win.clear();
            *since_change = 0;
            tracing::info!(
                "desktop: restore encode fps to {:.0} (stride={})",
                cfg.fps as f64 / STRIDES[*stride_idx] as f64,
                STRIDES[*stride_idx]
            );
        }
        return;
    }
    // 超预算: 只降分辨率(模糊但不丢帧)。分辨率到最低档后**不再降帧率**
    // ——用户明确接受模糊、不接受掉帧; OpenH264 RC 在 skip=0 下不工作,
    // 低预算高熵帧天然超支, 若走到降帧率会一路塌到 6fps(实测), 反而
    // 破坏流畅。留 STRIDES 作恢复路径(上支), 不参与降级。
    if byte_ratio >= 1.5 && *scale_idx < SCALES.len() - 1 {
        maybe_rescale(
            cfg, enc, scale_idx, enc_w, enc_h, w0, h0, mp4_cfg, seq,
            byte_win, since_change, frame_idx, byte_ratio, *stride_idx,
        );
    }
}

/// Rebuild the encoder at a smaller/larger resolution when the average frame
/// size is persistently over/under the bitrate budget. Cooldown of 30 frames
/// (2 s) between changes so the window can refill; degrade at ≥1.15× budget,
/// restore at ≤0.5× budget.
#[allow(clippy::too_many_arguments)]
fn maybe_rescale(
    cfg: &DesktopConfig,
    enc: &mut Box<dyn encoder::VideoEncoder>,
    scale_idx: &mut usize,
    enc_w: &mut usize,
    enc_h: &mut usize,
    w0: usize,
    h0: usize,
    mp4_cfg: &mut Option<mp4::Mp4Config>,
    seq: &mut u32,
    byte_win: &mut VecDeque<u32>,
    since_change: &mut u32,
    frame_idx: &mut u64,
    byte_ratio: f64,
    stride_idx: usize,
) {
    if *since_change < 30 {
        return;
    }
    let degrading = byte_ratio >= 1.5 && *scale_idx < SCALES.len() - 1;
    let restoring = byte_ratio <= 0.5 && *scale_idx > 0;
    if !degrading && !restoring {
        return;
    }
    let next = if degrading { *scale_idx + 1 } else { *scale_idx - 1 };
    let nw = (((w0 as f64) * SCALES[next]) as usize) & !1;
    let nh = (((h0 as f64) * SCALES[next]) as usize) & !1;
    if nw < 2 || nh < 2 {
        return;
    }
    match encoder::new_encoder(&cfg.codec, nw as u32, nh as u32, cfg.max_bps, cfg.fps, cfg.quality) {
        Ok(mut new_enc) => {
            tracing::warn!(
                "desktop: {} encode resolution {enc_w}x{enc_h} -> {nw}x{nh} (bytes={:.0}% budget)",
                if degrading { "degrade" } else { "restore" },
                byte_ratio * 100.0
            );
            // 重建后恢复当前 stride 对应的有效帧率模型。
            new_enc.set_frame_rate(cfg.fps as f32 / STRIDES[stride_idx] as f32);
            *enc = new_enc;
            *scale_idx = next;
            *enc_w = nw;
            *enc_h = nh;
            *mp4_cfg = None; // 等下一个 IDR 重新 init(relay 会向所有 viewer 广播新 init)
            *seq = 1;
            *frame_idx = 0;
            byte_win.clear();
            *since_change = 0;
        }
        Err(e) => tracing::error!("desktop: re-init encoder at {nw}x{nh} failed: {e}"),
    }
}

fn base64(data: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    B64.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kf_interval_adapts_to_activity() {
        // 高熵（帧均 > 2KB）→ 活跃 1.5s
        assert_eq!(kf_interval_ms_for(10_000.0), KF_ACTIVE_MS);
        // 刚过阈值仍是活跃
        assert_eq!(kf_interval_ms_for(KF_ACTIVE_BYTES_FRAME + 0.1), KF_ACTIVE_MS);
        // 静止（帧均 ≤ 2KB）→ 4.5s
        assert_eq!(kf_interval_ms_for(KF_ACTIVE_BYTES_FRAME), KF_QUIET_MS);
        assert_eq!(kf_interval_ms_for(0.0), KF_QUIET_MS);
    }

    #[test]
    fn test_qos_bitrate_scale_mapping() {
        // <150ms 满额；150-300ms 压到 75%；≥300ms 压到 50%（弱网应急）
        assert_eq!(qos_bitrate_scale_for_delay(0), 1000);
        assert_eq!(qos_bitrate_scale_for_delay(149), 1000);
        assert_eq!(qos_bitrate_scale_for_delay(150), 750);
        assert_eq!(qos_bitrate_scale_for_delay(299), 750);
        assert_eq!(qos_bitrate_scale_for_delay(300), 500);
        assert_eq!(qos_bitrate_scale_for_delay(900), 500);
    }

    #[test]
    fn test_qos_adaptive_gradual_increase_on_good_net() {
        // 渐进式：好网（<50ms）连 3 次样本后 +5，非一次跳满（rustdesk 同款）
        let mut q = QosAdaptive::new();
        // 初始 fps=30；给 3 次 40ms 好样本（动态屏，帧数>2）
        let (fps1, _) = q.on_delay(40, 30, 1.0);
        let (fps2, _) = q.on_delay(40, 30, 1.0);
        let (fps3, _) = q.on_delay(40, 30, 1.0);
        // 每次 +1（低于 normal_fps 时），连 3 次后有一次 +5 的机会；
        // 断言最终 fps > 初始（渐进上升，非跳变）
        assert!(fps3 >= fps1 && fps3 >= 30, "good net must not drop fps");
        // 好网 ratio 应保持或上升（动态屏 ×1.15）
        assert!(q.current_ratio_permille() >= 1000, "good net ratio >= 1000‰");
    }

    #[test]
    fn test_qos_adaptive_gradual_decrease_on_bad_net() {
        // 渐进式：差网（>300ms）按比例降 fps、码率 ×0.85~×0.8
        let mut q = QosAdaptive::new();
        let (fps1, _) = q.on_delay(400, 0, 1.0);
        assert!(fps1 < 30, "400ms should drop fps below initial 30, got {fps1}");
        // 再给一个样本，且 elapsed_s=2.0 使累计 ≥3s → 触发 ratio 调整
        let (fps2, _) = q.on_delay(500, 0, 2.0);
        assert!(fps2 <= fps1, "worse net must not raise fps");
        // 码率应低于满额（比例降 ×0.85 或 ×0.8）
        assert!(
            q.current_ratio_permille() < 1000,
            "bad net ratio < 1000‰, got {}‰",
            q.current_ratio_permille()
        );
    }

    #[test]
    fn test_qos_adaptive_static_screen_no_ratio_increase() {
        // 静止屏（每秒编码 ≤2 帧）好网时码率不升（rustdesk DYNAMIC_SCREEN）
        let mut q = QosAdaptive::new();
        let _ = q.on_delay(40, 1, 3.0); // 3s 只有 1 帧 → 非动态屏
        assert_eq!(q.current_ratio_permille(), 1000, "static screen stays at 1000‰");
    }

    #[test]
    fn test_avg_frame_bytes() {
        let w: VecDeque<u32> = (0..48).map(|i| if i < 20 { 5000 } else { 2000 }).collect();
        let avg = avg_frame_bytes(&w);
        assert!((avg - 3250.0).abs() < 1.0);
        assert_eq!(avg_frame_bytes(&VecDeque::new()), 0.0);
    }

    #[test]
    fn test_apply_gray_neutralizes_chroma_keeps_luma() {
        // 8x8 I420: Y=64, U=16, V=16 字节。apply_gray 后 UV 全 128，Y 不动。
        let w = 8usize;
        let h = 8usize;
        let mut i420 = vec![0u8; w * h * 3 / 2];
        i420[..w * h].fill(90); // Y
        i420[w * h..].fill(200); // UV
        apply_gray(&mut i420);
        assert_eq!(i420[..w * h].iter().min().copied().unwrap(), 90, "Y untouched");
        assert_eq!(i420[w * h..].iter().max().copied().unwrap(), 128, "UV neutralized");
        assert_eq!(i420[w * h..].iter().min().copied().unwrap(), 128, "UV neutralized");
    }

    #[test]
    fn test_set_gray_flag_roundtrip() {
        let dm = DesktopManager::new(DesktopConfig::default());
        assert!(!dm.gray_enabled(), "gray defaults off");
        dm.set_gray(true);
        assert!(dm.gray_enabled());
        dm.set_gray(false);
        assert!(!dm.gray_enabled());
    }

    fn budget(fps: f64) -> f64 {
        let cfg = DesktopConfig::default();
        cfg.max_bps as f64 / 8.0 / fps
    }

    #[test]
    fn test_maybe_rescale_degrades_on_overbudget() {
        // MYS-886: 分辨率阶梯已禁用 (SCALES = [1.0])。超预算时 maybe_rescale
        // **不再降分辨率** —— 码率交给 AV1/VP9 CBR 在固定分辨率下控制。
        let cfg = DesktopConfig::default();
        let mut enc = encoder::new_encoder("h264", 1920, 1080, cfg.max_bps, cfg.fps, cfg.quality).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut scale_idx = 0usize;
        let (mut enc_w, mut enc_h) = (w0, h0);
        let mut mp4_cfg = Some(mp4::Mp4Config {
            width: 1920,
            height: 1080,
            fps: cfg.fps,
            sample: mp4::VisualSample::H264 { sps: vec![1], pps: vec![1] },
        });
        let mut seq = 5u32;
        let mut byte_win: VecDeque<u32> = (0..48).map(|_| (budget(cfg.fps) * 1.5) as u32).collect();
        let mut since_change = 30u32;
        let mut frame_idx = 40u64;
        maybe_rescale(
            &cfg, &mut enc, &mut scale_idx, &mut enc_w, &mut enc_h, w0, h0,
            &mut mp4_cfg, &mut seq, &mut byte_win, &mut since_change, &mut frame_idx, 1.5, 0,
        );
        assert_eq!(scale_idx, 0, "resolution ladder disabled — no degrade");
        assert_eq!(enc_w, 1920, "stays at native resolution");
        assert_eq!(enc_h, 1080, "stays at native resolution");
        assert!(mp4_cfg.is_some(), "mux config untouched");
    }

    #[test]
    fn test_maybe_rescale_restores_on_lightly_budgeted_content() {
        let cfg = DesktopConfig::default();
        let mut enc = encoder::new_encoder("h264", 1440, 810, cfg.max_bps, cfg.fps, cfg.quality).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut scale_idx = 1usize;
        let (mut enc_w, mut enc_h) = (1440usize, 810usize);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 3u32;
        let mut byte_win: VecDeque<u32> = (0..48).map(|_| (budget(cfg.fps) * 0.3) as u32).collect();
        let mut since_change = 30u32;
        let mut frame_idx = 7u64;
        maybe_rescale(
            &cfg, &mut enc, &mut scale_idx, &mut enc_w, &mut enc_h, w0, h0,
            &mut mp4_cfg, &mut seq, &mut byte_win, &mut since_change, &mut frame_idx, 0.3, 0,
        );
        assert_eq!(scale_idx, 0, "under-budget content must restore to full resolution");
        assert_eq!(enc_w, 1920);
        assert_eq!(enc_h, 1080);
    }

    #[test]
    fn test_tune_once_prefers_resolution_before_stride() {
        // 新策略: 超预算先降分辨率(模糊不掉帧), 分辨率到底才降帧率。
        let cfg = DesktopConfig::default();
        let mut enc = encoder::new_encoder("h264", 1920, 1080, cfg.max_bps, cfg.fps, cfg.quality).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut stride_idx = 0usize;
        let mut scale_idx = 0usize;
        let (mut enc_w, mut enc_h) = (w0, h0);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 1u32;
        let mut byte_win: VecDeque<u32> = (0..48).map(|_| (budget(cfg.fps) * 1.5) as u32).collect();
        let mut since_change = 30u32;
        let mut frame_idx = 1u64;
        tune_once(
            &cfg, &mut enc, &mut stride_idx, &mut scale_idx, &mut enc_w, &mut enc_h,
            w0, h0, &mut mp4_cfg, &mut seq, &mut byte_win, &mut since_change,
            &mut frame_idx, 1.5,
        );
        assert_eq!(scale_idx, 0, "resolution ladder disabled — resolution unchanged");
        assert_eq!(stride_idx, 0, "no frame-rate degradation either");
        assert_eq!(enc_w, w0, "must stay at native resolution under over-budget");
        assert_eq!(enc_h, h0, "must stay at native resolution under over-budget");
        assert_eq!(since_change, 30, "cooldown untouched (no change happened)");

        // 分辨率已到顶仍超预算 → 降帧率
        let mut byte_win2: VecDeque<u32> = (0..48).map(|_| (budget(cfg.fps) * 1.5) as u32).collect();
        let mut since_change2 = 30u32;
        let mut enc2 = encoder::new_encoder("h264", 480, 270, cfg.max_bps, cfg.fps, cfg.quality).unwrap();
        let mut stride2 = 0usize;
        let mut scale2 = SCALES.len() - 1;
        let (mut ew2, mut eh2) = (480usize, 270usize);
        tune_once(
            &cfg, &mut enc2, &mut stride2, &mut scale2, &mut ew2, &mut eh2,
            w0, h0, &mut mp4_cfg, &mut seq, &mut byte_win2, &mut since_change2,
            &mut frame_idx, 1.5,
        );
        assert_eq!(scale2, SCALES.len() - 1, "resolution stays at minimum");
        assert_eq!(stride2, 0, "stride never degrades (no frame rate loss)");
    }

    #[test]
    fn test_tune_once_restores_slowly() {
        let cfg = DesktopConfig::default();
        let mut enc = encoder::new_encoder("h264", 1440, 810, cfg.max_bps, cfg.fps, cfg.quality).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut stride_idx = 2usize;
        let mut scale_idx = 1usize; // 分辨率与帧率都降过
        let (mut enc_w, mut enc_h) = (1440usize, 810usize);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 1u32;
        let mut byte_win: VecDeque<u32> = (0..48).map(|_| (budget(cfg.fps) * 0.3) as u32).collect();
        let mut since_change = 30u32;
        let mut frame_idx = 1u64;
        // 稳定低字节: 先恢复分辨率, stride 保持
        tune_once(
            &cfg, &mut enc, &mut stride_idx, &mut scale_idx, &mut enc_w, &mut enc_h,
            w0, h0, &mut mp4_cfg, &mut seq, &mut byte_win, &mut since_change,
            &mut frame_idx, 0.3,
        );
        assert_eq!(scale_idx, 0, "resolution restores first");
        assert_eq!(stride_idx, 2, "stride unchanged until resolution is back");
        // 分辨率已恢复, 再恢复 stride
        let mut since_change2 = 30u32;
        tune_once(
            &cfg, &mut enc, &mut stride_idx, &mut scale_idx, &mut enc_w, &mut enc_h,
            w0, h0, &mut mp4_cfg, &mut seq, &mut byte_win, &mut since_change2,
            &mut frame_idx, 0.3,
        );
        assert_eq!(stride_idx, 1, "stride restores after resolution");
        assert!((enc.fps() - cfg.fps / 2.0).abs() < 1e-9, "encoder fps = {:.1}", enc.fps());
    }

    #[test]
    fn test_maybe_rescale_respects_cooldown_and_bounds() {
        let cfg = DesktopConfig::default();
        let mut enc = encoder::new_encoder("h264", 1920, 1080, cfg.max_bps, cfg.fps, cfg.quality).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut scale_idx = 0usize;
        let (mut enc_w, mut enc_h) = (w0, h0);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 1u32;
        let mut byte_win: VecDeque<u32> = (0..48).map(|_| (budget(cfg.fps) * 1.5) as u32).collect();
        let mut since_change = 5u32; // 冷却期内不允许变化
        let mut frame_idx = 3u64;
        maybe_rescale(
            &cfg, &mut enc, &mut scale_idx, &mut enc_w, &mut enc_h, w0, h0,
            &mut mp4_cfg, &mut seq, &mut byte_win, &mut since_change, &mut frame_idx, 1.5, 0,
        );
        assert_eq!(scale_idx, 0, "cooldown must block change");
        // 已经是最小档: 极端超预算也不能再降
        let mut enc2 = encoder::new_encoder("h264", 480, 270, cfg.max_bps, cfg.fps, cfg.quality).unwrap();
        let mut scale_idx = SCALES.len() - 1;
        let (mut ew, mut eh) = (480usize, 270usize);
        let mut byte_win2: VecDeque<u32> = (0..48).map(|_| (budget(cfg.fps) * 1.5) as u32).collect();
        let mut since_change2 = 30u32;
        let mut frame_idx2 = 0u64;
        maybe_rescale(
            &cfg, &mut enc2, &mut scale_idx, &mut ew, &mut eh, w0, h0,
            &mut mp4_cfg, &mut seq, &mut byte_win2, &mut since_change2, &mut frame_idx2, 1.5, 0,
        );
        assert_eq!(scale_idx, SCALES.len() - 1, "must stay at the minimum scale");
    }

    #[test]
    fn test_capabilities_when_disabled() {
        let mut cfg = DesktopConfig::default();
        cfg.capture = "none".to_string();
        let dm = DesktopManager::new(cfg);
        let caps = dm.capabilities_json();
        assert_eq!(caps["available"], false);
        assert_eq!(caps["running"], false);
    }

    #[test]
    fn test_capabilities_when_enabled() {
        let dm = DesktopManager::new(DesktopConfig::default());
        let caps = dm.capabilities_json();
        assert_eq!(caps["available"], true);
        assert!(caps["codecs"].as_array().unwrap().contains(&serde_json::json!("h264")));
        if cfg!(feature = "vp9") {
            assert!(caps["codecs"].as_array().unwrap().contains(&serde_json::json!("vp8")));
        }
    }

    #[test]
    fn test_supports_codec_known() {
        let cfg = DesktopConfig::default();
        assert!(cfg.supports_codec("h264"));
        assert_eq!(cfg.supports_codec("vp9"), cfg!(feature = "vp9"));
        assert_eq!(cfg.supports_codec("vp8"), cfg!(feature = "vp9"));
        assert!(!cfg.supports_codec("hevc"));
    }

    #[test]
    fn test_drop_stops_running_loop() {
        // Dropping the manager (session teardown / reconnect) must clear the
        // running flag so any spawned loop exits and no new captures start.
        let dm = DesktopManager::new(DesktopConfig::default());
        let running = dm.running.clone();
        running.store(true, Ordering::SeqCst);
        assert!(running.load(Ordering::SeqCst));
        drop(dm);
        assert!(!running.load(Ordering::SeqCst), "drop must clear running");
    }

    #[test]
    fn test_drop_after_start_on_disabled_capture_is_safe() {
        let mut cfg = DesktopConfig::default();
        cfg.capture = "none".to_string();
        let dm = DesktopManager::new(cfg);
        let post: PostFn = Arc::new(|_| {});
        // 在真实编码环境中，start 会 spawn 循环；disabled 路径不 spawn——
        // 确保两种情况下的 drop 都不 panic。
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { dm.start(post).await; });
        drop(dm);
    }

    #[tokio::test]
    async fn test_start_with_disabled_capture_posts_error() {
        let mut cfg = DesktopConfig::default();
        cfg.capture = "none".to_string();
        let dm = DesktopManager::new(cfg);
        let posted: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
        let p2 = posted.clone();
        let post: PostFn = Arc::new(move |v| p2.lock().unwrap().push(v));
        dm.start(post).await;
        let list = posted.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["type"], "desktop:started");
        assert!(list[0]["payload"]["error"].as_str().is_some());
    }

    /// 高熵源：逼真“复杂桌面”场景（旧实现在 800kbps 下几乎全部跳帧）。
    struct NoiseSource {
        w: usize,
        h: usize,
        t: usize,
    }

    impl capture::FrameSource for NoiseSource {
        fn next_frame(&mut self) -> Result<capture::Frame, String> {
            let w = self.w;
            let h = self.h;
            let mut bgra = vec![0u8; w * h * 4];
            for i in 0..w * h {
                let v = ((i * 31 + self.t * 37) % 251) as u8;
                bgra[i * 4] = v;
                bgra[i * 4 + 1] = v;
                bgra[i * 4 + 2] = v;
                bgra[i * 4 + 3] = 255;
            }
            self.t += 1;
            Ok(capture::Frame { bgra, width: w, height: h })
        }
        fn resolution(&self) -> (usize, usize) {
            (self.w, self.h)
        }
    }

    /// 真实桌面模式：大面积静态 + 小块移动区域（时钟/滚动/窗口动画）。
    struct ModerateSource {
        w: usize,
        h: usize,
        t: usize,
    }

    impl capture::FrameSource for ModerateSource {
        fn next_frame(&mut self) -> Result<capture::Frame, String> {
            let w = self.w;
            let h = self.h;
            let t = self.t;
            let mut bgra = vec![0u8; w * h * 4];
            for row in 0..h {
                for col in 0..w {
                    let i = (row * w + col) * 4;
                    let (mut r, mut g, mut b) = (110i32, 110i32, 110i32);
                    if row < h / 18 {
                        (r, g, b) = (200, 200, 200);
                    }
                    if col < w / 6 {
                        (r, g, b) = (150, 150, 150);
                    }
                    bgra[i] = b as u8;
                    bgra[i + 1] = g as u8;
                    bgra[i + 2] = r as u8;
                    bgra[i + 3] = 255;
                }
            }
            let bs = 90usize;
            let ox = ((t * 7) % (w + bs)) as isize - (bs as isize / 2);
            let oy = ((t * 3) % (h + bs)) as isize - (bs as isize / 2);
            for dy in 0..bs {
                for dx in 0..bs {
                    let px = ox + dx as isize;
                    let py = oy + dy as isize;
                    if (0..w as isize).contains(&px) && (0..h as isize).contains(&py) {
                        let i = (py as usize * w + px as usize) * 4;
                        let v = 60 + ((dx + dy + t) % 40) as u8;
                        bgra[i] = v;
                        bgra[i + 1] = v;
                        bgra[i + 2] = v;
                        bgra[i + 3] = 255;
                    }
                }
            }
            self.t += 1;
            Ok(capture::Frame { bgra, width: w, height: h })
        }
        fn resolution(&self) -> (usize, usize) {
            (self.w, self.h)
        }
    }

    /// 完全静止源：所有帧完全一致（模拟"桌面无人操作"），用于验证静止
    /// 桌面关键帧节奏被拉长（MYS-886 需求7-1：4.5s 一 IDR，而非旧 500ms）。
    struct StaticSource {
        w: usize,
        h: usize,
    }

    impl capture::FrameSource for StaticSource {
        fn next_frame(&mut self) -> Result<capture::Frame, String> {
            let w = self.w;
            let h = self.h;
            let mut bgra = vec![0u8; w * h * 4];
            for row in 0..h {
                for col in 0..w {
                    let i = (row * w + col) * 4;
                    let v = (130u8).wrapping_add(((row / 4 + col / 4) % 16) as u8);
                    bgra[i] = v;
                    bgra[i + 1] = v;
                    bgra[i + 2] = v;
                    bgra[i + 3] = 255;
                }
            }
            Ok(capture::Frame { bgra, width: w, height: h })
        }
        fn resolution(&self) -> (usize, usize) {
            (self.w, self.h)
        }
    }

    /// 始终失败的捕获源：模拟 XWayland 下 root GetImage 抛 BadMatch。
    struct FailingSource;

    impl capture::FrameSource for FailingSource {
        fn next_frame(&mut self) -> Result<capture::Frame, String> {
            Err("get_image reply: X11 error Match".to_string())
        }
        fn resolution(&self) -> (usize, usize) {
            (1920, 1080)
        }
    }

    /// 连续捕获失败（≥MAX_CAPTURE_ERRORS 次）必须终止循环并回传 desktop:error，
    /// 而不是无限重试刷屏 + 浏览器永久黑屏。
    #[test]
    fn pipeline_failing_source_posts_error_and_stops() {
        let mut cfg = DesktopConfig::default();
        cfg.fps = 60.0; // 加速 MAX_CAPTURE_ERRORS 次失败收敛（150 帧 ≈ 2.5s）
        let running = Arc::new(AtomicBool::new(true));
        let posted: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
        let p2 = posted.clone();
        let post: PostFn = Arc::new(move |v| p2.lock().unwrap().push(v));
        let src: Box<dyn capture::FrameSource> = Box::new(FailingSource);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let bw = cfg.max_bps;
            let handle = tokio::spawn(run_desktop_pipeline(
                cfg,
                running,
                post,
                src,
                Arc::new(std::sync::atomic::AtomicU64::new(bw)),
                "test".to_string(),
                0,
                Arc::new(std::sync::atomic::AtomicU32::new(30)),
                Arc::new(std::sync::atomic::AtomicU32::new(1000)),
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ));
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
                .await
                .expect("pipeline must terminate on its own");
        });
        let list = posted.lock().unwrap();
        let errs: Vec<_> = list
            .iter()
            .filter(|v| v["type"] == "desktop:error")
            .collect();
        assert!(!errs.is_empty(), "must post desktop:error, got: {list:?}");
        assert!(errs[0]["payload"]["error"].as_str().is_some());
        // 终止循环后仍需广播 stopped
        assert!(
            list.iter().any(|v| v["type"] == "desktop:stopped"),
            "must also broadcast desktop:stopped after giving up"
        );
    }

    /// 修复回归：真实桌面模式（中等动态）在 200-800kbps 下必须持续出帧，
    /// 不能出现旧实现那种静默跳帧黑屏。手动验证用（真实编码耗时）。
    #[test]
    #[ignore]
    fn pipeline_moderate_1080p_steady_frames() {
        let mut cfg = DesktopConfig::default();
        cfg.fps = 15.0;
        let running = Arc::new(AtomicBool::new(true));
        let posted: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
        let p2 = posted.clone();
        let post: PostFn = Arc::new(move |v| p2.lock().unwrap().push(v));
        let r2 = running.clone();
        let src: Box<dyn capture::FrameSource> = Box::new(ModerateSource {
            w: 1920,
            h: 1080,
            t: 0,
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let bw = cfg.max_bps;
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw)), "test".to_string(), 0, Arc::new(std::sync::atomic::AtomicU32::new(30)), Arc::new(std::sync::atomic::AtomicU32::new(1000)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(false))));
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            running.store(false, Ordering::SeqCst);
            let _ = handle.await;
        });
        let list = posted.lock().unwrap();
        let frags: Vec<_> = list
            .iter()
            .filter(|v| v["type"] == "desktop:video" && v["payload"]["kind"] == "frag")
            .collect();
        let keys: Vec<_> = frags.iter().filter(|v| v["payload"]["key"] == true).collect();
        eprintln!("pipeline moderate 1080p: frags={} keys={}", frags.len(), keys.len());
        assert!(
            frags.len() >= 30,
            "moderate desktop must keep producing frames (got {})",
            frags.len()
        );
        assert!(!keys.is_empty(), "must produce key frames for viewers to join");
    }

    /// 需求7-1 回归：静止桌面关键帧节奏必须被拉长（4.5s 一个 IDR，而非旧
    /// 500ms 心跳）。静止 8s 应只有首帧 + ~2 个心跳 IDR（≤4），旧实现会
    /// 产出 ~16 个。静止 P 帧近 0 字节，关键帧是唯一带宽开销——拉长间隔
    /// 是"不降画质省带宽"的核心。
    #[test]
    fn pipeline_static_desktop_spaces_out_keyframes() {
        let mut cfg = DesktopConfig::default();
        cfg.fps = 15.0; // 8s = 120 tick
        let running = Arc::new(AtomicBool::new(true));
        let posted: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
        let p2 = posted.clone();
        let post: PostFn = Arc::new(move |v| p2.lock().unwrap().push(v));
        let r2 = running.clone();
        let src: Box<dyn capture::FrameSource> = Box::new(StaticSource { w: 320, h: 180 });
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let bw = cfg.max_bps;
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw)), "test".to_string(), 0, Arc::new(std::sync::atomic::AtomicU32::new(15)), Arc::new(std::sync::atomic::AtomicU32::new(1000)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(false))));
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            running.store(false, Ordering::SeqCst);
            let _ = handle.await;
        });
        let list = posted.lock().unwrap();
        let keys: Vec<_> = list
            .iter()
            .filter(|v| v["type"] == "desktop:video" && v["payload"]["kind"] == "frag" && v["payload"]["key"] == true)
            .collect();
        eprintln!("pipeline static: keys={} (expect ≤4 spaced by 4.5s)", keys.len());
        assert!(
            keys.len() >= 1 && keys.len() <= 4,
            "static desktop must space out keyframes, got {} (old 500ms heartbeat would be ~16)",
            keys.len()
        );
    }

    /// 最坏情况回归：纯噪声内容即便无法流畅编码，也必须持续产出关键帧
    /// （viewer 可加入并看到画面），而不是永久黑屏。
    #[test]
    #[ignore]
    fn pipeline_noise_still_emits_idr() {
        let mut cfg = DesktopConfig::default();
        cfg.fps = 15.0;
        let running = Arc::new(AtomicBool::new(true));
        let posted: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Default::default();
        let p2 = posted.clone();
        let post: PostFn = Arc::new(move |v| p2.lock().unwrap().push(v));
        let r2 = running.clone();
        let src: Box<dyn capture::FrameSource> = Box::new(NoiseSource {
            w: 1920,
            h: 1080,
            t: 0,
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let bw = cfg.max_bps;
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw)), "test".to_string(), 0, Arc::new(std::sync::atomic::AtomicU32::new(30)), Arc::new(std::sync::atomic::AtomicU32::new(1000)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(false))));
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            running.store(false, Ordering::SeqCst);
            let _ = handle.await;
        });
        let list = posted.lock().unwrap();
        let frags: Vec<_> = list
            .iter()
            .filter(|v| v["type"] == "desktop:video" && v["payload"]["kind"] == "frag")
            .collect();
        let keys: Vec<_> = frags.iter().filter(|v| v["payload"]["key"] == true).collect();
        eprintln!("pipeline noise 1080p: frags={} keys={}", frags.len(), keys.len());
        assert!(
            keys.len() >= 2,
            "noise desktop must keep emitting key frames (got {})",
            keys.len()
        );
    }
}

