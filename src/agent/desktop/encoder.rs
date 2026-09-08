//! Codec-agnostic video encoder abstraction for the desktop pipeline.
//!
//! Both `openh264::H264Encoder` and `aom::AomEncoder` implement
//! [`VideoEncoder`] so `run_desktop_pipeline` can be written once. The
//! returned [`EncodedFrame`] carries codec-specific parameter sets (SPS/PPS
//! for H.264; profile/level for AV1 via [`VideoEncoder::mux_sample`]).

/// One encoded picture. `nalu` is Annex-B for H.264, a raw AV1 frame for AV1.
/// `sps`/`pps` (H.264) present on IDR frames.
#[derive(Debug)]
pub struct EncodedFrame {
    pub nalu: Vec<u8>,
    /// True when this frame is a random-access point (IDR / AV1 key frame).
    pub is_idr: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

/// The fMP4 sample-description fragment both encoders can produce.
pub type VisualSample = crate::agent::desktop::mp4::VisualSample;

/// Unified surface both software encoders implement. `Send` because the
/// encoder is driven from the single `DesktopManager` task.
pub trait VideoEncoder: Send {
    fn encode(&mut self, i420: &[u8]) -> Result<EncodedFrame, String>;
    fn force_idr(&mut self);
    fn set_bitrate(&mut self, bps: u64);
    fn bitrate_bps(&self) -> u64;
    fn set_frame_rate(&mut self, fps: f32);
    fn fps(&self) -> f64;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// `h264` | `av1`.
    fn codec(&self) -> &'static str;
    /// Produce the MP4 sample description (codec parameter set) used to build
    /// the init segment. H.264 needs SPS/PPS from an IDR frame; VP9 carries
    /// profile/level statically.
    fn mux_sample(&self, frame: &EncodedFrame) -> Option<VisualSample>;
    /// 运行时按质量档调整编码（rustdesk QoS 行为）：重算目标码率 + QP 区间。
    /// 默认 no-op（H.264 软编不支持动态质量）。
    fn set_quality(&mut self, _ratio: f32) {}
    /// 帧级码率守卫（MYS-886 BitrateGuard）：按档位收紧编码器量化上限，
    /// 压住 CBR 瞬时 overshoot（用户方案：动态调 QP 压码率，不降 fps）。
    /// `level` 0..=BitrateGuard::MAX_LEVEL；0 = 回到质量档默认 QP 上限。
    /// 默认 no-op（无对应运行时控件的编码器安全忽略）。
    fn set_overshoot_qp(&mut self, _level: u32) {}
}

/// rustdesk 同款质量档 → 码率倍率（`libs/scrap/src/common/codec.rs`）。
pub const QUALITY_SPEED: f32 = 0.5;
pub const QUALITY_BALANCED: f32 = 0.67;
pub const QUALITY_BEST: f32 = 1.5;

/// 编码器线程数（对齐 rustdesk `codec_thread_num(64)`）：
/// 按可用核数与当前负载自适应，clamp 到 libvpx/libaom 的 MAX_NUM_THREADS=64
/// 的常见档位（64/32/16/8/4/2/1）。比固定 4 线程更充分地利用多核——软编
/// 1080p 每帧编码耗时随线程数显著下降（低延迟关键，MYS-886）。
/// 读系统 1 分钟 load average（R5#82，对齐 rustdesk `codec_thread_num`
/// 的 `(核数 - loadavg) × 0.5`）。Linux 读 /proc/loadavg 第一字段；
/// 不可得时返回 None（调用方回退核数一半）。
pub fn loadavg_one() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/loadavg").ok()?;
        let first = s.split_whitespace().next()?;
        first.parse::<f64>().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = ();
        None
    }
}

/// 编码器线程数（对齐 rustdesk `codec_thread_num`）：按可用核数与系统
/// 负载自适应——`res = (核数 - loadavg) × 0.5`，负载高时编码线程自动减少
/// （不抢桌面/IO 的 CPU）；无 loadavg 时回退核数一半。clamp 到
/// 64/32/16/8/4/2/1 档（libvpx/libaom 的 MAX_NUM_THREADS 常见值）。
pub fn codec_thread_num() -> usize {
    let max = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let res = match loadavg_one() {
        // rustdesk: (核数 - 负载) × 0.5。负载 > 核数时结果为 0 → 取 1。
        Some(avg) => (((max as f64) - avg) * 0.5).round().max(1.0) as usize,
        None => (max as f64 * 0.5).round() as usize,
    };
    let mut res = res.min(max).max(1);
    res = match res {
        _ if res >= 64 => 64,
        _ if res >= 32 => 32,
        _ if res >= 16 => 16,
        _ if res >= 8 => 8,
        _ if res >= 4 => 4,
        _ if res >= 2 => 2,
        _ => 1,
    };
    res.min(64)
}

/// 按分辨率给基准码率（kbps 级，rustdesk `base_bitrate` 表）。
/// 1080p → 2073kbps；目标码率 = base_bitrate × quality 档。
pub fn base_bitrate(width: u32, height: u32) -> u64 {
    const PRESETS: &[(u32, u32, u64)] = &[
        (640, 480, 400),
        (800, 600, 500),
        (1024, 768, 800),
        (1280, 720, 1000),
        (1366, 768, 1100),
        (1440, 900, 1300),
        (1600, 900, 1500),
        (1920, 1080, 2073),
        (2048, 1080, 2200),
        (2560, 1440, 3000),
        (3440, 1440, 4000),
        (3840, 2160, 5000),
        (7680, 4320, 12000),
    ];
    let pixels = (width as u64) * (height as u64);
    let (preset_pixels, preset_bitrate) = PRESETS
        .iter()
        .map(|(w, h, b)| (*w as u64 * *h as u64, *b))
        .min_by_key(|(pp, _)| {
            if *pp >= pixels {
                *pp - pixels
            } else {
                pixels - *pp
            }
        })
        .unwrap_or(((1920 * 1080) as u64, 2073));
    (preset_bitrate as f64 * (pixels as f64 / preset_pixels as f64)).round() as u64
}

/// rustdesk AV1 专用 QP 区间（`libs/scrap/src/common/aom.rs calc_q_values`）：
/// q_min∈[24,5]、q_max∈[45,25]。与 VP9（q_min∈[36,0]/q_max∈[56,37]）不同档。
/// AV1 的 QP 标度更紧（libaom 量化 5-45 覆盖全质量谱），直接套 VP9 的
/// 区间会在低 quality 时过度量化（卡顿/糊）或高 quality 超预算。
///
/// MYS-954：VP8/VP9 已移除，VP9 式 `calc_q_values`（q_max∈[37,56]）随之
/// 删除，这里只剩 AV1 的映射。
///
/// MYS-954 码率失控修复：q_max 高端由 45 放开到 63。实时屏显在"大量移动
/// 画面"下 CBR 预算不足时，RC 需要把 QP 压得足够狠才能把瞬时码率拖回预算
/// （旧上限 38~45 在 720p/1080p 高动态下实测压不下来，码率冲到预算数倍）。
/// q_min 端点不动（低 QP = 清晰，正常内容下 RC 不会用到 q_max；q_max 只
/// 是钳制，普通场景无画质代价）。
pub fn calc_q_values_aom(ratio: f32) -> (u32, u32) {
    let b = (ratio * 100.0) as u32;
    let b = b.min(200);
    let (q_min1, q_min2) = (24u32, 5u32);
    let (q_max1, q_max2) = (63u32, 35u32);
    let t = b as f32 / 200.0;
    let q_min = (((1.0 - t) * q_min1 as f32 + t * q_min2 as f32).round() as u32).clamp(q_min2, q_min1);
    let q_max = (((1.0 - t) * q_max1 as f32 + t * q_max2 as f32).round() as u32).clamp(q_max2, q_max1);
    (q_min, q_max)
}

/// AV1 BitrateGuard 收紧 QP 映射（qindex 0..=63 标度）：把 overshoot 档位
/// `level`（1..=`max_level`）从质量档基准 `base_q_max` **线性插值到 AV1 量化
/// 上限 63**（= [`calc_q_values_aom`] 的 q_max 高端）。
///
/// 旧实现是固定 `base_q_max + 8*level` 封顶 55——但"码率失控修复"把
/// `calc_q_values_aom` 的 q_max 高端放开到 63 后，speed/balanced 的基准
/// q_max 已达 54~56，`+8` 每档都撞 55：6 档的状态机退化成**只有 1 个有效档**
/// （best 也只有 2 档），且释放时要从 level 6 空走回 0（每档 ≥10 帧 hold）≈2s
/// 一直钉在 55 才恢复画质（MYS-969 review）。线性插值保证**每档单调、用满
/// `[base_q_max, 63]` 区间**，重压缩能力对齐 RC 已放开的 63 上限。
pub fn overshoot_qp_aom(base_q_max: u32, level: u32, max_level: u32) -> u32 {
    const AV1_Q_CAP: u32 = 63;
    let base = base_q_max.min(AV1_Q_CAP);
    if level == 0 || max_level == 0 {
        return base;
    }
    let level = level.min(max_level);
    // 四舍五入的线性插值：base + round((63-base) * level / max_level)。
    base + ((AV1_Q_CAP - base) * level + max_level / 2) / max_level
}

/// 目标码率（bps）：rustdesk 模型 `base_bitrate(w,h) × quality`（base 单位
/// kbps，乘 1000 转 bps），用户 `--desktop-max-bitrate` 显式设值时作为
/// 硬顶（max_bps>0）；0 = 自动跟随 rustdesk 模型。
pub fn target_bitrate(width: u32, height: u32, max_bps: u64, quality: f32) -> u64 {
    let auto = (base_bitrate(width, height) as f64 * quality as f64 * 1000.0).round() as u64;
    if max_bps > 0 {
        auto.min(max_bps)
    } else {
        auto
    }
}

/// Construct an encoder for a codec name (`av1` / `vp9` / `h264`).
/// `max_bps` 语义：0 = 自动按 rustdesk 模型（base_bitrate × quality），
/// >0 = 用户硬顶。`quality` = 质量档倍率（speed/balanced/best）。
/// `monochrome`（MYS-954 灰度增强）：仅 AV1 生效（libaom 原生 monochrome
/// 码流）；其它 codec 忽略（走编码前 UV 置 128 的像素级灰度）。
pub fn new_encoder(
    codec: &str,
    w: u32,
    h: u32,
    max_bps: u64,
    fps: f64,
    quality: f32,
    monochrome: bool,
) -> Result<Box<dyn VideoEncoder>, String> {
    let target = target_bitrate(w, h, max_bps, quality);
    match codec.to_ascii_lowercase().as_str() {
        "h264" => crate::agent::desktop::openh264::H264Encoder::new(w, h, target, fps)
            .map(|e| Box::new(e) as Box<dyn VideoEncoder>),
        #[cfg(feature = "av1")]
        "av1" => {
            let (q_min_a, q_max_a) = calc_q_values_aom(quality);
            crate::agent::desktop::aom::AomEncoder::new(
                w, h, target, fps, q_min_a, q_max_a, monochrome,
            )
            .map(|e| Box::new(e) as Box<dyn VideoEncoder>)
        }
        other => Err(format!("unsupported desktop codec: {other}")),
    }
}

/// 自动降级创建编码器（rustdesk `set_fallback` 行为）：请求的 codec 初始化
/// 失败时按 `av1 → h264` 顺序回退（MYS-954：VP8/VP9 已移除），返回
/// (编码器, 实际生效 codec)。用于硬件不可用/静态链接缺失时保证桌面流仍能启动。
pub fn create_encoder_fallback(
    codec: &str,
    w: u32,
    h: u32,
    max_bps: u64,
    fps: f64,
    quality: f32,
    monochrome: bool,
) -> Result<(Box<dyn VideoEncoder>, String), String> {
    let codec_l = codec.to_ascii_lowercase();
    let chain: Vec<&str> = match codec_l.as_str() {
        "av1" => vec!["av1", "h264"],
        "h264" => vec!["h264"],
        other => vec![other, "h264"],
    };
    let mut last_err = String::new();
    for c in chain {
        match new_encoder(c, w, h, max_bps, fps, quality, monochrome) {
            Ok(e) => return Ok((e, c.to_string())),
            Err(e) => last_err = format!("{c}: {e}"),
        }
    }
    Err(format!("all encoders failed — {last_err}"))
}

/// 返回编码复杂度顺序中的下一档更低 codec（`av1 → h264`，MYS-954：
/// VP8/VP9 已移除）。用于编码耗时预算降级（R5#84）：当前 codec 软编跑不动
/// 时换更廉价的档。已是 h264（末档）返回 `None`。
pub fn next_lower_codec(codec: &str) -> Option<String> {
    let order = ["av1", "h264"];
    let idx = order.iter().position(|c| *c == codec)?;
    if idx + 1 < order.len() {
        Some(order[idx + 1].to_string())
    } else {
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// #82 loadavg 感知线程数：Linux 上应能读到 /proc/loadavg；
    /// 且线程数恒 ≥1、≤可用核数（不超配抢 CPU）。
    #[test]
    fn test_codec_thread_num_bounded() {
        let n = codec_thread_num();
        let max = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(4);
        assert!(n >= 1 && n <= max.min(64), "threads {n} out of [1,{max}]");
        // 档位合法（2 的幂序列）
        assert!(matches!(n, 1 | 2 | 4 | 8 | 16 | 32 | 64));
    }

    /// loadavg_one：Linux 上 /proc/loadavg 存在，应解析出正数（可为 0）。
    #[test]
    fn test_loadavg_one_parses_or_none() {
        match loadavg_one() {
            Some(v) => {
                assert!(v.is_finite() && v >= 0.0, "loadavg must be >= 0, got {v}");
            }
            None => {
                // 非 Linux 或 /proc 不可读：允许 None（调用方回退核数一半）。
            }
        }
    }

    #[test]
    fn test_base_bitrate_rustdesk_table() {
        // rustdesk 同款分辨率基准：1080p→2073k、720p→1000k。
        assert_eq!(base_bitrate(1920, 1080), 2073);
        assert_eq!(base_bitrate(1280, 720), 1000);
        assert_eq!(base_bitrate(640, 480), 400);
    }

    #[test]
    fn test_overshoot_qp_aom_monotonic_and_uses_full_range() {
        let max = 6u32;
        // 每个质量档基准（speed≈56 / balanced≈54 / best≈42）都应：level 0 = 基准；
        // 逐档严格单调递增；level=max 打满到 63（AV1 量化上限）。
        for base in [56u32, 54, 42] {
            assert_eq!(overshoot_qp_aom(base, 0, max), base, "level0 = 基准");
            let mut prev = overshoot_qp_aom(base, 0, max);
            for lvl in 1..=max {
                let qp = overshoot_qp_aom(base, lvl, max);
                assert!(qp > prev, "base={base} level{lvl}: {qp} 必须 > 前档 {prev}");
                assert!(qp <= 63, "base={base} level{lvl}: {qp} 不得超过 63");
                prev = qp;
            }
            assert_eq!(overshoot_qp_aom(base, max, max), 63, "满档打满到 63");
        }
        // 基准已 ≥63 的极端：clamp 到 63，不越界。
        assert_eq!(overshoot_qp_aom(63, 3, max), 63);
        assert_eq!(overshoot_qp_aom(70, 3, max), 63);
    }

    #[test]
    fn test_target_bitrate_quality_model() {
        // 1080p balanced(0.67) → ≈1389kbps；best(1.5) → ≈3110kbps。
        let b = target_bitrate(1920, 1080, 0, QUALITY_BALANCED);
        assert!((1_350_000..1_430_000).contains(&b), "1080p balanced = {b}");
        let best = target_bitrate(1920, 1080, 0, QUALITY_BEST);
        assert!((3_000_000..3_220_000).contains(&best), "1080p best = {best}");
        // 用户硬顶覆盖
        let capped = target_bitrate(1920, 1080, 800_000, QUALITY_BEST);
        assert_eq!(capped, 800_000);
    }

    #[test]
    fn test_next_lower_codec_chain() {
        // 编码复杂度降级链 av1→h264（MYS-954：VP8/VP9 已移除）；末档为 None（不再降）。
        assert_eq!(next_lower_codec("av1").as_deref(), Some("h264"));
        assert_eq!(next_lower_codec("h264"), None);
        // 未知 codec：不猜测降级（保持原样，防把未知串错降）。
        assert_eq!(next_lower_codec("wegotno"), None);
    }
}
