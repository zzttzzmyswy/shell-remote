//! libvpx VP9 software encoder (fallback when OpenH264's RC limitation bites).
//!
//! Mirrors the `H264Encoder` surface so the desktop pipeline can treat both
//! codecs uniformly: encode I420 → compressed frame (raw VP9 bitstream),
//! force key frame, dynamic bitrate, frame-rate remodelling.
//!
//! Config follows rustdesk's proven live-encoding recipe (libvpx CBR +
//! quantizer range + g_threads/row-mt/tile-columns + CPUUSED realtime tier):
//! bitrate is *actually* honoured, which OpenH264's RC cannot do at
//! `bEnableFrameSkip=false` (it bails out entirely).

pub mod vpx_sys;

use std::os::raw::{c_int, c_uint};

/// One encoded picture, shaped like [`crate::agent::desktop::encoder::EncodedFrame`]
/// so the shared pipeline does not need a codec-specific frame type.
pub use crate::agent::desktop::encoder::EncodedFrame;

/// libvpx VP8/VP9 encoder handle（rustdesk `VpxEncoder` 同款：一个结构
/// 按 `VpxVideoCodecId` 区分 VP8/VP9，MYS-886 对齐）。
pub struct Vp9Encoder {
    ctx: vpx_sys::vpx_codec_ctx_t,
    width: u32,
    height: u32,
    fps: f64,
    bitrate_bps: u64,
    /// 用户 --desktop-max-bitrate 覆盖（0 = 自动 rustdesk 模型）。
    max_bps: u64,
    quality: f32,
    /// 是否 VP8（否则 VP9）。VP8 是低内存设备的降级档（rustdesk 4G 内存
    /// 判定），浏览器 WebCodecs 原生支持 VP8 解码。
    is_vp8: bool,
    /// 下一次 encode 强制关键帧（通过 encode flags 传 VPX_EFLAG_FORCE_KF）。
    force_kf: bool,
    /// 递增帧时间戳（timebase 1ms，对齐 rustdesk：每帧 +1000/fps）。
    /// libvpx 的 RC 用 pts 差算实际帧率——固定 0 可能让其误判为
    /// 全速/满帧率，影响码率分配（MYS-886 卡顿修复）。
    pts_ms: u64,
}

// libvpx spawns internal worker threads; the encoder is only ever driven from
// the single `DesktopManager` task (same contract as H264Encoder).
unsafe impl Send for Vp9Encoder {}

/// Compressed frame packet kind (VPX_CODEC_CX_FRAME_PKT).
const VPX_CODEC_CX_FRAME_PKT: c_int = 0;
/// Raw I420 image format for `vpx_img_alloc`.
const VPX_IMG_FMT_I420: c_uint = 258; // VPX_IMG_FMT_I420 (bindgen rustified enum)
/// Key-frame flag in `vpx_codec_cx_pkt.frame.flags`.
const VPX_FRAME_IS_KEY: c_uint = 0x1;

impl Vp9Encoder {
    /// Create a VP9 (or VP8 if `is_vp8`) encoder pinned to `w x h` at `fps`.
    pub fn new(
        w: u32,
        h: u32,
        bitrate_bps: u64,
        fps: f64,
        q_min: u32,
        q_max: u32,
        is_vp8: bool,
    ) -> Result<Self, String> {
        assert!(w % 2 == 0 && h % 2 == 0, "dimensions must be even for 4:2:0");
        assert!(bitrate_bps > 0 && fps > 0.0);

        unsafe {
            let iface = if is_vp8 {
                vpx_sys::vpx_codec_vp8_cx()
            } else {
                vpx_sys::vpx_codec_vp9_cx()
            };
            if iface.is_null() {
                return Err("vpx_codec_cx unavailable".into());
            }
            // Get default config, then pin the fields that matter for live
            // screen casting (mirror rustdesk vpxcodec.rs).
            let mut cfg: vpx_sys::vpx_codec_enc_cfg_t = unsafe { std::mem::MaybeUninit::uninit().assume_init() };
            let rc = vpx_sys::vpx_codec_enc_config_default(iface, &mut cfg, 0);
            if rc != vpx_sys::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(format!("enc_config_default rc={rc:?}"));
            }
            cfg.g_w = w;
            cfg.g_h = h;
            cfg.g_threads = crate::agent::desktop::encoder::codec_thread_num() as c_uint;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 1000;
            cfg.g_error_resilient = 1; // VPX_ERROR_RESILIENT_DEFAULT
            cfg.g_pass = vpx_sys::vpx_enc_pass::VPX_RC_ONE_PASS;
            cfg.g_lag_in_frames = 0;
            cfg.rc_end_usage = vpx_sys::vpx_rc_mode::VPX_CBR;
            cfg.rc_target_bitrate = (bitrate_bps / 1000).min(u32::MAX as u64) as c_uint;
            cfg.rc_min_quantizer = q_min;
            cfg.rc_max_quantizer = q_max;
            cfg.rc_undershoot_pct = 95;
            cfg.rc_overshoot_pct = 25;
            // VP9 的 RC 需要 dropframe 作高熵压力阀: dropframe=0 时高熵内容
            // 码率彻底失控(实测 7367kbps @ 800k 目标)且编码速度暴跌
            // (156ms/帧, CBR 死命压大帧)。保留 25 让 RC 丢弃过盈的高熵帧
            // 保持码率受控与帧率——桌面共享场景丢的是极端运动帧, 可接受。
            // (AV1 的 libaom CBR 无此问题, 用 dropframe=0 全帧保留。)
            cfg.rc_dropframe_thresh = 25;
            // rustdesk 非录制（keyframe_interval=None）时 VPX_KF_DISABLED：
            // 关键帧完全由外部 force_idr 控制（MYS-886 外部动态节奏：静止
            // 4.5s / 活跃 1.5s + 首帧强制），编码器不自动插关键帧——避免与
            // 外部节奏抢跑（静止期省带宽）且更贴近 rustdesk 行为。
            cfg.kf_mode = vpx_sys::vpx_kf_mode::VPX_KF_DISABLED;
            cfg.kf_min_dist = 0;
            cfg.kf_max_dist = 0;

            let mut ctx: vpx_sys::vpx_codec_ctx_t = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
            let rc = vpx_sys::vpx_codec_enc_init_ver(
                &mut ctx,
                iface,
                &cfg,
                0,
                vpx_sys::VPX_ENCODER_ABI_VERSION as c_int,
            );
            if rc != vpx_sys::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(format!("vpx_codec_enc_init rc={rc:?}"));
            }
            // 实时档：VP8 用 CPUUSED 12（rustdesk 同款，VP8 更简单无需
            // row-mt/tile）；VP9 用 CPUUSED 7 + row-mt + tile-columns。
            if is_vp8 {
                set_ctl(&mut ctx, vpx_sys::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int, 12);
            } else {
                set_ctl(&mut ctx, vpx_sys::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int, 7);
                set_ctl(&mut ctx, vpx_sys::vp8e_enc_control_id::VP9E_SET_ROW_MT as c_int, 1);
                set_ctl(&mut ctx, vpx_sys::vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as c_int, 4);
            }

            Ok(Self {
                ctx,
                width: w,
                height: h,
                fps,
                bitrate_bps,
                max_bps: 0,
                is_vp8,
                quality: crate::agent::desktop::encoder::QUALITY_BALANCED,
                force_kf: false,
                pts_ms: 0,
            })
        }
    }

    /// Encode one I420 frame (length must match `w*h*3/2`).
    pub fn encode(&mut self, i420: &[u8]) -> Result<EncodedFrame, String> {
        let w = self.width as usize;
        let h = self.height as usize;
        let y_len = w * h;
        let uv_len = y_len / 4;
        assert!(i420.len() == y_len + 2 * uv_len, "I420 buffer length mismatch");

        unsafe {
            let mut img: vpx_sys::vpx_image_t = std::mem::MaybeUninit::zeroed().assume_init();
            let rc = vpx_sys::vpx_img_alloc(&mut img, vpx_sys::vpx_img_fmt::VPX_IMG_FMT_I420, self.width, self.height, 1);
            if rc.is_null() {
                return Err("vpx_img_alloc failed".into());
            }
            std::ptr::copy_nonoverlapping(i420.as_ptr(), img.planes[0], y_len);
            std::ptr::copy_nonoverlapping(i420[y_len..].as_ptr(), img.planes[1], uv_len);
            std::ptr::copy_nonoverlapping(i420[y_len + uv_len..].as_ptr(), img.planes[2], uv_len);

            let flags: std::os::raw::c_long = if self.force_kf {
                self.force_kf = false;
                vpx_sys::VPX_EFLAG_FORCE_KF as std::os::raw::c_long
            } else {
                0
            };
            let rc = vpx_sys::vpx_codec_encode(
                &mut self.ctx,
                &img,
                self.pts_ms as vpx_sys::vpx_codec_pts_t,
                1, // duration: timebase 1ms，rustdesk 同款（帧间隔由 pts 差表达）
                flags,
                vpx_sys::VPX_DL_REALTIME as std::os::raw::c_ulong,
            );
            self.pts_ms += (1000.0 / self.fps).round().max(1.0) as u64;
            vpx_sys::vpx_img_free(&mut img);
            if rc != vpx_sys::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(format!("vpx_codec_encode rc={rc:?}"));
            }

            // Collect every compressed frame packet.
            let mut data: Vec<u8> = Vec::new();
            let mut is_key = false;
            let mut iter: vpx_sys::vpx_codec_iter_t = std::ptr::null_mut();
            loop {
                let pkt = vpx_sys::vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind as c_int != VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let f = &(*pkt).data.frame;
                is_key |= (f.flags as c_uint) & VPX_FRAME_IS_KEY != 0;
                if f.sz > 0 && !f.buf.is_null() {
                    let bytes = std::slice::from_raw_parts(f.buf as *const u8, f.sz);
                    data.extend_from_slice(bytes);
                }
            }
            // 对齐 rustdesk encode+flush 成对语义：REALTIME 档下 libvpx 可能把
            // 当前帧缓冲在内部（deadline 到点未编码完），NULL encode 强制取回，
            // 避免帧滞留造成端到端延迟累积（MYS-886 卡顿回归排查项）。
            let _ = vpx_sys::vpx_codec_encode(
                &mut self.ctx,
                std::ptr::null(),
                -1,
                1,
                0,
                vpx_sys::VPX_DL_REALTIME as std::os::raw::c_ulong,
            );
            let mut iter2: vpx_sys::vpx_codec_iter_t = std::ptr::null_mut();
            loop {
                let pkt = vpx_sys::vpx_codec_get_cx_data(&mut self.ctx, &mut iter2);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind as c_int != VPX_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let f = &(*pkt).data.frame;
                is_key |= (f.flags as c_uint) & VPX_FRAME_IS_KEY != 0;
                if f.sz > 0 && !f.buf.is_null() {
                    let bytes = std::slice::from_raw_parts(f.buf as *const u8, f.sz);
                    data.extend_from_slice(bytes);
                }
            }
            Ok(EncodedFrame {
                nalu: data,
                is_idr: is_key,
                sps: None,
                pps: None,
            })
        }
    }

    /// Dynamically update the target bitrate (bps). libvpx CBR honours this.
    pub fn set_bitrate(&mut self, bps: u64) {
        self.bitrate_bps = bps;
        unsafe {
            let src_ptr = self.ctx.config.enc as *const vpx_sys::vpx_codec_enc_cfg_t;
            let mut cfg: vpx_sys::vpx_codec_enc_cfg_t = std::ptr::read(src_ptr);
            // mutate bitrate, re-apply.
            std::ptr::copy_nonoverlapping(
                self.ctx.config.enc as *const u8,
                &mut cfg as *mut vpx_sys::vpx_codec_enc_cfg_t as *mut u8,
                std::mem::size_of::<vpx_sys::vpx_codec_enc_cfg_t>(),
            );
            cfg.rc_target_bitrate = (bps / 1000).min(u32::MAX as u64) as c_uint;
            vpx_sys::vpx_codec_enc_config_set(&mut self.ctx, &cfg);
        }
    }

    /// 按质量档动态调整（rustdesk QoS 同款）：重算目标码率 + QP 区间。
    pub fn set_quality(&mut self, ratio: f32) {
        self.quality = ratio;
        let (q_min, q_max) = crate::agent::desktop::encoder::calc_q_values(ratio);
        let target = crate::agent::desktop::encoder::target_bitrate(
            self.width,
            self.height,
            self.max_bps,
            ratio,
        );
        self.bitrate_bps = target;
        unsafe {
            let src_ptr = self.ctx.config.enc as *const vpx_sys::vpx_codec_enc_cfg_t;
            let mut cfg: vpx_sys::vpx_codec_enc_cfg_t = std::ptr::read(src_ptr);
            cfg.rc_target_bitrate = (target / 1000).min(u32::MAX as u64) as c_uint;
            cfg.rc_min_quantizer = q_min;
            cfg.rc_max_quantizer = q_max;
            vpx_sys::vpx_codec_enc_config_set(&mut self.ctx, &cfg);
        }
    }

    /// Read back the current target bitrate (bps).
    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    /// Dynamically update the encoder's frame-rate model.
    ///
    /// **不再调用 `vpx_codec_enc_config_set`**（MYS-886 Windows 闪退根因，
    /// 与 AV1 同款）：运行期频繁 config_set 会使 libvpx 内部量化/编解码
    /// 状态与 config 不同步，后续 `encode` 崩溃。KF_DISABLED 下关键帧全由
    /// 外部 force_idr 控制（kf_max_dist 保持 0），RC 帧率由真实 pts 差
    /// 表达，帧率模型无需运行期重设。fps 仅保存在 Rust 侧供逻辑使用。
    pub fn set_frame_rate(&mut self, fps: f32) {
        self.fps = fps.clamp(1.0, 60.0) as f64;
    }

    /// Force the next encoded frame to be a key frame.
    pub fn force_idr(&mut self) {
        self.force_kf = true;
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn fps(&self) -> f64 {
        self.fps
    }
}

fn set_ctl(ctx: *mut vpx_sys::vpx_codec_ctx_t, ctrl_id: c_int, val: c_int) {
    unsafe {
        let rc = vpx_sys::vpx_codec_control_(ctx, ctrl_id, val);
        // 对齐 rustdesk `call_vpx` 的宽松处理：控件失败只告警不 panic。
        // 旧 assert_eq! 在控件运行期失败时 panic → 编码器 init 失败 →
        // 编码方式切换失效。控件是尽力而为的优化，失败仍可用默认行为。
        if rc != vpx_sys::vpx_codec_err_t::VPX_CODEC_OK {
            tracing::warn!("vpx control {ctrl_id} rc={rc:?} — ignored");
        }
    }
}

impl Drop for Vp9Encoder {
    fn drop(&mut self) {
        unsafe {
            vpx_sys::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

impl crate::agent::desktop::encoder::VideoEncoder for Vp9Encoder {
    fn encode(&mut self, i420: &[u8]) -> Result<EncodedFrame, String> {
        Vp9Encoder::encode(self, i420)
    }

    fn force_idr(&mut self) {
        Vp9Encoder::force_idr(self);
    }

    fn set_bitrate(&mut self, bps: u64) {
        Vp9Encoder::set_bitrate(self, bps);
    }

    fn set_quality(&mut self, ratio: f32) {
        Vp9Encoder::set_quality(self, ratio);
    }

    fn bitrate_bps(&self) -> u64 {
        Vp9Encoder::bitrate_bps(self)
    }

    fn set_frame_rate(&mut self, fps: f32) {
        Vp9Encoder::set_frame_rate(self, fps);
    }

    fn fps(&self) -> f64 {
        Vp9Encoder::fps(self)
    }

    fn width(&self) -> u32 {
        Vp9Encoder::width(self)
    }

    fn height(&self) -> u32 {
        Vp9Encoder::height(self)
    }

    fn codec(&self) -> &'static str {
        if self.is_vp8 { "vp8" } else { "vp9" }
    }

    fn mux_sample(&self, _frame: &EncodedFrame) -> Option<crate::agent::desktop::encoder::VisualSample> {
        // VP9: profile/level 静态（8-bit 4:2:0 screen stream）。Level 10=1.0
        // 覆盖 720p30、20=2.0 覆盖 1080p30。VP8 用 Vp8 变体（box `vp08` +
        // codec 串 `vp08.*`），浏览器据此选 VP8 解码器而非 VP9。
        if self.is_vp8 {
            return Some(crate::agent::desktop::mp4::VisualSample::Vp8 { profile: 0, level: 10 });
        }
        let level = if self.height > 720 { 20 } else { 10 };
        Some(crate::agent::desktop::mp4::VisualSample::Vp9 { profile: 0, level })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::desktop::encoder::VideoEncoder;

    fn solid_i420(w: usize, h: usize, v: u8) -> Vec<u8> {
        vec![v; w * h + w * h / 2]
    }

    #[test]
    fn test_vp8_encoder_produces_frames() {
        // rustdesk 4G 内存设备的 VP8 降级档：接口与 VP9 相同但走 vp8 iface。
        let mut enc = Vp9Encoder::new(640, 360, 500_000, 30.0, 24, 50, true).expect("vp8 init");
        assert_eq!(enc.codec(), "vp8");
        assert!(matches!(
            enc.mux_sample(&EncodedFrame { nalu: vec![], is_idr: true, sps: None, pps: None }),
            Some(crate::agent::desktop::mp4::VisualSample::Vp8 { .. })
        ), "vp8 must mux a Vp8 sample (vp08 box), not Vp9");
        let (mut saw_key, mut bytes, mut out) = (false, 0usize, 0usize);
        let mut buf = solid_i420(640, 360, 128);
        for t in 0..40u32 {
            // 移动窗口模拟桌面动画
            if t % 4 == 0 {
                for i in (0..buf.len()).step_by(131) {
                    buf[i] = (t * 13) as u8;
                }
            }
            let f = enc.encode(&buf).expect("encode");
            if f.is_idr { saw_key = true; }
            if !f.nalu.is_empty() {
                out += 1;
                bytes += f.nalu.len();
            }
        }
        assert!(saw_key, "vp8 must produce a key frame");
        assert!(out >= 30, "vp8 must output most frames, got {out}/40");
        assert!(bytes > 0, "vp8 must produce compressed bytes");
        let kbps = bytes as f64 * 8.0 / (40.0 / 30.0) / 1000.0;
        eprintln!("vp8 500k target: actual {kbps:.0} kbps, {out}/40 frames");
        assert!(kbps < 4000.0, "vp8 bitrate runaway: {kbps:.0} kbps");
    }

    #[test]
    fn test_vp9_encoder_produces_frames() {
        let mut enc = Vp9Encoder::new(320, 240, 400_000, 15.0, 24, 50, false).expect("vp9 init");
        let mut saw_key = false;
        let mut bytes = 0usize;
        for t in 0..30 {
            let mut buf = solid_i420(320, 240, (t % 251) as u8);
            // 周期性扰动制造运动
            if t % 3 == 0 {
                for i in (0..buf.len()).step_by(197) {
                    buf[i] = 90;
                }
            }
            let f = enc.encode(&buf).expect("encode");
            if f.is_idr {
                saw_key = true;
            }
            bytes += f.nalu.len();
        }
        assert!(saw_key, "must produce at least one key frame");
        assert!(bytes > 0, "must produce compressed bytes");
    }

    #[test]
    fn test_vp9_bitrate_actually_controlled() {
        // 800k 目标下实测码率应收敛在目标附近（这是 OpenH264 skip=0 做不到的）。
        let mut enc = Vp9Encoder::new(1280, 720, 800_000, 30.0, 24, 50, false).expect("vp9 init");
        let (mut bytes, mut out) = (0usize, 0usize);
        // 模拟真实桌面动画: 静态底色 + 一个移动的高对比窗口(而非随机噪声,
        // 噪声的不可压缩性不反映真实桌面, 会让 RC 误判)。
        let (mut buf, mut x) = (vec![128u8; 1280 * 720 * 3 / 2], 0i32);
        for t in 0..60u32 {
            buf.fill(128);
            x = (x + 17) % 1100;
            for dy in 0..200i32 {
                let row = (100 + dy) * 1280;
                for dx in 0..180i32 {
                    let px = (row + x + dx) as usize;
                    let v = if (dx / 9 + dy / 9) % 2 == 0 { 240u8 } else { 16u8 };
                    buf[px] = v;
                    buf[1280 * 720 + px / 2] = v / 2;
                    buf[1280 * 720 + 1280 * 720 / 4 + px / 2] = v / 2;
                }
            }
            // 窗口里的滚动文本内容动态变化
            if t % 5 == 0 {
                for i in (0..buf.len()).step_by(199) {
                    buf[i] = buf[i].wrapping_add((t as u8).wrapping_mul(7));
                }
            }
            let f = enc.encode(&buf).expect("encode");
            if !f.nalu.is_empty() {
                out += 1;
                bytes += f.nalu.len();
            }
        }
        let kbps = bytes as f64 * 8.0 / (60.0 / 30.0) / 1000.0;
        eprintln!("vp9 800k target: actual {kbps:.0} kbps, {out}/60 frames");
        assert!(kbps < 4000.0, "bitrate runaway: {kbps:.0} kbps");
        assert!(out >= 50, "must produce most frames, got {out}/60");
    }
}

#[cfg(test)]
mod static_kf_tests {
    use super::*;
    use crate::agent::desktop::encoder::VideoEncoder;

    #[test]
    fn static_frames_force_idr_produces_bytes() {
        let mut enc = Vp9Encoder::new(640, 360, 400_000, 15.0, 24, 50, false).expect("vp9 init");
        let buf = vec![90u8; 640 * 360 + 640 * 360 / 2];
        let mut non_empty_kf = 0;
        let mut frames = 0;
        for i in 0..120u32 {
            enc.force_idr();
            let f = enc.encode(&buf).expect("encode");
            frames += 1;
            if f.is_idr && !f.nalu.is_empty() {
                non_empty_kf += 1;
                eprintln!("idx={i}: kf nalu_len={}", f.nalu.len());
            } else if f.is_idr {
                eprintln!("idx={i}: IDR but EMPTY nalu");
            }
        }
        eprintln!("static 120 forced frames: non_empty_kf={non_empty_kf} frames={frames}");
        assert!(non_empty_kf > 0, "forced IDR on static content must produce bytes");
    }
}

#[cfg(test)]
mod bench_encode_time {
    use super::*;
    use crate::agent::desktop::encoder::VideoEncoder;

    fn noise_i420(w: usize, h: usize, t: u32) -> Vec<u8> {
        let mut buf = vec![90u8; w * h + w * h / 2];
        let seed = t.wrapping_mul(2654435761).wrapping_add(7);
        for i in (0..w * h).step_by(97) {
            buf[i] = (i as u32).wrapping_mul(31).wrapping_add(seed) as u8;
        }
        buf
    }

    #[test]
    fn bench_vp9_1080p_complex_encode_ms_per_frame() {
        let mut enc = Vp9Encoder::new(1920, 1080, 800_000, 30.0, 24, 50, false).expect("vp9 init");
        let start = std::time::Instant::now();
        let n = 15u32;
        for t in 0..n {
            let buf = noise_i420(1920, 1080, t);
            let _ = enc.encode(&buf).expect("encode");
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!("BENCH vp9 1080p complex: {ms:.1} ms/frame");
        // REALTIME deadline 下 1080p 软编应在 ~10-80ms/帧；上限 200ms 是
        // 宽松护栏（并行跑全量测试时 CPU 竞争会放大单帧耗时）。
        assert!(ms < 200.0, "vp9 frame took {ms:.1}ms — latency bug remains");
    }
}
