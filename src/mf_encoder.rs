//! H.264 encoding on the GPU through a Media Foundation hardware encoder MFT (e.g. Intel Quick
//! Sync, NVENC, AMF). Frames are converted to NV12 on the CPU (in parallel) and handed to the
//! encoder in system memory.
//!
//! Hardware encoder MFTs are asynchronous: they announce with events when they want input and
//! when output is ready. `encode` drives that as one input in, one output out, which holds with
//! low-latency mode and no B-frames.

use std::mem::ManuallyDrop;

use windows::core::{Error, Interface, Result, PWSTR, VARIANT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
    eAVEncH264VProfile_Base, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFStartup, MFTEnumEx,
    CODECAPI_AVEncCommonQuality, CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode,
    eAVEncCommonRateControlMode_Quality,
    CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    METransformHaveOutput, METransformNeedInput, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::capture::BgraFrame;
use crate::encoder::Picture;

/// MF_VERSION (MF_SDK_VERSION 0x0002 << 16 | MF_API_VERSION 0x0070).
const MF_VERSION: u32 = 0x0002_0070;

pub struct MfEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: ICodecAPI,
    size: (u32, u32),
    bitrate: u32,
    fps: u32,
    nv12: Vec<u8>,
    /// METransformNeedInput events not yet answered with an input.
    wanted: u32,
    outputs: Vec<Picture>,
    frames: u64,
    /// SPS + PPS (Annex B) seen in the stream, re-attached to keyframes that come without them
    /// so that a viewer joining mid-stream can start decoding.
    parameter_sets: Vec<u8>,
    name: String,
}

impl MfEncoder {
    /// Open the first hardware H.264 encoder Media Foundation reports for this size.
    pub fn new(width: u32, height: u32, bitrate: u32, fps: u32) -> Result<Self> {
        let (width, height) = (width & !1, height & !1);
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let (activate, name) = first_hardware_encoder()?;
            let transform: IMFTransform = activate.ActivateObject()?;

            let attributes = transform.GetAttributes()?;
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);

            let codec: ICodecAPI = transform.cast()?;
            // Best effort: encoders differ in what they accept.
            let set = |api, value: VARIANT| codec.SetValue(api, &value);
            let _ = set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            // Constant quality, not a bitrate target: with CBR or VBR the encoder spends the whole
            // budget refining unchanged frames (CBR kept ~7 Mbps flowing for a static screen);
            // here an unchanged frame costs ~80 bytes and the picture is sharper.
            let _ = set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32));
            let _ = set(&CODECAPI_AVEncCommonQuality, VARIANT::from(60u32));
            let _ = set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
            // 0 = fastest. Latency matters more than compression for remote control.
            let _ = set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(0u32));
            // Keyframes only when asked for (new viewer, decode error); the screen is mostly static.
            let _ = set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * 60));

            let size = ((width as u64) << 32) | height as u64;
            let rate = ((fps as u64) << 32) | 1;
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            output.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            transform.SetOutputType(0, &output, 0)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0)?;

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = transform.cast()?;

            Ok(Self {
                transform,
                events,
                codec,
                size: (width, height),
                bitrate,
                fps,
                nv12: Vec::new(),
                wanted: 0,
                outputs: Vec::new(),
                frames: 0,
                parameter_sets: Vec::new(),
                name,
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Encode a new frame (its size must match the encoder's; callers recreate on resize).
    pub fn encode(&mut self, frame: &BgraFrame, keyframe: bool) -> Result<Picture> {
        bgra_to_nv12(frame, self.size, &mut self.nv12);
        self.submit(keyframe)
    }

    /// Encode the previous frame again (keepalive while the window is static).
    pub fn encode_last(&mut self, keyframe: bool) -> Result<Picture> {
        self.submit(keyframe)
    }

    fn submit(&mut self, keyframe: bool) -> Result<Picture> {
        unsafe {
            if keyframe {
                let _ = self.codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
            while self.wanted == 0 {
                self.next_event()?;
            }

            let len = self.nv12.len() as u32;
            let buffer = MFCreateMemoryBuffer(len)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let duration = 10_000_000 / self.fps as i64;
            sample.SetSampleTime(self.frames as i64 * duration)?;
            sample.SetSampleDuration(duration)?;
            self.transform.ProcessInput(0, &sample, 0)?;
            self.wanted -= 1;
            self.frames += 1;

            while self.outputs.is_empty() {
                self.next_event()?;
            }
        }
        // Normally exactly one; join them if the encoder split a frame.
        let mut pictures = std::mem::take(&mut self.outputs).into_iter();
        let mut picture = pictures.next().unwrap();
        for more in pictures {
            picture.keyframe |= more.keyframe;
            picture.data.extend_from_slice(&more.data);
        }
        Ok(picture)
    }

    /// Wait for the encoder's next event and act on it.
    unsafe fn next_event(&mut self) -> Result<()> {
        let event = self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))?;
        let kind = event.GetType()?;
        if kind == METransformNeedInput.0 as u32 {
            self.wanted += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            self.take_output()?;
        }
        Ok(())
    }

    unsafe fn take_output(&mut self) -> Result<()> {
        let info = self.transform.GetOutputStreamInfo(0)?;
        let sample: Option<IMFSample> = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
            None
        } else {
            let sample = MFCreateSample()?;
            sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
            Some(sample)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
        drop(ManuallyDrop::take(&mut buffers[0].pEvents));
        match result {
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder settled its output format; accept what it proposes.
                let proposed = self.transform.GetOutputAvailableType(0, 0)?;
                return self.transform.SetOutputType(0, &proposed, 0);
            }
            Err(e) => return Err(e),
            Ok(()) => {}
        }
        let Some(sample) = sample else { return Ok(()) };

        let buffer = sample.ConvertToContiguousBuffer()?;
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let mut data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock()?;

        let nal_types: Vec<u8> = nal_units(&data).map(|n| n[0] & 0x1f).collect();
        let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 || nal_types.contains(&5);
        if nal_types.contains(&7) {
            self.parameter_sets = nal_units(&data)
                .filter(|n| matches!(n[0] & 0x1f, 7 | 8))
                .flat_map(|n| [&[0, 0, 0, 1][..], n].concat())
                .collect();
        } else if keyframe && !self.parameter_sets.is_empty() {
            data = [self.parameter_sets.clone(), data].concat();
        }
        self.outputs.push(Picture { data, keyframe });
        Ok(())
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // MFShutdown is left alone: MFStartup is reference-counted per call, and the process keeps
        // making encoders.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

/// The best hardware H.264 encoder MFT for NV12 input, and its friendly name.
unsafe fn first_hardware_encoder() -> Result<(IMFActivate, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &mut list,
        &mut count,
    )?;
    let activates = std::slice::from_raw_parts_mut(list, count as usize);
    let first = activates.first().and_then(|a| a.clone());
    for a in activates.iter_mut() {
        std::ptr::drop_in_place(a);
    }
    CoTaskMemFree(Some(list as *const _));
    let activate = first.ok_or_else(|| Error::new(E_FAIL, "no hardware H.264 encoder"))?;

    let mut name = PWSTR::null();
    let mut len = 0u32;
    let friendly = if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len).is_ok() {
        let s = name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(name.0 as *const _));
        s
    } else {
        "hardware H.264 encoder".into()
    };
    Ok((activate, friendly))
}

/// NAL unit payloads of an Annex B stream (start codes stripped).
fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // Back over the start code, including the 4-byte form's extra zero.
            let mut e = s - 3;
            if e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts.into_iter().zip(ends).filter(|(s, e)| s < e).map(move |(s, e)| &data[s..e])
}

/// BGRA (top-left `size` region) to NV12 with BT.601 limited-range coefficients, split across
/// threads by row pairs.
pub fn bgra_to_nv12(frame: &BgraFrame, size: (u32, u32), out: &mut Vec<u8>) {
    let (w, h) = (size.0 as usize, size.1 as usize);
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let stride = frame.width as usize * 4;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let pairs = h / 2;
    let per_thread = pairs.div_ceil(threads);
    std::thread::scope(|scope| {
        let chunks = y_plane.chunks_mut(w * 2 * per_thread).zip(uv_plane.chunks_mut(w * per_thread));
        for (n, (y_chunk, uv_chunk)) in chunks.enumerate() {
            let first_pair = n * per_thread;
            let src = &frame.data;
            scope.spawn(move || {
                for p in 0..y_chunk.len() / (w * 2) {
                    let row = (first_pair + p) * 2;
                    for x in (0..w).step_by(2) {
                        let mut sum = [0i32; 3];
                        for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                            let i = (row + dy) * stride + (x + dx) * 4;
                            let (b, g, r) = (src[i] as i32, src[i + 1] as i32, src[i + 2] as i32);
                            y_chunk[(p * 2 + dy) * w + x + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                            sum[0] += r;
                            sum[1] += g;
                            sum[2] += b;
                        }
                        let (r, g, b) = (sum[0] / 4, sum[1] / 4, sum[2] / 4);
                        uv_chunk[p * w + x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                        uv_chunk[p * w + x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
                    }
                }
            });
        }
    });
}
