//! Input injection into the Claude app window with SendInput.
//!
//! Every event is checked before it is injected:
//! - pointer events must land on the Claude app window itself (or a dialog it owns), so a window
//!   of another app lying on top of it never receives remote clicks;
//! - keys are sent only while the Claude app is the foreground window;
//! - only a fixed set of keys is accepted. Alt and Win are never sent, so shortcuts that leave
//!   the app (Win+R, Alt+Tab, ...) cannot be produced.

use std::sync::Mutex;

use serde::Deserialize;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetCursorPos, GetForegroundWindow, GetWindow, SetCursorPos, SetForegroundWindow,
    WindowFromPoint, GA_ROOT, GW_OWNER,
};

/// One input event from the browser. Coordinates are normalized to the window (0..1).
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum InputMsg {
    Move { x: f64, y: f64 },
    /// `b`: 0 = left, 1 = middle, 2 = right (as in DOM `MouseEvent.button`).
    Down { x: f64, y: f64, b: u8 },
    Up { x: f64, y: f64, b: u8 },
    /// Deltas in DOM pixels (`WheelEvent.deltaY`, ~100 per notch).
    Wheel { x: f64, y: f64, #[serde(default)] dx: f64, #[serde(default)] dy: f64 },
    /// Text already composed by the client's IME.
    Text { s: String },
    /// A named key (DOM `KeyboardEvent.key`) with optional Ctrl / Shift.
    Key { k: String, #[serde(default)] ctrl: bool, #[serde(default)] shift: bool },
}

pub struct Injector {
    /// The Claude app main window, stored as an integer so the injector can cross threads.
    hwnd: usize,
    /// Where the local cursor was before a click from a viewer on this PC, to put it back after.
    parked: Mutex<Option<(i32, i32)>>,
}

impl Injector {
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd: hwnd.0 as usize, parked: Mutex::new(None) }
    }

    /// For a viewer on this PC: clicks and wheel have to move the real cursor onto the Claude
    /// window, so return it to where the user left it once the click or scroll is done.
    pub fn handle_local(&self, msg: InputMsg) -> Result<(), &'static str> {
        let restore_after = matches!(msg, InputMsg::Up { .. } | InputMsg::Wheel { .. });
        if matches!(msg, InputMsg::Down { .. } | InputMsg::Wheel { .. }) {
            let mut parked = self.parked.lock().unwrap();
            if parked.is_none() {
                let mut p = POINT::default();
                if unsafe { GetCursorPos(&mut p) }.is_ok() {
                    *parked = Some((p.x, p.y));
                }
            }
        }
        let result = self.handle(msg);
        if restore_after {
            if let Some((x, y)) = self.parked.lock().unwrap().take() {
                unsafe {
                    let _ = SetCursorPos(x, y);
                }
            }
        }
        result
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }

    /// Inject one event. Returns why it was dropped, if it was.
    pub fn handle(&self, msg: InputMsg) -> Result<(), &'static str> {
        match msg {
            InputMsg::Move { x, y } => {
                let p = self.to_screen(x, y)?;
                unsafe { SetCursorPos(p.x, p.y).map_err(|_| "SetCursorPos failed") }
            }
            InputMsg::Down { x, y, b } | InputMsg::Up { x, y, b } if b > 2 => {
                let _ = (x, y);
                Err("unknown button")
            }
            InputMsg::Down { x, y, b } => {
                self.point_on_claude(x, y)?;
                let flags = [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_RIGHTDOWN][b as usize];
                send(&[mouse(flags, 0)])
            }
            InputMsg::Up { x, y, b } => {
                // Release even if the pointer drifted off the window, so no button stays stuck.
                if let Ok(p) = self.to_screen(x, y) {
                    unsafe {
                        let _ = SetCursorPos(p.x, p.y);
                    }
                }
                let flags = [MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTUP][b as usize];
                send(&[mouse(flags, 0)])
            }
            InputMsg::Wheel { x, y, dx, dy } => {
                self.point_on_claude(x, y)?;
                // Browsers report ~100 px per notch, Windows expects 120 per notch (WHEEL_DELTA).
                let mut events = Vec::new();
                if dy != 0.0 {
                    events.push(mouse(MOUSEEVENTF_WHEEL, (-dy * 1.2) as i32));
                }
                if dx != 0.0 {
                    events.push(mouse(MOUSEEVENTF_HWHEEL, (dx * 1.2) as i32));
                }
                send(&events)
            }
            InputMsg::Text { s } => {
                self.ensure_foreground()?;
                let mut events = Vec::new();
                for unit in s.encode_utf16() {
                    if unit == '\n' as u16 {
                        // A newline inside the prompt, not "send".
                        events.extend(key_combo(VK_RETURN, false, true));
                    } else if unit != '\r' as u16 {
                        events.push(unicode(unit, KEYBD_EVENT_FLAGS(0)));
                        events.push(unicode(unit, KEYEVENTF_KEYUP));
                    }
                }
                send(&events)
            }
            InputMsg::Key { k, ctrl, shift } => {
                let vk = allowed_key(&k, ctrl).ok_or("key not allowed")?;
                self.ensure_foreground()?;
                send(&key_combo(vk, ctrl, shift))
            }
        }
    }

    /// Normalized window coordinates to physical screen pixels. Off-window points are rejected.
    fn to_screen(&self, x: f64, y: f64) -> Result<POINT, &'static str> {
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
            return Err("outside the window");
        }
        let mut r = RECT::default();
        unsafe {
            DwmGetWindowAttribute(
                self.hwnd(),
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut r as *mut RECT as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
            .map_err(|_| "window gone")?;
        }
        let px = r.left + ((r.right - r.left - 1) as f64 * x).round() as i32;
        let py = r.top + ((r.bottom - r.top - 1) as f64 * y).round() as i32;
        Ok(POINT { x: px, y: py })
    }

    /// Move the cursor to the point and make sure what is under it belongs to the Claude app.
    fn point_on_claude(&self, x: f64, y: f64) -> Result<(), &'static str> {
        let p = self.to_screen(x, y)?;
        let root = unsafe { GetAncestor(WindowFromPoint(p), GA_ROOT) };
        if !self.is_claude(root) {
            return Err("another window is on top at that point");
        }
        unsafe { SetCursorPos(p.x, p.y).map_err(|_| "SetCursorPos failed") }
    }

    fn ensure_foreground(&self) -> Result<(), &'static str> {
        if self.is_claude(unsafe { GetForegroundWindow() }) {
            return Ok(());
        }
        unsafe {
            let _ = SetForegroundWindow(self.hwnd());
        }
        if self.is_claude(unsafe { GetForegroundWindow() }) {
            Ok(())
        } else {
            Err("Claude app is not in the foreground")
        }
    }

    /// The main window, or a window it owns (dialogs such as the file picker).
    fn is_claude(&self, hwnd: HWND) -> bool {
        let main = self.hwnd();
        let mut h = hwnd;
        for _ in 0..4 {
            if h == main {
                return true;
            }
            match unsafe { GetWindow(h, GW_OWNER) } {
                Ok(owner) if !owner.is_invalid() => h = owner,
                _ => return false,
            }
        }
        false
    }
}

/// Keys the client may send. Ctrl is only allowed for send / editing shortcuts inside the app.
fn allowed_key(k: &str, ctrl: bool) -> Option<VIRTUAL_KEY> {
    let vk = match k {
        "Enter" => VK_RETURN,
        "Escape" => VK_ESCAPE,
        "Tab" => VK_TAB,
        "Backspace" => VK_BACK,
        "Delete" => VK_DELETE,
        "ArrowUp" => VK_UP,
        "ArrowDown" => VK_DOWN,
        "ArrowLeft" => VK_LEFT,
        "ArrowRight" => VK_RIGHT,
        "Home" => VK_HOME,
        "End" => VK_END,
        "PageUp" => VK_PRIOR,
        "PageDown" => VK_NEXT,
        // Letters only as Ctrl shortcuts (plain text goes through `Text`).
        "a" | "c" | "v" | "x" | "z" | "y" if ctrl => VIRTUAL_KEY(k.as_bytes()[0].to_ascii_uppercase() as u16),
        _ => return None,
    };
    let ctrl_ok = matches!(k, "Enter" | "a" | "c" | "v" | "x" | "z" | "y" | "Backspace" | "Delete"
        | "ArrowLeft" | "ArrowRight" | "Home" | "End");
    (!ctrl || ctrl_ok).then_some(vk)
}

fn key_combo(vk: VIRTUAL_KEY, ctrl: bool, shift: bool) -> Vec<INPUT> {
    let mut down = Vec::new();
    if ctrl {
        down.push(VK_CONTROL);
    }
    if shift {
        down.push(VK_SHIFT);
    }
    down.push(vk);
    let mut events: Vec<INPUT> = down.iter().map(|&k| key(k, KEYBD_EVENT_FLAGS(0))).collect();
    events.extend(down.iter().rev().map(|&k| key(k, KEYEVENTF_KEYUP)));
    events
}

fn key(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn unicode(unit: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            // Wheel deltas are signed; the field is declared u32, so reinterpret the bits.
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn send(events: &[INPUT]) -> Result<(), &'static str> {
    if events.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(events, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == events.len() {
        Ok(())
    } else {
        Err("SendInput was blocked")
    }
}
