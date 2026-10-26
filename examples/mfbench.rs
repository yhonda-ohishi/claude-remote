//! Media Foundation hardware encoder vs OpenH264 on the Claude window.
//!
//! cargo run --release --example mfbench

use std::time::{Duration, Instant};

use claude_remote_probe::capture::Capturer;
use claude_remote_probe::encoder::H264Encoder;
use claude_remote_probe::mf_encoder::{bgra_to_nv12, MfEncoder};
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
    let bgra = capturer.read_bgra(&frame).unwrap();
    let size = (bgra.width & !1, bgra.height & !1);
    println!("frame {}x{}", size.0, size.1);

    let mut nv12 = Vec::new();
    let t = Instant::now();
    for _ in 0..N {
        bgra_to_nv12(&bgra, size, &mut nv12);
    }
    println!("BGRA -> NV12 (threads)        {:6.1} ms", per_frame(t));

    let mut mf = match MfEncoder::new(size.0, size.1, 8_000_000, 30) {
        Ok(mf) => mf,
        Err(e) => {
            println!("Media Foundation hardware encoder unavailable: {e}");
            return;
        }
    };
    println!("encoder: {}", mf.name());
    let first = mf.encode(&bgra, true).unwrap();
    let nals: Vec<u8> = first.data.windows(4).filter(|w| w[..3] == [0, 0, 1]).map(|w| w[3] & 0x1f).collect();
    println!("first output: {} bytes, keyframe={}, NAL types {:?}", first.data.len(), first.keyframe, nals);

    let t = Instant::now();
    let mut bytes = 0;
    for _ in 0..N {
        bytes += mf.encode(&bgra, true).unwrap().data.len();
    }
    println!("MF keyframe   (incl. NV12)    {:6.1} ms  {:7} B/frame", per_frame(t), bytes / N as usize);
    let t = Instant::now();
    let mut bytes = 0;
    let mut keys = 0;
    for _ in 0..N {
        let p = mf.encode(&bgra, false).unwrap();
        bytes += p.data.len();
        keys += p.keyframe as u32;
    }
    println!("MF unchanged  (incl. NV12)    {:6.1} ms  {:7} B/frame  keyframes {keys}", per_frame(t), bytes / N as usize);
    // A keyframe asked for mid-stream must carry SPS/PPS for a joining viewer.
    let again = mf.encode(&bgra, true).unwrap();
    let has_sps = again.data.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7);
    println!("mid-stream keyframe: keyframe={}, carries SPS={has_sps}", again.keyframe);

    let mut soft = H264Encoder::new(size.0, size.1, 8_000_000, 30.0).unwrap();
    let _ = soft.encode(&bgra, true).unwrap();
    let t = Instant::now();
    for _ in 0..N {
        let _ = soft.encode(&bgra, true).unwrap();
    }
    println!("OpenH264 keyframe (incl. YUV) {:6.1} ms", per_frame(t));
    let t = Instant::now();
    for _ in 0..N {
        let _ = soft.encode(&bgra, false).unwrap();
    }
    println!("OpenH264 unchanged (incl. YUV){:6.1} ms", per_frame(t));
}

fn per_frame(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0 / N as f64
}
