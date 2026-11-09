//! CPU frame layouts. All use encoded SDR Rec.709, opaque pixels and no padding.
//! YUV is limited-range BT.709 with block-aligned chroma; RGB is full-range.
use crate::{Frame, Result};
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PixelFormat {
    Rgb16,
    Rgba8,
    Yuv420p8,
    Yuv420p10,
    Yuv422p10,
    Yuv444p10,
}
impl PixelFormat {
    pub fn depth(self) -> u32 {
        match self {
            Self::Rgb16 => 16,
            Self::Rgba8 | Self::Yuv420p8 => 8,
            _ => 10,
        }
    }
    pub fn subsampling(self) -> Option<(usize, usize)> {
        match self {
            Self::Yuv420p8 | Self::Yuv420p10 => Some((2, 2)),
            Self::Yuv422p10 => Some((2, 1)),
            Self::Yuv444p10 => Some((1, 1)),
            _ => None,
        }
    }
}
/// An executable input/output pair, not two independent lists. The host selects
/// one before processing and keeps it for the session's entire lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FormatPair {
    pub input: PixelFormat,
    pub output: PixelFormat,
}
impl FormatPair {
    pub const RGB16: Self = Self::same(PixelFormat::Rgb16);
    pub const fn same(format: PixelFormat) -> Self {
        Self {
            input: format,
            output: format,
        }
    }
}
#[derive(Clone)]
pub enum FramePixels {
    Rgb16(Vec<u16>),
    Rgba8(Arc<Vec<u8>>),
    Yuv8([Arc<Vec<u8>>; 3]),
    Yuv16([Arc<Vec<u16>>; 3]),
}
#[derive(Clone)]
pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub format: PixelFormat,
    pub pixels: FramePixels,
    pub pts: i64,
    pub duration: i64,
    pub time_base: [u32; 2],
}
impl From<Frame> for VideoFrame {
    fn from(f: Frame) -> Self {
        Self {
            width: f.width,
            height: f.height,
            format: PixelFormat::Rgb16,
            pixels: FramePixels::Rgb16(f.pixels),
            pts: f.pts,
            duration: f.duration,
            time_base: f.time_base,
        }
    }
}
impl VideoFrame {
    pub fn into_rgb16(self) -> Result<Frame> {
        self.validate()?;
        let FramePixels::Rgb16(pixels) = self.pixels else {
            return Err("host must convert to RGB16".into());
        };
        Ok(Frame {
            width: self.width,
            height: self.height,
            pixels,
            pts: self.pts,
            duration: self.duration,
            time_base: self.time_base,
        })
    }
    pub fn byte_len(&self) -> usize {
        match &self.pixels {
            FramePixels::Rgb16(p) => p.len() * 2,
            FramePixels::Rgba8(p) => p.len(),
            FramePixels::Yuv8(p) => p.iter().map(|p| p.len()).sum(),
            FramePixels::Yuv16(p) => p.iter().map(|p| p.len() * 2).sum(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        use PixelFormat::*;
        if self.width == 0
            || self.height == 0
            || self.width > 8192
            || self.height > 8192
            || self.duration <= 0
            || self.time_base.contains(&0)
        {
            return Err("invalid frame geometry or timing".into());
        }
        let n = self.width * self.height;
        let chroma = self
            .format
            .subsampling()
            .map(|(x, y)| self.width.div_ceil(x) * self.height.div_ceil(y));
        let valid = match (&self.pixels, self.format) {
            (FramePixels::Rgb16(p), Rgb16) => p.len() == n * 3,
            (FramePixels::Rgba8(p), Rgba8) => {
                p.len() == n * 4 && p.as_chunks::<4>().0.iter().all(|p| p[3] == 255)
            }
            (FramePixels::Yuv8(p), Yuv420p8) => {
                p[0].len() == n && p[1].len() == chroma.unwrap() && p[2].len() == chroma.unwrap()
            }
            (FramePixels::Yuv16(p), Yuv420p10 | Yuv422p10 | Yuv444p10) => {
                p[0].len() == n
                    && p[1].len() == chroma.unwrap()
                    && p[2].len() == chroma.unwrap()
                    && p.iter().all(|p| p.iter().all(|&v| v <= 1023))
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err("frame storage does not match negotiated format (or contains alpha)".into())
        }
    }
}
