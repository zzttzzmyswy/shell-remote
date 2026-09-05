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

/// libvpx VP9 encoder handle.
pub struct Vp9Encoder {
    ctx: vpx_sys::vpx_codec_ctx_t,
    width: u32,
    height: u32,
    fps: f64,
    bitrate_bps: u64,
    /// 用户 --desktop-max-bitrate 覆盖（0 = 自动 rustdesk 模型）。
    max_bps: u64,
    quality: f32,
    /// 下一次 encode 强制关键帧（通过 encode flags 传 VPX_EFLAG_FORCE_KF）。
    force_kf: bool,
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
    /// Create a VP9 encoder pinned to `w x h` at `fps` frames/s.
    pub fn new(
        w: u32,
        h: u32,
        bitrate_bps: u64,
        fps: f64,
        q_min: u32,
        q_max: u32,
    ) -> Result<Self, String> {
        assert!(w % 2 == 0 && h % 2 == 0, "dimensions must be even for 4:2:0");
        assert!(bitrate_bps > 0 && fps > 0.0);

        unsafe {
            let iface = vpx_sys::vpx_codec_vp9_cx();
            if iface.is_null() {
                return Err("vpx_codec_vp9_cx unavailable".into());
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
            cfg.g_threads = 4;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 1000;
            cfg.g_error_resilient = 1; // VPX_ERROR_RESILIENT_DEFAULT
            cfg.g_pass = vpx_sys::vpx_enc_pass::VPX_RC_ONE_PASS;
            cfg.g_lag_in_frames = 0;
            cfg.rc_end_usage = vpx_sys::vpx_rc_mode::VPX_CBR;
            cfg.rc_target_bitrate = (bitrate_bps / 1000).min(u32::MAX as u64) as c_uint;
            cfg.rc_min_quantizer = q_min;
            cfg.rc_max_quantizer = q_max;
            cfg.rc_undershoot_pct = 25;
            cfg.rc_overshoot_pct = 25;
            // VP9 的 RC 需要 dropframe 作高熵压力阀: dropframe=0 时高熵内容
            // 码率彻底失控(实测 7367kbps @ 800k 目标)且编码速度暴跌
            // (156ms/帧, CBR 死命压大帧)。保留 25 让 RC 丢弃过盈的高熵帧
            // 保持码率受控与帧率——桌面共享场景丢的是极端运动帧, 可接受。
            // (AV1 的 libaom CBR 无此问题, 用 dropframe=0 全帧保留。)
            cfg.rc_dropframe_thresh = 25;
            cfg.kf_mode = vpx_sys::vpx_kf_mode::VPX_KF_AUTO;
            cfg.kf_min_dist = 0;
            cfg.kf_max_dist = fps.max(1.0) as c_uint;

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
            // 实时档: CPUUSED 7 (live), row-mt + tile-columns 多线程。
            set_ctl(&mut ctx, vpx_sys::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int, 7);
            set_ctl(&mut ctx, vpx_sys::vp8e_enc_control_id::VP9E_SET_ROW_MT as c_int, 1);
            set_ctl(&mut ctx, vpx_sys::vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as c_int, 4);

            Ok(Self {
                ctx,
                width: w,
                height: h,
                fps,
                bitrate_bps,
                max_bps: 0,
                quality: crate::agent::desktop::encoder::QUALITY_BALANCED,
                force_kf: false,
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
                0,
                33,
                flags,
                1_000_000, // deadline: infinite, best for realtime
            );
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
    pub fn set_frame_rate(&mut self, fps: f32) {
        self.fps = fps.clamp(1.0, 60.0) as f64;
        // kf_max_dist drives key-frame spacing; keep it in sync with the model.
        unsafe {
            let src_ptr = self.ctx.config.enc as *const vpx_sys::vpx_codec_enc_cfg_t;
            let mut cfg: vpx_sys::vpx_codec_enc_cfg_t = std::ptr::read(src_ptr);
            cfg.kf_max_dist = self.fps.max(1.0) as c_uint;
            vpx_sys::vpx_codec_enc_config_set(&mut self.ctx, &cfg);
        }
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
        assert_eq!(rc, vpx_sys::vpx_codec_err_t::VPX_CODEC_OK, "vpx control {ctrl_id} rc={rc:?}");
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
        "vp9"
    }

    fn mux_sample(&self, _frame: &EncodedFrame) -> Option<crate::agent::desktop::encoder::VisualSample> {
        // VP9: profile/level are static for our 8-bit 4:2:0 screen stream.
        // Level 10 = 1.0 covers 720p30; level 20 = 2.0 covers 1080p30.
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
    fn test_vp9_encoder_produces_frames() {
        let mut enc = Vp9Encoder::new(320, 240, 400_000, 15.0, 24, 50).expect("vp9 init");
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
        let mut enc = Vp9Encoder::new(1280, 720, 800_000, 30.0, 24, 50).expect("vp9 init");
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
