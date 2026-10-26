//! Where does a frame's time go? Captures the Claude window and times each stage of the host's
//! pipeline separately: GPU readback, BGRA -> YUV conversion, H.264 encode.
//!
//! cargo run --release --example bench

use std::time::{Duration, Instant};

use claude_remote_probe::capture::Capturer;
use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;
use claude_remote_probe::windows_enum::{find_windows, is_claude_app, pick_main};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

const N: u32 = 30;

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let windows = find_windows(is_claude_app);
    let main = &windows[pick_main(&windows).expect("Claude window")];
    let capturer = Capturer::new().unwrap();
    let mut stream = capturer.stream(main.hwnd).unwrap();
    let (frame, _) = stream.latest(&capturer, Duration::from_secs(2)).expect("a frame");

    let t = Instant::now();
    let mut bgra = None;
    for _ in 0..N {
        bgra = Some(capturer.read_bgra(&frame).unwrap());
    }
    let bgra = bgra.unwrap();
    report("readback (GPU -> CPU BGRA)", t, bgra.width, bgra.height);

    for (label, w, h) in [("full", bgra.width & !1, bgra.height & !1), ("75%", (bgra.width * 3 / 4) & !1, (bgra.height * 3 / 4) & !1)] {
        let src = bgra.downscale(w, h);
        let t = Instant::now();
        for _ in 0..N {
            let _ = bgra.downscale(w, h);
        }
        if label != "full" {
            report("downscale to 75%", t, w, h);
        }

        let mut yuv = YUVBuffer::new(w as usize, h as usize);
        let t = Instant::now();
        for _ in 0..N {
            yuv.read_rgb(BgraSliceU8::new(&src.data, (w as usize, h as usize)));
        }
        report(&format!("BGRA -> YUV ({label})"), t, w, h);

        for threads in [1u16, 4] {
            let config = EncoderConfig::new()
                .usage_type(UsageType::ScreenContentRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(8_000_000))
                .max_frame_rate(FrameRate::from_hz(30.0))
                .skip_frames(false)
                .complexity(Complexity::Low)
                .num_threads(threads);
            let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
            let _ = encoder.encode(&yuv).unwrap();
            // Every frame forced to a keyframe: the worst case, like a fully changed screen.
            let t = Instant::now();
            for _ in 0..N {
                encoder.force_intra_frame();
                let _ = encoder.encode(&yuv).unwrap();
            }
            report(&format!("encode keyframe ({label}, {threads} threads)"), t, w, h);
            // Unchanged picture: what a static screen costs.
            let t = Instant::now();
            for _ in 0..N {
                let _ = encoder.encode(&yuv).unwrap();
            }
            report(&format!("encode unchanged ({label}, {threads} threads)"), t, w, h);
        }
    }
}

fn report(stage: &str, start: Instant, w: u32, h: u32) {
    let ms = start.elapsed().as_secs_f64() * 1000.0 / N as f64;
    println!("{stage:<38} {w}x{h}  {ms:6.1} ms/frame");
}
