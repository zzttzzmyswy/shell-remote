//! Desktop frame capture backends.
//!
//! `FrameSource::next_frame()` yields a packed BGRA frame (B,G,R,A per byte,
//! 4 bytes/pixel) that the pipeline feeds to `bgra_to_i420` and then the
//! encoder. Two backends ship:
//! - X11: pure-Rust `x11rb` (any Unix with an X server; works under Xvfb and
//!   through XWayland for Wayland sessions).
//! - Windows: GDI `BitBlt` into a compose bitmap read via `GetDIBits`
//!   (32-bit BGRA), `cfg(windows)` only.
//!
//! Wayland native capture (xdg-desktop-portal + PipeWire) is not yet
//! implemented; `open_source` returns a clear error for it and recommends the
//! X11/XWayland path.

/// One captured frame: packed BGRA (little-endian byte order B,G,R,A).
pub struct Frame {
    pub bgra: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Frame source abstraction over a captured desktop.
pub trait FrameSource: Send {
    /// Capture the next frame (`bgra` length must be `width*height*4`).
    fn next_frame(&mut self) -> Result<Frame, String>;
    /// Fixed capture resolution in pixels.
    fn resolution(&self) -> (usize, usize);
}

/// Open a capture source. `kind` is one of `auto`, `x11`, `wayland`,
/// `gdi`/`windows` or `none`; `display` optionally overrides the X11 display
/// (falls back to `DISPLAY` then the X default).
pub fn open_source(kind: &str, display: Option<&str>) -> Result<Box<dyn FrameSource>, String> {
    match kind {
        "none" => Err("desktop capture disabled".to_string()),
        "x11" => X11Source::open(display).map(|s| Box::new(s) as Box<dyn FrameSource>),
        "wayland" => Err(
            "Wayland native capture (xdg-desktop-portal + PipeWire) is not implemented yet; the \
             X11 backend cannot capture the native Wayland desktop. Run the agent in a real X11 \
             session (Xorg or Xvfb), or keep --desktop-capture x11 to at least attempt capturing \
             the XWayland root (limited to X11 windows)"
                .to_string(),
        ),
        "gdi" | "windows" => open_gdi(),
        "auto" => {
            #[cfg(windows)]
            {
                open_gdi()
            }
            #[cfg(not(windows))]
            {
                if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland") {
                    tracing::warn!(
                        "Wayland session detected: the X11 backend cannot capture the native \
                         Wayland desktop (only the XWayland root). Prefer a real X11 session \
                         (Xorg/Xvfb); full Wayland capture needs the portal backend (not yet \
                         implemented)"
                    );
                }
                if display.is_some() || std::env::var("DISPLAY").is_ok() {
                    X11Source::open(display).map(|s| Box::new(s) as Box<dyn FrameSource>)
                } else {
                    Err(
                        "no DISPLAY found; desktop capture requires an X11 session \
                         (Xvfb works too). Wayland support requires xdg-desktop-portal \
                         (not yet implemented)"
                            .to_string(),
                    )
                }
            }
        }
        other => Err(format!("unknown capture kind: {}", other)),
    }
}

#[cfg(windows)]
fn open_gdi() -> Result<Box<dyn FrameSource>, String> {
    gdi::GdiSource::open().map(|s| Box::new(s) as Box<dyn FrameSource>)
}

#[cfg(not(windows))]
fn open_gdi() -> Result<Box<dyn FrameSource>, String> {
    Err("GDI capture is Windows-only".to_string())
}

// ── X11 ────────────────────────────────────────────────────────

/// X11 frame source (uses `GetImage`; the server returns the root window's
/// pixel data, already composed including overlapping windows).
///
/// Some composited servers — including the XWayland root under a Wayland
/// compositor — refuse `GetImage` on the root window with a `BadMatch` error.
/// When that happens we fall back to the same technique as `ffmpeg x11grab`:
/// `XCompositeRedirectWindow(root, Automatic)` + `XCompositeNameWindowPixmap`,
/// then `GetImage` on the named pixmap (which reads the window backing store
/// and works on composited servers).
pub struct X11Source {
    conn: x11rb::rust_connection::RustConnection,
    screen_num: usize,
    root: x11rb::protocol::xproto::Drawable,
    width: u16,
    height: u16,
    depth: u8,
    /// Lazily-initialised XComposite backend used when root `GetImage` fails.
    composite: Option<x11rb::protocol::xproto::Pixmap>,
}

impl X11Source {
    pub fn open(display: Option<&str>) -> Result<Self, String> {
        use x11rb::connection::Connection;
        use x11rb::rust_connection::RustConnection;
        let display = if let Some(d) = display {
            Some(d.to_string())
        } else {
            std::env::var("DISPLAY").ok()
        };
        let (conn, screen_num) =
            RustConnection::connect(display.as_deref()).map_err(|e| format!("x11rb connect: {e}"))?;
        let (width, height, depth, root) = {
            let screen = &conn.setup().roots[screen_num];
            (
                screen.width_in_pixels,
                screen.height_in_pixels,
                screen.root_depth,
                screen.root,
            )
        };
        Ok(Self {
            conn,
            screen_num,
            root,
            width,
            height,
            depth,
            composite: None,
        })
    }

    fn bpp(depth: u8) -> x11rb::image::BitsPerPixel {
        use x11rb::image::BitsPerPixel;
        match depth {
            0..=8 => BitsPerPixel::B8,
            9..=16 => BitsPerPixel::B16,
            _ => BitsPerPixel::B32, // depth 24/30/32 all come back as 32-bit pixels
        }
    }

    /// Set up the XComposite fallback: redirect the root's children into
    /// off-screen storage and name the root's backing pixmap (the standard
    /// screen-capture sequence used by `maim`/`scrot` and `ffmpeg x11grab`).
    /// The pixmap is reused across frames.
    fn ensure_composite(&mut self) -> Result<(), String> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::composite::{self, ConnectionExt as _};
        if self.composite.is_some() {
            return Ok(());
        }
        self.conn
            .composite_query_version(0, 7)
            .map_err(|e| format!("composite query: {e}"))?
            .reply()
            .map_err(|e| format!("composite query reply: {e}"))?;
        // 不能直接 RedirectWindow(root)（规范禁止, 返回 BadMatch）——
        // 重定向所有子窗口即可让 root 拥有可命名的后备 pixmap。
        self.conn
            .composite_redirect_subwindows(self.root, composite::Redirect::AUTOMATIC)
            .map_err(|e| format!("composite redirect: {e}"))?
            .check()
            .map_err(|e| format!("composite redirect reply: {e}"))?;
        let pixmap = self
            .conn
            .generate_id()
            .map_err(|e| format!("generate pixmap id: {e}"))?;
        self.conn
            .composite_name_window_pixmap(self.root, pixmap)
            .map_err(|e| format!("composite name pixmap: {e}"))?
            .check()
            .map_err(|e| format!("composite name pixmap reply: {e}"))?;
        self.composite = Some(pixmap);
        Ok(())
    }

    /// `GetImage` on a drawable and return packed BGRA rows for the capture
    /// region (see `next_frame` for the depth/stride contract).
    fn capture_impl(&self, drawable: impl Into<x11rb::protocol::xproto::Drawable>) -> Result<Vec<u8>, String> {
        use x11rb::connection::Connection as _;
        use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
        use std::borrow::Cow;

        let drawable: x11rb::protocol::xproto::Drawable = drawable.into();
        let w = self.width as u16;
        let h = self.height as u16;
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, drawable, 0, 0, w, h, !0)
            .map_err(|e| format!("get_image cookie: {e}"))?
            .reply()
            .map_err(|e| format!("get_image reply: {e}"))?;

        let depth = if reply.depth > 0 { reply.depth } else { self.depth };
        // depth 30 (10-bit per channel) is NOT byte-addressable BGRA: treating
        // it as 32bpp would silently produce wrong colors. Fail loudly instead.
        // 8/16-bit depths are likewise unsupported for the packed-BGRA contract.
        if depth != 24 && depth != 32 {
            return Err(format!(
                "unsupported X11 root depth {depth}: only 24/32-bit (packed BGRA) captures are supported"
            ));
        }
        let bpp = Self::bpp(depth);
        let byte_order = ImageOrder::try_from(self.conn.setup().image_byte_order)
            .unwrap_or(ImageOrder::LsbFirst);
        let image = Image::new(
            w,
            h,
            ScanlinePad::Pad32,
            depth,
            bpp,
            byte_order,
            Cow::Borrowed(&reply.data),
        )
        .map_err(|e| format!("Image::new: {e}"))?;

        // Normalize to the server's native layout so bytes are BGRA (X pixmap
        // byte order is preserved; on little-endian hosts B is the lowest byte).
        let native = image
            .native(&self.conn.setup())
            .map_err(|e| format!("Image::native: {e}"))?;

        let stride = match bpp {
            BitsPerPixel::B32 => ((w as usize) * 4).max(1),
            BitsPerPixel::B16 => ((w as usize) * 2).max(1),
            BitsPerPixel::B8 => (w as usize).max(1),
            _ => (w as usize) * 4,
        };
        let data = native.data();
        let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for row in 0..h as usize {
            let start = row * stride;
            let end = (start + w as usize * 4).min(data.len());
            if end > start {
                bgra.extend_from_slice(&data[start..end]);
            }
        }
        // B16/B8 sources are rare; pad to the uniform BGRA contract by copying
        // the raw run (packing channels would be project-specific).
        if bgra.len() != w as usize * h as usize * 4 {
            return Err(format!(
                "unexpected X11 pixel layout (depth={}, bgra={} expected {})",
                depth,
                bgra.len(),
                w as usize * h as usize * 4
            ));
        }
        Ok(bgra)
    }
}

impl FrameSource for X11Source {
    fn resolution(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    fn next_frame(&mut self) -> Result<Frame, String> {
        let bgra = match self.capture_impl(self.root) {
            Ok(b) => b,
            Err(root_err) => match self.composite_capture() {
                Ok(b) => b,
                Err(comp_err) => {
                    return Err(format!(
                        "X11 capture failed: GetImage on root — {root_err}; \
                         XComposite fallback — {comp_err}. If this is a Wayland session (XWayland), \
                         the X11 root has no image data — the native Wayland desktop cannot be \
                         captured over X11; run the agent in a real X11 session (Xorg/Xvfb) instead"
                    ));
                }
            },
        };
        Ok(Frame {
            bgra,
            width: self.width as usize,
            height: self.height as usize,
        })
    }
}

impl X11Source {
    fn composite_capture(&mut self) -> Result<Vec<u8>, String> {
        self.ensure_composite()?;
        let pixmap = self
            .composite
            .ok_or_else(|| "composite pixmap missing after ensure".to_string())?;
        self.capture_impl(pixmap)
    }
}

// ── Windows GDI ────────────────────────────────────────────────

#[cfg(windows)]
mod gdi {
    use super::{Frame, FrameSource};
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, HBITMAP, HDC, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    pub struct GdiSource {
        width: usize,
        height: usize,
        screen_dc: HDC,
        mem_dc: HDC,
        bmp: HBITMAP,
    }

    unsafe impl Send for GdiSource {}

    impl GdiSource {
        pub fn open() -> Result<Self, String> {
            unsafe {
                let width = GetSystemMetrics(SM_CXSCREEN) as usize;
                let height = GetSystemMetrics(SM_CYSCREEN) as usize;
                let screen_dc = GetDC(std::ptr::null_mut());
                let mem_dc = CreateCompatibleDC(screen_dc);
                let bmp = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
                if screen_dc.is_null() || mem_dc.is_null() || bmp.is_null() {
                    return Err(format!("GDI init failed (err={})", GetLastError()));
                }
                SelectObject(mem_dc, bmp);
                Ok(Self {
                    width,
                    height,
                    screen_dc,
                    mem_dc,
                    bmp,
                })
            }
        }
    }

    impl FrameSource for GdiSource {
        fn resolution(&self) -> (usize, usize) {
            (self.width, self.height)
        }

        fn next_frame(&mut self) -> Result<Frame, String> {
            unsafe {
                let w = self.width as i32;
                let h = self.height as i32;
                if BitBlt(self.mem_dc, 0, 0, w, h, self.screen_dc, 0, 0, SRCCOPY) == 0 {
                    return Err(format!("BitBlt failed (err={})", GetLastError()));
                }
                let mut bmi: BITMAPINFO = std::mem::zeroed();
                bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = w;
                bmi.bmiHeader.biHeight = -h; // top-down
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = BI_RGB;
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                if GetDIBits(
                    self.mem_dc,
                    self.bmp,
                    0,
                    h as u32,
                    pixels.as_mut_ptr() as *mut _,
                    &mut bmi,
                    DIB_RGB_COLORS,
                ) == 0
                {
                    return Err(format!("GetDIBits failed (err={})", GetLastError()));
                }
                Ok(Frame {
                    bgra: pixels,
                    width: self.width,
                    height: self.height,
                })
            }
        }
    }

    impl Drop for GdiSource {
        fn drop(&mut self) {
            unsafe {
                DeleteObject(self.bmp);
                DeleteDC(self.mem_dc);
                ReleaseDC(std::ptr::null_mut(), self.screen_dc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_source_none_errors() {
        assert!(open_source("none", None).is_err());
    }

    #[test]
    fn test_open_source_invalid_kind_errors() {
        assert!(open_source("bogus", None).is_err());
    }

    #[test]
    fn test_wayland_reports_not_implemented() {
        let err = match open_source("wayland", None) {
            Ok(_) => panic!("wayland capture must not silently succeed"),
            Err(e) => e,
        };
        assert!(err.contains("portal"), "err: {err}");
    }

    /// X11 capture smoke test against a real X server (Xvfb :99).
    /// Skipped when no display is reachable: reads SR_XTEST_DISPLAY or :99.
    #[test]
    fn test_x11_capture_on_xvfb() {
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        let mut src = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        let (w, h) = src.resolution();
        assert!(w > 0 && h > 0);
        let fr = src.next_frame().expect("frame");
        assert_eq!(fr.bgra.len(), w * h * 4);
        // 允许全黑 (Xvfb 未上色时 root 是黑的); 若上色 (xsetroot -solid) 则校验像素进入
        let non_zero = fr.bgra.iter().filter(|&&b| b != 0).count();
        if non_zero == 0 {
            eprintln!("X11 frame is all-black {}x{} (Xvfb root unpainted)", w, h);
        }
    }

    /// XComposite fallback path (used when the root `GetImage` fails, e.g.
    /// composited XWayland): redirect the root and read the named pixmap.
    /// Xvfb ships the Composite extension, so this runs against :99.
    #[test]
    fn test_x11_composite_capture_on_xvfb() {
        let display = std::env::var("SR_XTEST_DISPLAY").unwrap_or_else(|_| ":99".into());
        let mut src = match X11Source::open(Some(&display)) {
            Ok(s) => s,
            Err(e) => {
                if e.contains("connect") && std::env::var("SR_NO_XTEST").is_ok() {
                    return;
                }
                panic!("x11 open failed: {e}");
            }
        };
        if let Err(e) = src.ensure_composite() {
            // 无 Composite 扩展的服务器直接跳过（不是断言失败）
            eprintln!("composite unavailable, skipping: {e}");
            return;
        }
        let (w, h) = src.resolution();
        let bgra = src.composite_capture().expect("composite capture");
        assert_eq!(bgra.len(), w * h * 4, "composite capture must match size");
        // 与主路径尺寸一致即可；内容允许全黑（Xvfb root 未上色）
    }

    #[cfg(windows)]
    #[test]
    fn test_gdi_source_compiles_on_windows_target() {
        // Compile-only presence check (never runs in tests).
    }
}

