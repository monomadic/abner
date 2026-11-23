//! Search-window samples from deep Y'CbCr, before display conversion.
use filmcraft_color::Range;
use filmcraft_frame::{PixelData, VideoFrame};
use rayon::prelude::*;
use std::hash::{DefaultHasher, Hash, Hasher};

#[derive(Clone)]
pub(super) struct Planes {
    pub luma: Vec<u16>,
    chroma: [Vec<u16>; 2],
    chroma_width: usize,
    shifts: [u32; 2],
    phase: [usize; 2],
    pub noise_scale: f32,
}

impl Planes {
    pub fn capture(
        picture: &VideoFrame,
        [x, y, w, h]: [usize; 4],
        scale: usize,
    ) -> Result<Option<Self>, String> {
        let PixelData::Yuv16 {
            planes,
            chroma,
            bits,
            ..
        } = &picture.data
        else {
            return Ok(None);
        };
        if !(9..=16).contains(bits) {
            return Err("Unsupported planar sample depth for detection".into());
        }
        let (sx, sy) = chroma.shifts();
        let shifts = [sx, sy];
        let (mx, my) = (1 << sx, 1 << sy);
        let (width, height) = (picture.width as usize, picture.height as usize);
        let (cw, ch) = (width.div_ceil(mx), height.div_ceil(my));
        if planes[0].len() < width * height || planes[1..].iter().any(|p| p.len() < cw * ch) {
            return Err("Incomplete native planes for detection".into());
        }
        let [ow, oh] = [w.div_ceil(scale), h.div_ceil(scale)];
        let phase = if scale == 1 { [x % mx, y % my] } else { [0, 0] };
        let chroma_width = (ow + phase[0]).div_ceil(mx);
        let chroma_height = (oh + phase[1]).div_ceil(my);

        let code_scale = 1u32 << (*bits - 8);
        let (luma_min, luma_span, chroma_min, chroma_span) = match picture.color.range {
            Range::Full => (0, (1u32 << bits) - 1, 0, (1u32 << bits) - 1),
            Range::Limited => (
                16 * code_scale,
                219 * code_scale,
                16 * code_scale,
                224 * code_scale,
            ),
        };
        let luma = capture_plane(
            &planes[0],
            width,
            [ow, oh],
            luma_min,
            luma_span,
            |col, row| {
                let (left, top) = (col * scale, row * scale);
                [
                    x + left,
                    y + top,
                    x + (left + scale).min(w),
                    y + (top + scale).min(h),
                ]
            },
        );
        let chroma = std::array::from_fn(|c| {
            capture_plane(
                &planes[c + 1],
                cw,
                [chroma_width, chroma_height],
                chroma_min,
                chroma_span,
                |col, row| {
                    if scale == 1 {
                        let (left, top) = ((x >> sx) + col, (y >> sy) + row);
                        return [left, top, left + 1, top + 1];
                    }
                    // One chroma cell describes a block of the downsampled luma grid.
                    let (left, top) = (col * mx * scale, row * my * scale);
                    [
                        (x + left) >> sx,
                        (y + top) >> sy,
                        (x + (left + mx * scale).min(w)).div_ceil(mx),
                        (y + (top + my * scale).min(h)).div_ceil(my),
                    ]
                },
            )
        });
        Ok(Some(Self {
            luma,
            chroma,
            chroma_width,
            shifts,
            phase,
            noise_scale: 255. / luma_span as f32,
        }))
    }

    pub fn pixel(&self, x: usize, y: usize, width: usize) -> [f32; 3] {
        let col = (x + self.phase[0]) >> self.shifts[0];
        let row = (y + self.phase[1]) >> self.shifts[1];
        let uv = row * self.chroma_width + col;
        [
            self.luma[y * width + x],
            self.chroma[0][uv],
            self.chroma[1][uv],
        ]
        .map(normalize)
    }

    pub fn same_samples(&self, other: &Self) -> bool {
        self.luma == other.luma
            && self.chroma == other.chroma
            && self.shifts == other.shifts
            && self.phase == other.phase
    }

    pub fn fingerprint(&self) -> u64 {
        let mut hash = DefaultHasher::new();
        self.shifts.hash(&mut hash);
        self.phase.hash(&mut hash);
        super::search::fingerprint(&self.luma, &mut hash);
        for plane in &self.chroma {
            super::search::fingerprint(plane, &mut hash);
        }
        hash.finish()
    }
}

pub(super) fn normalize(sample: u16) -> f32 {
    f32::from(sample) / 65535.
}

fn capture_plane(
    input: &[u16],
    stride: usize,
    [w, h]: [usize; 2],
    black: u32,
    span: u32,
    bounds: impl Fn(usize, usize) -> [usize; 4] + Sync,
) -> Vec<u16> {
    let mut plane = vec![0; w * h];
    let fill = |(row, out): (usize, &mut [u16])| {
        for (col, pixel) in out.iter_mut().enumerate() {
            let [left, top, right, bottom] = bounds(col, row);
            let mut sum = 0u64;
            for y in top..bottom {
                sum += input[y * stride + left..y * stride + right]
                    .iter()
                    .map(|&v| u64::from(v))
                    .sum::<u64>();
            }
            let area = ((right - left) * (bottom - top)) as u64;
            let black = u64::from(black) * area;
            let span = u64::from(span) * area;
            let sample = sum.saturating_sub(black).min(span);
            *pixel = ((sample * 65535 + span / 2) / span) as u16;
        }
    };
    if w * h >= 512 * 1024 {
        plane.par_chunks_mut(w).enumerate().for_each(fill);
    } else {
        plane.chunks_mut(w).enumerate().for_each(fill);
    }
    plane
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_frame::Chroma;
    use std::sync::Arc;

    #[test]
    fn captures_odd_chroma_phase_and_partial_coarse_cells() {
        let luma: Vec<_> = (300..335).collect();
        let cb: Vec<_> = (0..12).map(|i| 180 + i * i * 5).collect();
        let mut color = filmcraft_color::ColorInfo::REC709;
        color.range = Range::Full;
        let picture = VideoFrame {
            width: 7,
            height: 5,
            data: PixelData::Yuv16 {
                planes: [
                    Arc::new(luma.clone()),
                    Arc::new(cb.clone()),
                    Arc::new(vec![512; 12]),
                ],
                chroma: Chroma::C420,
                bits: 10,
                alpha: None,
            },
            color,
            par: (1, 1),
            pts: filmcraft_time::Tick::ZERO,
        };
        let code = |v: f32| ((v / 1023.) * 65535.).round() as u16;
        let native = Planes::capture(&picture, [1, 1, 5, 4], 1).unwrap().unwrap();
        assert_eq!(native.luma[0], code(f32::from(luma[8])));
        assert_eq!(native.luma[1], code(f32::from(luma[9])));
        assert_eq!(native.pixel(0, 0, 5)[1], normalize(code(f32::from(cb[0]))));
        assert_eq!(native.pixel(1, 1, 5)[1], normalize(code(f32::from(cb[5]))));
        let coarse = Planes::capture(&picture, [1, 1, 5, 4], 2).unwrap().unwrap();
        assert_eq!(coarse.luma[5], code(f32::from(luma[26] + luma[33]) / 2.));
        assert_eq!(
            coarse.pixel(2, 1, 3)[1],
            normalize(code(f32::from(cb[2] + cb[6] + cb[10]) / 3.))
        );
    }
}
