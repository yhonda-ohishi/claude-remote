//! P0 capture probe for the Claude app remote tool.
//!
//! Finds the Claude app's top-level windows, captures each with Windows.Graphics.Capture,
//! saves one PNG per window plus a composite of all of them at their screen positions,
//! and writes report.json with sizes, DPI and timings.
//!
//! Usage: claude-remote-probe [--list] [--out DIR] [--delay SECS] [--wait-popup SECS]
//!                            [--no-restore] [--reminimize]




use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use image::{Rgba, RgbaImage};
use serde::Serialize;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MINIMIZE, SW_RESTORE};

use claude_remote_probe::capture::Capturer;
use claude_remote_probe::windows_enum::{find_windows, is_claude_app, pick_main, WindowInfo};

struct Args {
    list: bool,
    out: PathBuf,
    delay: u64,
    wait_popup: u64,
    restore: bool,
    reminimize: bool,
}

#[derive(Serialize)]
struct Report {
    unix_time: u64,
    wgc_supported: bool,
    borderless: bool,
    main_was_minimized: bool,
    restored: bool,
    windows: Vec<WindowReport>,
    composite: Option<String>,
}

#[derive(Serialize)]
struct WindowReport {
    #[serde(flatten)]
    info: WindowInfo,
    role: &'static str,
    capture: Option<CaptureReport>,
}

#[derive(Serialize)]
struct CaptureReport {
    file: Option<String>,
    width: u32,
    height: u32,
    item_size: (i32, i32),
    /// Captured size equals the DWM frame size, i.e. no DPI scaling mismatch.
    matches_frame: bool,
    first_frame_ms: u128,
    frames_seen: u32,
    error: Option<String>,
}

fn main() {
    let args = parse_args();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }

    if args.delay > 0 {
        println!("waiting {}s ...", args.delay);
        std::thread::sleep(Duration::from_secs(args.delay));
    }

    if args.wait_popup > 0 {
        wait_for_popup(Duration::from_secs(args.wait_popup));
    }

    let mut windows = find_windows(is_claude_app);
    print_table(&windows);
    if args.list {
        return;
    }

    let Some(main_idx) = pick_main(&windows) else {
        eprintln!("no Claude app main window found");
        std::process::exit(1);
    };
    let main_hwnd = windows[main_idx].hwnd;
    let main_was_minimized = windows[main_idx].iconic;
    let mut restored = false;
    if main_was_minimized && args.restore {
        println!("main window is minimized; restoring");
        unsafe {
            let _ = ShowWindow(main_hwnd, SW_RESTORE);
        }
        restored = true;
        // Let the restore animation finish, then re-read positions.
        std::thread::sleep(Duration::from_millis(700));
        windows = find_windows(is_claude_app);
        print_table(&windows);
    }
    let main_idx = windows.iter().position(|w| w.hwnd == main_hwnd).unwrap_or(main_idx);

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let out_dir = args.out.join(stamp.to_string());
    std::fs::create_dir_all(&out_dir).expect("create output dir");

    let capturer = Capturer::new().expect("init Direct3D / WGC");
    println!("WGC supported: {}, borderless: {}", Capturer::supported(), capturer.borderless());

    let mut reports = Vec::new();
    let mut shots: Vec<(usize, RgbaImage)> = Vec::new();
    for (i, w) in windows.iter().enumerate() {
        let role = if i == main_idx {
            "main"
        } else if w.is_capturable() {
            "popup"
        } else {
            "skipped"
        };
        let capture = (role != "skipped").then(|| {
            match capturer.capture(w.hwnd, Duration::from_secs(3), Duration::from_millis(300)) {
                Ok(shot) => {
                    let file = format!("{:02}-{}-{}.png", i, role, w.hwnd_hex);
                    shot.image.save(out_dir.join(&file)).expect("save png");
                    let (width, height) = shot.image.dimensions();
                    let report = CaptureReport {
                        file: Some(file),
                        width,
                        height,
                        item_size: shot.item_size,
                        matches_frame: width as i32 == w.width() && height as i32 == w.height(),
                        first_frame_ms: shot.first_frame_ms,
                        frames_seen: shot.frames_seen,
                        error: None,
                    };
                    shots.push((i, shot.image));
                    report
                }
                Err(e) => CaptureReport {
                    file: None,
                    width: 0,
                    height: 0,
                    item_size: (0, 0),
                    matches_frame: false,
                    first_frame_ms: 0,
                    frames_seen: 0,
                    error: Some(e.to_string()),
                },
            }
        });
        if let Some(c) = &capture {
            println!(
                "[{role:7}] {} {}x{} (frame {}x{}, dpi {}) first frame {}ms {}",
                w.hwnd_hex,
                c.width,
                c.height,
                w.width(),
                w.height(),
                w.dpi,
                c.first_frame_ms,
                c.error.as_deref().unwrap_or(""),
            );
        }
        reports.push(WindowReport { info: w.clone(), role, capture });
    }

    let composite = (!shots.is_empty()).then(|| {
        let image = composite(&windows, &shots);
        image.save(out_dir.join("composite.png")).expect("save composite");
        "composite.png".to_string()
    });

    if restored && args.reminimize {
        unsafe {
            let _ = ShowWindow(main_hwnd, SW_MINIMIZE);
        }
    }

    let report = Report {
        unix_time: stamp,
        wgc_supported: Capturer::supported(),
        borderless: capturer.borderless(),
        main_was_minimized,
        restored,
        windows: reports,
        composite,
    };
    std::fs::write(
        out_dir.join("report.json"),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .expect("write report");
    println!("wrote {}", out_dir.display());
}

/// Poll until a capturable window other than the main one appears (e.g. a file picker).
fn wait_for_popup(timeout: Duration) {
    println!("waiting up to {}s for a dialog/popup ...", timeout.as_secs());
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let windows = find_windows(is_claude_app);
        let main = pick_main(&windows);
        if windows.iter().enumerate().any(|(i, w)| Some(i) != main && w.is_capturable()) {
            // Give the dialog a moment to finish drawing.
            std::thread::sleep(Duration::from_millis(500));
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    println!("no popup appeared; capturing anyway");
}

/// Place every captured window at its screen position, bottom of the z-order first.
fn composite(windows: &[WindowInfo], shots: &[(usize, RgbaImage)]) -> RgbaImage {
    let frames: Vec<[i32; 4]> = shots.iter().map(|(i, _)| windows[*i].frame).collect();
    let left = frames.iter().map(|f| f[0]).min().unwrap();
    let top = frames.iter().map(|f| f[1]).min().unwrap();
    let right = frames.iter().map(|f| f[2]).max().unwrap();
    let bottom = frames.iter().map(|f| f[3]).max().unwrap();
    let mut canvas = RgbaImage::from_pixel(
        (right - left) as u32,
        (bottom - top) as u32,
        Rgba([32, 32, 32, 255]),
    );
    // EnumWindows lists topmost first, so paint in reverse.
    for (i, image) in shots.iter().rev() {
        let f = windows[*i].frame;
        image::imageops::overlay(&mut canvas, image, (f[0] - left) as i64, (f[1] - top) as i64);
    }
    canvas
}

fn print_table(windows: &[WindowInfo]) {
    println!(
        "{:<12} {:>6} {:>4} {:>5} {:>5} {:>6} {:>11} {:>5}  {:<28} {}",
        "hwnd", "pid", "vis", "cloak", "icon", "dpi", "size", "owner", "class", "title"
    );
    for w in windows {
        println!(
            "{:<12} {:>6} {:>4} {:>5} {:>5} {:>6} {:>11} {:>5}  {:<28} {}",
            w.hwnd_hex,
            w.pid,
            w.visible as u8,
            w.cloaked as u8,
            w.iconic as u8,
            w.dpi,
            format!("{}x{}", w.width(), w.height()),
            if w.owner_hex.is_some() { "yes" } else { "-" },
            truncate(&w.class, 28),
            w.title,
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn parse_args() -> Args {
    let mut args = Args {
        list: false,
        out: PathBuf::from("captures"),
        delay: 0,
        wait_popup: 0,
        restore: true,
        reminimize: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => args.list = true,
            "--out" => args.out = PathBuf::from(it.next().expect("--out needs a path")),
            "--delay" => args.delay = it.next().and_then(|s| s.parse().ok()).expect("--delay needs seconds"),
            "--wait-popup" => {
                args.wait_popup = it.next().and_then(|s| s.parse().ok()).expect("--wait-popup needs seconds")
            }
            "--no-restore" => args.restore = false,
            "--reminimize" => args.reminimize = true,
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}
