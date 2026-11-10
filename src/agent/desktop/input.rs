//! Desktop input injection — mouse + keyboard events from the browser.
//!
//! The web view captures pointer/keyboard events on the `<video>` element,
//! ships them as `desktop:mouse` / `desktop:key` JSON messages through the
//! relay, and the agent replays them on the real desktop:
//!
//! - Windows: `SendInput` (via enigo's win backend)
//! - Linux X11/XWayland: XTEST (via enigo's x11rb backend)
//! - Wayland native / macOS: not implemented yet (portal / CGEvent later)
//!
//! Injection runs on a dedicated thread with a single serial consumer,
//! mirroring RustDesk's `input_service` QUEUE pattern, so mouse moves and key
//! presses replay in order and never race each other.
//!
//! Queue policy (MYS-2765 #11): **释放事件（MouseUp/KeyUp）与按键、点击、
//! 滚轮一律不得丢弃**——丢一个 KeyUp 远端就永久卡住那个键（按 Ctrl 切走、
//! 远端 Ctrl 一直按着）。可丢的只有连续的 `MouseMove`，而且用"合并到最新
//! 坐标"代替丢弃：发送侧最多只有一个排队中的 move，消费时取最新坐标。
//! 因此队列实际只会被人有速率的事件填充，用无界 channel 是安全的。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// One injected input event, already normalized by [`parse_mouse`] /
/// [`parse_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Absolute pointer position in desktop pixels **of the captured
    /// monitor**（原点偏移由 [`apply_origin`] 在入队前加上）。
    MouseMove { x: i32, y: i32 },
    MouseDown { button: u8 },
    MouseUp { button: u8 },
    /// Scroll delta in "clicks" (positive = away from user / up).
    Scroll { dx: i32, dy: i32 },
    KeyDown { code: String },
    KeyUp { code: String },
}

impl InputEvent {
    /// 释放类事件（MouseUp/KeyUp）：绝不能因队列拥塞丢弃——丢一个 KeyUp
    /// 远端那个键就永久按住。
    pub fn is_release(&self) -> bool {
        matches!(self, InputEvent::MouseUp { .. } | InputEvent::KeyUp { .. })
    }

    /// 连续鼠标移动：唯一可以合并/丢弃的事件类型。
    pub fn is_move(&self) -> bool {
        matches!(self, InputEvent::MouseMove { .. })
    }
}

/// 单次滚轮事件的最大格数（防客户端异常值把注入线程卡在 `enigo.scroll`
/// 的逐格循环里——enigo 内部按 `0..length.abs()` 逐格发 X button click，
/// 而 `i32::MIN.abs()` 在 debug 下 panic、release 下得到负数）。
pub const SCROLL_MAX_CLICKS: i32 = 32;

/// 把 `Scroll` 的增量夹到 `[-SCROLL_MAX_CLICKS, SCROLL_MAX_CLICKS]`：
/// 既挡住 `i32::MIN` 的 `abs()` 溢出，也把异常大值（一次滚 10 万格会让
/// 注入线程卡几十秒）压成正常的滚轮格数。
pub fn clamp_scroll(dx: i32, dy: i32) -> (i32, i32) {
    fn one(v: i32) -> i32 {
        v.clamp(-SCROLL_MAX_CLICKS, SCROLL_MAX_CLICKS)
    }
    (one(dx), one(dy))
}

/// i64 → i32 饱和转换：browser 发来的坐标越界时钳到 i32 边界，而不是让
/// `as i32` 静默截断成任意（可能为负的）值。
fn clamp_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// 给鼠标移动加上所选显示器的原点偏移（MYS-2765 #10）。
///
/// 浏览器发来的是**采集画面内**的坐标（0..capture_w）；注入必须落在
/// 虚拟桌面坐标系里，非主屏（如 X11 第二块屏 CRTC `+1280+0`、Windows
/// `DISPLAY2`）要把该屏原点加上，否则点击会落到主屏左上角。
/// 非 move 事件原样返回（按钮/滚轮/按键不带坐标）。
pub fn apply_origin(ev: InputEvent, origin: (i32, i32)) -> InputEvent {
    match ev {
        InputEvent::MouseMove { x, y } => InputEvent::MouseMove {
            x: x.saturating_add(origin.0),
            y: y.saturating_add(origin.1),
        },
        other => other,
    }
}

/// Normalize a browser `mousedown`/`mouseup` `button` field (0=left,
/// 1=middle, 2=right) to the platform-agnostic button byte used here.
fn norm_button(b: u8) -> u8 {
    match b {
        1 => 1, // middle
        2 => 2, // right
        _ => 0, // left (and anything unexpected)
    }
}

/// Parse a `desktop:mouse` message payload into an injectable event.
/// `x`/`y` are desktop-pixel coordinates (the web side already scaled them).
pub fn parse_mouse(p: &serde_json::Value) -> Option<InputEvent> {
    let typ = p.get("type")?.as_str()?;
    match typ {
        "move" => Some(InputEvent::MouseMove {
            x: clamp_i32(p.get("x")?.as_i64()?),
            y: clamp_i32(p.get("y")?.as_i64()?),
        }),
        "down" => Some(InputEvent::MouseDown {
            button: norm_button(p.get("button")?.as_u64()? as u8),
        }),
        "up" => Some(InputEvent::MouseUp {
            button: norm_button(p.get("button")?.as_u64()? as u8),
        }),
        "wheel" => {
            // 滚轮增量在**解析**时就夹紧（不只是注入时）：后端是逐格发
            // X button click / WHEEL_DELTA，一次 10 万格能卡死注入线程几十秒，
            // `i32::MIN` 还会让 enigo 内部 `abs()` 溢出（MYS-2765 #11）。
            let (dx, dy) = clamp_scroll(
                clamp_i32(p.get("dx").and_then(|v| v.as_i64()).unwrap_or(0)),
                clamp_i32(p.get("dy")?.as_i64()?),
            );
            Some(InputEvent::Scroll { dx, dy })
        }
        _ => None,
    }
}

/// Parse a `desktop:key` message payload. `code` is a browser `KeyboardEvent.code`
/// value ("KeyA", "ShiftLeft", "ArrowUp", ...) mapped per-platform at injection.
pub fn parse_key(p: &serde_json::Value) -> Option<InputEvent> {
    let code = p.get("code")?.as_str()?.to_string();
    let down = p.get("down")?.as_bool()?;
    if down {
        Some(InputEvent::KeyDown { code })
    } else {
        Some(InputEvent::KeyUp { code })
    }
}

/// Map a browser `KeyboardEvent.code` to an enigo `Key`.
/// 字母/数字用 `Unicode(char)`（enigo 0.6 的 Layout 键以字符表达——
/// X11 侧转 keysym、Windows 侧转 VK，均按 US 布局解释）。
fn key_from_code(code: &str) -> Option<enigo::Key> {
    use enigo::Key;
    Some(match code {
        // Letters & digits: KeyA..KeyZ / Digit0..Digit9 (browser naming)
        c if (c.len() == 4 && c.starts_with("Key") && c.as_bytes()[3].is_ascii_alphabetic()) => {
            Key::Unicode(c[3..].to_lowercase().chars().next()?)
        }
        c if (c.len() == 6 && c.starts_with("Digit") && c.as_bytes()[5].is_ascii_digit()) => {
            Key::Unicode(c[5..].chars().next()?)
        }
        "Space" => Key::Space,
        "Enter" | "NumpadEnter" => Key::Return,
        "Backspace" => Key::Backspace,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Delete" => Key::Delete,
        "Insert" => Key::Insert,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "ShiftLeft" | "ShiftRight" => Key::Shift,
        "ControlLeft" | "ControlRight" => Key::Control,
        "AltLeft" | "AltRight" => Key::Alt,
        "MetaLeft" | "MetaRight" => Key::Meta,
        "CapsLock" => Key::CapsLock,
        // ScrollLock 在 enigo 是平台条件变体（win 叫 Scroll），跨平台直接
        // 用 Unicode 符号键会错位 —— 暂不映射（极少用于远程操作）。
        "Pause" => Key::Pause,
        "F1" => Key::F1, "F2" => Key::F2, "F3" => Key::F3, "F4" => Key::F4,
        "F5" => Key::F5, "F6" => Key::F6, "F7" => Key::F7, "F8" => Key::F8,
        "F9" => Key::F9, "F10" => Key::F10, "F11" => Key::F11, "F12" => Key::F12,
        // Punctuation (US layout codes)
        "Backquote" => Key::Unicode('`'),
        "Minus" => Key::Unicode('-'),
        "Equal" => Key::Unicode('='),
        "BracketLeft" => Key::Unicode('['),
        "BracketRight" => Key::Unicode(']'),
        "Backslash" => Key::Unicode('\\'),
        "Semicolon" => Key::Unicode(';'),
        "Quote" => Key::Unicode('\''),
        "Comma" => Key::Unicode(','),
        "Period" => Key::Unicode('.'),
        "Slash" => Key::Unicode('/'),
        "Numpad0" => Key::Unicode('0'), "Numpad1" => Key::Unicode('1'),
        "Numpad2" => Key::Unicode('2'), "Numpad3" => Key::Unicode('3'),
        "Numpad4" => Key::Unicode('4'), "Numpad5" => Key::Unicode('5'),
        "Numpad6" => Key::Unicode('6'), "Numpad7" => Key::Unicode('7'),
        "Numpad8" => Key::Unicode('8'), "Numpad9" => Key::Unicode('9'),
        "NumpadAdd" => Key::Add, "NumpadSubtract" => Key::Subtract,
        "NumpadMultiply" => Key::Multiply, "NumpadDivide" => Key::Divide,
        "NumpadDecimal" => Key::Decimal,
        _ => return None,
    })
}

/// The serial injection worker handle. The platform backend (enigo) is
/// `!Send` on some platforms, so it only ever lives on the injection thread.
pub struct InputInjector {
    /// 事件/信号通道。**只有**本结构体持有发送端——drop 掉它就等于告诉
    /// worker "没有更多输入了"，worker 排空后退出（测试与 stop 都依赖这点）。
    tx: mpsc::Sender<QueuedInput>,
    shared: Arc<QueueShared>,
}

impl InputInjector {
    /// Spawn the injection thread. Returns immediately; the enigo backend is
    /// created lazily on the thread and failures surface as log lines (the
    /// stream keeps playing — losing input injection must not kill video).
    pub fn start() -> Self {
        let (inj, shared) = Self::with_queue();
        std::thread::Builder::new()
            .name("desktop-input".into())
            .spawn(move || {
                let backend = enigo::Enigo::new(&enigo::Settings::default())
                    .map(|e| Box::new(EnigoBackend { enigo: e }) as Box<dyn InjectBackend>)
                    .map_err(|e| format!("desktop input backend unavailable: {e}"));
                drive_worker(backend, &shared);
            })
            .expect("spawn desktop-input thread");
        inj
    }

    /// 建队列 + 返回可发送的句柄（线程由 [`Self::start`] 或测试自行驱动）。
    fn with_queue() -> (Self, Arc<QueueShared>) {
        let (tx, rx) = mpsc::channel::<QueuedInput>();
        let shared = Arc::new(QueueShared {
            pending_move: Mutex::new(None),
            move_signalled: AtomicBool::new(false),
            rx: Mutex::new(Some(rx)),
            closed: AtomicBool::new(false),
        });
        (
            Self {
                tx,
                shared: shared.clone(),
            },
            shared,
        )
    }

    /// Queue one event.
    ///
    /// 鼠标移动合并到最新坐标；按下/抬起/滚轮/按键逐个入队，**不会因队列
    /// 拥塞被丢弃**（MYS-2765 #11——丢一个 KeyUp 远端那个键就永久按住）。
    pub fn send(&self, ev: InputEvent) {
        self.shared.push(&self.tx, ev);
    }

    /// 注入线程是否还在跑。worker 退出（或后端创建失败）后为 false，调用方
    /// 据此重建 injector 而不是把后续输入交给一个死线程。
    pub fn is_alive(&self) -> bool {
        !self.shared.closed.load(Ordering::SeqCst)
    }
}

/// 注入队列里的一条消息。
#[derive(Debug)]
enum QueuedInput {
    /// "把当前最新的鼠标移动注入进去"——坐标放在共享槽 `pending_move` 里，
    /// 发送侧覆盖式写入，所以队列里最多只有一个这样的信号。
    LatestMove,
    /// 离散事件（按下/抬起/滚轮/按键）：按 FIFO 逐个投递。
    Event(InputEvent),
}

/// 队列的消费侧状态（发送侧只借用 [`InputInjector::tx`]）。
struct QueueShared {
    /// 待注入的最新鼠标移动（发送侧覆盖写）。
    pending_move: Mutex<Option<InputEvent>>,
    /// 已有一个 `LatestMove` 信号在队列里（尚未被消费），无需重复发。
    move_signalled: AtomicBool,
    /// worker 端接收器（只被取出一次）。
    rx: Mutex<Option<mpsc::Receiver<QueuedInput>>>,
    /// worker 已退出（后端不可用 / 线程结束）。
    closed: AtomicBool,
}

impl QueueShared {
    /// 发送侧入口。
    ///
    /// 浏览器 mousemove 可达 ~120Hz，而注入是串行消费；积压的旧坐标毫无价值
    /// （远端桌面本来就跟不上），所以 move 只留最新一个。其余事件全部入队。
    /// channel 无界 + 这些事件都是人有速率产生的，不会把内存吃满。
    fn push(&self, tx: &mpsc::Sender<QueuedInput>, ev: InputEvent) {
        if ev.is_move() {
            *self.pending_move.lock().unwrap() = Some(ev);
            if !self.move_signalled.swap(true, Ordering::SeqCst) {
                // 无界 channel：仅在 worker 已退出时才失败（此时队列关闭）。
                let _ = tx.send(QueuedInput::LatestMove);
            }
            return;
        }
        if tx.send(QueuedInput::Event(ev.clone())).is_err() {
            // 只有 worker 已退出才会走到这里（无界 channel 不会满）。释放事件
            // 丢了最致命（远端按键卡住），单独 warn 以便日志里能定位。
            if ev.is_release() {
                tracing::warn!("desktop input worker gone — release event {ev:?} was NOT injected");
            } else {
                tracing::debug!("desktop input worker gone — event dropped");
            }
        }
    }

    /// 消费侧取下一条要注入的事件（`None` = 队列关闭，worker 退出）。
    ///
    /// `LatestMove` 是"信号"而不是事件：信号与坐标槽是分开写的，取信号时
    /// 槽可能已被更新的 move 覆盖又被另一条信号带走（并发发送侧），此时
    /// 没有坐标可注入——**继续等下一条消息**，不能返回 `None`（那会被当成
    /// "队列关闭"让 worker 退出）。
    fn next(&self, rx: &mpsc::Receiver<QueuedInput>) -> Option<InputEvent> {
        loop {
            match rx.recv() {
                Ok(QueuedInput::LatestMove) => {
                    self.move_signalled.store(false, Ordering::SeqCst);
                    if let Some(ev) = self.pending_move.lock().unwrap().take() {
                        return Some(ev);
                    }
                    // 槽已被另一次唤醒取空：继续等（下面还有信号或事件）。
                }
                Ok(QueuedInput::Event(ev)) => return Some(ev),
                Err(_) => return None,
            }
        }
    }

    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// 注入后端抽象：真实实现是 enigo（平台相关，`!Send`，只能在注入线程里
/// 活着）；单测用记录型实现验证"哪些事件真的被投递了"。
trait InjectBackend {
    fn inject(&mut self, ev: &InputEvent) -> Result<(), String>;
}

/// enigo 后端（X11 XTEST / Windows SendInput）。
struct EnigoBackend {
    enigo: enigo::Enigo,
}

impl InjectBackend for EnigoBackend {
    fn inject(&mut self, ev: &InputEvent) -> Result<(), String> {
        use enigo::{Axis, Coordinate, Direction, Keyboard, Mouse};
        let r: Result<(), Box<dyn std::error::Error>> = (|| {
            match ev {
                InputEvent::MouseMove { x, y } => {
                    self.enigo.move_mouse(*x, *y, Coordinate::Abs)?
                }
                InputEvent::MouseDown { button } => {
                    self.enigo.button(btn(*button).ok_or("button")?, Direction::Press)?
                }
                InputEvent::MouseUp { button } => {
                    self.enigo.button(btn(*button).ok_or("button")?, Direction::Release)?
                }
                InputEvent::Scroll { dx, dy } => {
                    // dy/dx 语义: 正=向下/向右（与浏览器一致; enigo 的
                    // scroll 单位即"格"——win 侧 1 格=WHEEL_DELTA，X11 侧
                    // 一次 button click, 无需再取负号）。
                    // 增量先夹到 ±SCROLL_MAX_CLICKS：enigo 内部对每次 scroll
                    // 做 `0..length.abs()` 逐格循环，`i32::MIN` 会让它
                    // panic/空转，异常大值会把注入线程卡死（MYS-2765 #11）。
                    let (dx, dy) = clamp_scroll(*dx, *dy);
                    if dy != 0 {
                        self.enigo.scroll(dy, Axis::Vertical)?;
                    }
                    if dx != 0 {
                        self.enigo.scroll(dx, Axis::Horizontal)?;
                    }
                }
                InputEvent::KeyDown { code } => {
                    if let Some(k) = key_from_code(code) {
                        self.enigo.key(k, Direction::Press)?;
                    }
                }
                InputEvent::KeyUp { code } => {
                    if let Some(k) = key_from_code(code) {
                        self.enigo.key(k, Direction::Release)?;
                    }
                }
            }
            Ok(())
        })();
        r.map_err(|e| e.to_string())
    }
}

/// 注入线程主体：逐事件注入，**单事件失败/panic 互相隔离**——一次注入错误
/// 不能吞掉后续事件（尤其 KeyUp），否则远端照样会卡键。
fn drive_worker(backend: Result<Box<dyn InjectBackend>, String>, shared: &QueueShared) {
    let mut backend = match backend {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("{e}");
            // 后端不可用：标记关闭，让 `is_alive()` 为假，调用方可重建。
            shared.mark_closed();
            return;
        }
    };
    let rx = match shared.rx.lock().unwrap().take() {
        Some(rx) => rx,
        None => return,
    };
    while let Some(ev) = shared.next(&rx) {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| backend.inject(&ev)));
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!("input inject failed: {e}"),
            Err(_) => tracing::warn!("input inject panicked (event skipped, worker continues)"),
        }
    }
    shared.mark_closed();
}

fn btn(b: u8) -> Option<enigo::Button> {
    match b {
        0 => Some(enigo::Button::Left),
        1 => Some(enigo::Button::Middle),
        2 => Some(enigo::Button::Right),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_mouse_events() {
        assert!(matches!(
            parse_mouse(&json!({"type":"move","x":100,"y":200})),
            Some(InputEvent::MouseMove { x: 100, y: 200 })
        ));
        assert!(matches!(
            parse_mouse(&json!({"type":"down","button":2})),
            Some(InputEvent::MouseDown { button: 2 })
        ));
        // 浏览器 middle=1 right=2 须归一化保留
        assert!(matches!(
            parse_mouse(&json!({"type":"down","button":1})),
            Some(InputEvent::MouseDown { button: 1 })
        ));
        assert!(matches!(
            parse_mouse(&json!({"type":"wheel","dx":0,"dy":-3})),
            Some(InputEvent::Scroll { dx: 0, dy: -3 })
        ));
        assert!(parse_mouse(&json!({"type":"bogus"})).is_none());
        assert!(parse_mouse(&json!({"type":"move","x":"NaN"})).is_none());
    }

    /// #11 滚轮增量必须夹紧：`i32::MIN` 会让 enigo 内部 `abs()` 溢出，
    /// 异常大值会让逐格循环卡死注入线程。
    #[test]
    fn test_scroll_is_clamped() {
        assert_eq!(clamp_scroll(0, 3), (0, 3), "正常增量不改");
        assert_eq!(clamp_scroll(-3, 5), (-3, 5));
        assert_eq!(
            clamp_scroll(i32::MIN, i32::MAX),
            (-SCROLL_MAX_CLICKS, SCROLL_MAX_CLICKS),
            "极值必须夹紧（abs() 溢出隐患）"
        );
        assert_eq!(clamp_scroll(1_000_000, -1_000_000), (SCROLL_MAX_CLICKS, -SCROLL_MAX_CLICKS));
        // parse 路径同样夹紧（浏览器可以发任意大数）。
        match parse_mouse(&json!({"type":"wheel","dy":9_000_000_000i64})) {
            Some(InputEvent::Scroll { dy, .. }) => {
                assert_eq!(dy, SCROLL_MAX_CLICKS, "超大滚轮增量必须夹到上限")
            }
            other => panic!("expected scroll, got {other:?}"),
        }
        // 坐标同样不得因 `as i32` 静默回绕。
        match parse_mouse(&json!({"type":"move","x":i64::MAX,"y":i64::MIN})) {
            Some(InputEvent::MouseMove { x, y }) => {
                assert_eq!((x, y), (i32::MAX, i32::MIN));
            }
            other => panic!("expected move, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_key_events() {
        assert!(matches!(
            parse_key(&json!({"code":"KeyA","down":true})),
            Some(InputEvent::KeyDown { code }) if code == "KeyA"
        ));
        assert!(matches!(
            parse_key(&json!({"code":"Enter","down":false})),
            Some(InputEvent::KeyUp { code }) if code == "Enter"
        ));
        assert!(parse_key(&json!({"code":"KeyA"})).is_none()); // 缺 down
        assert!(parse_key(&json!({"down":true})).is_none()); // 缺 code
    }

    #[test]
    fn test_key_map_covers_common_codes() {
        for code in [
            "KeyA", "KeyZ", "Digit0", "Digit9", "Space", "Enter", "Backspace",
            "Tab", "Escape", "Delete", "ArrowUp", "ArrowLeft", "ShiftLeft",
            "ControlRight", "AltLeft", "MetaLeft", "F1", "F12", "Comma", "Slash",
            "Numpad5", "NumpadEnter",
        ] {
            assert!(key_from_code(code).is_some(), "{code} must map");
        }
        assert!(key_from_code("NotAKey").is_none());
    }

    /// 记录型后端：把收到的每个事件存起来，供断言"哪些事件真的被投递"。
    #[derive(Default)]
    struct RecordingBackend {
        seen: Arc<Mutex<Vec<InputEvent>>>,
    }

    impl InjectBackend for RecordingBackend {
        fn inject(&mut self, ev: &InputEvent) -> Result<(), String> {
            self.seen.lock().unwrap().push(ev.clone());
            Ok(())
        }
    }

    /// 在测试线程里同步驱动注入（避开真实 enigo 后端）。发送端已 drop 时
    /// `drive_worker` 处理完队列即返回。
    fn drain_sync(queue: &Arc<QueueShared>, backend: Box<dyn InjectBackend>) {
        drive_worker(Ok(backend), queue);
    }

    /// 队列里塞爆大量鼠标移动，随后的 KeyUp/MouseUp 仍然必须被投递
    /// （MYS-2765 #11：旧实现队满 `try_send` 丢**新**事件，释放事件会被丢，
    /// 远端按键/鼠标卡住）。
    #[test]
    fn test_release_events_survive_backlog() {
        let (inj, queue) = InputInjector::with_queue();
        let seen: Arc<Mutex<Vec<InputEvent>>> = Default::default();
        // 先灌 10k 次移动：合并语义下只留最新一个，队列不会"满"。
        for i in 0..10_000 {
            inj.send(InputEvent::MouseMove { x: i, y: i });
        }
        inj.send(InputEvent::KeyDown { code: "ControlLeft".into() });
        inj.send(InputEvent::KeyUp { code: "ControlLeft".into() });
        inj.send(InputEvent::MouseUp { button: 0 });
        // 关掉发送端让 worker 在处理完后退出。
        drop(inj);
        drain_sync(&queue, Box::new(RecordingBackend { seen: seen.clone() }));

        let got = seen.lock().unwrap().clone();
        // 移动合并成一条（最新坐标），释放事件一个不少且保持顺序。
        let moves = got.iter().filter(|e| e.is_move()).count();
        assert_eq!(moves, 1, "连续 move 必须合并成一条，实际 {got:?}");
        assert_eq!(
            got.last(),
            Some(&InputEvent::MouseUp { button: 0 }),
            "MouseUp 必须投递且排在最后: {got:?}"
        );
        assert!(
            got.contains(&InputEvent::KeyUp { code: "ControlLeft".into() }),
            "KeyUp 绝不能因队列拥塞被丢: {got:?}"
        );
        assert!(
            got.contains(&InputEvent::KeyDown { code: "ControlLeft".into() }),
            "KeyDown 不得被丢: {got:?}"
        );
    }

    /// 单事件错误/panic 隔离：后端对某个事件 panic 后，后续（尤其 KeyUp）
    /// 仍要被投递。
    #[test]
    fn test_worker_survives_panicking_event() {
        struct Flaky {
            seen: Arc<Mutex<Vec<InputEvent>>>,
        }
        impl InjectBackend for Flaky {
            fn inject(&mut self, ev: &InputEvent) -> Result<(), String> {
                if matches!(ev, InputEvent::MouseDown { .. }) {
                    panic!("backend exploded on mousedown");
                }
                if matches!(ev, InputEvent::Scroll { .. }) {
                    return Err("scroll rejected".into());
                }
                self.seen.lock().unwrap().push(ev.clone());
                Ok(())
            }
        }
        let (inj, queue) = InputInjector::with_queue();
        let seen: Arc<Mutex<Vec<InputEvent>>> = Default::default();
        inj.send(InputEvent::MouseDown { button: 0 });
        inj.send(InputEvent::Scroll { dx: 0, dy: 3 });
        inj.send(InputEvent::KeyDown { code: "KeyA".into() });
        inj.send(InputEvent::KeyUp { code: "KeyA".into() });
        drop(inj);
        drain_sync(
            &queue,
            Box::new(Flaky { seen: seen.clone() }),
        );
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                InputEvent::KeyDown { code: "KeyA".into() },
                InputEvent::KeyUp { code: "KeyA".into() },
            ],
            "panic/错误事件被跳过，后续事件必须继续投递: {got:?}"
        );
    }

    /// worker 退出后发送侧能观察到（调用方可据此重建 injector）。
    #[test]
    fn test_injector_detects_dead_worker() {
        let (inj, queue) = InputInjector::with_queue();
        assert!(inj.is_alive());
        // 模拟后端不可用：worker 立即退出并标记关闭。
        drive_worker(Err("no backend".into()), &queue);
        assert!(!inj.is_alive(), "worker 退出后必须能检测到（重建 injector）");
        // 再发事件不得 panic（静默丢弃）。
        inj.send(InputEvent::KeyUp { code: "KeyA".into() });
    }

    /// 真实 enigo 后端的存活路径：不 panic、能接受大量事件。
    #[test]
    fn test_injector_start_accepts_burst() {
        let inj = InputInjector::start();
        for _ in 0..1000 {
            inj.send(InputEvent::MouseMove { x: 1, y: 1 });
        }
        inj.send(InputEvent::KeyUp { code: "KeyA".into() });
    }
}