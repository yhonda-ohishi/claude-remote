//! Find the Claude app's top-level windows.

use serde::Serialize;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE, GW_OWNER,
};

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    #[serde(skip)]
    pub hwnd: HWND,
    pub hwnd_hex: String,
    pub pid: u32,
    pub exe: String,
    pub title: String,
    pub class: String,
    pub visible: bool,
    pub cloaked: bool,
    pub iconic: bool,
    pub owner_hex: Option<String>,
    pub style: u32,
    pub ex_style: u32,
    /// DWM extended frame bounds (physical pixels, no drop shadow): left, top, right, bottom.
    pub frame: [i32; 4],
    /// GetWindowRect, which includes the invisible resize border.
    pub window_rect: [i32; 4],
    pub dpi: u32,
}

impl WindowInfo {
    pub fn width(&self) -> i32 {
        self.frame[2] - self.frame[0]
    }

    pub fn height(&self) -> i32 {
        self.frame[3] - self.frame[1]
    }

    /// Worth capturing: shown on the current desktop and not degenerate.
    pub fn is_capturable(&self) -> bool {
        self.visible && !self.cloaked && self.width() > 1 && self.height() > 1
    }
}

pub fn hex(hwnd: HWND) -> String {
    format!("0x{:08X}", hwnd.0 as usize)
}

/// The desktop app is `...\WindowsApps\Claude_*\app\Claude.exe`. Claude Code's CLI is also
/// named `claude.exe`, so the image name alone is not enough.
pub fn is_claude_app(exe: &str) -> bool {
    let lower = exe.to_ascii_lowercase();
    lower.ends_with("\\claude.exe") && !lower.contains("\\claude-code\\")
}

/// Top-level windows of matching processes, in z-order (topmost first).
pub fn find_windows(filter: impl Fn(&str) -> bool) -> Vec<WindowInfo> {
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut hwnds as *mut Vec<HWND> as isize));
    }
    hwnds
        .into_iter()
        .filter_map(|hwnd| {
            let pid = window_pid(hwnd);
            let exe = process_image(pid)?;
            filter(&exe).then(|| describe(hwnd, pid, exe))
        })
        .collect()
}

pub fn describe(hwnd: HWND, pid: u32, exe: String) -> WindowInfo {
    unsafe {
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        let n = GetClassNameW(hwnd, &mut buf);
        let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);

        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        let mut window_rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut window_rect);
        let mut frame = window_rect;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        let owner = GetWindow(hwnd, GW_OWNER).ok().filter(|h| !h.is_invalid());

        WindowInfo {
            hwnd,
            hwnd_hex: hex(hwnd),
            pid,
            exe,
            title,
            class,
            visible: IsWindowVisible(hwnd).as_bool(),
            cloaked: cloaked != 0,
            iconic: IsIconic(hwnd).as_bool(),
            owner_hex: owner.map(hex),
            style: GetWindowLongW(hwnd, GWL_STYLE) as u32,
            ex_style: GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
            frame: rect_array(frame),
            window_rect: rect_array(window_rect),
            dpi: GetDpiForWindow(hwnd),
        }
    }
}

/// The main window: the largest capturable unowned window with a title.
pub fn pick_main(windows: &[WindowInfo]) -> Option<usize> {
    windows
        .iter()
        .enumerate()
        .filter(|(_, w)| w.owner_hex.is_none() && !w.title.is_empty() && (w.is_capturable() || w.iconic))
        .max_by_key(|(_, w)| w.width() as i64 * w.height() as i64)
        .map(|(i, _)| i)
}

fn rect_array(r: RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let hwnds = &mut *(lparam.0 as *mut Vec<HWND>);
    hwnds.push(hwnd);
    true.into()
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

fn process_image(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result =
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}
