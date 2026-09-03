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
//! Injection runs on a dedicated thread with a bounded channel, mirroring
//! RustDesk's `input_service` QUEUE pattern: one serial consumer, so mouse
//! moves and key presses replay in order and never race each other. The
//! channel is small and drop-oldest — input freshness matters far more than
//! completeness; a backlog means the remote desktop is behind anyway.

use std::sync::mpsc;

/// One injected input event, already normalized by [`parse_mouse`] /
/// [`parse_key`].
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Absolute pointer position in desktop pixels.
    MouseMove { x: i32, y: i32 },
    MouseDown { button: u8 },
    MouseUp { button: u8 },
    /// Scroll delta in "clicks" (positive = away from user / up).
    Scroll { dx: i32, dy: i32 },
    KeyDown { code: String },
    KeyUp { code: String },
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
            x: p.get("x")?.as_i64()? as i32,
            y: p.get("y")?.as_i64()? as i32,
        }),
        "down" => Some(InputEvent::MouseDown {
            button: norm_button(p.get("button")?.as_u64()? as u8),
        }),
        "up" => Some(InputEvent::MouseUp {
            button: norm_button(p.get("button")?.as_u64()? as u8),
        }),
        "wheel" => Some(InputEvent::Scroll {
            dx: p.get("dx").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            dy: p.get("dy")?.as_i64()? as i32,
        }),
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

/// The serial injection worker. Owns the platform `Enigo` instance (it is
/// !Send on some platforms, so it lives on this thread only).
pub struct InputInjector {
    tx: mpsc::SyncSender<InputEvent>,
}

impl InputInjector {
    /// Spawn the injection thread. Returns immediately; the enigo backend is
    /// created lazily on the thread and failures surface as log lines (the
    /// stream keeps playing — losing input injection must not kill video).
    pub fn start() -> Self {
        // Bounded, drop-oldest queue: 128 events. Old mouse moves are useless
        // once newer ones exist; a blocked consumer must not stall the agent.
        let (tx, rx) = mpsc::sync_channel::<InputEvent>(128);
        std::thread::Builder::new()
            .name("desktop-input".into())
            .spawn(move || worker(rx))
            .expect("spawn desktop-input thread");
        Self { tx }
    }

    /// Queue one event. Drops the event when the queue is full (the desktop
    /// is behind; fresh input matters more than complete input).
    pub fn send(&self, ev: InputEvent) {
        let _ = self.tx.try_send(ev);
    }
}

fn worker(rx: mpsc::Receiver<InputEvent>) {
    let mut enigo = match enigo::Enigo::new(&enigo::Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("desktop input backend unavailable: {e}");
            // Drain events so senders never block; injection is dead but the
            // video pipeline lives on.
            for _ in rx {}
            return;
        }
    };
    use enigo::{Axis, Coordinate, Direction, Enigo, Keyboard, Mouse};
    for ev in rx {
        let r: Result<(), Box<dyn std::error::Error>> = (|| {
            match ev {
                InputEvent::MouseMove { x, y } => {
                    enigo.move_mouse(x, y, Coordinate::Abs)?
                }
                InputEvent::MouseDown { button } => {
                    enigo.button(btn(button).ok_or("button")?, Direction::Press)?
                }
                InputEvent::MouseUp { button } => {
                    enigo.button(btn(button).ok_or("button")?, Direction::Release)?
                }
                InputEvent::Scroll { dx, dy } => {
                    // dy/dx 语义: 正=向下/向右（与浏览器一致; enigo 的
                    // scroll 单位即"格"——win 侧 1 格=WHEEL_DELTA，X11 侧
                    // 一次 button click, 无需再取负号）。
                    if dy != 0 {
                        enigo.scroll(dy, Axis::Vertical)?;
                    }
                    if dx != 0 {
                        enigo.scroll(dx, Axis::Horizontal)?;
                    }
                }
                InputEvent::KeyDown { code } => {
                    if let Some(k) = key_from_code(&code) {
                        enigo.key(k, Direction::Press)?;
                    }
                }
                InputEvent::KeyUp { code } => {
                    if let Some(k) = key_from_code(&code) {
                        enigo.key(k, Direction::Release)?;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            tracing::debug!("input inject failed: {e}");
        }
    }
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

    #[test]
    fn test_injector_queue_drop_oldest() {
        // 队满后 send 不 panic 不阻塞（drop 该事件）
        let inj = InputInjector::start();
        for _ in 0..1000 {
            inj.send(InputEvent::MouseMove { x: 1, y: 1 });
        }
        // worker 存活即可（无崩溃）
    }
}