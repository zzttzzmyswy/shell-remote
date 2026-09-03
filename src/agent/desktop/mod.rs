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
pub mod mp4;
pub mod openh264;
pub mod rate;

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
    /// Encoder codec (only `h264` today).
    pub codec: String,
    /// Nominal encode frame rate.
    pub fps: f64,
    /// Adaptive bitrate bounds in bps (user request: 800 / 200 → kbps).
    pub min_bps: u64,
    pub max_bps: u64,
    /// Optional X11 display override (`--desktop-display`).
    pub display: Option<String>,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            capture: "auto".to_string(),
            codec: "h264".to_string(),
            fps: 15.0,
            // 静态桌面 ~80k 足够 (openh264 实测 84k 满帧); 动态由 ABR 拉回
            min_bps: 80_000,
            max_bps: 800_000,
            display: None,
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
}

impl DesktopManager {
    pub fn new(config: DesktopConfig) -> Self {
        let bps = config.max_bps;
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            task: tokio::sync::Mutex::new(None),
            bandwidth: Arc::new(std::sync::atomic::AtomicU64::new(bps)),
        }
    }

    /// Browser-reported available bandwidth (bps) feedback (weak networks).
    pub fn set_bandwidth_bps(&self, bps: u64) {
        use std::sync::atomic::Ordering as O;
        self.bandwidth.store(bps.max(1), O::Relaxed);
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
        if !self.config.supports_codec(&self.config.codec) {
            self.post_started_error(&post, &format!("unsupported codec {}", self.config.codec));
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let cfg = self.config.clone();
        let bandwidth = self.bandwidth.clone();
        let task = tokio::task::spawn(async move {
            run_desktop_loop(cfg, running, post, bandwidth).await;
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
            "codecs": if cfg.enabled() { vec!["h264"] } else { Vec::new() },
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
/// would disable the adaptive resolution entirely.
const SCALES: &[f64] = &[1.0, 0.75, 0.5, 0.375];

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

/// 静止心跳间隔（毫秒）。桌面静止时 OpenH264 输出空帧不上行，超过
/// relay 观看者空闲超时（30s）会被误判断流；确保每 1.5s 至少编一个
/// IDR 心跳帧，让流上始终有字节（静止桌面一个 IDR 仅几 KB）。
const HEARTBEAT_INTERVAL_MS: u64 = 1500;

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
) {
    let src = match capture::open_source(&cfg.capture, cfg.display.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("desktop capture open failed: {}", e);
            (post)(serde_json::json!({
                "type": "desktop:started",
                "payload": { "codec": cfg.codec, "error": format!("capture unavailable: {e}") }
            }));
            return;
        }
    };
    run_desktop_pipeline(cfg, running, post, src, bandwidth).await;
}

/// The capture → convert → encode → mux → post pipeline. Split from
/// [`run_desktop_loop`] so tests can inject a synthetic [`capture::FrameSource`].
async fn run_desktop_pipeline(
    cfg: DesktopConfig,
    running: Arc<AtomicBool>,
    post: PostFn,
    mut src: Box<dyn capture::FrameSource>,
    bandwidth: Arc<std::sync::atomic::AtomicU64>,
) {
    let (w0, h0) = src.resolution();
    if w0 < 2 || h0 < 2 || w0 % 2 != 0 || h0 % 2 != 0 {
        (post)(serde_json::json!({
            "type": "desktop:started",
            "payload": { "codec": cfg.codec, "error": format!("resolution {w0}x{h0} must be even") }
        }));
        return;
    }

    let mut enc = match openh264::H264Encoder::new(w0 as u32, h0 as u32, cfg.max_bps, cfg.fps) {
        Ok(e) => e,
        Err(e) => {
            (post)(serde_json::json!({
                "type": "desktop:started",
                "payload": { "codec": cfg.codec, "error": format!("encoder init failed: {e}") }
            }));
            return;
        }
    };
    let mut abr = rate::Abr::new(cfg.min_bps, cfg.max_bps);

    // fMP4 config is final once the first IDR carries SPS/PPS at the *current*
    // encode resolution; a resolution change rebuilds both.
    let mut mp4_cfg: Option<mp4::Mp4Config> = None;
    let mut seq: u32 = 1;
    let mut pts_ms: u64 = 0;
    let frame_ms = (1000.0 / cfg.fps).round() as u64;

    // Adaptive encode resolution.
    let mut scale_idx: usize = 0;
    let mut enc_w: usize = w0;
    let mut enc_h: usize = h0;
    let mut skip_win: VecDeque<bool> = VecDeque::new();
    const OBS: usize = 48;
    let mut since_change: u32 = 0;

    // 帧抽取: 高熵内容下先把有效帧率阶梯式降低(15→7→5→3fps), 让平均码率
    // 在不动分辨率的前提下回到 800k 上限以内(模糊可接受、丢帧我们不 POST)。
    // stride 到顶仍跳帧时才降分辨率(maybe_rescale)。
    let mut stride_idx: usize = 0;
    let mut cap_no: u64 = 0;

    tracing::info!(
        width = %w0, height = %h0, fps = %cfg.fps,
        "desktop capture started"
    );
    (post)(serde_json::json!({
        "type": "desktop:started",
        "payload": {
            "codec": cfg.codec, "width": w0, "height": h0, "fps": cfg.fps,
            "min_kbps": cfg.min_bps / 1000, "max_kbps": cfg.max_bps / 1000,
        }
    }));

    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / cfg.fps));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = std::time::Instant::now();
    let mut frame_idx: u64 = 0;
    let mut err_count: u32 = 0;
    // 上次实际 POST 视频帧的 pts。静止桌面空帧不上行, 用它判断是否该发
    // 心跳 IDR（保证 relay 观看者流上始终有字节, 不被空闲超时误杀）。
    let mut last_posted_ms: u64 = 0;

    while running.load(Ordering::SeqCst) {
        tick.tick().await;
        if !running.load(Ordering::SeqCst) {
            break;
        }
        // 每 2 秒一个 IDR, 以真实采集帧计算（与 stride 无关）。
        cap_no += 1;
        if cap_no % (cfg.fps as u64 * 2) == 0 {
            enc.force_idr();
        }        let stride = STRIDES[stride_idx];
        if cap_no % stride as u64 != 0 {
            // 帧抽取: 本 tick 不编码, 时间线继续推进。
            pts_ms += frame_ms;
            // 跳帧期间也可能长期无上行（静止桌面 + 高 stride）: 到心跳
            // 间隔则不跳本帧, 强制编一个 IDR。
            if last_posted_ms + HEARTBEAT_INTERVAL_MS > pts_ms {
                continue;
            }
            enc.force_idr();
        }
        let fr = match src.next_frame() {
            Ok(f) => {
                err_count = 0;
                f
            }
            Err(e) => {
                // 持续性捕获失败（例如 XWayland 下 root GetImage 抛 BadMatch、
                // Windows 屏幕 DC 失效且重建失败）。无限重试只会刷屏且永远
                // 黑屏。连续失败 MAX_CAPTURE_ERRORS 帧后终止, 并把原因回传
                // 给浏览器展示。
                err_count += 1;
                if err_count >= MAX_CAPTURE_ERRORS {
                    tracing::error!("desktop capture failed {err_count} frames in a row — giving up: {e}");
                    (post)(serde_json::json!({
                        "type": "desktop:error",
                        "payload": { "error": format!("capture failed: {e}") }
                    }));
                    break;
                }
                tracing::warn!("capture frame error: {} — retrying ({}/{MAX_CAPTURE_ERRORS})", e, err_count);
                continue;
            }
        };
        let w = fr.width;
        let h = fr.height;
        let i420 = if w == enc_w && h == enc_h {
            color::bgra_to_i420(&fr.bgra, w, h, w * 4)
        } else {
            color::bgra_to_i420_scaled(&fr.bgra, w, h, w * 4, enc_w, enc_h)
        };
        let encoded = match enc.encode(&i420) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("encode error: {}", e);
                continue;
            }
        };
        frame_idx += 1;

        // 防御：RC 仍可能输出空帧（极少数情况）——空帧不 POST，记录为跳帧。
        if encoded.nalu.is_empty() {
            skip_win.push_back(true);
            while skip_win.len() > OBS {
                skip_win.pop_front();
            }
            since_change += 1;
            pts_ms += frame_ms;
            if frame_idx % 10 == 0 {
                let ratio = skipped_ratio(&skip_win);
                tune_once(
                    &cfg, &mut enc, &mut stride_idx, &mut scale_idx,
                    &mut enc_w, &mut enc_h, w0, h0, &mut mp4_cfg, &mut seq,
                    &mut skip_win, &mut since_change, &mut frame_idx, ratio,
                );
            }
            continue;
        }
        skip_win.push_back(false);
        while skip_win.len() > OBS {
            skip_win.pop_front();
        }
        since_change += 1;

        if encoded.is_idr && mp4_cfg.is_none() {
            // 首个 IDR（或分辨率重配后）携带 SPS/PPS，构建 mux config 并下发 init。
            if let (Some(sps), Some(pps)) = (&encoded.sps, &encoded.pps) {
                let c = mp4::Mp4Config {
                    width: enc_w as u32,
                    height: enc_h as u32,
                    fps: cfg.fps,
                    sps: sps.clone(),
                    pps: pps.clone(),
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
        let sample = mp4::annexb_to_avcc(&encoded.nalu);
        let frag = mp4::mp4_fragment(cfg_mp4, &sample, pts_ms, encoded.is_idr, seq);
        seq += 1;
        last_posted_ms = pts_ms;
        pts_ms += frame_ms;
        post(serde_json::json!({
            "type": "desktop:video",
            "payload": { "kind": "frag", "key": encoded.is_idr, "data": base64(&frag) }
        }));

        // Adaptive bitrate + 降级决策: 每 10 个编码帧评估一次。
        if frame_idx % 10 == 0 {
            let now = start.elapsed().as_secs_f64();
            let ratio = skipped_ratio(&skip_win);
            // 弱网：以浏览器实测带宽为动态天花板(clamp 到 [min, 配置峰值])。
            {
                use std::sync::atomic::Ordering as O;
                let eff_max = bandwidth.load(O::Relaxed).min(cfg.max_bps).max(cfg.min_bps);
                abr.set_ceiling(eff_max);
                if ratio >= 0.3 {
                    abr.set_target(eff_max);
                    let cur = enc.bitrate_bps();
                    if cur != eff_max {
                        enc.set_bitrate(eff_max);
                    }
                } else {
                    let target = abr.note_frame(now, encoded.nalu.len());
                    enc.set_bitrate(target);
                }
            }
            tune_once(
                &cfg, &mut enc, &mut stride_idx, &mut scale_idx,
                &mut enc_w, &mut enc_h, w0, h0, &mut mp4_cfg, &mut seq,
                &mut skip_win, &mut since_change, &mut frame_idx, ratio,
            );
        }
    }

    tracing::info!("desktop capture stopped");
    (post)(serde_json::json!({ "type": "desktop:stopped", "payload": {} }));
}

fn skipped_ratio(skip_win: &VecDeque<bool>) -> f64 {
    if skip_win.is_empty() {
        return 0.0;
    }
    skip_win.iter().filter(|&&s| s).count() as f64 / skip_win.len() as f64
}

/// One step of the high-entropy degradation ladder (called every ~10 encoded
/// frames, cooldown 30 frames ≈ 2 s):
/// - skip ratio ≥ 25%: first drop the *effective* frame rate (stride up,
///   15→7→5→3 fps; we skip frames ourselves so the average bitrate stays
///   under the ceiling). Once at the lowest frame rate and still skipping,
///   degrade the encode resolution ([`maybe_rescale`]).
/// - skip ratio ≤ 3% and stable: restore resolution first, then step the
///   frame rate back up.
/// Frame-rate changes only re-model the encoder input rate — no rebuild, no
/// re-init, so viewers keep their stream uninterrupted.
#[allow(clippy::too_many_arguments)]
fn tune_once(
    cfg: &DesktopConfig,
    enc: &mut openh264::H264Encoder,
    stride_idx: &mut usize,
    scale_idx: &mut usize,
    enc_w: &mut usize,
    enc_h: &mut usize,
    w0: usize,
    h0: usize,
    mp4_cfg: &mut Option<mp4::Mp4Config>,
    seq: &mut u32,
    skip_win: &mut VecDeque<bool>,
    since_change: &mut u32,
    frame_idx: &mut u64,
    ratio: f64,
) {
    if *since_change < 30 {
        return;
    }
    if ratio <= 0.03 {
        if *scale_idx > 0 {
            maybe_rescale(
                cfg, enc, scale_idx, enc_w, enc_h, w0, h0, mp4_cfg, seq,
                skip_win, since_change, frame_idx, ratio, *stride_idx,
            );
        } else if *stride_idx > 0 {
            *stride_idx -= 1;
            enc.set_frame_rate(cfg.fps as f32 / STRIDES[*stride_idx] as f32);
            skip_win.clear();
            *since_change = 0;
            tracing::info!(
                "desktop: restore encode fps to {:.0} (stride={})",
                cfg.fps as f64 / STRIDES[*stride_idx] as f64,
                STRIDES[*stride_idx]
            );
        }
        return;
    }
    if ratio >= 0.25 {
        if *stride_idx < STRIDES.len() - 1 {
            *stride_idx += 1;
            enc.set_frame_rate(cfg.fps as f32 / STRIDES[*stride_idx] as f32);
            skip_win.clear();
            *since_change = 0;
            tracing::warn!(
                "desktop: reduce encode fps to {:.0} (stride={}) skip={:.0}%",
                cfg.fps as f64 / STRIDES[*stride_idx] as f64,
                STRIDES[*stride_idx],
                ratio * 100.0
            );
        } else {
            maybe_rescale(
                cfg, enc, scale_idx, enc_w, enc_h, w0, h0, mp4_cfg, seq,
                skip_win, since_change, frame_idx, ratio, *stride_idx,
            );
        }
    }
}

/// Rebuild the encoder at a smaller/larger resolution when the skip ratio is
/// persistently bad/good. Cooldown of 30 frames (2 s) between changes so the
/// window can refill before the next decision; degrading needs 40% skipped,
/// restoring needs ≤ 5% skipped.
#[allow(clippy::too_many_arguments)]
fn maybe_rescale(
    cfg: &DesktopConfig,
    enc: &mut openh264::H264Encoder,
    scale_idx: &mut usize,
    enc_w: &mut usize,
    enc_h: &mut usize,
    w0: usize,
    h0: usize,
    mp4_cfg: &mut Option<mp4::Mp4Config>,
    seq: &mut u32,
    skip_win: &mut VecDeque<bool>,
    since_change: &mut u32,
    frame_idx: &mut u64,
    ratio: f64,
    stride_idx: usize,
) {
    if *since_change < 30 {
        return;
    }
    let degrading = ratio >= 0.40 && *scale_idx < SCALES.len() - 1;
    let restoring = ratio <= 0.05 && *scale_idx > 0;
    if !degrading && !restoring {
        return;
    }
    let next = if degrading { *scale_idx + 1 } else { *scale_idx - 1 };
    let nw = (((w0 as f64) * SCALES[next]) as usize) & !1;
    let nh = (((h0 as f64) * SCALES[next]) as usize) & !1;
    if nw < 2 || nh < 2 {
        return;
    }
    match openh264::H264Encoder::new(nw as u32, nh as u32, cfg.max_bps, cfg.fps) {
        Ok(mut new_enc) => {
            tracing::warn!(
                "desktop: {} encode resolution {enc_w}x{enc_h} -> {nw}x{nh} (skip={:.0}%)",
                if degrading { "degrade" } else { "restore" },
                ratio * 100.0
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
            skip_win.clear();
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
    fn test_skipped_ratio_empty_and_full() {
        let mut w = VecDeque::new();
        assert_eq!(skipped_ratio(&w), 0.0);
        for _ in 0..20 {
            w.push_back(false);
        }
        for _ in 0..20 {
            w.push_back(true);
        }
        assert!((skipped_ratio(&w) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_maybe_rescale_degrades_on_high_skip() {
        let cfg = DesktopConfig::default();
        let mut enc = openh264::H264Encoder::new(1920, 1080, cfg.max_bps, cfg.fps).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut scale_idx = 0usize;
        let (mut enc_w, mut enc_h) = (w0, h0);
        let mut mp4_cfg = Some(mp4::Mp4Config {
            width: 1920,
            height: 1080,
            fps: cfg.fps,
            sps: vec![1],
            pps: vec![1],
        });
        let mut seq = 5u32;
        let mut skip_win: VecDeque<bool> = (0..48).map(|i| i < 20).collect(); // ~42% skipped
        let mut since_change = 30u32;
        let mut frame_idx = 40u64;
        maybe_rescale(
            &cfg, &mut enc, &mut scale_idx, &mut enc_w, &mut enc_h, w0, h0,
            &mut mp4_cfg, &mut seq, &mut skip_win, &mut since_change, &mut frame_idx, 0.42, 0,
        );
        assert_eq!(scale_idx, 1, "must degrade one step");
        assert_eq!(enc_w, 1440, "0.75 of 1920");
        assert_eq!(enc_h, 810, "0.75 of 1080");
        assert!(mp4_cfg.is_none(), "mux config must reset for re-init");
        assert_eq!(seq, 1, "fragment sequence must reset");
        assert!(skip_win.is_empty(), "skip window must reset");
        assert_eq!(since_change, 0, "cooldown must reset");
        assert_eq!(frame_idx, 0, "ABR/IDR anchor must reset");
    }

    #[test]
    fn test_maybe_rescale_restores_on_clean_content() {
        let cfg = DesktopConfig::default();
        let mut enc = openh264::H264Encoder::new(1440, 810, cfg.max_bps, cfg.fps).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut scale_idx = 1usize;
        let (mut enc_w, mut enc_h) = (1440usize, 810usize);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 3u32;
        let mut skip_win: VecDeque<bool> = (0..48).map(|_| false).collect();
        let mut since_change = 30u32;
        let mut frame_idx = 7u64;
        maybe_rescale(
            &cfg, &mut enc, &mut scale_idx, &mut enc_w, &mut enc_h, w0, h0,
            &mut mp4_cfg, &mut seq, &mut skip_win, &mut since_change, &mut frame_idx, 0.02, 0,
        );
        assert_eq!(scale_idx, 0, "quiet content must restore to full resolution");
        assert_eq!(enc_w, 1920);
        assert_eq!(enc_h, 1080);
    }

    #[test]
    fn test_tune_once_prefers_stride_before_resolution() {
        let cfg = DesktopConfig::default();
        let mut enc = openh264::H264Encoder::new(1920, 1080, cfg.max_bps, cfg.fps).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut stride_idx = 0usize;
        let mut scale_idx = 0usize;
        let (mut enc_w, mut enc_h) = (w0, h0);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 1u32;
        let mut skip_win: VecDeque<bool> = (0..48).map(|i| i < 13).collect(); // ~27%
        let mut since_change = 30u32;
        let mut frame_idx = 1u64;
        tune_once(
            &cfg, &mut enc, &mut stride_idx, &mut scale_idx, &mut enc_w, &mut enc_h,
            w0, h0, &mut mp4_cfg, &mut seq, &mut skip_win, &mut since_change,
            &mut frame_idx, 0.27,
        );
        assert_eq!(stride_idx, 1, "high skip must first lower fps (stride up)");
        assert_eq!(scale_idx, 0, "resolution untouched while stride has room");
        assert!((enc.fps() - cfg.fps / 2.0).abs() < 1e-9, "encoder fps = {:.1}", enc.fps());
        assert_eq!(since_change, 0, "cooldown must reset");

        // stride 已到顶(最大)仍高跳帧 → 降分辨率
        let mut skip_win2: VecDeque<bool> = (0..48).map(|_| true).collect();
        let mut since_change2 = 30u32;
        let mut enc2 = openh264::H264Encoder::new(1920, 1080, cfg.max_bps, cfg.fps).unwrap();
        let mut stride_idx = STRIDES.len() - 1;
        let mut scale_idx = 0usize;
        tune_once(
            &cfg, &mut enc2, &mut stride_idx, &mut scale_idx, &mut enc_w, &mut enc_h,
            w0, h0, &mut mp4_cfg, &mut seq, &mut skip_win2, &mut since_change2,
            &mut frame_idx, 0.9,
        );
        assert_eq!(stride_idx, STRIDES.len() - 1, "stride stays at max");
        assert_eq!(scale_idx, 1, "resolution must then degrade");
    }

    #[test]
    fn test_tune_once_restores_slowly() {
        let cfg = DesktopConfig::default();
        let mut enc = openh264::H264Encoder::new(1440, 810, cfg.max_bps, cfg.fps).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut stride_idx = 2usize;
        let mut scale_idx = 1usize; // 分辨率与帧率都降过
        let (mut enc_w, mut enc_h) = (1440usize, 810usize);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 1u32;
        let mut skip_win: VecDeque<bool> = (0..48).map(|_| false).collect();
        let mut since_change = 30u32;
        let mut frame_idx = 1u64;
        // 稳定低跳帧: 先恢复分辨率, stride 保持
        tune_once(
            &cfg, &mut enc, &mut stride_idx, &mut scale_idx, &mut enc_w, &mut enc_h,
            w0, h0, &mut mp4_cfg, &mut seq, &mut skip_win, &mut since_change,
            &mut frame_idx, 0.0,
        );
        assert_eq!(scale_idx, 0, "resolution restores first");
        assert_eq!(stride_idx, 2, "stride unchanged until resolution is back");
        // 分辨率已恢复, 再恢复 stride
        let mut since_change2 = 30u32;
        tune_once(
            &cfg, &mut enc, &mut stride_idx, &mut scale_idx, &mut enc_w, &mut enc_h,
            w0, h0, &mut mp4_cfg, &mut seq, &mut skip_win, &mut since_change2,
            &mut frame_idx, 0.0,
        );
        assert_eq!(stride_idx, 1, "stride restores after resolution");
        assert!((enc.fps() - cfg.fps / 2.0).abs() < 1e-9, "encoder fps = {:.1}", enc.fps());
    }

    #[test]
    fn test_maybe_rescale_respects_cooldown_and_bounds() {
        let cfg = DesktopConfig::default();
        let mut enc = openh264::H264Encoder::new(1920, 1080, cfg.max_bps, cfg.fps).unwrap();
        let (w0, h0) = (1920usize, 1080usize);
        let mut scale_idx = 0usize;
        let (mut enc_w, mut enc_h) = (w0, h0);
        let mut mp4_cfg: Option<mp4::Mp4Config> = None;
        let mut seq = 1u32;
        let mut skip_win: VecDeque<bool> = (0..48).map(|_| true).collect();
        let mut since_change = 5u32; // 冷却期内不允许变化
        let mut frame_idx = 3u64;
        maybe_rescale(
            &cfg, &mut enc, &mut scale_idx, &mut enc_w, &mut enc_h, w0, h0,
            &mut mp4_cfg, &mut seq, &mut skip_win, &mut since_change, &mut frame_idx, 1.0, 0,
        );
        assert_eq!(scale_idx, 0, "cooldown must block change");
        // 已经是最小档: 极端跳帧也不能再降（scale_idx=3 → 停在 3）
        let mut enc2 = openh264::H264Encoder::new(480, 270, cfg.max_bps, cfg.fps).unwrap();
        let mut scale_idx = 4 - 1;
        let (mut ew, mut eh) = (480usize, 270usize);
        let mut skip_win2: VecDeque<bool> = (0..48).map(|_| true).collect();
        let mut since_change2 = 30u32;
        let mut frame_idx2 = 0u64;
        maybe_rescale(
            &cfg, &mut enc2, &mut scale_idx, &mut ew, &mut eh, w0, h0,
            &mut mp4_cfg, &mut seq, &mut skip_win2, &mut since_change2, &mut frame_idx2, 0.95, 0,
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
    }

    #[test]
    fn test_supports_codec_h264_only() {
        let cfg = DesktopConfig::default();
        assert!(cfg.supports_codec("h264"));
        assert!(!cfg.supports_codec("vp9"));
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
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw))));
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
            let handle = tokio::spawn(run_desktop_pipeline(cfg, r2, post, src, Arc::new(std::sync::atomic::AtomicU64::new(bw))));
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