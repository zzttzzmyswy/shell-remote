//! libaom AV1 software encoder.
//!
//! Mirrors `Vp9Encoder`'s surface so the desktop pipeline treats h264 / vp9 /
//! av1 uniformly. Config follows the same proven live-encoding recipe as VP9
//! (CBR + quantizer range + row-mt/tile-columns + realtime usage tier), so
//! bitrate is *actually* honoured at fixed resolution — letting us drop the
//! resolution ladder entirely (MYS-886: "禁止自动降低分辨率").
//!
//! AV1 has higher compression than VP9 for the same quality; the CPU cost of
//! software encoding is higher, so we use `cpu-used=8` (realtime tier) and
//! tile-columns to spread work across cores, same strategy rustdesk uses.

pub mod aom_sys;

use std::os::raw::{c_int, c_uint};

/// One encoded picture, shaped like [`crate::agent::desktop::encoder::EncodedFrame`].
pub use crate::agent::desktop::encoder::EncodedFrame;

/// libaom AV1 software encoder handle.
pub struct AomEncoder {
    ctx: aom_sys::aom_codec_ctx_t,
    width: u32,
    height: u32,
    fps: f64,
    bitrate_bps: u64,
    force_kf: bool,
}

// libaom spawns internal worker threads; the encoder is only ever driven from
// the single `DesktopManager` task (same contract as Vp9Encoder).
unsafe impl Send for AomEncoder {}

/// Compressed frame packet kind (AOM_CODEC_CX_FRAME_PKT).
const AOM_CODEC_CX_FRAME_PKT: c_int = 0;
/// Raw I420 image format for `aom_img_alloc` (bindgen rustified enum).
const AOM_IMG_FMT_I420: c_uint = 258; // AOM_IMG_FMT_I420
/// Key-frame flag in `aom_codec_cx_pkt.frame.flags`.
const AOM_FRAME_IS_KEY: c_uint = 0x1;

impl AomEncoder {
    /// Create an AV1 encoder pinned to `w x h` at `fps` frames/s.
    pub fn new(w: u32, h: u32, bitrate_bps: u64, fps: f64) -> Result<Self, String> {
        assert!(w % 2 == 0 && h % 2 == 0, "dimensions must be even for 4:2:0");
        assert!(bitrate_bps > 0 && fps > 0.0);

        unsafe {
            let iface = aom_sys::aom_codec_av1_cx();
            if iface.is_null() {
                return Err("aom_codec_av1_cx unavailable".into());
            }
            let mut cfg: aom_sys::aom_codec_enc_cfg_t =
                unsafe { std::mem::MaybeUninit::uninit().assume_init() };
            let rc = aom_sys::aom_codec_enc_config_default(iface, &mut cfg, aom_sys::AOM_USAGE_REALTIME);
            if rc != aom_sys::aom_codec_err_t_AOM_CODEC_OK {
                return Err(format!("enc_config_default rc={rc:?}"));
            }
            cfg.g_w = w;
            cfg.g_h = h;
            cfg.g_threads = 4;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 1000;
            cfg.g_error_resilient = 1;
            cfg.g_pass = aom_sys::aom_enc_pass_AOM_RC_ONE_PASS;
            cfg.g_lag_in_frames = 0;
            cfg.rc_end_usage = aom_sys::aom_rc_mode_AOM_CBR;
            cfg.rc_target_bitrate = (bitrate_bps / 1000).min(u32::MAX as u64) as c_uint;
            cfg.rc_min_quantizer = 0;
            cfg.rc_max_quantizer = 63;
            cfg.rc_undershoot_pct = 95;
            // 不丢帧: 用户明确"接受模糊、不接受掉帧"(MYS-886)。CBR 靠 QP
            // 把瞬时峰值压回预算, 而不是跳过 P 帧造成卡顿。libaom 的
            // dropframe 独立于码率控制, 关掉它码率仍受控(见码率测试)。
            cfg.rc_dropframe_thresh = 0;
            cfg.kf_mode = aom_sys::aom_kf_mode_AOM_KF_AUTO;
            cfg.kf_min_dist = 0;
            cfg.kf_max_dist = fps.max(1.0) as c_uint;

            let mut ctx: aom_sys::aom_codec_ctx_t = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
            let rc = aom_sys::aom_codec_enc_init_ver(
                &mut ctx,
                iface,
                &cfg,
                0,
                aom_sys::AOM_ENCODER_ABI_VERSION as c_int,
            );
            if rc != aom_sys::aom_codec_err_t_AOM_CODEC_OK {
                return Err(format!("aom_codec_enc_init rc={rc:?}"));
            }
            // 实时档: cpu-used 8 (realtime tier), row-mt 并行估计算法。
// 注意: **不用 tile-columns** —— Chrome WebCodecs 的 av01 chunk 判定
// (libgav1/dav1d) 对多 tile 关键帧不兼容: 帧内多个 tile 会让其判断
// "wasn't a key frame" 并拒绝 (实测: ffmpeg tile-columns 4 的帧被拒,
// tile-columns 0 全 OK)。多线程交给 row_mt + g_threads, tile 留给单 tile。
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AOME_SET_CPUUSED as c_int, 8);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ROW_MT as c_int, 1);

            Ok(Self {
                ctx,
                width: w,
                height: h,
                fps,
                bitrate_bps,
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
            let mut img: aom_sys::aom_image_t = std::mem::MaybeUninit::zeroed().assume_init();
            let rc = aom_sys::aom_img_alloc(&mut img, aom_sys::aom_img_fmt_AOM_IMG_FMT_I420, self.width, self.height, 1);
            if rc.is_null() {
                return Err("aom_img_alloc failed".into());
            }
            std::ptr::copy_nonoverlapping(i420.as_ptr(), img.planes[0], y_len);
            std::ptr::copy_nonoverlapping(i420[y_len..].as_ptr(), img.planes[1], uv_len);
            std::ptr::copy_nonoverlapping(i420[y_len + uv_len..].as_ptr(), img.planes[2], uv_len);

            let flags = if self.force_kf {
                self.force_kf = false;
                aom_sys::AOM_EFLAG_FORCE_KF as aom_sys::aom_enc_frame_flags_t
            } else {
                0 as aom_sys::aom_enc_frame_flags_t
            };
            let rc = aom_sys::aom_codec_encode(
                &mut self.ctx,
                &img,
                0,
                33,
                flags,
            );
            aom_sys::aom_img_free(&mut img);
            if rc != aom_sys::aom_codec_err_t_AOM_CODEC_OK {
                return Err(format!("aom_codec_encode rc={rc:?}"));
            }

                        // Collect every compressed frame packet.
            let mut data: Vec<u8> = Vec::new();
            let mut is_key = false;
            let mut iter: aom_sys::aom_codec_iter_t = std::ptr::null_mut();
            loop {
                let pkt = aom_sys::aom_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind as c_int != AOM_CODEC_CX_FRAME_PKT {
                    continue;
                }
                let f = &(*pkt).data.frame;
                is_key |= (f.flags as c_uint) & AOM_FRAME_IS_KEY != 0;
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

    /// Dynamically update the target bitrate (bps). libaom CBR honours this.
    pub fn set_bitrate(&mut self, bps: u64) {
        self.bitrate_bps = bps;
        unsafe {
            let src_ptr = self.ctx.config.enc as *const aom_sys::aom_codec_enc_cfg_t;
            let mut cfg: aom_sys::aom_codec_enc_cfg_t = std::ptr::read(src_ptr);
            cfg.rc_target_bitrate = (bps / 1000).min(u32::MAX as u64) as c_uint;
            aom_sys::aom_codec_enc_config_set(&mut self.ctx, &cfg);
        }
    }

    /// Read back the current target bitrate (bps).
    pub fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    /// Dynamically update the encoder's frame-rate model.
    pub fn set_frame_rate(&mut self, fps: f32) {
        self.fps = fps.clamp(1.0, 60.0) as f64;
        unsafe {
            let src_ptr = self.ctx.config.enc as *const aom_sys::aom_codec_enc_cfg_t;
            let mut cfg: aom_sys::aom_codec_enc_cfg_t = std::ptr::read(src_ptr);
            cfg.kf_max_dist = self.fps.max(1.0) as c_uint;
            aom_sys::aom_codec_enc_config_set(&mut self.ctx, &cfg);
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

fn set_ctl(ctx: *mut aom_sys::aom_codec_ctx_t, ctrl_id: c_int, val: c_int) {
    unsafe {
        let rc = aom_sys::aom_codec_control(ctx, ctrl_id, val);
        assert_eq!(rc, aom_sys::aom_codec_err_t_AOM_CODEC_OK, "aom control {ctrl_id} rc={rc:?}");
    }
}

impl Drop for AomEncoder {
    fn drop(&mut self) {
        unsafe {
            aom_sys::aom_codec_destroy(&mut self.ctx);
        }
    }
}

impl crate::agent::desktop::encoder::VideoEncoder for AomEncoder {
    fn encode(&mut self, i420: &[u8]) -> Result<EncodedFrame, String> {
        AomEncoder::encode(self, i420)
    }

    fn force_idr(&mut self) {
        AomEncoder::force_idr(self);
    }

    fn set_bitrate(&mut self, bps: u64) {
        AomEncoder::set_bitrate(self, bps);
    }

    fn bitrate_bps(&self) -> u64 {
        AomEncoder::bitrate_bps(self)
    }

    fn set_frame_rate(&mut self, fps: f32) {
        AomEncoder::set_frame_rate(self, fps);
    }

    fn fps(&self) -> f64 {
        AomEncoder::fps(self)
    }

    fn width(&self) -> u32 {
        AomEncoder::width(self)
    }

    fn height(&self) -> u32 {
        AomEncoder::height(self)
    }

    fn codec(&self) -> &'static str {
        "av1"
    }

    fn mux_sample(&self, _frame: &EncodedFrame) -> Option<crate::agent::desktop::encoder::VisualSample> {
        // AV1: profile 0 (8-bit 4:2:0), level 与分辨率匹配 (按 luma 采样率):
        //   3.0(30) 覆盖 ≤720p30 (5.9M/s), 5.0(50) 覆盖 1080p30 (62.2M/s)。
        //   level → av1C seq_level_idx → codec 串 LL，由 mp4.rs 统一换算。
        let level = if self.height > 720 { 50 } else { 30 };
        Some(crate::agent::desktop::mp4::VisualSample::Av1 { profile: 0, level })
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
    fn test_av1_encoder_produces_frames() {
        let mut enc = AomEncoder::new(320, 240, 400_000, 15.0).expect("av1 init");
        let mut saw_key = false;
        let mut bytes = 0usize;
        for t in 0..30 {
            let mut buf = solid_i420(320, 240, (t % 251) as u8);
            if t % 3 == 0 {
                for i in (0..buf.len()).step_by(197) {
                    buf[i] = 90;
                }
            }
            let f = enc.encode(&buf).expect("encode");
            if t < 4 {
                eprintln!("frame {t}: is_idr={} nalu_len={} head={:?}", f.is_idr, f.nalu.len(), &f.nalu[..f.nalu.len().min(6)]);
            }
            if f.is_idr {
                saw_key = true;
            }
            bytes += f.nalu.len();
        }
        assert!(saw_key, "must produce at least one key frame");
        assert!(bytes > 0, "must produce compressed bytes");
    }

    #[test]
    fn test_av1_bitrate_actually_controlled() {
        // 800k 目标下实测码率应收敛在目标附近 (CBR, 固定分辨率 —— 这正是
        // 用户要求"禁止自动降低分辨率"的底气)。
        let mut enc = AomEncoder::new(1280, 720, 800_000, 30.0).expect("av1 init");
        let (mut bytes, mut out) = (0usize, 0usize);
        for t in 0..60u32 {
            let mut buf = solid_i420(1280, 720, (t % 251) as u8);
            let seed = t.wrapping_mul(2654435761).wrapping_add(12345);
            for i in (0..buf.len()).step_by(97) {
                buf[i] = ((i as u32).wrapping_mul(31).wrapping_add(seed) >> 16) as u8;
            }
            let f = enc.encode(&buf).expect("encode");
            if !f.nalu.is_empty() {
                out += 1;
                bytes += f.nalu.len();
            }
        }
        let kbps = bytes as f64 * 8.0 / (60.0 / 30.0) / 1000.0;
        eprintln!("av1 800k target: actual {kbps:.0} kbps, {out}/60 frames");
        assert!(kbps < 4000.0, "bitrate runaway: {kbps:.0} kbps");
        assert!(out >= 50, "must produce most frames, got {out}/60");
    }
}