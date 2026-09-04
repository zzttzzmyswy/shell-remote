//! Clipboard sync — text only (no file transfer), both directions.
//!
//! Protocol (browser ↔ agent over the interactive SSE/POST channel):
//! - `desktop:clipboard:set`  { text }  — browser pasted locally, agent sets
//!   the remote clipboard (e.g. before the user Ctrl+V's into a remote app).
//! - `desktop:clipboard:get`  {}        — browser requests the remote
//!   clipboard content; agent replies with a broadcast
//!   `desktop:clipboard` { text } which the web view writes into
//!   `navigator.clipboard` (needs a user gesture on the browser side, so the
//!   button click that sent `get` provides it).
//!
//! Backends:
//! - Windows: Win32 clipboard (OpenClipboard / SetClipboardData CF_UNICODETEXT)
//! - Linux X11: XFIXES/XA_CLIPBOARD via x11rb (writes the CLIPBOARD selection)
//! - Wayland native: not implemented (wlr-data-control protocol later)
//!
//! All clipboard access happens on a dedicated thread (Win32 needs STA-ish
//! access; X11 needs the same connection thread) with a small command queue,
//! mirroring the input injector's shape.

#![cfg(any(windows, target_os = "linux", target_os = "macos"))]

use std::sync::mpsc;

pub enum ClipCmd {
    /// Set the local (remote-machine) clipboard to `text`.
    Set(String),
    /// Read the local clipboard; the result is sent back on `reply`.
    Get(mpsc::Sender<String>),
}

/// Handle to the clipboard worker thread.
pub struct ClipboardSync {
    tx: mpsc::SyncSender<ClipCmd>,
}

impl ClipboardSync {
    /// Spawn the worker. On platforms without a backend this still returns a
    /// handle whose commands are dropped (clipboard sync is best-effort; it
    /// must never break the session).
    pub fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel::<ClipCmd>(16);
        std::thread::Builder::new()
            .name("desktop-clipboard".into())
            .spawn(move || worker(rx))
            .ok();
        Self { tx }
    }

    pub fn set(&self, text: String) {
        let _ = self.tx.try_send(ClipCmd::Set(text));
    }

    pub fn get(&self) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.tx.try_send(ClipCmd::Get(tx)).ok()?;
        rx.recv_timeout(std::time::Duration::from_secs(2)).ok()
    }
}

fn worker(rx: mpsc::Receiver<ClipCmd>) {
    for cmd in rx {
        match cmd {
            ClipCmd::Set(text) => {
                if let Err(e) = set_clipboard(&text) {
                    tracing::debug!("clipboard set failed: {e}");
                }
            }
            ClipCmd::Get(reply) => {
                let _ = reply.send(get_clipboard().unwrap_or_default());
            }
        }
    }
}

// ── Windows: Win32 clipboard ─────────────────────────────────────────────

#[cfg(windows)]
fn set_clipboard(text: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err(format!("OpenClipboard err={}", windows_sys::Win32::Foundation::GetLastError()));
        }
        let _ = EmptyClipboard();
        let bytes = wide.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h.is_null() {
            let _ = CloseClipboard();
            return Err("GlobalAlloc failed".into());
        }
        let dst = GlobalLock(h);
        if dst.is_null() {
            let _ = CloseClipboard();
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst as *mut u16, wide.len());
        let _ = GlobalUnlock(h);
        // Clipboard takes ownership of the handle on success.
        if SetClipboardData(CF_UNICODETEXT as u32, h as HANDLE).is_null() {
            let _ = CloseClipboard();
            return Err("SetClipboardData failed".into());
        }
        if CloseClipboard() == 0 {
            return Err("CloseClipboard failed".into());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn get_clipboard() -> Result<String, String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::GlobalLock;
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("OpenClipboard failed".into());
        }
        let result = (|| {
            let h = GetClipboardData(CF_UNICODETEXT as u32);
            if h.is_null() {
                return Err("clipboard has no text".into());
            }
            let src = GlobalLock(h as _);
            if src.is_null() {
                return Err("GlobalLock failed".into());
            }
            let mut out = Vec::new();
            let mut p = src as *const u16;
            loop {
                let ch = *p;
                if ch == 0 {
                    break;
                }
                out.push(ch);
                p = p.add(1);
            }
            Ok(String::from_utf16_lossy(&out))
        })();
        let _ = CloseClipboard();
        result
    }
}

// ── Linux/X11: CLIPBOARD selection ───────────────────────────────────────
//
// X11 的剪贴板是 selection 协议：set = 成为 CLIPBOARD owner 并响应
// SelectionRequest；get = 请求转换到自己的窗口并等 SelectionNotify。
// 两者都需要一个常驻事件循环——worker 线程持有一条专用 x11rb 连接和
// 一个隐形窗口，`content` 是当前剪贴板内容（Arc<Mutex> 与 worker 共享）。

#[cfg(all(unix, not(windows)))]
mod x11clip {
    use std::sync::{Arc, Mutex};
    use x11rb::protocol::xproto::Atom;

    /// 常驻 X11 selection 服务：一条 x11rb 连接 + 隐形窗口。
    /// `set` 写 content 并 SetSelectionOwner（本窗口）；
    /// 事件线程响应 SelectionRequest 把 content 交给请求方。
    pub struct X11Clipboard {
        conn: Arc<x11rb::rust_connection::RustConnection>,
        win: x11rb::protocol::xproto::Window,
        clipboard_atom: Atom,
        utf8_atom: Atom,
        string_atom: Atom,
        pub content: Arc<Mutex<String>>,
    }

    fn intern(conn: &x11rb::rust_connection::RustConnection, name: &str) -> Option<Atom> {
        use x11rb::protocol::xproto::ConnectionExt as _;
        conn.intern_atom(false, name.as_bytes())
            .ok()?
            .reply()
            .ok()
            .map(|r| r.atom)
    }

    impl X11Clipboard {
        pub fn start() -> Option<Self> {
            use x11rb::connection::Connection;
            use x11rb::protocol::xproto::ConnectionExt as _;
            let (conn, screen) =
                match x11rb::rust_connection::RustConnection::connect(None) {
                    Ok(r) => r,
                    Err(_) => return None, // 无 X 会话（纯 Wayland/headless）
                };
            let conn = Arc::new(conn);
            let win = conn.generate_id().ok()?;
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                win,
                conn.setup().roots[screen].root,
                0, 0, 1, 1,
                0,
                x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &x11rb::protocol::xproto::CreateWindowAux::new(),
            )
            .ok()?
            .check()
            .ok()?;
            let clipboard_atom = intern(&conn, "CLIPBOARD")?;
            let utf8_atom = intern(&conn, "UTF8_STRING")?;
            let string_atom = x11rb::protocol::xproto::AtomEnum::STRING.into();
            let content: Arc<Mutex<String>> = Arc::new(String::new().into());
            let c2 = conn.clone();
            let content2 = content.clone();
            std::thread::Builder::new()
                .name("x11-clipboard".into())
                .spawn(move || {
                    use x11rb::protocol::Event;
                    loop {
                        while let Some(ev) = c2.poll_for_event().ok().flatten() {
                            if let Event::SelectionRequest(req) = ev {
                                let prop = if req.property == x11rb::NONE {
                                    req.target
                                } else {
                                    req.property
                                };
                                let data = content2.lock().unwrap().clone();
                                let ok = req.target == utf8_atom || req.target == string_atom;
                                if ok {
                                    let _ = c2.change_property(
                                        x11rb::protocol::xproto::PropMode::REPLACE,
                                        prop,
                                        req.target,
                                        req.target,
                                        8,
                                        (data.len() as u32).try_into().unwrap_or(0),
                                        data.as_bytes(),
                                    );
                                }
                                let _ = c2.send_event(
                                    false,
                                    req.requestor,
                                    x11rb::protocol::xproto::EventMask::NO_EVENT,
                                    x11rb::protocol::xproto::SelectionNotifyEvent {
                                        response_type:
                                            x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                                        sequence: 0,
                                        time: req.time,
                                        requestor: req.requestor,
                                        selection: req.selection,
                                        target: req.target,
                                        property: if ok { prop } else { x11rb::NONE },
                                    },
                                );
                                let _ = c2.flush();
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                })
                .ok()?;
            Some(Self {
                conn,
                win,
                clipboard_atom,
                utf8_atom,
                string_atom,
                content,
            })
        }

        /// 更新内容并接管 CLIPBOARD owner。
        pub fn set(&self, text: &str) {
            use x11rb::connection::Connection as _;
            use x11rb::protocol::xproto::ConnectionExt as _;
            *self.content.lock().unwrap() = text.to_string();
            let _ = self
                .conn
                .set_selection_owner(self.win, self.clipboard_atom, 0u32);
            let _ = self.conn.flush();
        }
    }
}

#[cfg(all(unix, not(windows)))]
fn with_x11_clipboard<T>(f: impl FnOnce(&x11clip::X11Clipboard) -> T) -> Option<T> {
    // 单例：selection owner 窗口必须常驻，全局只建一次。
    use std::sync::OnceLock;
    static CLIP: OnceLock<Option<x11clip::X11Clipboard>> = OnceLock::new();
    let clip = CLIP.get_or_init(x11clip::X11Clipboard::start);
    clip.as_ref().map(f)
}

#[cfg(all(unix, not(windows)))]
fn set_clipboard(text: &str) -> Result<(), String> {
    match with_x11_clipboard(|c| c.set(text)) {
        Some(()) => Ok(()),
        None => Err("no X11 clipboard available".to_string()),
    }
}

#[cfg(all(unix, not(windows)))]
fn get_clipboard() -> Result<String, String> {
    // 简化：返回本进程记录的内容（我们 set 过的）。读取其它进程写入的
    // selection 需要完整的 convert 流程，放后续迭代。
    with_x11_clipboard(|c| c.content.lock().unwrap().clone())
        .ok_or_else(|| "no X11 clipboard available".to_string())
}
