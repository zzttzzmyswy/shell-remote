//! Desktop video sharing (capture → H.264 encode → fMP4 stream).
//!
//! Modules:
//! - `color`: BGRA→I420 conversion
//! - `rate`: adaptive bitrate controller (200–800 kbps)
//! - `openh264`: H.264 software encoder wrapper
//! - `mp4`: fMP4 muxer (init segment + fragments) for browser MSE playback
//! - `capture`: desktop frame capture backends (X11 / Windows GDI)
//!
//! Encode pipeline: FrameSource → bgra_to_i420 → H264Encoder → mp4 mux → relay.

pub mod color;

// The remaining modules are added by later tasks:
pub mod rate;
// pub mod openh264;
// pub mod mp4;
// pub mod capture;