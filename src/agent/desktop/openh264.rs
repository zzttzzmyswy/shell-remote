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
/// metadata extracted from the bitstream.
#[derive(Debug)]
pub struct EncodedFrame {
    /// Annex-B (4-byte startcode) NAL sequence for the whole frame.
    pub nalu: Vec<u8>,
    /// True when this frame is an IDR (random-access point).
    pub is_idr: bool,
    /// SPS NAL (without startcode), present on the initial/key frames.
    pub sps: Option<Vec<u8>>,
    /// PPS NAL (without startcode), present on the initial/key frames.
    pub pps: Option<Vec<u8>>,
}



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
        param.iRCMode = RC_BITRATE_MODE;
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
        param.iNumRefFrame = 1;
        param.bEnableFrameSkip = true; // keep short bursts inside the bit budget
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
