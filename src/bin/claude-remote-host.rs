//! Host: streams the Claude app's main window (H.264 over a WebSocket) and injects the viewer's
//! input. Meant to sit behind Cloudflare Tunnel + Access, so it listens on 127.0.0.1 by default.
//!
//! Usage: claude-remote-host [--bind 127.0.0.1] [--port 8080] [--bitrate 8000000] [--fps 30]
//! Locally: open http://localhost:8080/ (WebCodecs needs HTTPS or localhost).

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_RESTORE};

use claude_remote_probe::capture::{BgraFrame, Capturer, FrameStream};
use claude_remote_probe::encoder::H264Encoder;
use claude_remote_probe::input::{ime_open_in_foreground, Injector};
use claude_remote_probe::windows_enum::{find_windows, frame_bounds, is_claude_app, monitor_of, pick_main};
use claude_remote_probe::ws::{serve, EncodedFrame, Hub};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HMONITOR;

const INDEX_HTML: &str = include_str!("../../web/index.html");

struct Args {
    bind: String,
    port: u16,
    bitrate: u32,
    fps: f32,
}

fn main() {
    let args = parse_args();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let windows = find_windows(is_claude_app);
    let Some(main) = pick_main(&windows).map(|i| windows[i].clone()) else {
        eprintln!("no Claude app main window found");
        std::process::exit(1);
    };
    if main.iconic {
        unsafe {
            let _ = ShowWindow(main.hwnd, SW_RESTORE);
        }
        thread::sleep(Duration::from_millis(700));
    }
    println!("capturing {} ({}x{}, dpi {})", main.hwnd_hex, main.width(), main.height(), main.dpi);

    let hub = Arc::new(Hub::default());
    let injector = Arc::new(Injector::new(main.hwnd));
    let hwnd = main.hwnd.0 as usize;
    let capture_hub = hub.clone();
    thread::spawn(move || {
        if let Err(e) = capture_loop(hwnd, &capture_hub, args.bitrate, args.fps) {
            eprintln!("capture loop failed: {e}");
            std::process::exit(1);
        }
    });

    let listener = TcpListener::bind((args.bind.as_str(), args.port)).expect("bind HTTP port");
    if args.bind != "127.0.0.1" {
        eprintln!("warning: listening on {} — this server has no authentication of its own", args.bind);
    }
    println!("listening on http://{}:{}/ (locally: http://localhost:{}/)", args.bind, args.port, args.port);
    serve(listener, hub, injector, INDEX_HTML);
}

/// Capture, encode and publish frames. Runs on its own thread so all WinRT/D3D objects live here.
fn capture_loop(hwnd: usize, hub: &Hub, bitrate: u32, fps: f32) -> windows::core::Result<()> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let hwnd = HWND(hwnd as *mut _);
    let capturer = Capturer::new()?;
    let mut stream = capturer.stream(hwnd)?;
    let frame_interval = Duration::from_secs_f32(1.0 / fps);

    let mut last: Option<BgraFrame> = None;
    let mut encoder: Option<H264Encoder> = None;
    let mut last_sent = Instant::now();
    let mut stats = Stats::new();
    let mut screen = ScreenView::new();

    loop {
        // Wait briefly when a keyframe is owed so a new viewer doesn't stare at black.
        let wait = if hub.keyframe_pending() { Duration::from_millis(50) } else { Duration::from_secs(1) };
        // With the IME on in the Claude window its candidate list can be up. Windows 11 draws
        // that list above all windows, out of reach of window capture, so meanwhile the picture
        // is cut out of the monitor at the Claude window's position instead.
        let from_screen = ime_open_in_foreground(hwnd);
        let mut fresh = from_screen != screen.active();

        let captured_at = if from_screen {
            let at = screen.next(&capturer, hwnd, wait);
            // Keep following the window too, so its picture is current when the IME goes off.
            if let Some((frame, _)) = stream.latest(&capturer, Duration::ZERO) {
                last = Some(capturer.read_bgra(&frame)?);
            }
            fresh |= at.is_some();
            at.unwrap_or_else(Instant::now)
        } else {
            screen.stop();
            match stream.latest(&capturer, wait) {
                Some((frame, at)) => {
                    last = Some(capturer.read_bgra(&frame)?);
                    fresh = true;
                    at
                }
                // No change on screen: resend the last picture as a keepalive.
                None => Instant::now(),
            }
        };

        let Some(bgra) = screen.picture().or(last.as_ref()) else { continue };
        if !hub.has_subscriber() {
            encoder = None;
            continue;
        }

        let keyframe = hub.take_keyframe_request();
        let encode_start = Instant::now();
        // A new encoder starts with an empty (green) picture, so it must load `last` before its
        // first encode even when no new frame arrived.
        let created = encoder.is_none();
        let enc = match encoder.as_mut() {
            Some(enc) => enc,
            None => encoder.insert(
                H264Encoder::new(bgra.width, bgra.height, bitrate, fps).expect("create H.264 encoder"),
            ),
        };
        let result = if fresh || created || enc.size() != (bgra.width & !1, bgra.height & !1) {
            enc.encode(bgra, keyframe)
        } else {
            enc.encode_last(keyframe)
        };
        let picture = match result {
            Ok(picture) if !picture.data.is_empty() => picture,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("encode failed: {e}");
                encoder = None;
                continue;
            }
        };
        let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
        let age = captured_at.elapsed();
        stats.add(picture.data.len(), encode_ms, age.as_secs_f64() * 1000.0, enc.size());
        let captured_unix_ms = (SystemTime::now() - age).duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0;
        hub.publish(EncodedFrame { data: picture.data, keyframe: picture.keyframe, captured_unix_ms });

        // Cap the frame rate.
        let since = last_sent.elapsed();
        if since < frame_interval {
            thread::sleep(frame_interval - since);
        }
        last_sent = Instant::now();
        stats.report_every(Duration::from_secs(2));
    }
}

/// Capture of the monitor the Claude window is on, cropped to the Claude window. Used only while
/// the Claude window has the IME on and is in the foreground, so that the IME candidate list
/// shows. Anything else lying over the Claude window would show too, which is why window
/// capture stays the default.
struct ScreenView {
    stream: Option<(HMONITOR, FrameStream)>,
    picture: Option<BgraFrame>,
}

impl ScreenView {
    fn new() -> Self {
        Self { stream: None, picture: None }
    }

    fn active(&self) -> bool {
        self.stream.is_some()
    }

    fn picture(&self) -> Option<&BgraFrame> {
        self.picture.as_ref()
    }

    /// Wait up to `wait` for the screen to change and crop the new picture. Returns its capture
    /// time, or None if nothing changed (or the monitor cannot be captured).
    fn next(&mut self, capturer: &Capturer, claude: HWND, wait: Duration) -> Option<Instant> {
        let (monitor, monitor_rect) = monitor_of(claude)?;
        if self.stream.as_ref().map(|(m, _)| *m) != Some(monitor) {
            match capturer.stream_monitor(monitor) {
                Ok(stream) => self.stream = Some((monitor, stream)),
                Err(e) => {
                    eprintln!("monitor capture unavailable: {e}");
                    return None;
                }
            }
        }
        let (_, stream) = self.stream.as_mut()?;
        let (frame, at) = stream.latest(capturer, wait)?;
        let full = capturer.read_bgra(&frame).ok()?;
        let c = frame_bounds(claude)?;
        let (x, y) = (c[0] - monitor_rect[0], c[1] - monitor_rect[1]);
        self.picture = Some(full.crop(x, y, (c[2] - c[0]) as u32, (c[3] - c[1]) as u32)?);
        Some(at)
    }

    fn stop(&mut self) {
        self.stream = None;
        self.picture = None;
    }
}

/// Rolling numbers printed to the console: fps, encode time, capture-to-send latency, bitrate.
struct Stats {
    since: Instant,
    frames: u32,
    bytes: usize,
    encode_ms: f64,
    pipeline_ms: f64,
    size: (u32, u32),
}

impl Stats {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, bytes: 0, encode_ms: 0.0, pipeline_ms: 0.0, size: (0, 0) }
    }

    fn add(&mut self, bytes: usize, encode_ms: f64, pipeline_ms: f64, size: (u32, u32)) {
        self.frames += 1;
        self.bytes += bytes;
        self.encode_ms += encode_ms;
        self.pipeline_ms += pipeline_ms;
        self.size = size;
    }

    fn report_every(&mut self, period: Duration) {
        let elapsed = self.since.elapsed();
        if elapsed < period || self.frames == 0 {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let n = self.frames as f64;
        println!(
            "{}x{} {:.1} fps, encode {:.1} ms, capture->send {:.1} ms, {:.0} kbps",
            self.size.0,
            self.size.1,
            n / secs,
            self.encode_ms / n,
            self.pipeline_ms / n,
            self.bytes as f64 * 8.0 / secs / 1000.0,
        );
        *self = Self::new();
    }
}

fn parse_args() -> Args {
    let mut args = Args { bind: "127.0.0.1".into(), port: 8080, bitrate: 8_000_000, fps: 30.0 };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let value = it.next();
        let number = value.as_deref().and_then(|v| v.parse::<f64>().ok());
        match (a.as_str(), number) {
            ("--bind", _) if value.is_some() => args.bind = value.unwrap(),
            ("--port", Some(v)) => args.port = v as u16,
            ("--bitrate", Some(v)) => args.bitrate = v as u32,
            ("--fps", Some(v)) => args.fps = v as f32,
            _ => {
                eprintln!("usage: claude-remote-host [--bind ADDR] [--port N] [--bitrate BPS] [--fps N]");
                std::process::exit(2);
            }
        }
    }
    args
}
