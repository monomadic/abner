//! Host-owned CPU conversions. Matching layouts return the original allocation.
use abner_processor_api::{FramePixels, PixelFormat, Result, VideoFrame};
use filmcraft_frame::{Chroma, PixelData};
use std::sync::Arc;

/// Preserve supported decoder planes; other SDR layouts take the explicit RGB16 fallback.
pub fn from_decoded(
    frame: &filmcraft_frame::VideoFrame,
    pts: i64,
    duration: i64,
    time_base: [u32; 2],
) -> Result<VideoFrame> {
    use PixelFormat::*;
    let color = frame.color;
    if color.primaries != filmcraft_color::Primaries::Bt709
        || color.transfer != filmcraft_color::Transfer::Bt709
    {
        return Err("requires SDR Rec.709".into());
    }
    let planar = color.matrix == filmcraft_color::Matrix::Bt709
        && color.range == filmcraft_color::Range::Limited;
    let (format, pixels) = match &frame.data {
        PixelData::Rgba8(p) => (Rgba8, FramePixels::Rgba8(p.clone())),
        PixelData::Yuv8 {
            planes,
            chroma: Chroma::C420,
            alpha: None,
        } if planar => (Yuv420p8, FramePixels::Yuv8(planes.clone())),
        PixelData::Yuv16 {
            planes,
            chroma,
            bits: 10,
            alpha: None,
        } if planar => (
            match chroma {
                Chroma::C420 => Yuv420p10,
                Chroma::C422 => Yuv422p10,
                Chroma::C444 => Yuv444p10,
            },
            FramePixels::Yuv16(planes.clone()),
        ),
        _ => (Rgb16, FramePixels::Rgb16(crate::media::rgb16(frame)?)),
    };
    let out = VideoFrame {
        width: frame.width as usize,
        height: frame.height as usize,
        format,
        pixels,
        pts,
        duration,
        time_base,
    };
    out.validate()?;
    Ok(out)
}
fn decoded(frame: &VideoFrame) -> Result<filmcraft_frame::VideoFrame> {
    let chroma = match frame.format.subsampling() {
        Some((2, 2)) => Chroma::C420,
        Some((2, 1)) => Chroma::C422,
        _ => Chroma::C444,
    };
    let data = match &frame.pixels {
        FramePixels::Rgba8(p) => PixelData::Rgba8(p.clone()),
        FramePixels::Yuv8(p) => PixelData::Yuv8 {
            planes: p.clone(),
            chroma,
            alpha: None,
        },
        FramePixels::Yuv16(p) => PixelData::Yuv16 {
            planes: p.clone(),
            chroma,
            bits: 10,
            alpha: None,
        },
        _ => return Err("RGB16 already has native storage".into()),
    };
    Ok(filmcraft_frame::VideoFrame {
        width: frame.width as u32,
        height: frame.height as u32,
        data,
        color: filmcraft_color::ColorInfo::REC709,
        par: (1, 1),
        pts: filmcraft_time::Tick::ZERO,
    })
}
pub fn convert(mut frame: VideoFrame, target: PixelFormat) -> Result<VideoFrame> {
    frame.validate()?;
    if frame.format == target {
        return Ok(frame);
    }
    if let (Some((ix, iy)), Some((ox, oy))) = (frame.format.subsampling(), target.subsampling()) {
        // Resample encoded YUV planes directly. Preserve legal headroom/footroom;
        // no RGB clipping or matrix round trip is needed for a layout change.
        let (w, h) = (frame.width, frame.height);
        let (icw, ocw, och) = (w.div_ceil(ix), w.div_ceil(ox), h.div_ceil(oy));
        let sample = |plane: usize, index: usize| -> u32 {
            match &frame.pixels {
                FramePixels::Yuv8(p) => u32::from(p[plane][index]),
                FramePixels::Yuv16(p) => u32::from(p[plane][index]),
                _ => unreachable!(),
            }
        };
        let from_scale = 1u32 << (frame.format.depth() - 8);
        let to_scale = 1u32 << (target.depth() - 8);
        let quantize = |sum: u32, count: u32| -> u16 {
            ((sum * to_scale + count * from_scale / 2) / (count * from_scale))
                .min((1 << target.depth()) - 1) as u16
        };
        let mut p = [
            (0..w * h)
                .map(|i| quantize(sample(0, i), 1))
                .collect::<Vec<_>>(),
            vec![0; ocw * och],
            vec![0; ocw * och],
        ];
        for cy in 0..och {
            for cx in 0..ocw {
                let mut sum = [0; 2];
                let mut count = 0;
                for y in cy * oy..((cy + 1) * oy).min(h) {
                    for x in cx * ox..((cx + 1) * ox).min(w) {
                        let index = (y / iy) * icw + x / ix;
                        sum[0] += sample(1, index);
                        sum[1] += sample(2, index);
                        count += 1;
                    }
                }
                for c in 0..2 {
                    p[c + 1][cy * ocw + cx] = quantize(sum[c], count);
                }
            }
        }
        frame.pixels = if target.depth() == 8 {
            FramePixels::Yuv8(p.map(|p| Arc::new(p.into_iter().map(|v| v as u8).collect())))
        } else {
            FramePixels::Yuv16(p.map(Arc::new))
        };
        frame.format = target;
        return Ok(frame);
    }
    let rgb = match &frame.pixels {
        FramePixels::Rgb16(_) => {
            match std::mem::replace(&mut frame.pixels, FramePixels::Rgb16(vec![])) {
                FramePixels::Rgb16(p) => p,
                _ => unreachable!(),
            }
        }
        _ => crate::media::rgb16(&decoded(&frame)?)?,
    };
    let pixels = match target {
        PixelFormat::Rgb16 => FramePixels::Rgb16(rgb),
        PixelFormat::Rgba8 => FramePixels::Rgba8(Arc::new(
            rgb.as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| {
                    [
                        ((u32::from(p[0]) + 128) / 257) as u8,
                        ((u32::from(p[1]) + 128) / 257) as u8,
                        ((u32::from(p[2]) + 128) / 257) as u8,
                        255,
                    ]
                })
                .collect(),
        )),
        PixelFormat::Yuv422p10 => {
            // Preserve the established RGB16 -> ProRes numeric conversion.
            let p = crate::media::prores_frame(&rgb, frame.width, frame.height)?;
            FramePixels::Yuv16([Arc::new(p.y), Arc::new(p.cb), Arc::new(p.cr)])
        }
        _ => {
            let (sx, sy) = target.subsampling().unwrap();
            let (w, h) = (frame.width, frame.height);
            let (cw, ch) = (w.div_ceil(sx), h.div_ceil(sy));
            let mut planes = [vec![0u16; w * h], vec![0u16; cw * ch], vec![0u16; cw * ch]];
            let scale = if target.depth() == 8 { 1. } else { 4. };
            for cy in 0..ch {
                for cx in 0..cw {
                    let mut uv = [0f32; 2];
                    let mut count = 0;
                    for y in cy * sy..((cy + 1) * sy).min(h) {
                        for x in cx * sx..((cx + 1) * sx).min(w) {
                            let p = (y * w + x) * 3;
                            let c = filmcraft_color::rgb_to_ycbcr(
                                rgb[p] as f32 / 65535.,
                                rgb[p + 1] as f32 / 65535.,
                                rgb[p + 2] as f32 / 65535.,
                                filmcraft_color::Matrix::Bt709,
                            );
                            planes[0][y * w + x] = ((16. + 219. * c[0]) * scale)
                                .round_ties_even()
                                .clamp(16. * scale, 235. * scale)
                                as u16;
                            uv[0] += c[1];
                            uv[1] += c[2];
                            count += 1;
                        }
                    }
                    for c in 0..2 {
                        planes[c + 1][cy * cw + cx] = ((128. + 224. * uv[c] / count as f32) * scale)
                            .round_ties_even()
                            .clamp(16. * scale, 240. * scale)
                            as u16;
                    }
                }
            }
            if target.depth() == 8 {
                FramePixels::Yuv8(
                    planes.map(|p| Arc::new(p.into_iter().map(|v| v as u8).collect())),
                )
            } else {
                FramePixels::Yuv16(planes.map(Arc::new))
            }
        }
    };
    frame.format = target;
    frame.pixels = pixels;
    Ok(frame)
}
/// ProRes can consume matching YUV planes directly; no YUV→RGB→YUV round trip.
pub fn into_prores(frame: VideoFrame) -> Result<filmcraft_prores::Frame> {
    let frame = convert(frame, PixelFormat::Yuv422p10)?;
    let FramePixels::Yuv16(planes) = frame.pixels else {
        unreachable!()
    };
    let mut out =
        filmcraft_prores::Frame::new(0, 0, filmcraft_prores::ChromaFormat::Yuv422, 10, false);
    let [y, cb, cr] = planes.map(|p| Arc::try_unwrap(p).unwrap_or_else(|p| (*p).clone()));
    out.width = frame.width as u32;
    out.height = frame.height as u32;
    out.y = y;
    out.cb = cb;
    out.cr = cr;
    Ok(out)
}

/// Display branch from the processor's native output. Borrow RGB16 instead of
/// cloning a full-resolution buffer merely to quantize it for the display.
pub fn rgba8(frame: &VideoFrame) -> Result<Arc<Vec<u8>>> {
    frame.validate()?;
    match &frame.pixels {
        FramePixels::Rgba8(p) => Ok(p.clone()),
        FramePixels::Rgb16(p) => Ok(Arc::new(
            p.as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| {
                    [
                        ((u32::from(p[0]) + 128) / 257) as u8,
                        ((u32::from(p[1]) + 128) / 257) as u8,
                        ((u32::from(p[2]) + 128) / 257) as u8,
                        255,
                    ]
                })
                .collect(),
        )),
        _ => {
            let FramePixels::Rgba8(p) = convert(frame.clone(), PixelFormat::Rgba8)?.pixels else {
                unreachable!()
            };
            Ok(p)
        }
    }
}
