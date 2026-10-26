//! H.264 encoding of captured BGRA frames: the GPU encoder through Media Foundation when
//! available ([`VideoEncoder`]), OpenH264 in software (screen-content mode) otherwise.

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;

use crate::capture::BgraFrame;
use crate::mf_encoder::MfEncoder;

/// The encoder the host uses: the GPU's (Media Foundation) when there is one, else OpenH264.
pub enum VideoEncoder {
    Hardware(MfEncoder),
    Software(H264Encoder),
}

impl VideoEncoder {
    pub fn new(width: u32, height: u32, bitrate: u32, fps: f32) -> Result<Self, String> {
        match MfEncoder::new(width, height, bitrate, fps.round() as u32) {
            Ok(hw) => Ok(Self::Hardware(hw)),
            Err(e) => {
                eprintln!("hardware encoder unavailable ({e}); using OpenH264");
                H264Encoder::new(width, height, bitrate, fps).map(Self::Software).map_err(|e| e.to_string())
            }
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Hardware(hw) => hw.name().to_string(),
            Self::Software(_) => "OpenH264 (software)".into(),
        }
    }

    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Hardware(hw) => hw.size(),
            Self::Software(sw) => sw.size(),
        }
    }

    /// Encode a new frame. A frame of another size gets a fresh encoder of the same kind.
    pub fn encode(&mut self, frame: &BgraFrame, keyframe: bool) -> Result<Picture, String> {
        match self {
            Self::Hardware(hw) => {
                if even(frame.width, frame.height) != hw.size() {
                    *hw = MfEncoder::new(frame.width, frame.height, hw.bitrate(), hw.fps()).map_err(|e| e.to_string())?;
                }
                hw.encode(frame, keyframe).map_err(|e| e.to_string())
            }
            Self::Software(sw) => sw.encode(frame, keyframe).map_err(|e| e.to_string()),
        }
    }

    /// Encode the previous frame again (keepalive while the window is static).
    pub fn encode_last(&mut self, keyframe: bool) -> Result<Picture, String> {
        match self {
            Self::Hardware(hw) => hw.encode_last(keyframe).map_err(|e| e.to_string()),
            Self::Software(sw) => sw.encode_last(keyframe).map_err(|e| e.to_string()),
        }
    }
}

/// One encoded access unit: Annex B NAL units (start-code delimited).
pub struct Picture {
    pub data: Vec<u8>,
    /// Decodable on its own (IDR / I); a viewer can start here.
    pub keyframe: bool,
}

pub struct H264Encoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    size: (u32, u32),
    bitrate: u32,
    max_fps: f32,
}

impl H264Encoder {
    pub fn new(width: u32, height: u32, bitrate: u32, max_fps: f32) -> Result<Self, openh264::Error> {
        let (width, height) = even(width, height);
        Ok(Self {
            encoder: make_encoder(bitrate, max_fps)?,
            yuv: YUVBuffer::new(width as usize, height as usize),
            size: (width, height),
            bitrate,
            max_fps,
        })
    }

    /// Encode a new frame.
    pub fn encode(&mut self, frame: &BgraFrame, keyframe: bool) -> Result<Picture, openh264::Error> {
        let (width, height) = even(frame.width, frame.height);
        if (width, height) != self.size {
            // New window size: start over with a fresh encoder so the stream opens with SPS/PPS.
            *self = Self::new(width, height, self.bitrate, self.max_fps)?;
        }
        if (width, height) == (frame.width, frame.height) {
            self.yuv.read_rgb(BgraSliceU8::new(&frame.data, (width as usize, height as usize)));
        } else {
            let cropped = crop(frame, width, height);
            self.yuv.read_rgb(BgraSliceU8::new(&cropped, (width as usize, height as usize)));
        }
        self.encode_last(keyframe)
    }

    /// Encode the previous frame again (keepalive while the window is static).
    pub fn encode_last(&mut self, keyframe: bool) -> Result<Picture, openh264::Error> {
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let bitstream = self.encoder.encode(&self.yuv)?;
        Ok(Picture {
            keyframe: matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I),
            data: bitstream.to_vec(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}

fn make_encoder(bitrate: u32, max_fps: f32) -> Result<Encoder, openh264::Error> {
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(max_fps))
        .skip_frames(false)
        // Software encoding of a full window is the main latency cost; trade quality for speed.
        .complexity(Complexity::Low)
        .num_threads(4);
    Encoder::with_api_config(OpenH264API::from_source(), config)
}

/// 4:2:0 chroma needs even dimensions.
fn even(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

fn crop(frame: &BgraFrame, width: u32, height: u32) -> Vec<u8> {
    let src_row = frame.width as usize * 4;
    let dst_row = width as usize * 4;
    let mut out = Vec::with_capacity(dst_row * height as usize);
    for y in 0..height as usize {
        out.extend_from_slice(&frame.data[y * src_row..y * src_row + dst_row]);
    }
    out
}
