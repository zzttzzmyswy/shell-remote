//! Desktop frame capture backends.
//!
//! `FrameSource::next_frame()` yields a packed BGRA frame (B,G,R,A per byte,
//! 4 bytes/pixel) that the pipeline feeds to `bgra_to_i420` and then the
//! encoder. Backends:
//! - Windows: DXGI Desktop Duplication (`dxgi`, default — GPU composed
//!   surface, supports 60fps) with GDI `BitBlt` fallback (`gdi`).
//! - X11: pure-Rust `x11rb` (any Unix with an X server; works under Xvfb and
//!   through XWayland for Wayland sessions).
//! - Wayland native: xdg-desktop-portal ScreenCast → PipeWire (`wayland`,
//!   `cfg(target_os = "linux")` only).
//!
//! `open_source` resolves `auto` per-platform: Windows tries dxgi→gdi, Linux
//! tries wayland (when a portal is reachable) then X11.

use std::sync::Arc;

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
/// `dxgi`, `gdi`/`windows` or `none`; `display` optionally overrides the X11
/// display (falls back to `DISPLAY` then the X default).
///
/// Returns the source plus the **实际生效**的 backend 名（`dxgi` / `gdi` /
/// `x11` / `wayland`）——`auto` 解析后回退到的后端与请求值可能不同
/// （例如虚拟显示器上 dxgi 验证失败回退 gdi），浏览器指标面板展示
/// 的是这个真值。
pub fn open_source(
    kind: &str,
    display: Option<&str>,
) -> Result<(Box<dyn FrameSource>, String), String> {
    match kind {
        "none" => Err("desktop capture disabled".to_string()),
        "x11" => ok_backend(X11Source::open(display), "x11"),
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        "wayland" => ok_backend(crate::agent::desktop::wayland::WaylandSource::open(), "wayland"),
        #[cfg(not(all(target_os = "linux", feature = "wayland")))]
        "wayland" => Err(
            "Wayland native capture requires a Linux build with the `wayland` feature \
             (xdg-desktop-portal + PipeWire). Use an X11 session or Xvfb otherwise"
                .to_string(),
        ),
        #[cfg(windows)]
        // 显式指定时尊重用户选择（不验证首帧）
        "dxgi" => ok_backend(crate::agent::desktop::dxgi::DxgiSource::open(), "dxgi"),
        #[cfg(not(windows))]
        "dxgi" => Err("DXGI capture is Windows-only".to_string()),
        "gdi" | "windows" => open_gdi().map(|s| (s, "gdi".to_string())),
        "auto" => open_auto(display),
        other => Err(format!("unknown capture kind: {}", other)),
    }
}

fn ok_backend(
    r: Result<impl FrameSource + 'static, String>,
    name: &str,
) -> Result<(Box<dyn FrameSource>, String), String> {
    r.map(|s| (Box::new(s) as Box<dyn FrameSource>, name.to_string()))
}

/// Platform auto-detect: Windows prefers dxgi (60fps capable) then GDI;
/// Linux prefers wayland-portal when running under a Wayland session, else X11.
fn open_auto(display: Option<&str>) -> Result<(Box<dyn FrameSource>, String), String> {
    #[cfg(windows)]
    {
        match crate::agent::desktop::dxgi::DxgiSource::open_verified() {
            Ok(s) => Ok((Box::new(s) as Box<dyn FrameSource>, "dxgi".to_string())),
            Err(e) => {
                tracing::warn!("dxgi capture unavailable ({e}) — falling back to GDI");
                open_gdi().map(|s| (s, "gdi".to_string()))
            }
        }
    }
    #[cfg(not(windows))]
    {
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        let wayland_session = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
            || std::env::var("WAYLAND_DISPLAY").is_ok();
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        if wayland_session {
            match crate::agent::desktop::wayland::WaylandSource::open() {
                Ok(s) => {
                    return Ok((Box::new(s) as Box<dyn FrameSource>, "wayland".to_string()))
                }
                Err(e) => {
                    tracing::warn!("wayland portal capture unavailable ({e}) — falling back to X11/XWayland")
                }
            }
        }
        if display.is_some() || std::env::var("DISPLAY").is_ok() {
            ok_backend(X11Source::open(display), "x11")
        } else {
            Err(
                "no DISPLAY found; desktop capture requires an X11 session (Xvfb works \
                 too) or a Wayland session with xdg-desktop-portal (build with the \
                 `wayland` feature)"
                    .to_string(),
            )
        }
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

// ── 截图线程化（rustdesk capture 线程对齐） ────────────────

/// 独立抓帧线程包装：把 `FrameSource` 挪到专用线程持续抓帧，主循环用
/// [`ThreadedFrameSource::try_latest`] 非阻塞取最新帧——编码线程不再被慢
/// 抓帧（X11 GetImage / DXGI 等）阻塞，抓帧与编码并行。
///
/// - `latest` 只保留**最新**一帧（旧帧丢弃，等同"追最新帧"跳帧语义）；
/// - capture 线程持续出错时累计 `err_count` 并保存 `last_err`，供主循环
///   按 `MAX_CAPTURE_ERRORS` 终止决策；
/// - drop 时置 stop 并 join，保证线程干净退出（不再有后台孤儿线程）。
pub struct ThreadedFrameSource {
    latest: Arc<std::sync::Mutex<Option<Frame>>>,
    err_count: Arc<std::sync::atomic::AtomicU32>,
    last_err: Arc<std::sync::Mutex<Option<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    width: usize,
    height: usize,
}

impl ThreadedFrameSource {
    /// 启动抓帧线程。`inner` 被 move 进线程（不再可从外部访问）。
    pub fn spawn(mut inner: Box<dyn FrameSource>) -> Result<Self, String> {
        use std::sync::atomic::Ordering as O;
        let (width, height) = inner.resolution();
        let latest = Arc::new(std::sync::Mutex::new(None));
        let err_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let last_err = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s1, l1, e1, le1) = (stop.clone(), latest.clone(), err_count.clone(), last_err.clone());
        let builder = std::thread::Builder::new().name("desktop-capture".to_string());
        let thread = builder
            .spawn(move || {
                use std::sync::atomic::Ordering as O;
                // unwind 捕获：capture 内部若有 Rust panic（如 unwrap/越界），
                // 转成错误上报而非让整个 agent 进程闪退（panic=unwind 构建）。
                // abort 构建下无法拦截，但有 crash 日志 hook 记录现场。
                let run = || {
                    // rustdesk would-block 语义：缓存上一帧原始像素，逐字节判重，
                    // 画面未变则不发布"新帧"。静止=零发布=编码循环真正闲置
                    // （不再复用 last_frame 喂时钟空转），动态变化首帧立即唤醒。
                    let mut last_raw: Option<(usize, usize, Vec<u8>)> = None;
                    loop {
                    if s1.load(O::Relaxed) {
                        break;
                    }
                    let mut same = false; // 本拍画面未变（静止退化）
                    match inner.next_frame() {
                        Ok(f) => {
                            e1.store(0, O::Relaxed);
                            same = match &last_raw {
                                Some((lw, lh, buf)) => {
                                    (*lw == f.width && *lh == f.height && buf.as_slice() == f.bgra.as_slice())
                                }
                                None => false,
                            };
                            if same {
                                // 画面未变：不发布（等变化帧），静止零产。
                            } else {
                                last_raw = Some((f.width, f.height, f.bgra.clone()));
                                *l1.lock().unwrap() = Some(f);
                            }
                        }
                        Err(e) => {
                            e1.fetch_add(1, O::Relaxed);
                            *le1.lock().unwrap() = Some(e);
                        }
                    }
                    // 防空转（对齐 rustdesk 静止停抓）：
                    // - 动态（本拍画面变化）：让出 CPU 即可，快源（测试 mock）全速产帧；
                    // - 静止（本拍画面未变）：退避 sleep 缩短抓帧频率——X 往返/DXGI
                    //   acquire 在此刻只是白耗 CPU，静置桌面 agent 应空载（R3 乙85）。
                    //   上限 ~100ms ≈ 10fps 轮询：内容一旦变化下一拍立即恢复全速
                    //   （waiting-on-change 抓帧语义，R3 乙57/82）。
                    if same {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    } else {
                        std::thread::yield_now();
                    }
                    }
                };
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                    // 置 u32::MAX 触发 pipeline 的 MAX_CAPTURE_ERRORS 终止并回传。
                    e1.store(u32::MAX, O::Relaxed);
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "capture thread panicked (unknown payload)".to_string());
                    *le1.lock().unwrap() = Some(format!("capture thread panic: {msg}"));
                }
            })
            .map_err(|e| format!("spawn capture thread: {e}"))?;
        Ok(Self {
            latest,
            err_count,
            last_err,
            stop,
            thread: Some(thread),
            width,
            height,
        })
    }

    /// 取走当前最新帧；尚无新帧时返回 `None`（主循环跳帧，不阻塞）。
    pub fn try_latest(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    /// capture 线程累计的连续失败次数（成功后清零）。
    pub fn err_count(&self) -> u32 {
        use std::sync::atomic::Ordering as O;
        self.err_count.load(O::Relaxed)
    }

    /// 最近一次抓帧错误（用于报错回传浏览器）。
    pub fn last_err(&self) -> Option<String> {
        self.last_err.lock().unwrap().clone()
    }

    /// 捕获源初始分辨率（抓帧线程内 self-inner 自身会随 display 变更更新
    /// 帧尺寸，主循环用帧尺寸检测变更）。
    pub fn resolution(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}

impl Drop for ThreadedFrameSource {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering as O;
        self.stop.store(true, O::Relaxed);
        if let Some(t) = self.thread.take() {
            // join 等线程退出；真实后端最多阻塞一个抓帧周期（DXGI 静止
            // 200ms timeout / X11 一次往返），毫秒级可接受。
            let _ = t.join();
        }
    }
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
///
/// Fast path: when the X server exposes the MIT-SHM extension we attach a
/// System V shared-memory segment and use `ShmGetImage` (rustdesk
/// `x11/capturer.rs`): the server writes pixels directly into our memory,
/// avoiding a full-frame round-trip through the X protocol on every capture.
/// Falls back to plain `GetImage` if SHM init fails (no extension / server
/// refuses the attach).
pub struct X11Source {
    conn: x11rb::rust_connection::RustConnection,
    screen_num: usize,
    root: x11rb::protocol::xproto::Drawable,
    width: u16,
    height: u16,
    depth: u8,
    /// Lazily-initialised XComposite backend used when root `GetImage` fails.
    composite: Option<x11rb::protocol::xproto::Pixmap>,
    /// MIT-SHM fast-path state (segment + x11rb shm seg id). `None` = SHM
    /// unavailable (not tried yet, or init failed) — capture falls back.
    shm: Option<ShmState>,
    /// SHM init already failed on this geometry — don't retry every frame.
    /// Reset when the display geometry changes (segment would be stale).
    shm_failed: bool,
    /// 每 N 帧重查一次 root geometry（display 分辨率运行时变更检测，
    /// rustdesk `Rect`/HotPlug 对齐）。X setup 缓存不反映 xrandr 变更。
    recheck: u32,
    /// 是否已注册 XRANDR ScreenChangeNotify（R5#131 分辨率事件驱动）。
    /// 注册后 `next_frame` 用 `poll_for_event` 读事件——只有真变更才触发
    /// 重建，替代固定 30 帧一次的 `get_geometry` 轮询往返。
    randr_registered: bool,
}

/// MIT-SHM segment: SysV shared memory mapped into our address space and
/// attached to the X server under an x11rb `shm::Seg` id.
///
/// `ptr` aliases a SysV mapping that outlives this struct only while the
/// struct lives (dropped via `shmdt`); X writes into it on `ShmGetImage`.
/// Access is guarded: capture reads it only after the `ShmGetImage` reply
/// has been waited on, so no explicit sync fence is needed between X and us.
struct ShmState {
    shmid: i32,
    ptr: *mut u8,
    size: usize,
    seg: x11rb::protocol::shm::Seg,
    width: u16,
    height: u16,
}

// SAFETY: the raw pointer is only ever dereferenced from the owning thread
// (capture loop) after a `ShmGetImage` reply, so it is safe to consider the
// struct Send within this single-threaded `ThreadedFrameSource` usage.
unsafe impl Send for ShmState {}

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
        let mut src = Self {
            conn,
            screen_num,
            root,
            width,
            height,
            depth,
            composite: None,
            shm: None,
            shm_failed: false,
            recheck: 0,
            randr_registered: false,
        };
        // 注册 XRANDR ScreenChangeNotify（R5#131）：分辨率事件驱动替代轮询。
        // 失败静默——事件驱动是优化，无扩展时回落现有 30 帧轮询。
        use x11rb::protocol::randr::{ConnectionExt as _, NotifyMask};
        match src
            .conn
            .randr_select_input(src.root, NotifyMask::SCREEN_CHANGE)
        {
            Ok(cookie) => match cookie.check() {
                Ok(()) => {
                    src.randr_registered = true;
                }
                Err(e) => {
                    tracing::debug!(%e, "randr select_input failed — falling back to geometry polling");
                }
            },
            Err(e) => {
                tracing::debug!(%e, "randr select_input cookie failed — falling back to geometry polling");
            }
        }
        Ok(src)
    }

    /// Try to initialise the MIT-SHM fast path. Quietly falls back to
    /// `GetImage` on any failure (no extension, or the server refuses to
    /// attach our SysV segment) — SHM is an optimisation, not a requirement.
    fn try_init_shm(&mut self) -> Result<(), String> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::shm::ConnectionExt as _;

        if self.shm.is_some() {
            return Ok(());
        }
        // Query the extension version — if the server has no MIT-SHM, shmget
        // attach would just hang the capture; fail fast instead.
        self.conn
            .shm_query_version()
            .map_err(|e| format!("shm_query_version cookie: {e}"))?
            .reply()
            .map_err(|e| format!("shm_query_version reply: {e}"))?;
        let w = self.width as usize;
        let h = self.height as usize;
        let size = w * h * 4; // BGRA 32bpp
        // shmget with IPC_PRIVATE: server writes into this segment.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return Err(format!("shmget failed: {}", std::io::Error::last_os_error()));
        }
        // Attach in our address space.
        let ptr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if ptr as isize == -1 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(format!("shmat failed: {err}"));
        }
        // Register the segment with the X server.
        let seg = self.conn.generate_id().map_err(|e| format!("generate shm id: {e}"))?;
        self.conn
            .shm_attach(seg, shmid as u32, false)
            .map_err(|e| {
                unsafe { libc::shmdt(ptr) };
                unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
                format!("shm_attach cookie: {e}")
            })?
            .check()
            .map_err(|e| {
                unsafe { libc::shmdt(ptr) };
                unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
                format!("shm_attach reply: {e}")
            })?;
        self.shm = Some(ShmState {
            shmid,
            ptr: ptr as *mut u8,
            size,
            seg,
            width: self.width,
            height: self.height,
        });
        tracing::debug!("X11 MIT-SHM fast path enabled ({w}x{h} @ {size} bytes)");
        Ok(())
    }

    /// Drop the SHM segment (server detach + sysv detach/remove). Called on
    /// Drop and before re-init after a display geometry change.
    fn teardown_shm(&mut self) {
        use x11rb::protocol::shm::ConnectionExt as _;
        if let Some(s) = self.shm.take() {
            let _ = self.conn.shm_detach(s.seg);
            unsafe { libc::shmdt(s.ptr as *const _ as *mut _) };
            unsafe { libc::shmctl(s.shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        }
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
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};

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
        Self::bgra_from_pixels(&reply.data, depth, self.width as usize, self.height as usize, &self.conn)
    }

    /// `ShmGetImage` on the *current root* — the server writes the pixels
    /// straight into our attached SysV segment (no X protocol frame payload),
    /// then we copy the segment into a fresh BGRA buffer for the encoder.
    /// This is the rustdesk `x11/capturer.rs` fast path.
    fn capture_shm(&self) -> Result<Vec<u8>, String> {
        use x11rb::protocol::shm::ConnectionExt as _;
        use x11rb::protocol::xproto::ImageFormat;

        let s = self
            .shm
            .as_ref()
            .ok_or_else(|| "capture_shm called without SHM state".to_string())?;
        let w = self.width as u16;
        let h = self.height as u16;
        // Wait for the reply: after it lands the segment holds the frame.
        self.conn
            .shm_get_image(self.root, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), s.seg, 0)
            .map_err(|e| format!("shm_get_image cookie: {e}"))?
            .reply()
            .map_err(|e| format!("shm_get_image reply: {e}"))?;
        // SAFETY: `s.ptr` points at our own SysV segment (`shmat`), which stays
        // mapped until `teardown_shm`/`Drop`; the `ShmGetImage` reply above
        // guarantees the server has finished writing `size` bytes.
        let data = unsafe { std::slice::from_raw_parts(s.ptr, s.size) };
        Self::bgra_from_pixels(data, self.depth, self.width as usize, self.height as usize, &self.conn)
    }

    /// Normalize a raw X11 pixel buffer (depth 24/32, server byte order) into
    /// packed BGRA rows (B,G,R,A bytes), shared by the `GetImage` and
    /// MIT-SHM paths so both produce byte-identical output.
    ///
    /// `data` is the pixel payload in the server's native byte order
    /// (x11rb `Image::new` handles byte-order/scanline details), read from a
    /// `GetImage` reply or a MIT-SHM segment.
    fn bgra_from_pixels(
        data: &[u8],
        depth: u8,
        w: usize,
        h: usize,
        conn: &x11rb::rust_connection::RustConnection,
    ) -> Result<Vec<u8>, String> {
        use x11rb::connection::Connection as _;
        use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
        use std::borrow::Cow;

        // depth 30 (10-bit per channel) is NOT byte-addressable BGRA: treating
        // it as 32bpp would silently produce wrong colors. Fail loudly instead.
        // 8/16-bit depths are likewise unsupported for the packed-BGRA contract.
        if depth != 24 && depth != 32 {
            return Err(format!(
                "unsupported X11 root depth {depth}: only 24/32-bit (packed BGRA) captures are supported"
            ));
        }
        let bpp = Self::bpp(depth);
        let byte_order = ImageOrder::try_from(conn.setup().image_byte_order)
            .unwrap_or(ImageOrder::LsbFirst);
        let image = Image::new(
            w as u16,
            h as u16,
            ScanlinePad::Pad32,
            depth,
            bpp,
            byte_order,
            Cow::Borrowed(data),
        )
        .map_err(|e| format!("Image::new: {e}"))?;

        // Normalize to the server's native layout so bytes are BGRA (X pixmap
        // byte order is preserved; on little-endian hosts B is the lowest byte).
        let native = image
            .native(&conn.setup())
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
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        // 分辨率变更检测（R5#131 事件驱动）：已注册 XRANDR ScreenChangeNotify
        // 时用 poll_for_event 读事件——只有真变更才重建，替代固定 30 帧一次的
        // get_geometry 轮询往返。无事件/未注册时回落轮询（保持旧行为）。
        let mut resized = false;
        if self.randr_registered {
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(ev)) => {
                        if let Ok(x11rb::protocol::Event::RandrScreenChangeNotify(randr_ev)) =
                            x11rb::protocol::Event::try_from(ev)
                        {
                            let width = randr_ev.width;
                            let height = randr_ev.height;
                            if width > 0 && height > 0 {
                                if self.width != width || self.height != height {
                                    resized = true;
                                }
                                self.width = width;
                                self.height = height;
                            }
                        }
                    }
                    Ok(None) => break, // 无更多事件
                    Err(e) => {
                        tracing::debug!(%e, "poll_for_event failed — falling back to geometry polling");
                        self.randr_registered = false;
                        break;
                    }
                }
            }
        } else {
            self.recheck += 1;
            if self.recheck >= 30 {
                self.recheck = 0;
                if let Ok(cookie) = self.conn.get_geometry(self.root) {
                    if let Ok(geo) = cookie.reply() {
                        if geo.width > 0 && geo.height > 0 {
                            if self.width != geo.width || self.height != geo.height {
                                resized = true;
                            }
                            self.width = geo.width;
                            self.height = geo.height;
                        }
                    }
                }
            }
        }
        if resized {
            self.teardown_shm();
            self.shm_failed = false;
        }
        // 首次（或分辨率变更后）尝试开启 MIT-SHM 快路径；失败则标记失败
        // 不再每帧重试（回落 GetImage）。
        if self.shm.is_none() && !self.shm_failed {
            if let Err(e) = self.try_init_shm() {
                tracing::debug!(%e, "X11 MIT-SHM unavailable, using GetImage");
                self.shm_failed = true;
            }
        }
        let has_shm = self.shm.as_ref().map(|s| {
            s.width == self.width && s.height == self.height && s.size >= (self.width as usize * self.height as usize * 4)
        });
        let bgra = match has_shm {
            Some(true) => match self.capture_shm() {
                Ok(b) => b,
                // SHM 读取失败（极端竞态/服务器限制）：回退 GetImage 保活
                Err(shm_err) => {
                    tracing::debug!(%shm_err, "X11 SHM capture failed, falling back to GetImage");
                    self.teardown_shm();
                    self.shm = None;
                    self.shm_failed = true;
                    match self.capture_impl(self.root) {
                        Ok(b) => b,
                        Err(root_err) => {
                            return self.err_composite(shm_err, root_err);
                        }
                    }
                }
            },
            _ => match self.capture_impl(self.root) {
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
            },
        };
        Ok(Frame {
            bgra,
            width: self.width as usize,
            height: self.height as usize,
        })
    }
}

impl Drop for X11Source {
    fn drop(&mut self) {
        self.teardown_shm();
    }
}

impl X11Source {
    /// Compose the composite-fallback error (shared by the shm-fallback path).
    fn err_composite(&mut self, shm_err: String, root_err: String) -> Result<Frame, String> {
        match self.composite_capture() {
            Ok(b) => Ok(Frame {
                bgra: b,
                width: self.width as usize,
                height: self.height as usize,
            }),
            Err(comp_err) => Err(format!(
                "X11 capture failed: ShmGetImage — {shm_err}; GetImage on root — {root_err}; \
                 XComposite fallback — {comp_err}. If this is a Wayland session (XWayland), \
                 the X11 root has no image data — the native Wayland desktop cannot be \
                 captured over X11; run the agent in a real X11 session (Xorg/Xvfb) instead"
            )),
        }
    }

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
        /// Cached screen DC (see `blit`): per-frame GetDC is 3-4x slower and
        /// starves high-fps capture; the cache is dropped on BitBlt failure
        /// via `rebuild`.
        screen_dc: HDC,
        mem_dc: HDC,
        bmp: HBITMAP,
    }

    unsafe impl Send for GdiSource {}

    impl GdiSource {
        pub fn open() -> Result<Self, String> {
            let mut s = Self {
                width: 0,
                height: 0,
                screen_dc: std::ptr::null_mut(),
                mem_dc: std::ptr::null_mut(),
                bmp: std::ptr::null_mut(),
            };
            unsafe { s.rebuild()? };
            Ok(s)
        }

        /// (Re)create the compatible DC + bitmap sized to the *current*
        /// screen, and refresh the cached resolution. Called at open and
        /// again to self-heal when a capture fails (e.g. display mode
        /// changed or a screen-DC handle went stale).
        unsafe fn rebuild(&mut self) -> Result<(), String> {
            if !self.bmp.is_null() {
                DeleteObject(self.bmp);
                self.bmp = std::ptr::null_mut();
            }
            if !self.mem_dc.is_null() {
                DeleteDC(self.mem_dc);
                self.mem_dc = std::ptr::null_mut();
            }
            if !self.screen_dc.is_null() {
                ReleaseDC(std::ptr::null_mut(), self.screen_dc);
                self.screen_dc = std::ptr::null_mut();
            }
            let width = GetSystemMetrics(SM_CXSCREEN) as usize;
            let height = GetSystemMetrics(SM_CYSCREEN) as usize;
            let screen_dc = GetDC(std::ptr::null_mut());
            if screen_dc.is_null() {
                return Err(format!("GetDC failed (err={})", GetLastError()));
            }
            let mem_dc = CreateCompatibleDC(screen_dc);
            let bmp = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
            let err = GetLastError();
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            if mem_dc.is_null() || bmp.is_null() {
                return Err(format!("GDI init failed (err={err})"));
            }
            SelectObject(mem_dc, bmp);
            self.width = width;
            self.height = height;
            self.mem_dc = mem_dc;
            self.bmp = bmp;
            Ok(())
        }

        /// Copy the current desktop into the compatible bitmap.
        ///
        /// The screen DC is cached between frames for speed (GetDC/ReleaseDC
        /// per frame costs 3-4x on high fps and starved 25-30fps capture on
        /// real hardware). A cached DC does go stale when the session/desktop
        /// changes (lock screen, UAC secure desktop, mode switch, screen
        /// off) — BitBlt then fails with `ERROR_INVALID_HANDLE` (err=6) —
        /// so on failure we rebuild the whole GDI context (fresh screen DC
        /// + size) once and retry; transient switches self-heal instead of
        /// freezing the feed.
        unsafe fn blit(&mut self) -> Result<(), String> {
            if self.screen_dc.is_null() {
                self.screen_dc = GetDC(std::ptr::null_mut());
                if self.screen_dc.is_null() {
                    return Err(format!("GetDC failed (err={})", GetLastError()));
                }
            }
            let w = self.width as i32;
            let h = self.height as i32;
            let ok = BitBlt(self.mem_dc, 0, 0, w, h, self.screen_dc, 0, 0, SRCCOPY);
            if ok != 0 {
                return Ok(());
            }
            let err1 = GetLastError();
            // stale DC — rebuild everything (drops the cached screen_dc too)
            if let Err(e) = self.rebuild() {
                return Err(format!("BitBlt failed (err={err1}); rebuild failed: {e}"));
            }
            let w = self.width as i32;
            let h = self.height as i32;
            let ok = BitBlt(self.mem_dc, 0, 0, w, h, self.screen_dc, 0, 0, SRCCOPY);
            if ok == 0 {
                return Err(format!("BitBlt failed (err={})", GetLastError()));
            }
            Ok(())
        }

        unsafe fn read_pixels(&self) -> Result<Vec<u8>, String> {
            let w = self.width as i32;
            let h = self.height as i32;
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
            Ok(pixels)
        }
    }

    impl FrameSource for GdiSource {
        fn resolution(&self) -> (usize, usize) {
            (self.width, self.height)
        }

        fn next_frame(&mut self) -> Result<Frame, String> {
            unsafe {
                // display 分辨率运行时变更检测：GetSystemMetrics 极廉价，每帧
                // 对比一次；变化则重建整个 GDI context（rustdesk display
                // 变更对齐）。rebuild 内部会重新 GetSystemMetrics + 建
                // screen DC / compatible bitmap，BitBlt 下一帧即新尺寸。
                let sw = GetSystemMetrics(SM_CXSCREEN) as usize;
                let sh = GetSystemMetrics(SM_CYSCREEN) as usize;
                if sw > 0 && sh > 0 && (sw != self.width || sh != self.height) {
                    tracing::info!(
                        "gdi display resize: {}x{} -> {}x{}",
                        self.width, self.height, sw, sh
                    );
                    self.rebuild()?;
                }
                self.blit()?;
                let h = self.height;
                let (w, h) = (self.width, h);
                let bgra = self.read_pixels()?;
                Ok(Frame {
                    bgra,
                    width: w,
                    height: h,
                })
            }
        }
    }

    impl Drop for GdiSource {
        fn drop(&mut self) {
            unsafe {
                if !self.bmp.is_null() {
                    DeleteObject(self.bmp);
                }
                if !self.mem_dc.is_null() {
                    DeleteDC(self.mem_dc);
                }
                if !self.screen_dc.is_null() {
                    ReleaseDC(std::ptr::null_mut(), self.screen_dc);
                }
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
    fn test_wayland_reports_actionable_error() {
        // 未启用 wayland feature 的构建必须给出可操作错误（而不是 panic）。
        match open_source("wayland", None) {
            Ok(_) => {
                // 启用了 feature 且环境恰好可用的构建：允许成功。
            }
            Err(e) => {
                assert!(
                    e.contains("wayland") || e.contains("portal") || e.contains("Wayland"),
                    "err: {e}"
                );
            }
        }
    }

    /// X11 SHM fast path smoke test against a real X server (Xvfb :99).
    /// Verifies: SHM init succeeds, the first frame is captured via the
    /// segment, and multiple frames agree on size — plus drop() releases the
    /// SysV segment without leaking it (would show up as shmseg reuse errors).
    /// Skipped when no display is reachable: reads SR_XTEST_DISPLAY or :99.
    #[test]
    fn test_x11_shm_capture_on_xvfb() {
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
        // First frame drives try_init_shm.
        let fr = src.next_frame().expect("frame");
        assert_eq!(fr.bgra.len(), w * h * 4);
        // On servers with MIT-SHM the fast path must be active after this.
        if src.shm.is_some() {
            assert_eq!(src.shm.as_ref().unwrap().size, w * h * 4);
            // Second frame also via SHM, same size.
            let fr2 = src.next_frame().expect("frame 2");
            assert_eq!(fr2.bgra.len(), w * h * 4);
        } else {
            eprintln!(
                "display {display} has no MIT-SHM support (or attach refused); \
                 GetImage fallback tested instead"
            );
        }
        // Drop must not leak: dropping the segment, a fresh X11Source to the
        // same display still captures (exercises segment id reuse + cleanup).
        // Xvfb aggressively resets a connection that closes then re-opens
        // immediately (os error 104 noise); retry briefly rather than flake.
        drop(src);
        let mut src2 = None;
        for attempt in 0..5 {
            match X11Source::open(Some(&display)) {
                Ok(s) => {
                    src2 = Some(s);
                    break;
                }
                Err(e) => {
                    if attempt == 4 {
                        panic!("reopen after drop, final attempt failed: {e}");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        let mut src2 = src2.unwrap();
        let _ = src2.next_frame().expect("frame after reopen");
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

    /// #131 分辨率事件驱动：支持 RANDR 的服务器上应成功注册
    /// ScreenChangeNotify（`randr_registered=true`）——事件驱动替代轮询。
    /// 无扩展时允许注册失败（回落轮询），但必须不破坏首帧捕获。
    #[test]
    fn test_x11_randr_event_driven_resolution() {
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
        // Xvfb :96 带 RANDR → 应注册成功；无扩展时允许失败但帧仍可捕获。
        if src.randr_registered {
            eprintln!("XRANDR ScreenChangeNotify registered (event-driven resolution)");
        } else {
            eprintln!("XRANDR unavailable — falling back to geometry polling (allowed)");
        }
        // 事件驱动路径必须不破坏捕获：首帧尺寸与分辨率一致。
        let (w, h) = src.resolution();
        let fr = src.next_frame().expect("frame with randr registered");
        assert_eq!(fr.bgra.len(), w * h * 4);
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

    /// 快速产帧源：每帧内容含递增序号（验证"取到最新帧"）。
    struct CounterSource {
        w: usize,
        h: usize,
        t: usize,
    }

    impl FrameSource for CounterSource {
        fn next_frame(&mut self) -> Result<Frame, String> {
            self.t += 1;
            let v = self.t as u8;
            Ok(Frame {
                bgra: vec![v; self.w * self.h * 4],
                width: self.w,
                height: self.h,
            })
        }
        fn resolution(&self) -> (usize, usize) {
            (self.w, self.h)
        }
    }

    #[test]
    fn test_threaded_source_yields_latest_frame() {
        let src: Box<dyn FrameSource> = Box::new(CounterSource { w: 16, h: 16, t: 0 });
        let ts = ThreadedFrameSource::spawn(src).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut frame = None;
        while std::time::Instant::now() < deadline {
            if let Some(f) = ts.try_latest() {
                frame = Some(f);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let f = frame.expect("threaded source must produce a frame within 2s");
        assert_eq!((f.width, f.height), (16, 16));
        assert_eq!(f.bgra.len(), 16 * 16 * 4);
        // 持续产帧：再取几帧都应成功
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut got = 0;
        while std::time::Instant::now() < deadline && got < 3 {
            if ts.try_latest().is_some() {
                got += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(got >= 3, "must keep yielding frames, got {got}");
    }

    /// 静止退避（R3 乙57/82 对齐）：画面恒定的源 → 抓帧线程应退避 sleep 控制
    /// 产帧频率（≤10fps 轮询），而不是全速空转。验证产帧被限频而非死锁：
    /// 静止源在 1.2s 窗口内产出的可取帧数应明显少于全速源。
    #[test]
    fn test_threaded_static_source_backs_off() {
        use std::sync::atomic::Ordering as O;
        // 恒定画面源：每帧字节完全相同，would-block 判 `same` → 退避 sleep。
        struct StaticSource {
            w: usize,
            h: usize,
        }
        impl FrameSource for StaticSource {
            fn next_frame(&mut self) -> Result<Frame, String> {
                Ok(Frame {
                    bgra: vec![7u8; self.w * self.h * 4],
                    width: self.w,
                    height: self.h,
                })
            }
            fn resolution(&self) -> (usize, usize) {
                (self.w, self.h)
            }
        }
        let src: Box<dyn FrameSource> = Box::new(StaticSource { w: 8, h: 8 });
        let ts = ThreadedFrameSource::spawn(src).unwrap();
        // 必须等到首帧：第一帧 same=false 发布，之后全部 same → 退避。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut first = None;
        while std::time::Instant::now() < deadline {
            if let Some(f) = ts.try_latest() {
                first = Some(f);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(first.is_some(), "static source must start producing");
        // 消费掉首帧后，统计后续 1.2s 内到达的帧数——退避 100ms 应 ≤ ~12 帧
        //（10fps 轮询），若仍然全速（无限产）则测试失败（回归不要退避）。
        let mut got = 0u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
        while std::time::Instant::now() < deadline {
            if ts.try_latest().is_some() {
                got += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // 上限：10fps × 1.2s ≈ 12 帧 + 调度余量；远小于全速（≥100 帧）。
        assert!(
            got <= 20,
            "static capture must back off (10fps poll) instead of spinning: {got} frames in 1.2s"
        );
        drop(ts);
    }

    #[test]
    fn test_threaded_source_drop_stops_thread() {
        let src: Box<dyn FrameSource> = Box::new(CounterSource { w: 8, h: 8, t: 0 });
        let ts = ThreadedFrameSource::spawn(src).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(ts.try_latest().is_some(), "capture thread must run before drop");
        // drop 触发 stop + join，不应 hang。
        drop(ts);
    }

    #[test]
    fn test_threaded_source_reports_errors() {
        struct Fail;
        impl FrameSource for Fail {
            fn next_frame(&mut self) -> Result<Frame, String> {
                Err("boom".into())
            }
            fn resolution(&self) -> (usize, usize) {
                (4, 4)
            }
        }
        let ts = ThreadedFrameSource::spawn(Box::new(Fail)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && ts.err_count() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(ts.err_count() > 0, "errors must accumulate in the thread");
        assert!(ts.try_latest().is_none(), "no frame when the source always fails");
        assert_eq!(ts.last_err().as_deref(), Some("boom"));
    }
}

