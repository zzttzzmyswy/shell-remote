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

use crate::agent::desktop::encoder::next_lower_codec;

/// R5#32 剪贴板大文本防护：超过该字节数截断（完整文本走文件传输为远期方向，
/// 先防超大控制消息阻塞上行 / 远端剪贴板写入卡顿）。512KB 覆盖正常剪贴板
/// 内容（代码/文档/日志），只有异常大复制才会触发。
pub const CLIPBOARD_MAX_CHARS: usize = 512 * 1024;

/// 安全截断到最近 UTF-8 字符边界（`floor_char_boundary`，Rust 1.73+）：避免
/// 在字节中间切出非法字符/半个码点。超过 `max` 时返回前缀，否则原样。
/// 纯函数（截断提示由调用方记日志）。
pub fn clipboard_truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        text
    } else {
        &text[..text.floor_char_boundary(max)]
    }
}

/// 当前 unix 毫秒（i64，活跃度打点用；时钟异常回退 0）。
fn unix_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 活跃度判定（R5#25 空闲回收可见性）：距最近真实新帧 ≤1500ms 视为活跃
/// （内容活动）；超过即静止/空闲（agent 已回收编码资源，仅 4s IDR 心跳）。
fn active_at(last_active_ms: i64, now_ms: i64) -> bool {
    now_ms.saturating_sub(last_active_ms) < 1500
}

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
    /// 抓帧独立上限（fps，R3 乙83 / R5#135）。0 = 不限制（默认，动态时
    /// 全速抓帧由编码 min_gap 跳帧）。设值则动态桌面抓帧也按此节流。
    pub capture_fps: f64,
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
            capture_fps: 0.0, // 0 = 抓帧不限速（编码 min_gap 跳帧兜底）
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

/// 解析 start 用的捕获显示器：运行时选屏覆盖（web 下拉可改）优先，
/// 否则回落启动配置的 --desktop-display（再否则 None = 平台默认屏）。
fn resolved_display(runtime: &Option<String>, config: &Option<String>) -> Option<String> {
    runtime.clone().or_else(|| config.clone())
}

/// 帧 buffer 池（R5 #127 SIMD 内存池第一块）：预分配复用 `Vec<u8>`，
/// 避开高频路径（字节转换/合成帧）每帧堆分配——编码输入 buffer 借出
/// 复用、用后归还（cap 保留）。运行期统计复用/新分配计数，可观测
/// 池命中率。为后续 frame-ring（捕获侧预分配 + 引用计数）的组件基础。
#[derive(Debug)]
pub struct FramePool {
    /// 空闲 buffer（复用 Pooled 保持 cap）。
    free: std::sync::Mutex<Vec<Vec<u8>>>,
    /// 池上限（超过则新释放的 buffer 直接丢弃，防无限膨胀）。
    capacity: usize,
    /// 借出命中（复用）次数。
    reused: std::sync::atomic::AtomicU64,
    /// 借出新分配次数。
    allocated: std::sync::atomic::AtomicU64,
}

impl FramePool {
    pub fn new(capacity: usize) -> Self {
        Self {
            free: std::sync::Mutex::new(Vec::new()),
            capacity: capacity.max(1),
            reused: std::sync::atomic::AtomicU64::new(0),
            allocated: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 预分配 `n` 个容量 `size` 的 buffer（启动时摊分初始化成本）。
    pub fn prealloc(&self, n: usize, size: usize) {
        let mut free = self.free.lock().unwrap();
        for _ in 0..n {
            free.push(vec![0u8; size]);
        }
    }

    /// 借出一个容量 ≥ `size` 的 buffer：池中有则复用（不重置长度，
    /// 调用方按需使用前 `size` 字节），否则新分配。
    pub fn acquire(&self, size: usize) -> Vec<u8> {
        let mut free = self.free.lock().unwrap();
        for i in (0..free.len()).rev() {
            if free[i].capacity() >= size {
                let mut buf = free.swap_remove(i);
                buf.clear();
                self.reused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return buf;
            }
        }
        drop(free);
        self.allocated.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Vec::with_capacity(size)
    }

    /// 归还用后 buffer（cap 保留供复用）；池满则丢弃。
    pub fn release(&self, mut buf: Vec<u8>) {
        buf.clear();
        let mut free = self.free.lock().unwrap();
        if free.len() < self.capacity {
            free.push(buf);
        }
    }

    /// (复用次数, 新分配次数)——可观测池命中率。
    pub fn stats(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering as O;
        (self.reused.load(O::Relaxed), self.allocated.load(O::Relaxed))
    }
}

/// 从池借 buffer 构建捕获帧（R5 #127 第二块）：合成帧（静态 IDR/reqkey
/// 重编）的 bgra 从 `FramePool` 借出并拷贝，主循环处理完 `release` 归还
/// ——避免每次合成帧 clone 都在堆上新分配一个大 buffer。
fn pooled_frame(pool: &FramePool, w: usize, h: usize, bgra: &[u8]) -> capture::Frame {
    let mut buf = pool.acquire(bgra.len());
    buf.extend_from_slice(bgra);
    capture::Frame {
        bgra: buf,
        width: w,
        height: h,
    }
}

/// Controls the desktop capture+encode task.
/// 心跳扩展 KPI 快照（R5#150）：agent 心跳携带的最小桌面运行态。
/// 供 relay/admin 侧可观测（不依赖浏览器面板），字段无锁原子读。
pub struct DesktopKpi {
    /// 桌面流是否在跑（agent 心跳时刻）。
    pub running: bool,
    /// 当前编码方案（`av1` / `vp9` / `h264`）。
    pub codec: String,
    /// 当前目标帧率（内容驱动：静态 1 / 动态满帧 / 背压降档）。
    pub fps: u32,
    /// QoS 码率缩放（千分比，1000 = 100%）。
    pub quality_permille: u32,
    /// 编码器当前目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 最近单帧编码耗时（ms）。暂缺（未做跨线程累计），0 = 不可得。
    pub encode_ms: u32,
    /// 内容活跃（最近真实新帧 ≤1.5s，R5#25）——admin KPI 曲线可观测
    /// 静止/活跃时间线。
    pub active: bool,
    /// relay→浏览器 fan-out 拥塞累计次数（R5#16）——admin KPI 曲线可观测
    /// 传输段拥塞时间线。
    pub bp_count: u32,
}

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
    /// 运行时目标编码帧率（内容驱动：静态 1fps / 动态满帧 / 解码背压才降帧，
/// 见 QosAdaptive）。
    fps: Arc<std::sync::atomic::AtomicU32>,
    /// 运行时质量档倍率（web 码率下拉可改：speed/balanced/best）。初始 =
    /// config.quality；改动后重建桌面流应用（set_codec 同机制）。
    quality: std::sync::RwLock<f32>,
    /// 运行时选屏覆盖（web 显示器下拉可改）：None = 用启动配置的
    /// --desktop-display（未指定则 X11 默认屏）；Some(name) = 切换捕获源。
    /// 初始 = config.display；改动后重建桌面流应用（set_codec 同机制）。
    display: std::sync::RwLock<Option<String>>,
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
    /// 编码器当前目标码率（bps，由 pipeline 每次建/重建编码器时同步）。
    /// QoS ratio 的 150kbps 限幅与 1Mbps 基线用它（rustdesk store_bitrate）。
    qos_bitrate: Arc<std::sync::atomic::AtomicU64>,
    /// 灰度模式（web 端可切）：编码前把 UV 平面置中性 128，色度≈0。
    /// 弱网下带宽占用显著下降（亮度是主观关键），画质降为灰度可接受。
    /// 运行时即时生效，不重建编码器/不重启流。
    gray: Arc<std::sync::atomic::AtomicBool>,
    /// 浏览器关键帧请求（desktop:reqkey → 本 flag → 编码循环 force_idr）。
    /// 接入/参考链断裂/解码错误时即时重同步，不再等周期 IDR（对齐 rustdesk
    /// 控制端 refresh_video 语义，MYS-886）。
    idr_request: Arc<std::sync::atomic::AtomicBool>,
    /// 最近一次真实新帧的墙钟毫秒。编码循环每收到真实新帧（内容活动，
    /// 非静态 IDR 心跳）即刷新；qos-ack 回传 `active = elapsed < 1500ms`
    /// 供浏览器面板显示"静止/活跃"（R5#25 空闲回收可见性——静止时 4s IDR、
    /// 不空转编码回收资源，用户在面板上能看到状态而非误以为卡死）。
    last_active_at: Arc<std::sync::atomic::AtomicI64>,
    /// relay→浏览器 fan-out 拥塞回传累计次数（R5#16 背压可观测闭环）：
    /// agent 每收到 `desktop:congested` 递增；qos-ack 回传 `bp_count` 供
    /// 浏览器面板显示"relay 拥塞"计数——传输段拥塞对用户/调试可见。
    backpressure: Arc<std::sync::atomic::AtomicU32>,
}

impl DesktopManager {
    pub fn new(config: DesktopConfig) -> Self {
        let bps = config.max_bps;
        let codec = config.codec.clone();
        let fps0 = config.fps.clamp(1.0, 60.0) as u32;
        let quality0 = config.quality;
        let max_bps0 = config.max_bps;
        let display0 = config.display.clone();
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
            display: std::sync::RwLock::new(display0),
            max_bps: std::sync::RwLock::new(max_bps0),
            qos_scale: Arc::new(std::sync::atomic::AtomicU32::new(1000)),
            qos: tokio::sync::Mutex::new(QosAdaptive::new(quality0)),
            qos_frames: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            qos_last_sample: std::sync::atomic::AtomicU64::new(0),
            qos_bitrate: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            gray: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            idr_request: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_active_at: Arc::new(std::sync::atomic::AtomicI64::new(unix_ms_now())),
            backpressure: Arc::new(std::sync::atomic::AtomicU32::new(0)),
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

    /// R5#25 空闲回收可见性：最近 1500ms 内是否有真实新帧（内容活动）。
    /// 静止时 agent 已回收编码资源（仅 KF_QUIET_MS 静态 IDR），面板据此
    /// 显示"静止"而非让用户误以为画面卡死。
    pub fn is_active(&self) -> bool {
        let now = unix_ms_now();
        let last = self.last_active_at.load(std::sync::atomic::Ordering::Relaxed);
        active_at(last, now)
    }

    /// R5#16 relay→浏览器 fan-out 拥塞回传计数：agent 每收到
    /// `desktop:congested` 递增，qos-ack 回传供面板显示（传输段拥塞可观测）。
    pub fn bump_backpressure(&self) {
        use std::sync::atomic::Ordering as O;
        self.backpressure.fetch_add(1, O::Relaxed);
    }

    /// 累计 relay 段拥塞次数（面板"relay 拥塞"计数）。
    pub fn backpressure_count(&self) -> u32 {
        use std::sync::atomic::Ordering as O;
        self.backpressure.load(O::Relaxed)
    }

    /// 请求下一个编码帧立即出关键帧（浏览器接入/解码错误/参考链断裂时调用，
    /// 对齐 rustdesk 控制端 refresh_video）。编码循环每拍检查该 flag。
    pub fn request_idr(&self) {
        use std::sync::atomic::Ordering as O;
        self.idr_request.store(true, O::Relaxed);
        tracing::info!("desktop IDR requested (reqkey)");
    }

    /// 编码循环每帧调用：为 QoS DYNAMIC_SCREEN 统计编码帧数。
    pub fn bump_qos_frame(&self) {
        use std::sync::atomic::Ordering as O;
        self.qos_frames.fetch_add(1, O::Relaxed);
    }

    /// QoS 动态调整目标帧率/码率：每次浏览器上报端到端延时 + 解码背压，
    /// fps 由内容活动驱动（见 QosAdaptive::on_delay：静态 1fps/动态满帧、
    /// 解码背压才降帧），码率由拥塞增量（avg−基线）平滑缩放。
    pub async fn on_qos_delay(
        &self,
        delay_ms: u32,
        probe_ms: u32,
        decode_fps: u32,
        decode_queue: u32,
        ack_seq: u64,
    ) -> (u32, u32, u64, QosQualityState) {
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
        let bitrate_bps = self.qos_bitrate.load(O::Relaxed);
        let ctx = QosSampleCtx {
            quality_ratio: self.config.quality.clamp(QOS_BR_SPEED, QOS_BR_BEST),
            highest_fps: cap,
            bitrate_kbps: (bitrate_bps / 1000).max(1) as u32,
            decode_fps_hint: decode_fps,
            decode_queue_hint: decode_queue,
            now_us,
        };
        let (fps, permille) = {
            let mut q = self.qos.lock().await;
            q.on_delay(delay_ms, probe_ms, frames, elapsed_s, &ctx)
        };
        let fps = fps.clamp(1, cap);
        let permille = permille.clamp(100, 1000);
        self.fps.store(fps, O::Relaxed);
        self.qos_scale.store(permille, O::Relaxed);
        // 当前生效目标码率（对齐 rustdesk TestDelay 携带 target_bitrate 回传，
        // MYS-886 #153）：用户硬顶 >0 用其值×缩放；否则按 abr ceiling×缩放。
        let base_bps = if self.config.max_bps > 0 {
            self.config.max_bps.max(self.config.min_bps)
        } else {
            encoder::target_bitrate(1920, 1080, 0, self.config.quality).max(self.config.min_bps)
        };
        let bitrate_kbps = base_bps * (permille as u64) / 1000 / 1000;
        // QoS 五态质量状态（R4 甲A0/A2）：on_delay 已更新，读取供日志与
        // qos-ack 回传浏览器（面板"QoS 状态"行）。
        let state = {
            let q = self.qos.lock().await;
            q.quality_state()
        };
        // QoS 快照（R5#149）：结构化单行，含状态、目标 fps、质量缩放、
        // 码率、解码背压（dfps/dq）与 ack 进度。admin/复盘按行解析即可
        // 还原每 1s 的 QoS 决策轨迹（对齐 rustdesk QoS 可观测性）。
        tracing::info!(
            delay_ms,
            probe_ms,
            fps,
            qos_scale = permille,
            qos_state = ?state,
            bitrate_kbps,
            cap,
            decode_fps = decode_fps,
            decode_queue = decode_queue,
            ack_seq,
            "desktop QoS: adaptive adjusted"
        );
        (fps, permille, bitrate_kbps, state)
    }

    /// QoS 动态调整目标帧率（内容驱动：静态 1fps/动态满帧/解码背压才降帧，下限
    /// 15）。编码循环按此值动态改 tick 周期。
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

    /// 运行时选屏值：None = 启动默认（--desktop-display 或 X11 默认屏）。
    pub fn display(&self) -> Option<String> {
        self.display.read().map(|d| d.clone()).unwrap_or(None)
    }

    /// 运行时热切换捕获显示器（rustdesk 对齐的选屏能力）。`""` / 缺失 →
    /// 恢复启动默认显示器；否则切到同名显示器（X11 RANDR name）。仅当
    /// 桌面正在运行时重建桌面流（stop 旧流 → start 新流，start 用新 display
    /// 打开捕获源）。display 不变时是 no-op。
    pub async fn select_display(&self, display: &str, post: PostFn) -> Result<(), String> {
        let next: Option<String> = if display.trim().is_empty() {
            None
        } else {
            Some(display.trim().to_string())
        };
        {
            let mut cur = self.display.write().map_err(|_| "display lock poisoned".to_string())?;
            if *cur == next {
                return Ok(());
            }
            *cur = next;
        }
        if self.is_running() {
            self.stop(post.clone()).await;
            self.start(post).await;
        }
        Ok(())
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

    /// 心跳扩展 KPI 快照（R3 丙140 / R5#150）：agent 心跳里附带当前桌面
    /// 运行态，relay/admin 侧可观测不依赖浏览器面板。字段全部读自原子，
    /// 无锁；`encode_ms` 暂缺（编码耗时未做跨线程累计，见 #84 预算降级），
    /// 为 0 表示不可得——admin 曲线只画有值的行。
    pub fn kpi_snapshot(&self) -> DesktopKpi {
        use std::sync::atomic::Ordering as O;
        DesktopKpi {
            running: self.running.load(O::Relaxed),
            codec: self.codec(),
            fps: self.fps.load(O::Relaxed),
            quality_permille: self.qos_scale.load(O::Relaxed),
            bitrate_kbps: (self.qos_bitrate.load(O::Relaxed) / 1000) as u32,
            encode_ms: 0,
            active: self.is_active(),
            bp_count: self.backpressure_count(),
        }
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
        let text = clipboard_truncate(text, CLIPBOARD_MAX_CHARS);
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
        let text = clipboard_truncate(&text, CLIPBOARD_MAX_CHARS);
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
        // 运行时选屏（web 显示器下拉可改）覆盖启动 --desktop-display。
        let disp = self.display.read().map(|d| d.clone()).unwrap_or(None);
        cfg.display = resolved_display(&disp, &cfg.display);
        let mb = self.max_bps.read().map(|b| *b).unwrap_or(0);
        if mb > 0 {
            cfg.max_bps = mb;
        }
        let bandwidth = self.bandwidth.clone();
        let clock_offset = self.clock_offset.load(std::sync::atomic::Ordering::Relaxed);
        let last_active_at = self.last_active_at.clone();
        let fps_ctl = self.fps.clone();
        let qos_scale = self.qos_scale.clone();
        let qos_frames = self.qos_frames.clone();
        let qos_bitrate = self.qos_bitrate.clone();
        let gray = self.gray.clone();
        let idr_request = self.idr_request.clone();
        let task = tokio::task::spawn(async move {
            run_desktop_loop(cfg, running, post, bandwidth, clock_offset, fps_ctl, qos_scale, qos_frames, qos_bitrate, gray, idr_request, last_active_at).await;
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
/// 关键帧是唯一带宽开销 → 心跳 IDR 从 500ms 拉长到 4s（带宽降至 1/8×）；
/// 活跃（帧均字节超阈值）时 1.5s 一个 IDR 保持 seek 与参考链健康。
/// 判定用 [`avg_frame_bytes`]：> [`KF_ACTIVE_BYTES_FRAME`] 视为活跃。
/// 静止间隔取 4s：浏览器端已不因静止无新帧而把 e2e 累加虚高（见
/// desktop.js 静止判定），渲染停住只是"画面本来就静止"，交互首帧由
/// pipeline 的"有新帧立即编码"保证即时恢复——长间隔同时压低静态带宽
/// （对齐 rustdesk 静止低带宽，MYS-886）。
pub const KF_QUIET_MS: u64 = 4000;
/// 活跃期 IDR 6s（原 1.5s）：IDR≈5×P 帧，1.5s 间隔吃掉 ~12% 动态带宽且在
/// WebCodecs 端是解码队列尖峰来源。接入/参考链断裂由浏览器 reqkey 即时兜底
/// （对齐 rustdesk 直播无周期 IDR 的语义，active sanity 点保留 6s 一个）。
pub const KF_ACTIVE_MS: u64 = 6000;
/// 帧均字节阈值：超过则视为活跃内容（需要高频关键帧）。
pub const KF_ACTIVE_BYTES_FRAME: f64 = 2048.0;

/// 动态关键帧间隔由 [`KF_QUIET_MS`]/[`KF_ACTIVE_MS`] 取代旧的 500ms 固定
/// 静止心跳（MYS-886 需求7-1：静止 4s 一个 IDR，带宽显著低于 relay 观看者
/// 30s 空闲超时，不会误判断流）。

/// The capture → convert → encode → mux → post loop.
/// Handles OpenH264's penalty frame-skipping (observed on high-motion
/// 编码器降级决策（R5#84 慢帧 / R5#85 故障热备统一出口）：返回应降级到的
/// 下一档 codec（av1→vp9→vp8→h264），None = 不降。触发条件二选一：
/// 连续 [`SLOW_ENCODE_TRIGGER`] 帧单帧编码 > [`SLOW_ENCODE_MS`]（CPU 跑
/// 不动，慢），或连续 [`ENCODE_ERR_TRIGGER`] 帧 encode 返回 Err（编码器
/// 实际故障/崩溃，坏）。已降级过（`already_degraded`，一次性防重建风暴）
/// 或已是末档（h264，`next_lower_codec` 返回 None）都不再降。
const SLOW_ENCODE_TRIGGER: u32 = 10;
/// 编码器故障热备阈值（R5#85）：encode 连续返回 Err 达此数即判定编码器
/// 故障，降级到更低 codec（区别于 #84 的"慢"——这是"坏"）。
const ENCODE_ERR_TRIGGER: u32 = 5;

fn next_degrade_codec(codec: &str, slow_streak: u32, err_streak: u32, already_degraded: bool) -> Option<String> {
    if already_degraded {
        return None;
    }
    let hit = slow_streak >= SLOW_ENCODE_TRIGGER || err_streak >= ENCODE_ERR_TRIGGER;
    if !hit {
        return None;
    }
    next_lower_codec(codec)
}

/// 编码器降级重建（R5#84 / R5#85 共用）：重建为 `lower` codec 并回写实际
/// 生效档。参数集变化 → `mp4_cfg` 置 None + `seq` 重置（下个 IDR 携带新
/// 参数集重发 init，广播给所有 viewer）；`force_idr` 让重建后首帧即关键帧。
/// 成功返回实际生效 codec，失败 Err（调用方置 `encode_degraded` 防重建风暴）。
async fn rebuild_encoder_degrade(
    enc: &mut Box<dyn encoder::VideoEncoder>,
    cfg: &mut DesktopConfig,
    lower: &str,
    enc_w: usize,
    enc_h: usize,
    mp4_cfg: &mut Option<mp4::Mp4Config>,
    seq: &mut u32,
) -> Result<String, String> {
    let (new_enc, actual) = encoder::create_encoder_fallback(
        lower,
        enc_w as u32,
        enc_h as u32,
        cfg.max_bps,
        cfg.fps,
        cfg.quality,
    )?;
    *enc = new_enc;
    cfg.codec = actual.clone();
    if mp4_cfg.is_some() {
        *mp4_cfg = None;
        *seq = 1;
    }
    enc.force_idr();
    Ok(actual)
}

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
    qos_bitrate: Arc<std::sync::atomic::AtomicU64>,
    gray: Arc<std::sync::atomic::AtomicBool>,
    idr_request: Arc<std::sync::atomic::AtomicBool>,
    last_active_at: Arc<std::sync::atomic::AtomicI64>,
) {
        let mut src = match capture::open_source(&cfg.capture, cfg.display.as_deref()) {
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
    run_desktop_pipeline(cfg, running, post, src, bandwidth, backend, clock_offset_ms, fps_ctl, qos_scale, qos_frames, qos_bitrate, gray, idr_request, last_active_at).await;
}

/// The capture → convert → encode → mux → post pipeline. Split from
/// [`run_desktop_loop`] so tests can inject a synthetic [`capture::FrameSource`].
async fn run_desktop_pipeline(
    cfg: DesktopConfig,
    running: Arc<AtomicBool>,
    post: PostFn,
    mut src: Box<dyn capture::FrameSource>,
    bandwidth: Arc<std::sync::atomic::AtomicU64>,
    backend: String,
    clock_offset_ms: i64,    fps_ctl: Arc<std::sync::atomic::AtomicU32>,
    qos_scale: Arc<std::sync::atomic::AtomicU32>,
    qos_frames: Arc<std::sync::atomic::AtomicU64>,
    qos_bitrate: Arc<std::sync::atomic::AtomicU64>,
    gray: Arc<std::sync::atomic::AtomicBool>,
    idr_request: Arc<std::sync::atomic::AtomicBool>,
    last_active_at: Arc<std::sync::atomic::AtomicI64>,
) {
    // cfg 需可变：编码器 fallback 后回写实际 codec。
    let mut cfg = cfg;
    // 光标独立通道（R5#64）：X11 GetImage 不含光标层，远程用户看不到鼠标
    // 指针。注入位置回调 → `desktop:cursor {x,y,shown}` 轻量消息（100ms 节流，
    // 光标移动不触发整帧重编码）。须在 src move 进抓帧线程前设置。
    let cursor_post = post.clone();
    src.set_cursor_cb(Box::new(move |x, y, shown| {
        (cursor_post)(serde_json::json!({
            "type": "desktop:cursor",
            "payload": { "x": x, "y": y, "shown": shown }
        }));
    }));
    // 多显示器最小子集（R5#136-146）：move 前枚举远端显示器拓扑，随
    // desktop:started 上报（浏览器/运维可见远端多屏；X11 实现返回 RANDR
    // 输出，其它后端空）。
    let monitors = src.list_monitors();
    // 截图线程化（rustdesk capture 线程对齐）：capture 挪到独立线程持续
    // 抓帧，编码循环 try_latest 非阻塞取最新帧——抓帧（X11/DXGI）不再拖慢
    // 编码，慢抓帧时跳帧追最新。src 被 move 进抓帧线程。
    let threaded = match capture::ThreadedFrameSource::spawn_with_max_fps(src, cfg.capture_fps) {
        Ok(t) => t,
        Err(e) => {
            (post)(serde_json::json!({
                "type": "desktop:started",
                "payload": { "codec": cfg.codec, "error": format!("capture thread failed: {e}") }
            }));
            return;
        }
    };
    let (mut w0, mut h0) = threaded.resolution();
    // R5 #127 内存池第二块：编码循环帧 buffer 池。预分配 2 个 cfg 尺寸
    // buffer；合成帧（静态 IDR/reqkey）从池借出拷贝、动态帧处理完归还
    // ——避开高频路径每帧堆分配（capture 侧预分配为后续框架级闭环）。
    let frame_pool = FramePool::new(4);
    frame_pool.prealloc(2, w0 as usize * h0 as usize * 4);
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
    // QoS ratio 的 current_bitrate 基准：同步编码器当前目标码率（bps）。
    qos_bitrate.store(abr_ceiling, std::sync::atomic::Ordering::Relaxed);

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
            "display": cfg.display,
            "displays": monitors.iter().map(|m| {
                serde_json::json!({
                    "name": m.name, "width": m.width, "height": m.height,
                    "x": m.x, "y": m.y,
                })
            }).collect::<Vec<_>>(),
        }
    }));

    // QoS 动态帧率：fps 是"两次编码的最小间隔"（上限节奏），不是硬节拍。
    // 反馈来自浏览器端到端延时（见 on_qos_delay），由 fps_ctl 读入。
    let start = std::time::Instant::now();
    let mut frame_idx: u64 = 0;
    // 上次强制 IDR 的虚拟墙钟（wall_ms 时基）。动态关键帧节拍用它判断
    // 是否到间隔（静止 KF_QUIET_MS / 活跃 KF_ACTIVE_MS，MYS-886 需求7-1）。
    let mut last_kf_wall: u64 = 0;
    // 上次编码/上次墙钟采样点（真实时间，见下方循环内说明）。
    let mut last_encode = std::time::Instant::now();
    let mut last_wall = std::time::Instant::now();
    // 最近一帧原始像素（静态 IDR / reqkey 重编用）。采样低频（≥500ms 才
    // 更新一次）限制拷贝开销；静态 IDR 只需要"最近画面"。
    let mut last_static: Option<(usize, usize, Vec<u8>)> = None;
    let mut last_static_at = std::time::Instant::now();
    // 编码耗时预算（R2 甲19/20 / R5#84）：连续 ≥10 帧单帧编码 >66ms 判定
    // 当前 codec 在软编上不堪重负，降级到更低复杂度的 codec（av1→vp9→h264，
    // 复用 fallback 链）。避免"持续超帧预算 → e2e 堆积 → QoS 误判拥塞"的
    // 假阳性路径；降级一次性，成功后重置计数。
    let mut slow_encode_streak: u32 = 0;
    // 编码器故障热备（R2 乙77 / R5#85）：encode 返回 Err 连续计数——编码器
    // 实际故障/崩溃（区别于 #84 的慢帧），达 ENCODE_ERR_TRIGGER 即降级。
    let mut encode_err_streak: u32 = 0;
    let mut encode_degraded = false;
    const SLOW_ENCODE_MS: f64 = 66.0;
    // moof 复用 muxer（R5#45）：tfhd 模板缓存，每帧只重建变化部分，
    // 减少高频路径的 moof 构建分配。
    let muxer = mp4::Mp4Muxer::new();

    while running.load(Ordering::SeqCst) {
        let cur_fps = fps_ctl.load(Ordering::SeqCst).clamp(1, 60);
        enc.set_frame_rate(cur_fps as f32);
        let min_gap = Duration::from_secs_f64(1.0 / cur_fps as f64);
        // 墙钟按真实时间推进——静止无帧时也要走，否则 IDR 节拍被推没
        // （IDR 是解码端 seek/清空后的重同步点，必须周期性刷新）。
        let now_wall = std::time::Instant::now();
        wall_ms += now_wall.duration_since(last_wall).as_millis() as u64;
        last_wall = now_wall;

        // 动态关键帧节拍（MYS-886 需求7-1）：由内容活跃度决定 IDR 间隔。
        // 静止（帧均字节 ≤ 阈值）时 P 帧近 0 字节，关键帧是唯一带宽开销
        // → 静止 KF_QUIET_MS / 活跃 KF_ACTIVE_MS 一 IDR。首帧强制 IDR
        // （同时就是 init 段的参数集）。IDR 决策只在"确有帧可编"时做：
        // 动态走新帧路径；静态用最近一帧按周期/reqkey 重编（viewer 可加入、
        // 参考链可刷新），静止其余时刻不空转编码（rustdesk would-block 语义）。
        let kf_ms = kf_interval_ms_for(avg_frame_bytes(&byte_win));

        // 取帧；无新帧时若 reqkey 或静态 IDR 到期 → 用最近一帧重编 IDR。
        // synthetic=true 标记静态心跳帧：它必须照常 POST（viewer 可加入/
        // 参考链可刷新），但**不计入 QoS 内容活动**（否则每 4s 一次的心跳
        // IDR 会被当成"内容在变"，fps 保持 30，静态永不闲置）。
        let mut synthetic = false;
        let fr = match threaded.try_latest() {
            Some(f) => {
                if last_static.is_none() || last_static_at.elapsed() >= Duration::from_millis(500) {
                    last_static = Some((f.width, f.height, f.bgra.clone()));
                    last_static_at = std::time::Instant::now();
                }
                // R5#25 空闲回收可见性：真实新帧 = 内容活动。静止（无新帧/
                // 静态 IDR 心跳）时保持 ||last_active_at|| 旧值，qos-ack 据此
                // 上报 active=false，浏览器面板显示"静止"。
                last_active_at.store(unix_ms_now(), std::sync::atomic::Ordering::Relaxed);
                Some(f)
            }
            None => {
                // 无新帧（capture 线程慢于编码，追最新帧跳帧）或线程在报错。
                let ec = threaded.err_count();
                if ec > 0 {
                    // 持续性捕获失败（例如 XWayland 下 root GetImage 抛 BadMatch、
                    // Windows 屏幕 DC 失效）。无限重试只会刷屏且永远黑屏。
                    //
                    // #130 对齐 rustdesk"黑屏 ≤2s 即报错"语义：
                    // - 首帧前失败（frame_idx==0）：没有可回退的画面，重试无意义，
                    //   立即终止并回传（浏览器秒级看到原因，而不是黑屏 150 次）；
                    // - 首帧后失败：首次失败即发 desktop:error 通知（浏览器即时
                    //   看到"捕获异常重试中"），但保留重试窗口（150 次 ≈ 10s，
                    //   Windows GDI 锁屏/安全桌面 DC 失效需重建上下文自愈）——
                    //   这是瞬时失效与持续失效的区分：通知即时、终止延后。
                    if frame_idx == 0 {
                        tracing::error!("desktop capture failed before first frame — giving up immediately");
                        let e = threaded.last_err().unwrap_or_else(|| "capture failed".to_string());
                        (post)(serde_json::json!({
                            "type": "desktop:error",
                            "payload": { "error": format!("capture failed before first frame: {e}") }
                        }));
                        break;
                    }
                    if ec == 1 {
                        // 首帧已出：首次失败立即通知浏览器（黑屏可见化），
                        // 不干等 150 次重试才报错。
                        let e = threaded.last_err().unwrap_or_else(|| "capture failed".to_string());
                        tracing::warn!("desktop capture error (retrying): {e}");
                        (post)(serde_json::json!({
                            "type": "desktop:error",
                            "payload": { "error": format!("capture error, retrying: {e}") }
                        }));
                    } else if ec >= MAX_CAPTURE_ERRORS {
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
                // 静态 IDR / reqkey：最近一帧重编一个关键帧（viewer 可加入、
                // 参考链可刷新），否则短轮询等帧，不整拍空转。
                let due = last_static.as_ref().is_some()
                    && (frame_idx == 0
                        || last_kf_wall + kf_ms <= wall_ms
                        || idr_request.load(Ordering::SeqCst));
                if due {
                    // 决策只读 flag；消费与 force_idr 由下方"有帧即决策"块统一处理。
                    synthetic = true;
                    let (w, h, bgra) = last_static.as_ref().unwrap();
                    // R5 #127：合成帧 bgra 从池借出（复用），处理完归还。
                    Some(pooled_frame(&frame_pool, *w, *h, bgra))
                } else {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
            }
        };
        let mut fr = match fr {
            Some(f) => f,
            None => continue,
        };
        // IDR 决策（仅在确有帧可编时）：首帧 / 周期到期 / reqkey 请求。
        if frame_idx == 0 || last_kf_wall + kf_ms <= wall_ms || idr_request.swap(false, Ordering::SeqCst) {
            enc.force_idr();
            last_kf_wall = wall_ms;
        }
        // 有新帧：fps 是两次编码的最小间隔（上限节奏）。间隔已满足 → 立即
        // 编码；未满足 → 等余量再编（期间 try_latest 追最新帧，跳过中间帧）。
        // 静止转动态：间隔通常早已满足，首帧立即编码（不整拍等）。
        let gap = last_encode.elapsed();
        if gap < min_gap {
            tokio::time::sleep(min_gap - gap).await;
            continue;
        }
        last_encode = std::time::Instant::now();
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
                    qos_bitrate.store(ceiling, std::sync::atomic::Ordering::Relaxed);
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
        // R5 #127：帧 bgra 已消耗（转换完成），buffer 归还池复用（cap 保留）。
        frame_pool.release(std::mem::take(&mut fr.bgra));
        // 灰度模式（web 端可选，弱网省带宽）：编码前把 UV 平面置中性 128，
        // 色度信息≈0，码率显著下降（亮度是弱网下的主观关键）。切换即时生效
        // （下帧起），不重建编码器。
        if gray.load(std::sync::atomic::Ordering::Relaxed) {
            apply_gray(&mut i420);
        }
        let encode_t0 = std::time::Instant::now();
        let encoded = match enc.encode(&i420) {
            Ok(e) => {
                encode_err_streak = 0;
                e
            }
            Err(e) => {
                // 编码器故障热备（R5#85）：连续 encode 报错（区别于 #84 慢帧）
                // 说明编码器实际故障/崩溃。不用 continue 直接丢弃——用空帧
                // 哨兵走统一降级出口（下方 next_degrade_codec 检查 err_streak），
                // 触发则重建为更低 codec；未触发则被下方空帧分支自然跳过。
                encode_err_streak += 1;
                tracing::warn!(streak = encode_err_streak, "encode error: {e}");
                crate::agent::desktop::encoder::EncodedFrame {
                    nalu: Vec::new(),
                    is_idr: false,
                    sps: None,
                    pps: None,
                }
            }
        };
        let encode_ms = encode_t0.elapsed().as_secs_f64() * 1000.0;
        // 编码耗时预算（R5#84）+ 编码器故障热备（R5#85）统一降级出口：
        // 慢帧（连续 >66ms）或故障（连续 Err）任一触发 → 重建为 fallback 链
        // 下一档 codec。一次性（encode_degraded）防重建风暴；h264 末档不再降。
        if !encode_degraded {
            if encode_ms > SLOW_ENCODE_MS {
                slow_encode_streak += 1;
            } else {
                slow_encode_streak = 0;
            }
            if let Some(lower) = next_degrade_codec(
                &cfg.codec,
                slow_encode_streak,
                encode_err_streak,
                encode_degraded,
            ) {
                match rebuild_encoder_degrade(
                    &mut enc,
                    &mut cfg,
                    &lower,
                    enc_w,
                    enc_h,
                    &mut mp4_cfg,
                    &mut seq,
                )
                .await
                {
                    Ok(actual) => {
                        tracing::warn!(
                            codec = %cfg.codec,
                            next = %actual,
                            ms = format!("{encode_ms:.0}"),
                            err_streak = encode_err_streak,
                            "encode degraded to {actual} (soft-encode budget / fault)"
                        );
                        encode_degraded = true;
                    }
                    Err(e) => {
                        tracing::warn!("encode degrade to {lower} failed: {e}");
                        encode_degraded = true;
                    }
                }
                if encoded.nalu.is_empty() {
                    // #85 Err 空哨兵触发降级：本帧无内容可 POST，重建完成后
                    // 下轮再编（#84 慢帧有真实帧，继续走原流程不丢帧）。
                    continue;
                }
            } else if slow_encode_streak >= SLOW_ENCODE_TRIGGER
                || encode_err_streak >= ENCODE_ERR_TRIGGER
            {
                // 达阈值但无档可降（已是 h264 末档）：一次性记录并停止，防刷屏。
                tracing::warn!(
                    ms = format!("{encode_ms:.0}"),
                    err_streak = encode_err_streak,
                    "encode degrade triggered but no lower codec ({}) — keeping",
                    cfg.codec
                );
                encode_degraded = true;
            }
        }
        frame_idx += 1;

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
            // 空帧不计入 QoS DYNAMIC_SCREEN 帧数：静止屏持续空转（内容不变
            // 也每拍编码）不能伪装成"动态屏"——否则高延迟会误当作差网
            // 证据，把 fps/码率打压（MYS-886 卡顿死锁的另一半根因）。
            continue;
        }
        // QoS DYNAMIC_SCREEN 统计：只计**实际内容变化**的帧（动态判定）。
        // synthetic（静态心跳 IDR/reqkey 重编）不算——它只是周期重同步，
        // 不表示内容在变（否则静态永不闲置）。
        use std::sync::atomic::Ordering as O;
        if !synthetic {
            qos_frames.fetch_add(1, O::Relaxed);
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
        let frag = muxer.fragment(cfg_mp4, &sample, pts_ms, encoded.is_idr, seq, capture_ms);
        seq += 1;
        pts_ms += frame_ms; // 只有真实 POST 的帧推进 fMP4 时间线
        post(serde_json::json!({
            "type": "desktop:video",
            "payload": {
                "kind": "frag", "key": encoded.is_idr, "data": base64(&frag),
                // R5#41 帧头字段显式化（JSON 阶段子集）：消息级 seq（递增帧
                // 号，与 fMP4 moof 内 seqn 同步）+ flags（key/delta）——协议
                // 层帧序号/标记，未来 binary 化时同构迁移；浏览器按需解析，
                // 向后兼容。
                "seq": seq,
                "flags": if encoded.is_idr { "key" } else { "delta" },
            }
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

/// QoS 码率/帧率控制器 —— rustdesk `src/server/video_qos.rs` 单用户版移植
/// （MYS-886 五轮对齐后的收敛版）。核心语义：
/// - **fps 由内容活动驱动（用户铁律）**：静态/无内容 → 1fps 省带宽；有内容
///   （字节帧出现）→ 立即拉满到 `--desktop-fps`。动态画面**永不**因网络降帧；
///   唯一例外是浏览器解码背压（解码 fps 低且队列深）→ 允许降到 24/15，
///   下限 `QOS_DYNAMIC_MIN_FPS=15`。网络/延时只作用于 quality。
/// - **quality（码率缩放）**：每 3s 按拥塞增量（avg−基线）平滑缩放（×1.05~×0.8）；
///   动态屏好网才升、拥塞**无条件**降；每次最多 +150kbps 限幅防陡升；
///   上下限按质量档与 1Mbps 基线计算。
/// - ratio 降级采用 rustdesk 同思路的"减基线"判定（基线 = 漏桶式近端最低
///   avg，等价 `delay−RTT`），固定 RTT 不触发降码率糊屏。
pub struct QosAdaptive {
    /// 全局当前 fps（对外生效值）。
    fps: u32,
    /// 码率缩放 ratio（rustdesk 语义：0.5/0.67/1.5 × 缩放倍数）。
    ratio: f32,
    /// 当前质量档 ratio（bound 到 0.5..=1.5）。
    quality_ratio: f32,
    /// QoS 质量状态（R4 甲 A0 五态 / A2 质量反馈状态机）：由网络层 probe
    /// 中值与拥塞增量 over 推导的显式状态，供日志/面板/复盘观测。**不影响
    /// 决策**——fps/ratio 仍由内容驱动 + over 判据决定，本状态只是可观测
    /// 快照（rustdesk QualityStatus 同构）。
    quality_state: QosQualityState,
    /// 最近一次浏览器上报的端到端延时（ms）。
    last_delay: u32,
    /// 延期历史（HISTORY_DELAY_LEN=2）。
    delay_history: std::collections::VecDeque<u32>,
    /// RTT 尖峰中值窗口（R2 甲25 / R5#113）：9 样本，取中值替代 2 窗口均值
    /// ——单次尖峰（TCP 重传/GC 停顿）不会瞬时触发降码率。
    median_window: std::collections::VecDeque<u32>,
    /// TestDelay 探针窗口（R4 甲 A1 / 对齐 rustdesk `cm::TestDelay`）：浏览器
    /// 单调时钟往返、纯网络层 RTT（不含编码/解码/渲染管线、不依赖时钟校准）。
    /// 5 样本中值抗尖峰。空 = 未上报（probe_ms=0，老浏览器/测试），
    /// [`probe_median_delay`] 返回 0 → 沿用原 e2e 判据。
    probe_window: std::collections::VecDeque<u32>,
    /// 首个 delay 样本后的用户 fps（None = 未收过样本）。
    delay_fps: Option<u32>,
    /// 距上次 ratio 调整的秒数累计（ADJUST_RATIO_INTERVAL=3s）。
    ratio_elapsed_s: u32,
    /// 本窗口编码帧数累计（DYNAMIC_SCREEN 判定，3s 一清）。
    frame_count_s: u32,
    /// 最近一次采样的动态屏判定（字节帧率 ≥ 2 帧/秒）。fps 决策用——动态
    /// 屏永不降到 `QOS_DYNAMIC_MIN_FPS` 以下。
    dynamic: bool,
    /// 健康基线延时（ms，漏桶式近端最低 avg，等价 rustdesk 的 RTT 估计）。
    /// 从首个样本即开始学习；quality 只对"avg 显著高于基线"的增量（拥塞
    /// 证据）做降档，固定传播延迟即使 800ms 也不降级。
    baseline_delay: u32,
    /// 编码器当前目标码率（kbps，由 abr ceiling 同步进来）。
    bitrate_kbps: u32,
    /// 首个 delay 样本时刻（unix us），代理 rustdesk new_user_instant
    /// （1s 内 cap INIT_FPS）。
    first_sample_us: Option<u64>,
}

/// rustdesk `video_qos.rs` / `scrap/codec.rs` 同款常量。
pub const QOS_FPS: u32 = 30;
pub const QOS_MIN_FPS: u32 = 1;
pub const QOS_MAX_FPS: u32 = 120;
pub const QOS_INIT_FPS: u32 = 15;
pub const QOS_DELAY_THRESHOLD_150MS: u32 = 150;
pub const QOS_BR_SPEED: f32 = 0.5;
pub const QOS_BR_BALANCED: f32 = 0.67;
pub const QOS_BR_BEST: f32 = 1.5;

/// QoS 质量状态（R4 甲 A0 五态 / A2 质量反馈状态机）。由网络层 probe 中值
/// 与拥塞增量 over 推导，供日志/面板/复盘观测；fps/ratio 决策不受其影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum QosQualityState {
    /// 首样本前（未建立基线）。
    Unknown,
    /// 网络健康（probe<100 且 over 低于阈值）。
    Good,
    /// 轻度劣化（probe 100-300 或 over 达阈值）。
    Medium,
    /// 明显劣化（probe 300-800 或 over 较大）。
    Degraded,
    /// 严重劣化（probe≥800 或 over 极大）。
    Critical,
}
/// BR_MIN（scrap）：ratio 绝对下限。
pub const QOS_BR_MIN: f32 = 0.2;
/// BR_MIN_HIGH_RESOLUTION（scrap）：高分屏 ratio 下限。
pub const QOS_BR_MIN_HIGH_RESOLUTION: f32 = 0.1;
/// MAX_BR_MULTIPLE=1.0：ratio 上限 = 当前质量档。
pub const QOS_MAX_BR_MULTIPLE: f32 = 1.0;
const QOS_ADJUST_RATIO_INTERVAL: u32 = 3; // 秒
const QOS_DYNAMIC_SCREEN_THRESHOLD: u32 = 2; // 帧/秒
const QOS_HISTORY_DELAY_LEN: usize = 2;
/// 动态屏 fps 下限（用户铁律：动态画面永不因网络降帧；唯一允许降帧的信号
/// 是浏览器解码背压，且下限为 15——低于此的动态内容毫无流畅可言）。静态屏
/// 保持 MIN_FPS=1 省带宽（静态无内容，掉帧不可见）。
const QOS_DYNAMIC_MIN_FPS: u32 = 15;
/// 解码背压触发 fps 降档的解码帧率阈值（浏览器每秒实际解码帧数）。
const QOS_DECODE_BACK_PRESSURE_FPS: u32 = 20;
/// 解码背压触发 fps 降档的解码队列深度阈值。
const QOS_DECODE_BACK_PRESSURE_QUEUE: u32 = 12;
/// 基线延时（健康 RTT）学习速率：每次样本把基线向当前 avg 收敛的比例。
/// 基线 = 近端观察到的"不拥塞"端到端延时；ratio 只对显著高于基线的
/// 增量（拥塞证据）做降档，固定 RTT 不会导致永久降码率糊屏。
const QOS_BASELINE_LEAK: f32 = 0.05;

/// `on_delay` 一次采样所需的会话资源（rustdesk video_qos 的输入）。
#[derive(Clone, Copy)]
pub struct QosSampleCtx {
    /// 当前质量档 ratio（= cfg.quality，0.5/0.67/1.5）。
    pub quality_ratio: f32,
    /// 帧率上限（--desktop-fps，对应 rustdesk highest_fps）。
    pub highest_fps: u32,
    /// 编码器当前目标码率 kbps（对应 store_bitrate 的 current_bitrate）。
    pub bitrate_kbps: u32,
    /// 浏览器最近 1s 实际解码帧率（0 = 未上报，不启用解码背压降帧）。
    pub decode_fps_hint: u32,
    /// 浏览器解码队列深度（WebCodecs decodeQueueSize）。
    pub decode_queue_hint: u32,
    /// 采样时刻（unix 微秒），用于 new_user 1s 窗口。
    pub now_us: u64,
}

impl QosAdaptive {
    pub fn new(quality: f32) -> Self {
        let quality_ratio = quality.clamp(QOS_BR_SPEED, QOS_BR_BEST);
        Self {
            fps: QOS_FPS,
            ratio: quality_ratio,
            quality_ratio,
            quality_state: QosQualityState::Unknown,
            last_delay: 0,
            delay_history: std::collections::VecDeque::new(),
            median_window: std::collections::VecDeque::new(),
            probe_window: std::collections::VecDeque::new(),
            delay_fps: None,
            ratio_elapsed_s: 0,
            frame_count_s: 0,
            dynamic: false,
            baseline_delay: 0,
            bitrate_kbps: 0,
            first_sample_us: None,
        }
    }

    /// 平均延时（rustdesk `avg_delay`：近 HISTORY_DELAY_LEN 样本均值）。
    fn avg_delay(&self) -> u32 {
        let len = self.delay_history.len();
        if len > 0 {
            self.delay_history.iter().sum::<u32>() / len as u32
        } else {
            QOS_DELAY_THRESHOLD_150MS
        }
    }

    /// RTT 中值（R2 甲25 / R5#113）：9 窗口排序取中位，单次尖峰不触发降档。
    fn median_delay(&self) -> u32 {
        let mut v: Vec<u32> = self.median_window.iter().copied().collect();
        if v.is_empty() {
            return QOS_DELAY_THRESHOLD_150MS;
        }
        v.sort_unstable();
        v[v.len() / 2]
    }

    /// TestDelay 网络层中值（R4 甲 A1 / 对齐 rustdesk `cm::TestDelay`）：
    /// 5 窗口排序取中位。0 = 未上报（probe_ms=0，老浏览器/测试）——
    /// 调用方沿用原 e2e 判据，不做网络层加固。
    fn probe_median_delay(&self) -> u32 {
        if self.probe_window.is_empty() {
            return 0;
        }
        let mut v: Vec<u32> = self.probe_window.iter().copied().collect();
        v.sort_unstable();
        v[v.len() / 2]
    }

    /// QoS 质量状态更新（R4 甲 A0 五态 / A2 质量反馈状态机）：由网络层
    /// probe 中值与拥塞增量 over 推导显式状态。状态**只作可观测快照**，
    /// 不驱动决策（fps/ratio 仍由内容活动 + over 判据决定）。迁移记日志，
    /// 供面板/复盘还原每 1s 的 QoS 走势。
    pub fn update_quality_state(&mut self) {
        let probe = self.probe_median_delay();
        let med = self.median_delay();
        let over = med.saturating_sub(self.baseline_delay.max(10));
        let band = self.rtt_band();
        let threshold = if band >= 2 { 60 } else { 100 };
        let next = if probe >= 800 || over >= threshold + 300 {
            QosQualityState::Critical
        } else if probe >= 300 || over >= threshold + 150 {
            QosQualityState::Degraded
        } else if probe >= 100 || over >= threshold {
            QosQualityState::Medium
        } else {
            QosQualityState::Good
        };
        if next != self.quality_state {
            tracing::info!(
                from = ?self.quality_state,
                to = ?next,
                probe_ms = probe,
                over,
                "desktop QoS state transition"
            );
            self.quality_state = next;
        }
    }

    /// 当前 QoS 质量状态（日志/面板观测用）。
    pub fn quality_state(&self) -> QosQualityState {
        self.quality_state
    }

    /// RTT 分带（R4 丁142 / R5#111）：按中值延时返回弱网档位，
    /// 供 adjust_ratio 参考——<100 正常 / 100-300 微调 / 300-800 低档 /
    /// >800 预警。返回 0=正常, 1=微调, 2=低档, 3=预警。
    fn rtt_band(&self) -> u8 {
        let m = self.median_delay();
        if m < 100 {
            0
        } else if m < 300 {
            1
        } else if m < 800 {
            2
        } else {
            3
        }
    }

    /// 处理一次新延时样本，返回 (目标 fps, 码率缩放‰ 相对满档)。`probe_ms` =
    /// TestDelay 网络层 RTT（0 = 未上报）；`frame_count` = 距上次采样编码帧数；
    /// `elapsed_s` = 距上次采样秒数；`ctx` = 会话资源。
    pub fn on_delay(
        &mut self,
        delay_ms: u32,
        probe_ms: u32,
        frame_count: u32,
        elapsed_s: f32,
        ctx: &QosSampleCtx,
    ) -> (u32, u32) {
        // 质量档/码率基准变化 → 按 rustdesk user_image_quality 重置 ratio。
        let quality_ratio = ctx.quality_ratio.clamp(QOS_BR_SPEED, QOS_BR_BEST);
        if self.quality_ratio != quality_ratio {
            self.quality_ratio = quality_ratio;
            self.ratio = quality_ratio;
        }
        if self.bitrate_kbps != ctx.bitrate_kbps {
            self.bitrate_kbps = ctx.bitrate_kbps;
        }

        let highest_fps = ctx.highest_fps.max(QOS_MIN_FPS);

        // ── delay 历史与均值（rustdesk add_delay/avg_delay）──
        let delay = delay_ms.max(10);
        self.last_delay = delay;
        if self.delay_history.len() > QOS_HISTORY_DELAY_LEN {
            self.delay_history.pop_front();
        }
        self.delay_history.push_back(delay);
        let avg = self.avg_delay().max(10);
        // RTT 尖峰中值滤波（R5#113）：9 窗口，单次尖峰不改变判定。
        if self.median_window.len() > 8 {
            self.median_window.pop_front();
        }
        self.median_window.push_back(delay);
        let med = self.median_delay().max(10);
        // TestDelay 探针窗口（R5#148 对齐 rustdesk cm::TestDelay）：
        // probe_ms=0 表示未上报（老浏览器/测试），跳过——probe_median_delay()
        // 返回 0 = 无探针，沿用原 e2e 判据。
        if probe_ms > 0 {
            if self.probe_window.len() > 4 {
                self.probe_window.pop_front();
            }
            self.probe_window.push_back(probe_ms.max(1));
        }
        // 基线延时更新（rustdesk RttCalculator 的简化版）：avg ≤ 基线立降，
        // 否则每次样本按 QOS_BASELINE_LEAK 缓慢上抬——固定 RTT（传播延迟）
        // 会慢慢被学成"新基线"，只有真正显著高于基线才判拥塞。
        if self.baseline_delay == 0 || avg <= self.baseline_delay {
            self.baseline_delay = avg;
        } else {
            self.baseline_delay = self.baseline_delay
                + (((avg - self.baseline_delay) as f32) * QOS_BASELINE_LEAK) as u32;
        }
        // 动态屏判定（无自锁版）：只要有实际字节帧出现即算动态。静止屏的帧
        // 是"空帧"（nalu 为空，不计入 frame_count），因此 frame_count>=1 就
        // 证明内容在变。**不再按"帧率/秒"判定**——那会被 fps 自锁：fps=1 时
        // 每秒只有 1 个字节帧，永远判不出动态，也就永远回不到高帧率（正是
        // 用户"动态页面被调到 1 帧"的机制根因）。首样本（elapsed<0.1，计时
        // 基准刚建立）沿用上一次判定。
        let dynamic = if elapsed_s >= 0.1 {
            frame_count >= 1
        } else {
            self.dynamic
        };
        self.dynamic = dynamic;

        // ── fps：内容驱动（用户铁律：动态画面永不因网络降帧）──
        // 静态 → 1fps（无内容，开销归零）；有内容 → 立即拉满到配置上限。
        // 网络/延时只作用于 quality（见 adjust_ratio）。唯一允许降帧的信号
        // = 浏览器解码背压（decode_fps 低且队列深 → 24；继续低 → 15 下限）。
        let mut fps = if elapsed_s < 0.1 {
            self.fps // 首样本：不因冷启动（帧还没出来/计时刚建）误判而改帧率
        } else if dynamic {
            let cap = highest_fps;
            let bp = ctx.decode_fps_hint > 0
                && ctx.decode_fps_hint < QOS_DECODE_BACK_PRESSURE_FPS
                && ctx.decode_queue_hint > QOS_DECODE_BACK_PRESSURE_QUEUE;
            if bp && ctx.decode_fps_hint < 12 {
                cap.min(QOS_DYNAMIC_MIN_FPS)
            } else if bp {
                cap.min(24)
            } else {
                cap
            }
        } else {
            QOS_MIN_FPS
        };
        // 下限 = min(状态下限, 上限)：防止 --desktop-fps 低于状态下限时
        // clamp(min>max) 越界 panic。
        let floor = (if dynamic { QOS_DYNAMIC_MIN_FPS } else { QOS_MIN_FPS }).min(highest_fps);
        fps = fps.clamp(floor, highest_fps);

        let first = self.delay_fps.is_none();
        self.delay_fps = Some(fps);
        // new_user_instant：首个样本起 1s 内 cap INIT_FPS（启动稳定，随后立即满帧）。
        match self.first_sample_us {
            None => {
                self.first_sample_us = Some(ctx.now_us);
            }
            Some(t) => {
                if ctx.now_us >= t && ctx.now_us - t < 1_000_000 && fps > QOS_INIT_FPS {
                    fps = QOS_INIT_FPS;
                }
            }
        }
        self.fps = fps;

        // ── ratio（rustdesk update_display_data：send_counter 每秒累计，
        //    每 3s 调 adjust_ratio；首次样本也调一次）──
        self.frame_count_s += frame_count;
        self.ratio_elapsed_s += elapsed_s.max(0.0) as u32;
        if self.ratio_elapsed_s >= QOS_ADJUST_RATIO_INTERVAL {
            self.ratio_elapsed_s = 0;
            // DYNAMIC_SCREEN：3s 内编码 ≥ ADJUST_RATIO_INTERVAL×阈值 帧
            // （=每秒 ≥2 帧）才视为动态屏。
            let dynamic =
                self.frame_count_s >= QOS_ADJUST_RATIO_INTERVAL * QOS_DYNAMIC_SCREEN_THRESHOLD;
            self.frame_count_s = 0;
            self.adjust_ratio(dynamic);
        } else if first {
            self.adjust_ratio(false);
        }

        // QoS 质量状态（A0 五态 / A2）：每次采样后更新可观测快照 + 迁移日志。
        self.update_quality_state();

        (self.fps, self.ratio_permille())
    }

    /// ratio 调整（对齐 rustdesk adjust_ratio 语义，判据改为基线相对）：
    /// 动态屏好网（over 小）才升；`avg − 基线`（拥塞增量）大则无条件降；
    /// +150kbps/3s 限幅防陡升；min 按质量档与 1Mbps 基线。
    fn adjust_ratio(&mut self, dynamic: bool) {
        // 用 9 窗口中值替代 2 窗口均值（R5#113 抗尖峰）：单次 RTT 尖峰
        // （TCP 重传/GC 停顿）不会瞬时把码率砍下去。
        let max_delay = self.median_delay();
        let target_ratio = self.quality_ratio;
        let current_ratio = self.ratio;
        let current_bitrate = self.bitrate_kbps as f32;

        // 高分屏 1Mbps 等效保底（rustdesk ratio_1mbps）。
        let ratio_1mbps = if current_bitrate > 0.0 {
            Some((current_ratio * 1000.0 / current_bitrate).max(QOS_BR_MIN_HIGH_RESOLUTION))
        } else {
            None
        };
        // 每 3s 码率增量上限 +150kbps（rustdesk ratio_add_150kbps）。
        let ratio_add_150kbps = if current_bitrate > 0.0 {
            Some((current_bitrate + 150.0) * current_ratio / current_bitrate)
        } else {
            None
        };

        let min = if target_ratio >= QOS_BR_BEST {
            let mut m = QOS_BR_BEST / 2.5;
            if let Some(r1) = ratio_1mbps {
                if m > r1 {
                    m = r1;
                }
            }
            m.max(QOS_BR_MIN)
        } else if target_ratio >= QOS_BR_BALANCED {
            let mut m = (QOS_BR_BALANCED / 2.0).min(0.4);
            if let Some(r1) = ratio_1mbps {
                if m > r1 {
                    m = r1;
                }
            }
            m.max(QOS_BR_MIN_HIGH_RESOLUTION)
        } else {
            QOS_BR_MIN_HIGH_RESOLUTION
        };
        let max = target_ratio * QOS_MAX_BR_MULTIPLE;

        let mut v = current_ratio;
        // 拥塞增量 = 当前 avg − 健康基线（等价 rustdesk 的 `delay−RTT`）：
        // **固定传播 RTT 不触发降码率**。上一版按绝对延时降档，公网 relay
        // 路径 200-400ms 固定 RTT 会把 ratio 一路压到下限（实测 321kbps、
        // 动态画面永久糊屏）——那才是与 rustdesk 表现走向两极端的一环。
        // 恢复路径（over 小）保留 rustdesk 的"动态屏才升"门。
        let over = max_delay.saturating_sub(self.baseline_delay.max(10));
        // TestDelay 网络层探针（R4 甲 A1 / 对齐 rustdesk cm::TestDelay）：
        // probe 是纯网络往返（不含编码/解码/渲染管线）。probe 健康而 e2e
        // 中值显著更高 → over 来自管线/解码积压而非网络拥塞——降码率只更糊
        // 无改善（动态画面保清晰铁律；解码积压已由 fps 背压降帧处理），
        // 跳过降档。probe 未上报（0）或 probe 同样高 → 维持原判据。
        let probe_med = self.probe_median_delay();
        let pipeline_bloated = probe_med >= 1 && probe_med + 100 <= max_delay;
        // RTT 分带（R4 丁142 / R5#111）：中值延时档位修正阈值——绝对 RTT 已
        // 很高的会话（band>=2），相对增量更小的拥塞也要降（弱网韧性）。
        // **不绕过 over 主判据**：恒定高 RTT（baseline 学成后 over≈0）是
        // 传播延迟而非拥塞，绝不该降码率（固定 RTT 降码率只更糊无改善）。
        // band 3（>800ms 预警）只是把降档步长加大，仍要求 over 证实拥塞。
        let band = self.rtt_band();
        let threshold = if band >= 2 { 60 } else { 100 }; // 低档带阈值收紧
        if pipeline_bloated {
            // 网络层健康、e2e 高来自管线积压：不降 ratio；动态屏保持升档。
            if dynamic {
                v = current_ratio * 1.05;
            }
        } else if over < threshold {
            if dynamic {
                v = current_ratio * 1.05;
            }
        } else if over < threshold + 50 {
            v = current_ratio * if band >= 3 { 0.9 } else { 0.95 };
        } else if over < threshold + 150 {
            v = current_ratio * 0.9;
        } else if over < threshold + 300 {
            v = current_ratio * 0.85;
        } else {
            v = current_ratio * 0.8;
        }

        if let Some(r150) = ratio_add_150kbps {
            if v > r150 && r150 > current_ratio && current_ratio >= QOS_BR_SPEED {
                v = r150;
            }
        }
        self.ratio = v.clamp(min, max);
    }

    /// 相对满档的千分比（=1000 表示满档，<1000 表示弱网已压码率）。
    pub fn ratio_permille(&self) -> u32 {
        if self.quality_ratio > 0.0 {
            (self.ratio / self.quality_ratio * 1000.0).round().clamp(100.0, 1000.0) as u32
        } else {
            1000
        }
    }

    pub fn current_ratio_permille(&self) -> u32 {
        self.ratio_permille()
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
    fn test_frame_pool_reuses_buffers() {
        // R5 #127 内存池第一块：acquire/release 循环复用——同尺寸 buffer
        // 借出 N 次只有首轮新分配，后续全部命中池（零堆分配）。
        let pool = FramePool::new(4);
        let b1 = pool.acquire(100);
        assert_eq!(pool.stats(), (0, 1), "首次借必需新分配");
        pool.release(b1);
        let b2 = pool.acquire(100);
        assert_eq!(pool.stats(), (1, 1), "第二次借应复用池中 buffer");
        pool.release(b2);
        // 归还后借出仍命中，不新增分配。
        let b3 = pool.acquire(100);
        assert_eq!(pool.stats(), (2, 1));
    }

    #[test]
    fn test_frame_pool_capacity_capped() {
        // 池上限 2：释放 3 个后只保留 2 个（防无限膨胀）。
        let pool = FramePool::new(2);
        for _ in 0..3 {
            pool.release(vec![0u8; 64]);
        }
        let mut nonempty = 0;
        for _ in 0..3 {
            let b = pool.acquire(64);
            if !b.is_empty() {
                nonempty += 1; // 复用保留 cap 的 buffer（已 clear 为空 Vec）
            }
            assert!(b.is_empty(), "acquire 返回的空 Vec（cap 保留）");
        }
        // 第三次借因池中只有 2 个 → 新分配 1 次。
        assert_eq!(pool.stats().1, 1, "第三个借出超出池容量 → 新分配");
    }

    #[test]
    fn test_frame_pool_grows_for_larger_size() {
        // 池中只有小 buffer 时借大尺寸 → 新分配（不硬塞）。
        let pool = FramePool::new(2);
        pool.release(vec![0u8; 10]);
        let big = pool.acquire(1000);
        assert!(big.capacity() >= 1000);
        let (reused, allocated) = pool.stats();
        assert_eq!((reused, allocated), (0, 1), "不匹配尺寸 → 新分配");
    }

    #[test]
    fn test_pooled_frame_allocates_from_pool() {
        // R5 #127 第二块：合成帧从预分配池借 buffer（复用命中、无新分配），
        // bgra 内容与尺寸正确。
        let pool = FramePool::new(4);
        pool.prealloc(1, 100);
        let f1 = pooled_frame(&pool, 5, 5, &[7u8; 100]);
        assert_eq!(f1.bgra, vec![7u8; 100]);
        assert_eq!(f1.width, 5);
        assert_eq!(f1.height, 5);
        let (reused, allocated) = pool.stats();
        assert_eq!(reused, 1, "预分配 buffer 命中复用");
        assert_eq!(allocated, 0, "未触发新分配");
    }

    #[test]
    fn test_active_at_threshold() {
        // R5#25 空闲回收可见性阈值：≤1500ms 间隔判定活跃，超过即静止。
        assert!(active_at(1_000_000, 1_000_800), "800ms 内应活跃");
        assert!(active_at(1_000_000, 1_001_499), "1499ms 边界仍活跃");
        assert!(!active_at(1_000_000, 1_001_500), "1500ms 边界起静止");
        assert!(!active_at(1_000_000, 1_005_000), "5s 静止");
        // 时钟异常（now < last）不应 panic，视为活跃（饱和为 0）。
        assert!(active_at(1_000_000, 999_000));
    }

    #[test]
    fn test_backpressure_counter_accumulates() {
        // R5#16 可观测闭环：relay 拥塞计数从 0 起、bump 递增、qos-ack 读值。
        let dm = DesktopManager::new(DesktopConfig::default());
        assert_eq!(dm.backpressure_count(), 0);
        dm.bump_backpressure();
        dm.bump_backpressure();
        assert_eq!(dm.backpressure_count(), 2);
    }

    #[test]
    fn test_resolved_display_precedence() {
        // 批次7 选屏：运行时覆盖（web 下拉）优先于启动配置 --desktop-display。
        assert_eq!(resolved_display(&Some(":1".into()), &None), Some(":1".into()));
        assert_eq!(resolved_display(&Some(":2".into()), &Some(":0".into())), Some(":2".into()));
        // 无运行时覆盖 → 回落启动配置。
        assert_eq!(resolved_display(&None, &Some(":0".into())), Some(":0".into()));
        // 均无 → None（平台默认屏）。
        assert_eq!(resolved_display(&None, &None), None);
    }

    #[test]
    fn test_select_display_updates_runtime_override() {
        // 未运行时 select_display 只更新覆盖值（不重建），后续 start 生效。
        let dm = DesktopManager::new(DesktopConfig::default());
        assert_eq!(dm.display(), None, "默认无运行时覆盖");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let post: PostFn = Arc::new(|_| {});
            dm.select_display(":1", post).await.unwrap();
            // 空字符串 → 恢复默认（None）。
            let post2: PostFn = Arc::new(|_| {});
            dm.select_display("", post2).await.unwrap();
            // 同值 no-op（不报错、不重建——running=false 时本就不重建）。
            let post3: PostFn = Arc::new(|_| {});
            dm.select_display("", post3).await.unwrap();
        });
        assert_eq!(dm.display(), None);
    }

    #[test]
    fn test_select_display_trim() {
        // 带空白/前后空格的名字会被 trim；纯空白等价空（恢复默认）。
        let dm = DesktopManager::new(DesktopConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let post: PostFn = Arc::new(|_| {});
            dm.select_display("  :3  ", post).await.unwrap();
        });
        assert_eq!(dm.display(), Some(":3".into()));
    }

    #[test]
    fn test_kf_interval_adapts_to_activity() {
        // 高熵（帧均 > 2KB）→ 活跃 1.5s
        assert_eq!(kf_interval_ms_for(10_000.0), KF_ACTIVE_MS);
        // 刚过阈值仍是活跃
        assert_eq!(kf_interval_ms_for(KF_ACTIVE_BYTES_FRAME + 0.1), KF_ACTIVE_MS);
        // 静止（帧均 ≤ 2KB）→ 长间隔（KF_QUIET_MS）
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

    fn qos_ctx(quality: f32, bitrate_kbps: u32, now_us: u64) -> QosSampleCtx {
        QosSampleCtx {
            quality_ratio: quality,
            highest_fps: 30,
            bitrate_kbps,
            decode_fps_hint: 0,
            decode_queue_hint: 0,
            now_us,
        }
    }

    fn qos_ctx_bp(quality: f32, bitrate_kbps: u32, now_us: u64, dfps: u32, dq: u32) -> QosSampleCtx {
        QosSampleCtx {
            quality_ratio: quality,
            highest_fps: 30,
            bitrate_kbps,
            decode_fps_hint: dfps,
            decode_queue_hint: dq,
            now_us,
        }
    }

    /// #113 中值滤波抗尖峰：9 窗口填满后再塞一个极端尖峰，median_delay
    /// 应保持在中值附近（不跟尖峰走），而 2 窗口均值会瞬间被拉高。
    #[test]
    fn test_qos_median_filters_spike() {
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        // 9 个正常样本 40ms。
        let mut now = 2_000_000u64;
        for _ in 0..9 {
            q.on_delay(40, 0, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
            now += 1_000_000;
        }
        let before = q.median_delay();
        assert_eq!(before, 40, "9x40ms 中值 = 40");
        // 一个 2000ms 尖峰。
        q.on_delay(2000, 0, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        let after = q.median_delay();
        assert!(
            after <= 40 || after == 40 || after < 1000,
            "median must resist a single 2000ms spike, got {after}"
        );
        assert!(after < 1000, "single spike must not move median to its value, got {after}");
    }

    /// #111 RTT 分带：<100 正常(0) / 100-300 微调(1) / 300-800 低档(2) /
    /// >800 预警(3)。中值窗口需填满才反映。
    #[test]
    fn test_qos_rtt_band_classification() {
        let cases: &[(u32, u8)] = &[
            (40, 0),
            (80, 0),
            (150, 1),
            (250, 1),
            (500, 2),
            (750, 2),
            (900, 3),
            (1200, 3),
        ];
        for &(delay, want) in cases {
            let mut q = QosAdaptive::new(QOS_BR_BALANCED);
            let mut now = 2_000_000u64;
            for _ in 0..9 {
                q.on_delay(delay, 0, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
                now += 1_000_000;
            }
            assert_eq!(q.rtt_band(), want, "rtt {delay}ms -> band {want}");
        }
    }

    #[test]
    fn test_qos_adaptive_gradual_increase_on_good_net() {
        // 好网动态屏：内容驱动 fps → 满档 30；ratio 动态屏回升/保持满档。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let mut now = 2_000_000u64; // 越过 new_user 1s 窗口
        let (fps1, _) = q.on_delay(40, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        now += 1_000_000;
        let (fps2, _) = q.on_delay(40, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        now += 1_000_000;
        let (fps3, _) = q.on_delay(40, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        assert!(fps3 >= fps1 && fps3 >= QOS_FPS, "good net must keep full fps");
        assert!(q.current_ratio_permille() >= 1000, "good net ratio >= 1000‰");
    }

    #[test]
    fn test_qos_constant_delay_never_touches_fps_or_ratio() {
        // 对齐 rustdesk `delay−RTT`：恒定延时不等于拥塞。800ms 固定传播延迟
        // 基线学成后：fps 满档（内容驱动，与网络无关）、码率满档。这正是
        // 此前 811ms→1fps/321kbps 自锁糊死的根因（固定 RTT 降 fps/降码率
        // 都毫无改善，只会更糊更卡）。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let mut now = 2_000_000u64;
        for _ in 0..8 {
            let (fps, _) = q.on_delay(800, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
            now += 1_000_000;
            assert_eq!(fps, QOS_FPS, "constant 800ms must keep full fps, got {fps}");
        }
        assert!(
            q.current_ratio_permille() >= 1000,
            "constant 800ms != congestion: ratio stays full, got {}‰",
            q.current_ratio_permille()
        );
    }

    #[test]
    fn test_qos_network_never_drops_dynamic_fps_congestion_only_cuts_quality() {
        // 用户铁律：动态画面永不因网络降帧。拥塞（净延时超基线）只降 ratio
        //（模糊），fps 保持满档。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let mut now = 2_000_000u64;
        for _ in 0..3 {
            q.on_delay(800, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
            now += 1_000_000;
        }
        let samples: Vec<u32> = (0..6)
            .map(|_| {
                let (f, _) = q.on_delay(1800, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
                now += 1_000_000;
                f
            })
            .collect();
        assert!(
            samples.iter().all(|f| *f == QOS_FPS),
            "congestion must NOT drop dynamic fps, got {samples:?}"
        );
        assert!(
            q.current_ratio_permille() < 1000,
            "congestion cuts bitrate ratio, got {}‰",
            q.current_ratio_permille()
        );
    }

    #[test]
    fn test_qos_dynamic_fps_never_network_driven_even_4s() {
        // 极端弱网（4s e2e）动态屏：fps 仍满档 30——网络不再驱动 fps
        //（radically 反转旧的"延时→降fps"逻辑，用户投诉的 1 帧状态从机制上
        // 出局）；慢网络的影响全部落在 ratio/质量上。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let (fps, _) = q.on_delay(4000, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, 700_000));
        assert_eq!(fps, QOS_FPS, "4s dynamic net must keep full fps, got {fps}");
        let mut sink = fps;
        let mut now = 1_700_000u64;
        for _ in 0..8 {
            let (nf, _) = q.on_delay(4000, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
            sink = nf;
            now += 1_000_000;
        }
        assert_eq!(sink, QOS_FPS, "sustained 4s net keeps full fps, got {sink}");
    }

    #[test]
    fn test_qos_dynamic_fps_decode_backpressure_steps_and_floor() {
        // 唯一允许降帧的信号 = 浏览器解码背压：decode_fps 低且队列深 →
        // 24 → 15（下限）；背压消失立即回 30。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let mut now = 2_000_000u64;
        let (fps1, _) = q.on_delay(40, 0, 30, 1.0, &qos_ctx_bp(QOS_BR_BALANCED, 600, now, 18, 20));
        assert_eq!(fps1, 24, "mild backpressure steps to 24, got {fps1}");
        now += 1_000_000;
        let (fps2, _) = q.on_delay(40, 0, 30, 1.0, &qos_ctx_bp(QOS_BR_BALANCED, 600, now, 9, 30));
        assert_eq!(fps2, QOS_DYNAMIC_MIN_FPS, "severe backpressure floors at 15, got {fps2}");
        now += 1_000_000;
        let (fps3, _) = q.on_delay(40, 0, 30, 1.0, &qos_ctx_bp(QOS_BR_BALANCED, 600, now, 30, 0));
        assert_eq!(fps3, QOS_FPS, "backpressure gone -> full fps back, got {fps3}");
    }

    #[test]
    fn test_qos_bad_net_drops_fps_regardless_of_static_screen() {
        // 静止屏（无字节帧）不受动态下限保护：4s e2e fps 直接压到 MIN_FPS=1
        // ——静止无内容，掉帧不可见，省带宽。网络对静止屏不影响行为。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let (fps, _) = q.on_delay(4000, 0, 0, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, 1_000_000));
        assert_eq!(fps, 1, "static screen at 1fps, got {fps}");
    }

    #[test]
    fn test_qos_adaptive_static_screen_no_ratio_increase() {
        // 动态屏判定（rustdesk DYNAMIC_SCREEN = 每秒≥2 帧编码）：3s 只有
        // 1 帧 → 非动态屏 → 好网也不升码率。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let _ = q.on_delay(40, 0, 1, 3.0, &qos_ctx(QOS_BR_BALANCED, 600, 3_000_000));
        assert_eq!(q.current_ratio_permille(), 1000, "static screen stays at 1000‰");
    }

    /// TestDelay 探针（R4 甲 A1 / 对齐 rustdesk cm::TestDelay）：probe 是纯
    /// 网络层往返（不含解码/渲染管线）。probe 健康而 e2e 高 → 拥塞证据不成立
    /// （管线/解码积压）→ 不降码率（动态画面保清晰铁律）；probe 也高（真
    /// 拥塞）或未上报（0）→ 维持原 e2e 判据降档。
    #[test]
    fn test_qos_probe_confirms_network_congestion() {
        // 健康期：baseline 学到 40ms。
        let mut now = 3_000_000u64;
        let mut warm = QosAdaptive::new(QOS_BR_BALANCED);
        for _ in 0..9 {
            now += 300_000;
            warm.on_delay(40, 20, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        // 场景 1：probe 恒 20ms（网络健康），e2e 跳到 800ms 并持续（管线积压）
        // → 即使 over 大也不降码率（pipeline_bloated 分支）。
        let mut q1 = warm;
        for _ in 0..24 {
            now += 300_000;
            q1.on_delay(800, 20, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        let p1 = q1.ratio_permille();
        assert!(p1 >= 900, "网络健康+管线积压不应降码率，实际 {p1}‰");
        // 场景 2：probe 与 e2e 同步高（真网络拥塞）→ 降码率。
        now = 3_000_000u64;
        let mut q2 = QosAdaptive::new(QOS_BR_BALANCED);
        for _ in 0..9 {
            now += 300_000;
            q2.on_delay(40, 40, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        for _ in 0..24 {
            now += 300_000;
            q2.on_delay(800, 800, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        let p2 = q2.ratio_permille();
        assert!(p2 < 900, "真拥塞应降码率，实际 {p2}‰");
        // 场景 3：probe 未上报（0，老浏览器/测试）→ 原 e2e 判据降档。
        now = 3_000_000u64;
        let mut q3 = QosAdaptive::new(QOS_BR_BALANCED);
        for _ in 0..9 {
            now += 300_000;
            q3.on_delay(40, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        for _ in 0..24 {
            now += 300_000;
            q3.on_delay(800, 0, 30, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        let p3 = q3.ratio_permille();
        assert!(p3 < 900, "无探针沿用原判据应降码率，实际 {p3}‰");
    }

    /// probe 中值抗尖峰：单次 probe 尖峰不移动网络层中值（降档证据稳定）。
    #[test]
    fn test_qos_probe_median_filters_spike() {
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let mut now = 3_000_000u64;
        // 5 个健康 probe 样本后塞一个 2000ms 尖峰：5 窗口中值应保持健康。
        for _ in 0..5 {
            now += 300_000;
            q.on_delay(40, 20, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        now += 300_000;
        q.on_delay(40, 2000, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        assert_eq!(q.probe_median_delay(), 20, "单次 probe 尖峰不应移动中值");
    }

    /// probe 未上报（0）：probe_median_delay() 返回 0 = 无探针。
    #[test]
    fn test_qos_probe_absent_returns_zero() {
        let q = QosAdaptive::new(QOS_BR_BALANCED);
        assert_eq!(q.probe_median_delay(), 0, "未上报时 probe 中值应为 0（兼容）");
    }

    /// R4 甲A0 五态 / A2 质量反馈状态机：网络层 probe 健康 → Good；probe
    /// 抬升 → Medium/Degraded/Critical；状态是纯观测快照，不影响 fps/ratio
    /// 决策（决策测试仍全绿）。
    #[test]
    fn test_qos_quality_state_transitions() {
        // 初始 Unknown（首样本前）。
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        assert_eq!(q.quality_state(), QosQualityState::Unknown);

        // 健康：probe 20ms 恒定 → Good。
        let mut now = 3_000_000u64;
        for _ in 0..9 {
            now += 300_000;
            q.on_delay(40, 20, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        assert_eq!(q.quality_state(), QosQualityState::Good, "网络健康应 Good");

        // probe 抬到 500ms（e2e 保持低 → over≈0，状态由网络层单独驱动）
        // → Degraded（300-800 带）。
        for _ in 0..6 {
            now += 300_000;
            q.on_delay(40, 500, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        assert_eq!(q.quality_state(), QosQualityState::Degraded, "probe 500 → Degraded");

        // probe 抬到 900ms → Critical（≥800 带）。
        for _ in 0..6 {
            now += 300_000;
            q.on_delay(40, 900, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        assert_eq!(q.quality_state(), QosQualityState::Critical, "probe 900 → Critical");

        // probe 回落 30ms → 恢复 Good（中值窗口排出尖峰后）。
        for _ in 0..8 {
            now += 300_000;
            q.on_delay(40, 30, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        assert_eq!(q.quality_state(), QosQualityState::Good, "网络恢复 → Good");
    }

    /// 无探针（probe=0）时状态由 e2e 拥塞增量 over 驱动（保持兼容）。
    #[test]
    fn test_qos_quality_state_without_probe() {
        let mut q = QosAdaptive::new(QOS_BR_BALANCED);
        let mut now = 3_000_000u64;
        // 健康期 e2e 40ms，baseline 学到 40。
        for _ in 0..9 {
            now += 300_000;
            q.on_delay(40, 0, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        // 持续高 e2e（无探针）→ over 大 → 至少 Medium。
        for _ in 0..12 {
            now += 300_000;
            q.on_delay(800, 0, 1, 1.0, &qos_ctx(QOS_BR_BALANCED, 600, now));
        }
        assert!(
            matches!(q.quality_state(), QosQualityState::Medium | QosQualityState::Degraded | QosQualityState::Critical),
            "无探针持续高 e2e → 状态劣化，实际 {:?}",
            q.quality_state()
        );
    }

    #[test]
    fn test_avg_frame_bytes() {
        let w: VecDeque<u32> = (0..48).map(|i| if i < 20 { 5000 } else { 2000 }).collect();
        let avg = avg_frame_bytes(&w);
        assert!((avg - 3250.0).abs() < 1.0);
        assert_eq!(avg_frame_bytes(&VecDeque::new()), 0.0);
    }

    /// #85 编码器故障热备 + #84 慢帧：统一降级决策纯函数。
    #[test]
    fn test_next_degrade_codec_trigger() {
        // 未达阈值：不降。
        assert_eq!(next_degrade_codec("av1", 9, 0, false), None, "slow <10 不降");
        assert_eq!(next_degrade_codec("av1", 0, 4, false), None, "err <5 不降");
        // #84 慢帧：连续 10 帧 >66ms → 降一档（av1→vp9）。
        assert_eq!(next_degrade_codec("av1", 10, 0, false).as_deref(), Some("vp9"));
        // #85 故障热备：连续 5 帧 encode Err → 降一档。
        assert_eq!(next_degrade_codec("av1", 0, 5, false).as_deref(), Some("vp9"));
        // 两路径都达：仍只降一档（复用 fallback 链）。
        assert_eq!(next_degrade_codec("vp9", 10, 5, false).as_deref(), Some("vp8"));
        // 已降级过：不再降（一次性防重建风暴）。
        assert_eq!(next_degrade_codec("vp9", 10, 5, true), None);
        // h264 末档：无更低 codec，不降（丢帧追新兜底）。
        assert_eq!(next_degrade_codec("h264", 10, 5, false), None);
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

    /// R5#32 剪贴板大文本截断：未超限原样；超限截断到最近 UTF-8 字符边界
    /// （中英文混排不切出非法字符/半个码点）。
    #[test]
    fn test_clipboard_truncate_boundary() {
        // 未超限：原样返回。
        assert_eq!(clipboard_truncate("hello", 100), "hello");
        // 英文超限：截断长度恰为 max。
        let long = "a".repeat(600_000);
        assert_eq!(clipboard_truncate(&long, 1000).len(), 1000);
        // 中文（UTF-8 每字 3 字节）+ floor_char_boundary：max 落在字符中间
        // 时前移到完整字符边界，绝不切半个字。
        let cjk: String = "中".repeat(1_000); // 3000 字节
        let t = clipboard_truncate(&cjk, 1_001);
        let n = t.len();
        assert!(n <= 1_001, "截断不得超过 max：{n}");
        assert!(n % 3 == 0, "不得切半个中文字符：{n}");
        // 混排尾部尽量贴近 max。
        let mixed = format!("{}abc{}", "中".repeat(333), "x".repeat(700));
        let tm = clipboard_truncate(&mixed, 1_400);
        assert!(std::str::from_utf8(tm.as_bytes()).is_ok(), "截断必须合法 UTF-8");
        assert!(tm.len() <= 1_400);
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
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicI64::new(unix_ms_now())),
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
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw)), "test".to_string(), 0, Arc::new(std::sync::atomic::AtomicU32::new(30)), Arc::new(std::sync::atomic::AtomicU32::new(1000)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicI64::new(unix_ms_now()))));
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
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw)), "test".to_string(), 0, Arc::new(std::sync::atomic::AtomicU32::new(15)), Arc::new(std::sync::atomic::AtomicU32::new(1000)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicI64::new(unix_ms_now()))));
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
        // R5#41 帧头字段显式化：frag 必须带消息级 seq（递增帧号）与 flags。
        let f0 = keys[0];
        assert!(
            f0["payload"]["seq"].is_u64() || f0["payload"]["seq"].is_number(),
            "frag must carry message-level seq, got {f0:?}"
        );
        assert_eq!(f0["payload"]["flags"], "key", "keyframe frag flags must be 'key'");
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
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw)), "test".to_string(), 0, Arc::new(std::sync::atomic::AtomicU32::new(30)), Arc::new(std::sync::atomic::AtomicU32::new(1000)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicU64::new(0)), Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(std::sync::atomic::AtomicI64::new(unix_ms_now()))));
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

