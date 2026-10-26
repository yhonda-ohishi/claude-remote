//! Window and monitor capture with Windows.Graphics.Capture.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use image::RgbaImage;
use windows::core::{factory, IInspectable, Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

pub struct Capturer {
    d3d: ID3D11Device,
    device: IDirect3DDevice,
    borderless: bool,
}

pub struct Shot {
    pub image: RgbaImage,
    /// GraphicsCaptureItem.Size when the session started.
    pub item_size: (i32, i32),
    pub first_frame_ms: u128,
    pub frames_seen: u32,
}

impl Capturer {
    pub fn new() -> Result<Self> {
        let mut d3d = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                None,
            )?;
        }
        let d3d = d3d.expect("D3D11CreateDevice returned no device");
        let dxgi: IDXGIDevice = d3d.cast()?;
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;

        // Unpackaged desktop apps are granted this without a prompt on Windows 11.
        let borderless = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
            .and_then(|op| op.get())
            .map(|status| status == AppCapabilityAccessStatus::Allowed)
            .unwrap_or(false);

        Ok(Self { d3d, device, borderless })
    }

    pub fn supported() -> bool {
        GraphicsCaptureSession::IsSupported().unwrap_or(false)
    }

    pub fn borderless(&self) -> bool {
        self.borderless
    }

    /// Start a session, wait for the first frame, let it settle, and return the latest frame.
    pub fn capture(&self, hwnd: HWND, timeout: Duration, settle: Duration) -> Result<Shot> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        let size = item.Size()?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &self.device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        if self.borderless {
            let _ = session.SetIsBorderRequired(false);
        }
        let _ = session.SetIsCursorCaptureEnabled(false);

        let (tx, rx) = mpsc::channel::<Direct3D11CaptureFrame>();
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |pool, _| {
                if let Some(pool) = pool.as_ref() {
                    if let Ok(frame) = pool.TryGetNextFrame() {
                        let _ = tx.send(frame);
                    }
                }
                Ok(())
            },
        ))?;

        let started = Instant::now();
        session.StartCapture()?;

        let result = (|| {
            let mut frame = rx
                .recv_timeout(timeout)
                .map_err(|_| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "no frame before timeout"))?;
            let first_frame_ms = started.elapsed().as_millis();
            let mut frames_seen = 1;
            // Keep the newest frame; dropping the old one returns its buffer to the pool.
            let deadline = Instant::now() + settle;
            while let Ok(next) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                frame = next;
                frames_seen += 1;
            }
            let image = self.read_frame(&frame)?;
            Ok(Shot {
                image,
                item_size: (size.Width, size.Height),
                first_frame_ms,
                frames_seen,
            })
        })();

        let _ = session.Close();
        let _ = pool.Close();
        result
    }

    fn read_frame(&self, frame: &Direct3D11CaptureFrame) -> Result<RgbaImage> {
        let bgra = self.read_bgra(frame)?;
        let mut image = RgbaImage::new(bgra.width, bgra.height);
        for (dst, src) in image.chunks_exact_mut(4).zip(bgra.data.chunks_exact(4)) {
            dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
        }
        Ok(image)
    }

    /// Start a capture session on a window that keeps delivering frames until the stream is dropped.
    pub fn stream(&self, hwnd: HWND) -> Result<FrameStream> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        self.stream_item(&item)
    }

    /// The same for a whole monitor. Unlike window capture this also shows UI that Windows draws
    /// above all windows, such as the Windows 11 IME candidate list.
    pub fn stream_monitor(&self, monitor: HMONITOR) -> Result<FrameStream> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor)? };
        self.stream_item(&item)
    }

    fn stream_item(&self, item: &GraphicsCaptureItem) -> Result<FrameStream> {
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &self.device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(item)?;
        if self.borderless {
            let _ = session.SetIsBorderRequired(false);
        }
        let _ = session.SetIsCursorCaptureEnabled(false);

        let (tx, rx) = mpsc::channel::<(Direct3D11CaptureFrame, Instant)>();
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |pool, _| {
                if let Some(pool) = pool.as_ref() {
                    if let Ok(frame) = pool.TryGetNextFrame() {
                        let _ = tx.send((frame, Instant::now()));
                    }
                }
                Ok(())
            },
        ))?;
        session.StartCapture()?;
        Ok(FrameStream { pool, session, rx, size: (size.Width, size.Height) })
    }

    /// Copy a frame to the CPU as tightly packed BGRA, cropped to its content size.
    pub fn read_bgra(&self, frame: &Direct3D11CaptureFrame) -> Result<BgraFrame> {
        let content = frame.ContentSize()?;
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            self.d3d.CreateTexture2D(&desc, None, Some(&mut staging))?;
            let staging = staging.expect("CreateTexture2D returned no texture");

            let context = self.d3d.GetImmediateContext()?;
            context.CopyResource(&staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            // The pool texture can be larger than the window if it resized; crop to the content.
            let width = (content.Width as u32).min(desc.Width);
            let height = (content.Height as u32).min(desc.Height);
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * desc.Height as usize);
            let row_len = width as usize * 4;
            let mut data = Vec::with_capacity(row_len * height as usize);
            for y in 0..height as usize {
                data.extend_from_slice(&src[y * pitch..y * pitch + row_len]);
            }
            context.Unmap(&staging, 0);
            Ok(BgraFrame { data, width, height })
        }
    }
}

#[derive(Clone)]
pub struct BgraFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl BgraFrame {
    /// The `width` x `height` region at (`x`, `y`), clamped to this frame. None if it is empty.
    pub fn crop(&self, x: i32, y: i32, width: u32, height: u32) -> Option<BgraFrame> {
        let x0 = x.clamp(0, self.width as i32) as usize;
        let y0 = y.clamp(0, self.height as i32) as usize;
        let x1 = (x + width as i32).clamp(0, self.width as i32) as usize;
        let y1 = (y + height as i32).clamp(0, self.height as i32) as usize;
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let stride = self.width as usize * 4;
        let mut data = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
        for row in y0..y1 {
            data.extend_from_slice(&self.data[row * stride + x0 * 4..row * stride + x1 * 4]);
        }
        Some(BgraFrame { data, width: (x1 - x0) as u32, height: (y1 - y0) as u32 })
    }

    /// Scale down to `width` x `height` by averaging the source pixels each output pixel
    /// covers (keeps text legible when shrinking). Never scales up.
    pub fn downscale(&self, width: u32, height: u32) -> BgraFrame {
        if width >= self.width && height >= self.height {
            return self.clone();
        }
        let (sw, sh) = (self.width as usize, self.height as usize);
        let (dw, dh) = (width.max(1) as usize, height.max(1) as usize);
        let span = |i: usize, src: usize, dst: usize| {
            let start = i * src / dst;
            (start, ((i + 1) * src / dst).max(start + 1))
        };
        let columns: Vec<(usize, usize)> = (0..dw).map(|x| span(x, sw, dw)).collect();
        let mut data = vec![0u8; dw * dh * 4];
        for y in 0..dh {
            let (y0, y1) = span(y, sh, dh);
            for (x, &(x0, x1)) in columns.iter().enumerate() {
                let mut sum = [0u32; 4];
                for sy in y0..y1 {
                    let row = &self.data[(sy * sw + x0) * 4..(sy * sw + x1) * 4];
                    for px in row.chunks_exact(4) {
                        for c in 0..4 {
                            sum[c] += px[c] as u32;
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                let out = &mut data[(y * dw + x) * 4..(y * dw + x) * 4 + 4];
                for c in 0..4 {
                    out[c] = (sum[c] / n) as u8;
                }
            }
        }
        BgraFrame { data, width: dw as u32, height: dh as u32 }
    }
}

/// The largest size with the frame's aspect ratio that fits in `fit`, never larger than the
/// frame itself, with even dimensions (H.264 4:2:0).
pub fn fit_size(frame: (u32, u32), fit: (u32, u32)) -> (u32, u32) {
    let scale = (fit.0 as f64 / frame.0 as f64).min(fit.1 as f64 / frame.1 as f64).min(1.0);
    let w = ((frame.0 as f64 * scale).round() as u32).max(2) & !1;
    let h = ((frame.1 as f64 * scale).round() as u32).max(2) & !1;
    (w, h)
}

/// A running capture session. Frames arrive only when the window's content changes.
pub struct FrameStream {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    rx: mpsc::Receiver<(Direct3D11CaptureFrame, Instant)>,
    size: (i32, i32),
}

impl FrameStream {
    /// Wait up to `timeout` for a frame and return the newest one, dropping older queued frames.
    /// Recreates the pool when the window has been resized so later frames get the new size.
    pub fn latest(
        &mut self,
        capturer: &Capturer,
        timeout: Duration,
    ) -> Option<(Direct3D11CaptureFrame, Instant)> {
        let mut latest = self.rx.recv_timeout(timeout).ok()?;
        while let Ok(next) = self.rx.try_recv() {
            latest = next;
        }
        if let Ok(content) = latest.0.ContentSize() {
            if (content.Width, content.Height) != self.size {
                self.size = (content.Width, content.Height);
                let _ = self.pool.Recreate(
                    &capturer.device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    content,
                );
            }
        }
        Some(latest)
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
