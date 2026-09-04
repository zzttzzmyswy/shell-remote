//! Codec-agnostic video encoder abstraction for the desktop pipeline.
//!
//! Both `openh264::H264Encoder` and `vpx::Vp9Encoder` implement
//! [`VideoEncoder`] so `run_desktop_pipeline` can be written once. The
//! returned [`EncodedFrame`] carries codec-specific parameter sets (SPS/PPS
//! for H.264; profile/level for VP9 via [`VideoEncoder::mux_sample`]).

/// One encoded picture. `nalu` is Annex-B for H.264, a raw VP9 frame for VP9.
/// `sps`/`pps` (H.264) present on IDR frames.
#[derive(Debug)]
pub struct EncodedFrame {
    pub nalu: Vec<u8>,
    /// True when this frame is a random-access point (IDR / VP9 key frame).
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
    /// `h264` | `vp9`.
    fn codec(&self) -> &'static str;
    /// Produce the MP4 sample description (codec parameter set) used to build
    /// the init segment. H.264 needs SPS/PPS from an IDR frame; VP9 carries
    /// profile/level statically.
    fn mux_sample(&self, frame: &EncodedFrame) -> Option<VisualSample>;
}

/// Construct an encoder for a codec name (`av1` / `vp9` / `h264`).
pub fn new_encoder(
    codec: &str,
    w: u32,
    h: u32,
    max_bps: u64,
    fps: f64,
) -> Result<Box<dyn VideoEncoder>, String> {
    match codec.to_ascii_lowercase().as_str() {
        "h264" => crate::agent::desktop::openh264::H264Encoder::new(w, h, max_bps, fps).map(|e| Box::new(e) as Box<dyn VideoEncoder>),
        #[cfg(feature = "vp9")]
        "vp9" => crate::agent::desktop::vpx::Vp9Encoder::new(w, h, max_bps, fps).map(|e| Box::new(e) as Box<dyn VideoEncoder>),
        #[cfg(feature = "av1")]
        "av1" => crate::agent::desktop::aom::AomEncoder::new(w, h, max_bps, fps).map(|e| Box::new(e) as Box<dyn VideoEncoder>),
        other => Err(format!("unsupported desktop codec: {other}")),
    }
}