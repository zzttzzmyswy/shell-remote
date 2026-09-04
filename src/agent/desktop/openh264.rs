//! H.264 software encoder wrapper over `openh264-sys2`.
//!
//! OpenH264 (Cisco, BSD-2-Clause) is statically linked via the `source`
//! feature so the final binary stays a single self-contained file. The
//! wrapper exposes the small surface the desktop pipeline needs:
//! dimension-pinned encoding, per-frame SPS/PPS extraction, dynamic bitrate
//! adjustment (via `ENCODER_OPTION_BITRATE`) and forced IDR. It does NOT
//! implement hardware encoders — those are planned as trait-compatible
//! backends behind the same `EncodedFrame` contract.

use std::os::raw::{c_int, c_void};

use openh264_sys2::*;

/// One encoded picture: Annex-B byte stream plus key-frame / parameter-set
/// metadata sketched by the shared codec-agnostic [`encoder::EncodedFrame`].
pub use crate::agent::desktop::encoder::EncodedFrame;



/// Index of the first NAL header byte (start code prefix skipped).
fn nal_header(na: &[u8]) -> Option<u8> {
    if na.len() >= 5 && na[0] == 0 && na[1] == 0 && na[2] == 0 && na[3] == 1 {
        Some(na[4])
    } else if na.len() >= 4 && na[0] == 0 && na[1] == 0 && na[2] == 1 {
        Some(na[3])
    } else {
        None
    }
}

/// Strip the Annex-B start code, yielding the bare NAL unit.
fn strip_startcode(na: &[u8]) -> &[u8] {
    if na.len() >= 4 && na[0] == 0 && na[1] == 0 && na[2] == 0 && na[3] == 1 {
        &na[4..]
    } else if na.len() >= 3 && na[0] == 0 && na[1] == 0 && na[2] == 1 {
        &na[3..]
    } else {
        na
    }
}

/// Hardware/software encoder handle.
pub struct H264Encoder {
    api: openh264_sys2::DynamicAPI,
    enc: *mut *const ISVCEncoderVtbl,
    initialize_ext: unsafe extern "C" fn(*mut ISVCEncoder, *const SEncParamExt) -> c_int,
    uninitialize: unsafe extern "C" fn(*mut ISVCEncoder) -> c_int,
    encode_frame: unsafe extern "C" fn(*mut ISVCEncoder, *const SSourcePicture, *mut SFrameBSInfo) -> c_int,
    set_option: unsafe extern "C" fn(*mut ISVCEncoder, ENCODER_OPTION, *mut c_void) -> c_int,
    get_option: unsafe extern "C" fn(*mut ISVCEncoder, ENCODER_OPTION, *mut c_void) -> c_int,
    force_intra: unsafe extern "C" fn(*mut ISVCEncoder, bool) -> c_int,
    width: u32,
    height: u32,
    fps: f64,
    bitrate_bps: u64,
}

// The encoder runs on a single task in `DesktopManager`; Rust-side accesses
// are never concurrent. OpenH264 may spawn internal worker threads, but they
// only touch encoder internals, which is the same contract the upstream
// `openh264` crate relies on.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    /// Create a new encoder pinned to `w x h` at `fps` frames/s.
    pub fn new(w: u32, h: u32, bitrate_bps: u64, fps: f64) -> Result<Self, String> {
        Self::new_ext(w, h, bitrate_bps, fps, false, 42, RC_BITRATE_MODE)
    }

    /// Parameterized variant used by the bitrate/skip experiments and the
    /// tests below. `rc_skip` restores OpenH264's rate-control frame dropping
    /// (production disables it), `max_qp` bounds how blurry a frame may get
    /// before the RC starts dropping, `rc_mode` selects the RC strategy
    /// (RC_BITRATE_MODE / RC_QUALITY_MODE / RC_OFF_MODE).
    pub fn new_ext(
        w: u32,
        h: u32,
        bitrate_bps: u64,
        fps: f64,
        rc_skip: bool,
        max_qp: i32,
        rc_mode: c_int,
    ) -> Result<Self, String> {
        assert!(w % 2 == 0 && h % 2 == 0, "dimensions must be even for 4:2:0");
        assert!(bitrate_bps > 0 && fps > 0.0);

        use openh264_sys2::API as _;
        let api = openh264_sys2::DynamicAPI::from_source();
        let mut enc: *mut *const ISVCEncoderVtbl = std::ptr::null_mut();
        // WelsCreateSVCEncoder signature: (ppEncoder: *mut *mut ISVCEncoder) — the
        // generated wrapper already applied the inner pointer indirection.
        let rc = unsafe { api.WelsCreateSVCEncoder(std::ptr::addr_of_mut!(enc)) };
        if rc != 0 || enc.is_null() {
            return Err(format!("WelsCreateSVCEncoder failed rc={rc}"));
        }

        let vtbl = || unsafe { &**enc };
        let initialize_ext = vtbl().InitializeExt.ok_or("missing InitializeExt")?;
        let uninitialize = vtbl().Uninitialize.ok_or("missing Uninitialize")?;
        let encode_frame = vtbl().EncodeFrame.ok_or("missing EncodeFrame")?;
        let set_option = vtbl().SetOption.ok_or("missing SetOption")?;
        let get_option = vtbl().GetOption.ok_or("missing GetOption")?;
        let force_intra = vtbl().ForceIntraFrame.ok_or("missing ForceIntraFrame")?;
        let get_default_params = vtbl().GetDefaultParams.ok_or("missing GetDefaultParams")?;

        // Start from encoder defaults, then pin the fields we care about.
        let mut param = std::mem::MaybeUninit::<SEncParamExt>::uninit();
        let prc = unsafe { get_default_params(enc, param.as_mut_ptr()) };
        if prc != 0 {
            unsafe { api.WelsDestroySVCEncoder(enc) };
            return Err(format!("GetDefaultParams failed rc={prc}"));
        }
        let mut param = unsafe { param.assume_init() };

        param.iUsageType = SCREEN_CONTENT_REAL_TIME;
        param.iPicWidth = w as c_int;
        param.iPicHeight = h as c_int;
        param.iTargetBitrate = bitrate_bps as c_int;
        param.iRCMode = rc_mode;
        param.fMaxFrameRate = fps as f32;
        param.iTemporalLayerNum = 1;
        param.iSpatialLayerNum = 1;
        param.sSpatialLayers[0].iVideoWidth = w as c_int;
        param.sSpatialLayers[0].iVideoHeight = h as c_int;
        param.sSpatialLayers[0].fFrameRate = fps as f32;
        param.sSpatialLayers[0].iSpatialBitrate = bitrate_bps as c_int;
        param.sSpatialLayers[0].iMaxSpatialBitrate = bitrate_bps as c_int;
        // Let the encoder pick profile/level automatically for screen content.
        param.sSpatialLayers[0].uiProfileIdc = 0;
        param.uiIntraPeriod = 0; // no periodic IDR; we force IDR explicitly
        // 低延迟多线程：screen-content 高熵帧（移动窗口）默认单线程编码
        // 单帧可达 40ms+，在 30fps tick(33ms) 内超时 → MissedTickBehavior::Skip
        // 使帧率塌陷、上行变稀疏突发（实测 909ms 黑洞），浏览器端表现为
        // 移动窗口卡顿/延迟打转。iMultipleThreadIdc=4 让 OpenH264 内部并行
        // 编码宏块，显著压缩单帧耗时；LOR 复杂度优先低延迟而非画质。
        param.iMultipleThreadIdc = 4;
        param.iComplexityMode = openh264_sys2::LOW_COMPLEXITY;
        param.iNumRefFrame = 1;
        // RC 跳帧: openh264 在码率预算不足时靠 drop-P 帧兑现码率。生产
        // (rc_skip=false) 关闭跳帧，防止高熵下 (几乎) 所有 P 帧被丢而
        // 黑屏; 但代价是 RC 无法控制码率(见 ParamValidation 警告——该
        // 模式下单帧冲出预算, 高熵桌面实测码率放飞 4000kbps)。实验对比
        // 见 tests 中的 probe_bitrate_rc 手工用例。
        param.bEnableFrameSkip = rc_skip;
        // 允许更高的 QP (更模糊) 而不是爆码率 — 单帧比特被量化上限约束。
        param.iMaxQp = max_qp;
        param.bPrefixNalAddingCtrl = false;
        param.bEnableDenoise = false;
        param.bEnableBackgroundDetection = false;
        param.eSpsPpsIdStrategy = 0;

        let irc = unsafe { initialize_ext(enc, &param) };
        if irc != 0 {
            unsafe { api.WelsDestroySVCEncoder(enc) };
            return Err(format!("InitializeExt failed rc={irc}"));
        }

        Ok(Self {
            api,
            enc,
            initialize_ext,
            uninitialize,
            encode_frame,
            set_option,
            get_option,
            force_intra,
            width: w,
            height: h,
            fps,
            bitrate_bps,
        })
    }

    /// Encode one I420 frame (length must match `w*h*3/2`).
    pub fn encode(&mut self, i420: &[u8]) -> Result<EncodedFrame, String> {
        let w = self.width as usize;
        let h = self.height as usize;
        let y_len = w * h;
        let uv_len = y_len / 4;
        assert!(i420.len() == y_len + 2 * uv_len, "I420 buffer length mismatch");

        // Safety: the encoder reads these planes during EncodeFrame; the slices
        // outlive the single synchronous call.
        let mut src = SSourcePicture {
            iColorFormat: videoFormatI420,
            iStride: [
                w as c_int,
                (w / 2) as c_int,
                (w / 2) as c_int,
                0,
            ],
            pData: [
                i420.as_ptr() as *mut u8,
                i420[y_len..].as_ptr() as *mut u8,
                i420[y_len + uv_len..].as_ptr() as *mut u8,
                std::ptr::null_mut(),
            ],
            iPicWidth: self.width as c_int,
            iPicHeight: self.height as c_int,
            uiTimeStamp: 0,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };

        let mut bs = SFrameBSInfo::default();
        let rc = unsafe { (self.encode_frame)(self.enc, &src, &mut bs) };
        if rc != 0 {
            return Err(format!("EncodeFrame failed rc={rc}"));
        }

        // Assemble an Annex-B byte stream from the layer buffers. OpenH264
        // emits each NAL unit already prefixed with a [00 00 00 01] / [00 00 01]
        // start code, so we concatenate them verbatim (no extra start code).
        let mut nalu: Vec<u8> = Vec::new();
        let mut sps: Option<Vec<u8>> = None;
        let mut pps: Option<Vec<u8>> = None;
        for layer in 0..bs.iLayerNum.max(0) as usize {
            let li = &bs.sLayerInfo[layer];
            let base = li.pBsBuf as *const u8;
            let mut offset: usize = 0;
            for i in 0..li.iNalCount.max(0) as usize {
                let len = unsafe { *li.pNalLengthInByte.add(i) };
                if len <= 0 || base.is_null() {
                    continue;
                }
                let na = unsafe { std::slice::from_raw_parts(base.add(offset), len as usize) };
                nalu.extend_from_slice(na);
                // NAL header sits after the start code prefix.
                let hdr = nal_header(na);
                let nal_type = hdr.map_or(na[0] & 0x1f, |h| h & 0x1f);
                match nal_type {
                    7 if sps.is_none() => sps = Some(strip_startcode(na).to_vec()),
                    8 if pps.is_none() => pps = Some(strip_startcode(na).to_vec()),
                    _ => {}
                }
                offset += len as usize;
            }
        }

        Ok(EncodedFrame {
            nalu,
            is_idr: bs.eFrameType == videoFrameTypeIDR,
            sps,
            pps,
        })
    }

    /// Dynamically update the target bitrate (bits per second).
    ///
    /// `ENCODER_OPTION_BITRATE` expects an `SBitrateInfo*` (iLayer + iBitrate),
    /// not a bare `c_int*` — passing the wrong shape makes OpenH264 read past
    /// the pointer and reject the update (observed as
    /// `SetOption():ENCODER_OPTION_BITRATE, iBitrate = <garbage>`).
    pub fn set_bitrate(&mut self, bps: u64) {
        if bps == 0 {
            return;
        }
        let mut bi = SBitrateInfo {
            iLayer: 0,
            iBitrate: bps.min(c_int::MAX as u64) as c_int,
        };
        unsafe {
            (self.set_option)(
                self.enc,
                ENCODER_OPTION_BITRATE,
                std::ptr::addr_of_mut!(bi) as *mut c_void,
            );
        }
        self.bitrate_bps = bps;
    }

    /// Read back the current target bitrate from the encoder.
    pub fn bitrate_bps(&self) -> u64 {
        let mut bi = SBitrateInfo {
            iLayer: 0,
            iBitrate: 0,
        };
        let rc = unsafe {
            (self.get_option)(
                self.enc,
                ENCODER_OPTION_BITRATE,
                std::ptr::addr_of_mut!(bi) as *mut c_void,
            )
        };
        if rc == 0 && bi.iBitrate > 0 {
            bi.iBitrate as u64
        } else {
            self.bitrate_bps
        }
    }

    /// Dynamically update the encoder's input frame-rate model
    /// (`ENCODER_OPTION_FRAME_RATE`). Used together with our own frame
    /// skipping to hold the *average* bitrate under the configured ceiling
    /// without letting the RC drop frames (which black-screens the stream).
    pub fn set_frame_rate(&mut self, fps: f32) {
        unsafe {
            (self.set_option)(
                self.enc,
                ENCODER_OPTION_FRAME_RATE,
                std::ptr::addr_of!(fps) as *mut c_void,
            );
        }
        self.fps = fps.clamp(1.0, 30.0) as f64;
    }

    /// Force the next encoded frame to be an IDR.
    pub fn force_idr(&mut self) {
        unsafe {
            (self.force_intra)(self.enc, true);
        }
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

impl Drop for H264Encoder {
    fn drop(&mut self) {
        if !self.enc.is_null() {
            unsafe {
                (self.uninitialize)(self.enc);
                use openh264_sys2::API as _;
                self.api.WelsDestroySVCEncoder(self.enc);
            }
        }
    }
}

impl crate::agent::desktop::encoder::VideoEncoder for H264Encoder {
    fn encode(&mut self, i420: &[u8]) -> Result<EncodedFrame, String> {
        H264Encoder::encode(self, i420)
    }

    fn force_idr(&mut self) {
        H264Encoder::force_idr(self);
    }

    fn set_bitrate(&mut self, bps: u64) {
        H264Encoder::set_bitrate(self, bps);
    }

    fn bitrate_bps(&self) -> u64 {
        H264Encoder::bitrate_bps(self)
    }

    fn set_frame_rate(&mut self, fps: f32) {
        H264Encoder::set_frame_rate(self, fps);
    }

    fn fps(&self) -> f64 {
        H264Encoder::fps(self)
    }

    fn width(&self) -> u32 {
        H264Encoder::width(self)
    }

    fn height(&self) -> u32 {
        H264Encoder::height(self)
    }

    fn codec(&self) -> &'static str {
        "h264"
    }

    fn mux_sample(&self, frame: &EncodedFrame) -> Option<crate::agent::desktop::encoder::VisualSample> {
        frame.sps.clone().zip(frame.pps.clone()).map(
            |(sps, pps)| crate::agent::desktop::mp4::VisualSample::H264 { sps, pps },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_i420(w: usize, h: usize, v: u8) -> Vec<u8> {
        vec![v; w * h + w * h / 2]
    }

    #[test]
    fn test_encoder_produces_annexb_avc() {
        let mut enc = H264Encoder::new(320, 240, 400_000, 15.0).expect("enc init");
        let frame = enc.encode(&solid_i420(320, 240, 128)).expect("encode");
        assert!(!frame.nalu.is_empty(), "must output bytes");
        assert_eq!(&frame.nalu[..4], &[0, 0, 0, 1], "must be Annex-B");
        // 前几帧应出现 SPS(7)/PPS(8)
        assert!(
            frame.sps.is_some() || frame.pps.is_some(),
            "initial frame should carry parameter sets"
        );
    }

    #[test]
    fn test_force_idr_returns_key_frame() {
        let mut enc = H264Encoder::new(320, 240, 400_000, 15.0).unwrap();
        // 先编码几帧 P，再 forcing IDR
        let _ = enc.encode(&solid_i420(320, 240, 10)).unwrap();
        enc.force_idr();
        let frame = enc.encode(&solid_i420(320, 240, 11)).unwrap();
        assert!(frame.is_idr, "forced frame must be IDR");
    }

    #[test]
    fn test_set_bitrate_readback() {
        // 初始化时用 max_bps（生产路径即 ABR 上限 800k），随后在范围内
        // 动态下调——OpenH264 只接受 ≤ iMaxSpatialBitrate 的目标。
        let mut enc = H264Encoder::new(320, 240, 800_000, 15.0).unwrap();
        enc.set_bitrate(500_000);
        let rb = enc.bitrate_bps();
        assert!(
            (400_000..=600_000).contains(&rb),
            "bitrate readback outside expected range: {rb}"
        );
        // 上调回上限也应被接受
        enc.set_bitrate(800_000);
        let rb = enc.bitrate_bps();
        assert!(
            (700_000..=900_000).contains(&rb),
            "bitrate readback after raise outside range: {rb}"
        );
    }

    #[test]
    fn test_encode_changes_with_input() {
        let mut enc = H264Encoder::new(320, 240, 800_000, 15.0).unwrap();
        let a = enc.encode(&solid_i420(320, 240, 0)).unwrap().nalu.len();
        // 大量纹理帧应产生更多数据（粗略 sanity）
        let mut noisy = solid_i420(320, 240, 0);
        for i in 0..noisy.len() {
            noisy[i] = (i * 7 % 251) as u8;
        }
        let b = enc.encode(&noisy).unwrap().nalu.len();
        assert!(b > 0, "noisy frame must encode");
        let _ = a;
    }
}
#[cfg(test)]
mod e2e_debug {
    use super::*;

    #[test]
    #[ignore]
    fn repro_set_bitrate_after_encode() {
        let mut enc = H264Encoder::new(320, 240, 800_000, 5.0).unwrap();
        let i420 = vec![128u8; 320 * 240 * 3 / 2];
        for i in 0..24 {
            let f = enc.encode(&i420).expect("encode");
            eprintln!("frame {i}: nalu={} is_idr={}", f.nalu.len(), f.is_idr);
            if i % 10 == 9 {
                let before = enc.bitrate_bps();
                enc.set_bitrate(500_000);
                let after = enc.bitrate_bps();
                eprintln!("  bitrate before={before} after={after}");
            }
        }
        eprintln!("DONE");
    }
}
#[cfg(test)]
mod bitrate_rc_probe {
    use super::*;
    use std::time::Instant;

    // 模拟"拖动窗口"高熵场景: 每帧全新随机纹理 + 一个移动色块。
    fn drag_frame(w: usize, h: usize, t: u32) -> Vec<u8> {
        let mut buf = vec![128u8; w * h + w * h / 2];
        let seed = t.wrapping_mul(2654435761).wrapping_add(12345);
        for i in 0..w * h {
            buf[i] = ((i as u32).wrapping_mul(31).wrapping_add(seed) >> 16) as u8;
        }
        let bx = ((t * 7) % (w as u32).saturating_sub(100)) as usize;
        let by = ((t * 13) % (h as u32).saturating_sub(100)) as usize;
        for y in by..(by + 80).min(h) {
            for x in bx..(bx + 90).min(w) {
                let i = y * w + x;
                buf[i] = 200;
                buf[w * h + (y / 2) * (w / 2) + x / 2] = 90;
            }
        }
        buf
    }

    #[test]
    #[ignore]
    fn probe_bitrate_rc() {
        let (w, h): (usize, usize) = (1280, 720);
        let combos = [
            ("skip0_qp42", false, 42), // 当前生产配置
            ("skip0_qp51", false, 51),
            ("skip1_qp51", true, 51),
            ("skip1_qp42", true, 42),
        ];
        for (name, skip, qp) in combos {
            let mut enc = H264Encoder::new_ext(w as u32, h as u32, 800_000, 30.0, skip, qp, RC_BITRATE_MODE).unwrap();
            let (mut bytes, mut out, mut empty) = (0usize, 0usize, 0usize);
            let mut max_frame = 0usize;
            let t0 = Instant::now();
            for t in 0..120 {
                let f = enc.encode(&drag_frame(w, h, t)).unwrap();
                if f.nalu.is_empty() {
                    empty += 1;
                    continue;
                }
                out += 1;
                bytes += f.nalu.len();
                max_frame = max_frame.max(f.nalu.len());
            }
            let dt = t0.elapsed().as_secs_f64();
            let kbps = bytes as f64 * 8.0 / (120.0 / 30.0) / 1000.0;
            eprintln!(
                "[{name}] skip={skip} qp={qp}: out={out}/120 empty={empty} actual={kbps:.0}kbps max_frame={:.0}KB avg={:.1}ms/f"
                ,
                max_frame as f64 / 1024.0,
                dt * 1000.0 / 120.0
            );
        }
    }

    #[test]
    #[ignore]
    fn probe_rc_real_capture() {
        // 用真实屏幕内容回放扫参数组合。前提: DISPLAY 上有一段持续动画
        // (Xvfb + 移动窗口脚本)。SR_XTEST_DISPLAY 覆盖测试用 display。
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":98".into());
        use crate::agent::desktop::capture::FrameSource as _;
        let mut src = crate::agent::desktop::capture::X11Source::open(Some(&display))
            .unwrap_or_else(|e| panic!("x11 open failed: {e}"));
        let (w, h) = src.resolution();
        let mut frames: Vec<(Vec<u8>, usize, usize)> = Vec::new();
        for _ in 0..120 {
            let fr = src.next_frame().expect("frame");
            frames.push((fr.bgra, fr.width as usize, fr.height as usize));
        }
        eprintln!("captured {} frames {}x{}", frames.len(), w, h);
        // (name, rc_skip, max_qp, rc_mode, enc_w, enc_h)
        let combos = [
            ("bitr_skip0_qp42_720p", false, 42, RC_BITRATE_MODE, 1280, 720), // 当前生产
            ("bitr_skip1_qp51_720p", true, 51, RC_BITRATE_MODE, 1280, 720),
            ("qual_skip0_qp51_720p", false, 51, RC_QUALITY_MODE, 1280, 720),
            ("qual_skip0_qp42_720p", false, 42, RC_QUALITY_MODE, 1280, 720),
            ("bitr_skip0_qp42_640x360", false, 42, RC_BITRATE_MODE, 640, 360),
            ("bitr_skip0_qp42_512x288", false, 42, RC_BITRATE_MODE, 512, 288),
        ];
        for (name, skip, qp, rc_mode, ew, eh) in combos {
            let mut enc = H264Encoder::new_ext(ew, eh, 800_000, 30.0, skip, qp, rc_mode).unwrap();
            let (mut bytes, mut out, mut empty) = (0usize, 0usize, 0usize);
            let mut max_frame = 0usize;
            let t0 = std::time::Instant::now();
            for (bgra, fw, fh) in &frames {
                let i420 = if ew as usize == *fw && eh as usize == *fh {
                    crate::agent::desktop::color::bgra_to_i420(bgra, *fw, *fh, *fw * 4)
                } else {
                    crate::agent::desktop::color::bgra_to_i420_scaled(
                        bgra, *fw, *fh, *fw * 4, ew as usize, eh as usize,
                    )
                };
                let ef = enc.encode(&i420).unwrap();
                if ef.nalu.is_empty() {
                    empty += 1;
                    continue;
                }
                out += 1;
                bytes += ef.nalu.len();
                max_frame = max_frame.max(ef.nalu.len());
            }
            let dt = t0.elapsed().as_secs_f64();
            let kbps = bytes as f64 * 8.0 / (frames.len() as f64 / 30.0) / 1000.0;
            eprintln!(
                "[{name}] skip={skip} qp={qp}: out={out}/{} empty={empty} kbps={kbps:.0} max_frame={:.0}KB avg={:.1}ms/f",
                frames.len(),
                max_frame as f64 / 1024.0,
                dt * 1000.0 / frames.len() as f64
            );
        }
    }

    #[test]
    #[ignore]
    fn probe_bitrate_low_ceiling() {
        // 200k 极限预算下 skip=true+高QP 是否仍然不出帧
        let (w, h): (usize, usize) = (1280, 720);
        for &target in &[200_000u64, 500_000] {
            let mut enc = H264Encoder::new_ext(w as u32, h as u32, target, 30.0, true, 51, RC_BITRATE_MODE).unwrap();
            let (mut out, mut empty) = (0usize, 0usize);
            let mut bytes = 0usize;
            for t in 0..120 {
                let f = enc.encode(&drag_frame(w, h, t)).unwrap();
                if f.nalu.is_empty() {
                    empty += 1;
                    continue;
                }
                out += 1;
                bytes += f.nalu.len();
            }
            let kbps = bytes as f64 * 8.0 / (120.0 / 30.0) / 1000.0;
            eprintln!(
                "[{}k skip=1 qp=51]: out={out}/120 empty={empty} actual={kbps:.0}kbps",
                target / 1000
            );
        }
    }
}

#[cfg(test)]
mod static_bitrate_probe {
    use super::*;

    #[test]
    #[ignore]
    fn probe_static_80k() {
        // 静态桌面模拟：内容几乎不动（每 3 帧小变化一次），
        // 测 openh264 在 80k/15fps 目标下的实际码率与输出。
        for &target in &[80_000u64, 150_000, 200_000, 400_000] {
            let mut enc = H264Encoder::new(1920, 1080, target, 15.0).unwrap();
            let (mut bytes, mut out) = (0usize, 0usize);
            for t in 0..90 {
                // 静态基线 + 周期性极小更新（光标/时钟）
                let mut buf = vec![128u8; 1920 * 1080 * 3 / 2];
                if t % 3 == 0 {
                    for i in (0..buf.len()).step_by(97) {
                        buf[i] = 90;
                    }
                }
                let f = enc.encode(&buf).unwrap();
                if !f.nalu.is_empty() {
                    bytes += f.nalu.len();
                    out += 1;
                }
            }
            let kbps = bytes as f64 * 8.0 / (90.0 / 15.0) / 1000.0;
            eprintln!("static @{}k: actual={kbps:.0}kbps out_frames={out}/90", target / 1000);
        }
    }
}
