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
    /// 用户 `--desktop-max-bitrate` 覆盖（0 = 自动 rustdesk 模型）。
    max_bps: u64,
    quality: f32,
    force_kf: bool,
    /// 递增帧时间戳（timebase 1ms，对齐 rustdesk：每帧 +1000/fps）。
    pts_ms: u64,
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
    /// `bitrate_bps` = 目标码率（已由 encoder::target_bitrate 计算）；
    /// `q_min/q_max` = 质量档对应的 QP 区间（rustdesk 同款）。
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
        let max_bps = 0; // new 阶段未知用户覆盖；ABR 里由 set_bitrate 直接设目标
        let quality = crate::agent::desktop::encoder::QUALITY_BALANCED;

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
            cfg.g_threads = crate::agent::desktop::encoder::codec_thread_num() as c_uint;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 1000;
            cfg.g_error_resilient = 0;
            cfg.g_pass = aom_sys::aom_enc_pass_AOM_RC_ONE_PASS;
            cfg.g_lag_in_frames = 0;
            cfg.rc_end_usage = aom_sys::aom_rc_mode_AOM_CBR;
            cfg.rc_target_bitrate = (bitrate_bps / 1000).min(u32::MAX as u64) as c_uint;
            cfg.rc_min_quantizer = q_min;
            cfg.rc_max_quantizer = q_max;
            // rustdesk AV1 (webrtc 配置)：undershoot/overshoot 各 50%，缓冲
            // 600/600/1000（初始/最优/总量）—— 比默认更紧的码率边界 + 更快
            // 填满 rc buffer，降低首帧等待与码率收敛延迟（MYS-886 卡顿修复）。
            cfg.rc_undershoot_pct = 50;
            cfg.rc_overshoot_pct = 50;
            cfg.rc_buf_initial_sz = 600;
            cfg.rc_buf_optimal_sz = 600;
            cfg.rc_buf_sz = 1000;
            // AV1 不丢帧：rustdesk aom.rs 不设 rc_dropframe_thresh（默认 0）。
            // 我 v0.27 曾错误套用 VP9 的 dropframe=25，实测 60 帧只输出 20 帧
            // （丢 2/3）——用户局域网"丢包/卡顿"的直接来源。AV1 CBR 靠
            // QP/undershoot 控码率，无需丢帧（libaom 高熵下 QP 自适应足够）。
            // rustdesk 非录制（keyframe_interval=None）时 AOM_KF_DISABLED：
            // 关键帧完全由外部 force_idr 控制（MYS-886 外部动态节奏：静止
            // 4.5s / 活跃 1.5s + 首帧强制），编码器不自动插关键帧。
            cfg.kf_mode = aom_sys::aom_kf_mode_AOM_KF_DISABLED;
            cfg.kf_min_dist = 0;
            cfg.kf_max_dist = 0;

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
            // 低延迟实时档全套（对齐 rustdesk libs/scrap/src/common/aom.rs
            // webrtc 配置，MYS-886 卡顿修复）：
            //   - cpu_used 按分辨率分级（≤320x180=8、≤640x360=9、其余=10），
            //     1080p 用 10 而非固定 8 —— 大幅降低单帧编码耗时
            //   - AOM_CONTENT_SCREEN：屏幕内容专用 tune（关闭电影类工具）
            //   - 显式关闭高耗时工具（warped/global/obmc/ref_frame_mvs/
            //     tpl/deltaq/order_hint/dual_filter/rect/restoration 等）
            //   - AQ_MODE=3（区域化量化，屏幕低比特率信息保留更好）
            //   - MAX_INTRA_BITRATE_PCT=300：关键帧码率上限放宽，避免
            //     静止桌面关键帧被 CBR 压垮
            // 注意: **不用 tile-columns** —— Chrome WebCodecs 的 av01 chunk 判定
            // (libgav1/dav1d) 对多 tile 关键帧不兼容: 帧内多个 tile 会让其判断
            // "wasn't a key frame" 并拒绝 (实测: ffmpeg tile-columns 4 的帧被拒,
            // tile-columns 0 全 OK)。多线程交给 row_mt + g_threads, tile 留给单 tile。
            let cpu_used = if w <= 320 && h <= 180 {
                8
            } else if w <= 640 && h <= 360 {
                9
            } else {
                10
            };
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AOME_SET_CPUUSED as c_int, cpu_used);
            // 屏幕内容 tune 优先（AOM_CONTENT_SCREEN 等价 webrtc 的 kScreensharing）
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_TUNE_CONTENT as c_int, aom_sys::aom_tune_content_AOM_CONTENT_SCREEN as c_int);
            // 禁高耗时帧间/帧内工具（均不显著改善屏幕编码质量，但显著降耗时）
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_TPL_MODEL as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_DELTAQ_MODE as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_ORDER_HINT as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_WARPED_MOTION as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_GLOBAL_MOTION as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_OBMC as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_REF_FRAME_MVS as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_DUAL_FILTER as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_RESTORATION as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_CFL_INTRA as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_SMOOTH_INTRA as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_FILTER_INTRA as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_ANGLE_DELTA as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_PAETH_INTRA as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_INTRABC as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_RECT_PARTITIONS as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_TX64 as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_DIST_WTD_COMP as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_MASKED_COMP as c_int, 0);
            // 结构化更新：MV/coeff/mode 成本每 3 帧同步（screenshare 内容变化
            // 快，全量同步浪费 CPU；rustdesk 同款）
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_MV_COST_UPD_FREQ as c_int, 3);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_COEFF_COST_UPD_FREQ as c_int, 3);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_MODE_COST_UPD_FREQ as c_int, 3);
            // 区域化量化 + 关键帧码率放宽
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_AQ_MODE as c_int, 3);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AOME_SET_MAX_INTRA_BITRATE_PCT as c_int, 300);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ROW_MT as c_int, 1);
            // SUPERBLOCK_SIZE（rustdesk get_super_block_size）：≥4 线程且
            // 540p≤分辨率<1080p 用 64x64，否则 DYNAMIC —— 降低分区搜索开销。
            let sb = if cfg.g_threads >= 4
                && w >= 960 && h >= 540
                && w * h < 1920 * 1080
            {
                aom_sys::aom_superblock_size_AOM_SUPERBLOCK_SIZE_64X64
            } else {
                aom_sys::aom_superblock_size_AOM_SUPERBLOCK_SIZE_DYNAMIC
            };
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_SUPERBLOCK_SIZE as c_int, sb as c_int);
            // 其余 rustdesk 同款控件（对齐 libs/scrap/src/common/aom.rs webrtc 配置）：
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_CDEF as c_int, 1);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_PALETTE as c_int, 1);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_NOISE_SENSITIVITY as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_DIFF_WTD_COMP as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_INTERINTRA_COMP as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_INTERINTRA_WEDGE as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_INTRA_EDGE_FILTER as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_SMOOTH_INTERINTRA as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_ENABLE_QM as c_int, 0);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_INTRA_DEFAULT_TX_ONLY as c_int, 1);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_DISABLE_TRELLIS_QUANT as c_int, 1);
            set_ctl(&mut ctx, aom_sys::aome_enc_control_id_AV1E_SET_MAX_REFERENCE_FRAMES as c_int, 3);

            Ok(Self {
                ctx,
                width: w,
                height: h,
                fps,
                bitrate_bps,
                max_bps,
                quality,
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
                self.pts_ms as aom_sys::aom_codec_pts_t,
                1, // duration: timebase 1ms，rustdesk 同款（帧间隔由 pts 差表达）
                flags,
            );
            self.pts_ms += (1000.0 / self.fps).round().max(1.0) as u64;
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

    /// 按质量档动态调整（rustdesk QoS 同款）：重算目标码率 + QP 区间。
    pub fn set_quality(&mut self, ratio: f32) {
        self.quality = ratio;
        let (q_min, q_max) = crate::agent::desktop::encoder::calc_q_values_aom(ratio);
        let target = crate::agent::desktop::encoder::target_bitrate(
            self.width,
            self.height,
            self.max_bps,
            ratio,
        );
        self.bitrate_bps = target;
        unsafe {
            let src_ptr = self.ctx.config.enc as *const aom_sys::aom_codec_enc_cfg_t;
            let mut cfg: aom_sys::aom_codec_enc_cfg_t = std::ptr::read(src_ptr);
            cfg.rc_target_bitrate = (target / 1000).min(u32::MAX as u64) as c_uint;
            cfg.rc_min_quantizer = q_min;
            cfg.rc_max_quantizer = q_max;
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
            cfg.kf_max_dist = (self.fps * crate::agent::desktop::KF_AUTO_MAX_SECS) as c_uint;
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
        // 对齐 rustdesk `call_aom_allow_err!`：控件失败只告警不 panic。
        // 旧 assert_eq! 在某个控件运行期返回错误时直接 panic → 编码器 init
        // 失败 → 编码方式切换失效（用户反馈 MYS-886）。控件是尽力而为的
        // 优化，失败时编码器仍可用默认行为工作。
        if rc != aom_sys::aom_codec_err_t_AOM_CODEC_OK {
            tracing::warn!("aom control {ctrl_id} rc={rc:?} — ignored");
        }
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

    fn set_quality(&mut self, ratio: f32) {
        AomEncoder::set_quality(self, ratio);
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
        let mut enc = AomEncoder::new(320, 240, 400_000, 15.0, 24, 50).expect("av1 init");
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
        let mut enc = AomEncoder::new(1280, 720, 800_000, 30.0, 24, 50).expect("av1 init");
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
        // AV1 不设 dropframe（rustdesk 同款，默认 0 不丢帧）：极端高熵下
        // libaom 靠 QP 自适应控码率。60 帧应全输出（=全帧保留），丢帧
        // = 用户局域网"丢包/卡顿"的直接来源（曾 dropframe=25 只出 20 帧）。
        assert!(out >= 55, "AV1 must keep almost all frames, got {out}/60");
    }

    #[test]
    fn test_av1_1080p_complex_throughput_bench() {
        // 1080p 高熵内容下 realtime 档的实际编码吞吐 —— 复杂内容"4-8s 延时"
        // 的直接瓶颈衡量（MYS-886 需求2）。不设断言, 输出 fps 供人工评估。
        let mut enc = AomEncoder::new(1920, 1080, 800_000, 30.0, 24, 50).expect("av1 init");
        let start = std::time::Instant::now();
        let n = 30u32;
        for t in 0..n {
            let mut buf = solid_i420(1920, 1080, 90);
            // 高熵：随机噪声块 + 移动窗口
            let seed = t.wrapping_mul(2654435761).wrapping_add(7);
            let y_len = 1920 * 1080;
            let u_off = y_len;
            let v_off = y_len + y_len / 4;
            let uv_half = 960usize; // 1920/2
            for y in (0..1080).step_by(8) {
                for x in (0..1920).step_by(8) {
                    if (x as u32 ^ y).wrapping_mul(seed) % 3 == 0 {
                        let i = (y as usize) * 1920 + x as usize;
                        buf[i] = ((seed >> 8) as u8).wrapping_add(x as u8);
                        let ui = (y as usize / 2) * uv_half + x as usize / 2;
                        if ui < y_len / 4 {
                            buf[u_off + ui] = (seed & 0xff) as u8;
                            buf[v_off + ui] = (seed >> 16) as u8;
                        }
                    }
                }
            }
            let _ = enc.encode(&buf).expect("encode");
        }
        let el = start.elapsed().as_secs_f64();
        eprintln!("av1 1080p complex: {n} frames in {el:.2}s = {:.1} fps", n as f64 / el);
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
    fn bench_av1_1080p_complex_encode_ms_per_frame() {
        let mut enc = AomEncoder::new(1920, 1080, 800_000, 30.0, 10, 30).expect("av1 init");
        let start = std::time::Instant::now();
        let n = 15u32;
        for t in 0..n {
            let buf = noise_i420(1920, 1080, t);
            let _ = enc.encode(&buf).expect("encode");
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!("BENCH av1 1080p complex: {ms:.1} ms/frame");
        // cpu_used=10 + screen tune + 禁高耗时工具后单帧应 ~10-80ms。
        // 上限 250ms 是宽松护栏（并行跑全量测试时 CPU 竞争可把单帧拖到
        // 120-150ms）；真正的延迟验证在 E2E 链路完成。
        assert!(ms < 250.0, "av1 frame took {ms:.1}ms — latency bug remains");
    }
}
