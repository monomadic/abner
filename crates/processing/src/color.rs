//! Host-owned CPU conversions. Matching layouts return the original allocation.
use abner_processor_api::{FramePixels, PixelFormat, Result, VideoFrame};
use filmcraft_frame::{Chroma, PixelData};
use rayon::prelude::*;
use std::sync::Arc;

/// Preserve supported Rec.709 decoder planes; other layouts, wide gamut and HDR take the
/// explicit SDR Rec.709 RGB16 conversion.
pub fn from_decoded(
    frame: &filmcraft_frame::VideoFrame,
    pts: i64,
    duration: i64,
    time_base: [u32; 2],
) -> Result<VideoFrame> {
    use PixelFormat::*;
    let color = frame.color;
    let rec709 = color.primaries == filmcraft_color::Primaries::Bt709
        && color.transfer == filmcraft_color::Transfer::Bt709;
    let planar = rec709
        && color.matrix == filmcraft_color::Matrix::Bt709
        && color.range == filmcraft_color::Range::Limited;
    let (format, pixels) = match &frame.data {
        PixelData::RgbaF32(p) if rec709 => (
            RgbF32,
            FramePixels::RgbF32(Arc::new(
                p.as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| p[..3].iter().copied())
                    .collect(),
            )),
        ),
        PixelData::Rgba8(p) if rec709 => (Rgba8, FramePixels::Rgba8(p.clone())),
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
/// Rows of pixels per parallel task: big enough that scheduling is noise next
/// to the arithmetic, small enough that a 4K frame splits across every core.
const TASK_PIXELS: usize = 8192;

/// Map packed `in_n`-sample pixels to packed `out_n`-sample pixels on every core.
/// Pixel order and per-pixel arithmetic are the caller's, so results are identical
/// to the serial loop.
pub(crate) fn map_pixels<I: Sync, O: Send + Copy + Default>(
    input: &[I],
    in_n: usize,
    out_n: usize,
    f: impl Fn(&[I], &mut [O]) + Sync,
) -> Vec<O> {
    let mut out = vec![O::default(); input.len() / in_n * out_n];
    out.par_chunks_mut(TASK_PIXELS * out_n)
        .zip(input.par_chunks(TASK_PIXELS * in_n))
        .for_each(|(o, i)| {
            for (o, i) in o.chunks_exact_mut(out_n).zip(i.chunks_exact(in_n)) {
                f(i, o)
            }
        });
    out
}

/// Planar Y'CbCr at `(sx, sy)` subsampling, one chroma row per parallel task.
/// `px(i)` returns pixel `i`'s luma code and its chroma contribution; `chroma`
/// turns a block's summed contribution and pixel count into the two codes.
/// Blocks are summed in the serial loop's order, so floats round identically.
pub(crate) fn subsample(
    w: usize,
    h: usize,
    (sx, sy): (usize, usize),
    px: impl Fn(usize) -> (u16, [f32; 2]) + Sync,
    chroma: impl Fn([f32; 2], usize) -> [u16; 2] + Sync,
) -> [Vec<u16>; 3] {
    let (cw, ch) = (w.div_ceil(sx), h.div_ceil(sy));
    let mut planes = [vec![0u16; w * h], vec![0; cw * ch], vec![0; cw * ch]];
    let [luma, u, v] = &mut planes;
    luma.par_chunks_mut(w * sy)
        .zip(u.par_chunks_mut(cw).zip(v.par_chunks_mut(cw)))
        .enumerate()
        .for_each(|(cy, (luma, (u, v)))| {
            for cx in 0..cw {
                let mut uv = [0f32; 2];
                let mut count = 0;
                for y in cy * sy..((cy + 1) * sy).min(h) {
                    for x in cx * sx..((cx + 1) * sx).min(w) {
                        let (l, c) = px(y * w + x);
                        luma[(y - cy * sy) * w + x] = l;
                        uv[0] += c[0];
                        uv[1] += c[1];
                        count += 1;
                    }
                }
                [u[cx], v[cx]] = chroma(uv, count);
            }
        });
    planes
}

fn narrow(planes: [Vec<u16>; 3]) -> FramePixels {
    FramePixels::Yuv8(planes.map(|p| Arc::new(p.into_par_iter().map(|v| v as u8).collect())))
}

/// One Y'CbCr sample of a planar frame, whatever its storage depth.
#[derive(Clone, Copy)]
enum Planes<'a> {
    U8(&'a [Arc<Vec<u8>>; 3]),
    U16(&'a [Arc<Vec<u16>>; 3]),
}
impl Planes<'_> {
    fn of(pixels: &FramePixels) -> Planes<'_> {
        match pixels {
            FramePixels::Yuv8(p) => Planes::U8(p),
            FramePixels::Yuv16(p) => Planes::U16(p),
            _ => unreachable!(),
        }
    }
    #[inline(always)]
    fn get(self, plane: usize, index: usize) -> u32 {
        match self {
            Planes::U8(p) => u32::from(p[plane][index]),
            Planes::U16(p) => u32::from(p[plane][index]),
        }
    }
}

/// A host planar frame (BT.709 limited range by construction, see
/// [`from_decoded`]) to packed RGB in one pass: each pixel goes through
/// `media::rgb16`'s arithmetic to its RGB16 value, and `put` stores it (as
/// is, or narrowed). No RGB16 frame in between, and `sample` is a generic
/// closure the compiler inlines, not `rgb16`'s `&dyn Fn` per sample: the
/// same bytes, `print_conversion_digests` unchanged.
fn planar_rgb<O: Send + Copy + Default>(
    frame: &VideoFrame,
    n: usize,
    sample: impl Fn(usize, usize) -> u32 + Sync,
    put: impl Fn([u16; 3], &mut [O]) + Sync,
) -> Vec<O> {
    use filmcraft_color::{Matrix, Range, normalize_c, normalize_y, ycbcr_to_rgb};
    let (w, h) = (frame.width, frame.height);
    let (sx, sy) = frame.format.subsampling().map_or((0, 0), |(x, y)| (x / 2, y / 2));
    let cw = w.div_ceil(1 << sx);
    let bits = frame.format.depth();
    // Every code's normalised value, computed by the same functions: the same
    // f32s, without a division and a conversion per sample. A validated frame
    // has no code past the table; `min` only keeps an invalid one in bounds.
    let max = (1u32 << bits) - 1;
    let ny: Vec<f32> = (0..=max).map(|v| normalize_y(v, bits, Range::Limited)).collect();
    let nc: Vec<f32> = (0..=max).map(|v| normalize_c(v, bits, Range::Limited)).collect();
    let code = |v: u32| v.min(max) as usize;
    let mut out = vec![O::default(); w * h * n];
    out.par_chunks_mut(w * n).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let (p, q) = (y * w + x, (y >> sy) * cw + (x >> sx));
            let rgb = ycbcr_to_rgb(
                ny[code(sample(0, p))],
                nc[code(sample(1, q))],
                nc[code(sample(2, q))],
                Matrix::Bt709,
            );
            put(
                rgb.map(|v| (v.clamp(0., 1.) * 65535.).round_ties_even() as u16),
                &mut row[x * n..x * n + n],
            );
        }
    });
    out
}
/// [`planar_rgb`] for either storage depth.
fn planar_rgb_any<O: Send + Copy + Default>(
    frame: &VideoFrame,
    n: usize,
    put: impl Fn([u16; 3], &mut [O]) + Sync,
) -> Vec<O> {
    match &frame.pixels {
        FramePixels::Yuv8(p) => planar_rgb(frame, n, |c, i| u32::from(p[c][i]), put),
        FramePixels::Yuv16(p) => planar_rgb(frame, n, |c, i| u32::from(p[c][i]), put),
        _ => unreachable!("planar_rgb takes planar frames"),
    }
}
/// RGB16 → RGBA8, one sample: what [`rgb16_to_rgba8`] does.
#[inline(always)]
fn narrow_rgb16(v: [u16; 3], o: &mut [u8]) {
    for c in 0..3 {
        o[c] = ((u32::from(v[c]) + 128) / 257) as u8;
    }
    o[3] = 255;
}
fn rgb16_to_rgba8(rgb: &[u16]) -> Vec<u8> {
    map_pixels(rgb, 3, 4, |p, o: &mut [u8]| {
        for c in 0..3 {
            o[c] = ((u32::from(p[c]) + 128) / 257) as u8;
        }
        o[3] = 255;
    })
}

pub fn convert(frame: VideoFrame, target: PixelFormat) -> Result<VideoFrame> {
    frame.validate()?;
    convert_validated(frame, target)
}

/// [`convert`] for a frame the caller has already validated: validation reads
/// every sample, as much as some conversions do, so a pipeline pays it once
/// per frame rather than once per step.
pub fn convert_validated(mut frame: VideoFrame, target: PixelFormat) -> Result<VideoFrame> {
    if frame.format == target {
        return Ok(frame);
    }
    if let (Some((ix, iy)), Some((ox, oy))) = (frame.format.subsampling(), target.subsampling()) {
        // Resample encoded YUV planes directly. Preserve legal headroom/footroom;
        // no RGB clipping or matrix round trip is needed for a layout change.
        let (w, h) = (frame.width, frame.height);
        let (icw, ocw, och) = (w.div_ceil(ix), w.div_ceil(ox), h.div_ceil(oy));
        let planes = Planes::of(&frame.pixels);
        let from_scale = 1u32 << (frame.format.depth() - 8);
        let to_scale = 1u32 << (target.depth() - 8);
        let quantize = |sum: u32, count: u32| -> u16 {
            ((sum * to_scale + count * from_scale / 2) / (count * from_scale))
                .min((1 << target.depth()) - 1) as u16
        };
        let luma: Vec<u16> = (0..w * h)
            .into_par_iter()
            .map(|i| quantize(planes.get(0, i), 1))
            .collect();
        let (mut u, mut v) = (vec![0; ocw * och], vec![0; ocw * och]);
        u.par_chunks_mut(ocw)
            .zip(v.par_chunks_mut(ocw))
            .enumerate()
            .for_each(|(cy, (ur, vr))| {
                for cx in 0..ocw {
                    let mut sum = [0; 2];
                    let mut count = 0;
                    for y in cy * oy..((cy + 1) * oy).min(h) {
                        for x in cx * ox..((cx + 1) * ox).min(w) {
                            let index = (y / iy) * icw + x / ix;
                            sum[0] += planes.get(1, index);
                            sum[1] += planes.get(2, index);
                            count += 1;
                        }
                    }
                    ur[cx] = quantize(sum[0], count);
                    vr[cx] = quantize(sum[1], count);
                }
            });
        let p = [luma, u, v];
        frame.pixels = if target.depth() == 8 { narrow(p) } else { FramePixels::Yuv16(p.map(Arc::new)) };
        frame.format = target;
        return Ok(frame);
    }
    if target == PixelFormat::RgbF32 {
        let samples = match &frame.pixels {
            FramePixels::Rgb16(p) => p.par_iter().map(|&v| v as f32 / 65535.0).collect(),
            FramePixels::Rgba8(p) => map_pixels(p, 4, 3, |p, o: &mut [f32]| {
                for c in 0..3 {
                    o[c] = p[c] as f32 / 255.0;
                }
            }),
            FramePixels::Yuv8(_) | FramePixels::Yuv16(_) => {
                let (sx, sy) = frame.format.subsampling().unwrap();
                let (w, cw) = (frame.width, frame.width.div_ceil(sx));
                let scale = if frame.format.depth() == 8 { 1.0 } else { 4.0 };
                let planes = Planes::of(&frame.pixels);
                let mut rgb = vec![0f32; w * frame.height * 3];
                rgb.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
                    for x in 0..w {
                        let l = (planes.get(0, y * w + x) as f32 / scale - 16.0) / 219.0;
                        let u = (planes.get(1, (y / sy) * cw + x / sx) as f32 / scale - 128.0) / 224.0;
                        let v = (planes.get(2, (y / sy) * cw + x / sx) as f32 / scale - 128.0) / 224.0;
                        row[x * 3..x * 3 + 3].copy_from_slice(&[
                            l + 1.5748 * v,
                            l - 0.187324 * u - 0.468124 * v,
                            l + 1.8556 * u,
                        ]);
                    }
                });
                rgb
            }
            FramePixels::RgbF32(_) => unreachable!(),
        };
        frame.format = target;
        frame.pixels = FramePixels::RgbF32(Arc::new(samples));
        return Ok(frame);
    }
    if let FramePixels::RgbF32(rgb) = &frame.pixels {
        if target == PixelFormat::Rgba8 {
            frame.pixels = FramePixels::Rgba8(Arc::new(map_pixels(rgb, 3, 4, |p, o: &mut [u8]| {
                for c in 0..3 {
                    o[c] = (p[c].clamp(0.0, 1.0) * 255.0).round_ties_even() as u8;
                }
                o[3] = 255;
            })));
            frame.format = target;
            return Ok(frame);
        }
    }
    // Quantize float samples only at the requested sink, never via RGB16.
    if let (FramePixels::RgbF32(rgb), Some(sub)) = (&frame.pixels, target.subsampling()) {
        let scale = if target.depth() == 8 { 1.0 } else { 4.0 };
        let max = ((1u32 << target.depth()) - 1) as f32;
        let q = |v: f32| v.round_ties_even().clamp(0.0, max) as u16;
        let planes = subsample(
            frame.width,
            frame.height,
            sub,
            |i| {
                let [r, g, b] = [rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]];
                let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                (q((16.0 + 219.0 * l) * scale), [(b - l) / 1.8556, (r - l) / 1.5748])
            },
            |uv, count| uv.map(|c| q((128.0 + 224.0 * c / count as f32) * scale)),
        );
        frame.pixels = if target.depth() == 8 { narrow(planes) } else { FramePixels::Yuv16(planes.map(Arc::new)) };
        frame.format = target;
        return Ok(frame);
    }
    // Planar to packed RGB in one pass (audit E9): 1.4 ms instead of 6.6 at 4K
    // for RGBA8, the display branch of every preview and export picture.
    if matches!(frame.pixels, FramePixels::Yuv8(_) | FramePixels::Yuv16(_))
        && matches!(target, PixelFormat::Rgba8 | PixelFormat::Rgb16)
    {
        frame.pixels = if target == PixelFormat::Rgba8 {
            FramePixels::Rgba8(Arc::new(planar_rgb_any(&frame, 4, narrow_rgb16)))
        } else {
            FramePixels::Rgb16(planar_rgb_any(&frame, 3, |v, o: &mut [u16]| o.copy_from_slice(&v)))
        };
        frame.format = target;
        return Ok(frame);
    }
    let rgb = match &frame.pixels {
        FramePixels::RgbF32(p) => p
            .par_iter()
            .map(|v| (v.clamp(0.0, 1.0) * 65535.0).round_ties_even() as u16)
            .collect(),
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
        PixelFormat::Rgba8 => FramePixels::Rgba8(Arc::new(rgb16_to_rgba8(&rgb))),
        PixelFormat::Yuv422p10 => {
            // Preserve the established RGB16 -> ProRes numeric conversion.
            let p = crate::media::prores_frame(&rgb, frame.width, frame.height)?;
            FramePixels::Yuv16([Arc::new(p.y), Arc::new(p.cb), Arc::new(p.cr)])
        }
        _ => {
            let scale = if target.depth() == 8 { 1. } else { 4. };
            let planes = subsample(
                frame.width,
                frame.height,
                target.subsampling().unwrap(),
                |i| {
                    let p = i * 3;
                    let c = filmcraft_color::rgb_to_ycbcr(
                        rgb[p] as f32 / 65535.,
                        rgb[p + 1] as f32 / 65535.,
                        rgb[p + 2] as f32 / 65535.,
                        filmcraft_color::Matrix::Bt709,
                    );
                    let l = ((16. + 219. * c[0]) * scale)
                        .round_ties_even()
                        .clamp(16. * scale, 235. * scale) as u16;
                    (l, [c[1], c[2]])
                },
                |uv, count| {
                    uv.map(|c| {
                        ((128. + 224. * c / count as f32) * scale)
                            .round_ties_even()
                            .clamp(16. * scale, 240. * scale) as u16
                    })
                },
            );
            if target.depth() == 8 { narrow(planes) } else { FramePixels::Yuv16(planes.map(Arc::new)) }
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
    rgba8_validated(frame)
}

/// [`rgba8`] for a frame the caller has already validated (a decoder's or a
/// format plan's output): the scan reads every sample, so pay it once.
pub fn rgba8_validated(frame: &VideoFrame) -> Result<Arc<Vec<u8>>> {
    match &frame.pixels {
        FramePixels::Rgba8(p) => Ok(p.clone()),
        FramePixels::Rgb16(p) => Ok(Arc::new(rgb16_to_rgba8(p))),
        _ => {
            let FramePixels::Rgba8(p) = convert_validated(frame.clone(), PixelFormat::Rgba8)?.pixels else {
                unreachable!()
            };
            Ok(p)
        }
    }
}

#[cfg(test)]
mod fingerprint {
    use super::*;
    use std::hash::{Hash, Hasher};
    fn frame(format: PixelFormat, w: usize, h: usize, seed: u64) -> VideoFrame {
        let mut s = seed;
        let mut next = move || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s };
        let n = w * h;
        let pixels = match format {
            PixelFormat::Rgb16 => FramePixels::Rgb16((0..n * 3).map(|_| next() as u16).collect()),
            PixelFormat::RgbF32 => FramePixels::RgbF32(Arc::new((0..n * 3).map(|_| (next() % 1200) as f32 / 1000. - 0.1).collect())),
            PixelFormat::Rgba8 => FramePixels::Rgba8(Arc::new((0..n * 4).map(|i| if i % 4 == 3 { 255 } else { next() as u8 }).collect())),
            f => {
                let (sx, sy) = f.subsampling().unwrap();
                let c = w.div_ceil(sx) * h.div_ceil(sy);
                if f.depth() == 8 {
                    FramePixels::Yuv8([n, c, c].map(|len| Arc::new((0..len).map(|_| 16 + (next() % 220) as u8).collect())))
                } else {
                    FramePixels::Yuv16([n, c, c].map(|len| Arc::new((0..len).map(|_| 64 + (next() % 877) as u16).collect())))
                }
            }
        };
        VideoFrame { width: w, height: h, format, pixels, pts: 0, duration: 1, time_base: [1, 1] }
    }
    fn digest(f: &VideoFrame) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", f.format).hash(&mut h);
        match &f.pixels {
            FramePixels::Rgb16(p) => p.hash(&mut h),
            FramePixels::RgbF32(p) => p.iter().for_each(|v| v.to_bits().hash(&mut h)),
            FramePixels::Rgba8(p) => p.hash(&mut h),
            FramePixels::Yuv8(p) => p.iter().for_each(|p| p.hash(&mut h)),
            FramePixels::Yuv16(p) => p.iter().for_each(|p| p.hash(&mut h)),
        }
        h.finish()
    }
    /// Conversion output digests, to prove a rewrite is bit-identical.
    #[test]
    #[ignore]
    fn print_conversion_digests() {
        use PixelFormat::*;
        let all = [Rgb16, RgbF32, Rgba8, Yuv420p8, Yuv420p10, Yuv422p10, Yuv444p10];
        let mut total = std::collections::hash_map::DefaultHasher::new();
        for (w, h) in [(37, 23), (64, 48), (1, 1), (5, 2)] {
            for (i, from) in all.iter().enumerate() {
                let src = frame(*from, w, h, 0x9e3779b97f4a7c15 ^ (i as u64 * 7919 + w as u64));
                for to in all {
                    let out = convert(src.clone(), to).unwrap();
                    let d = digest(&out);
                    d.hash(&mut total);
                    println!("{w}x{h} {from:?}->{to:?} {d:016x}");
                }
                let r = rgba8(&src).unwrap();
                r.hash(&mut total);
                let p = into_prores(src.clone()).unwrap();
                (&p.y, &p.cb, &p.cr).hash(&mut total);
            }
        }
        println!("TOTAL {:016x}", total.finish());
    }
}
