//! Shared editor ownership and render snapshots. Drafts never alias mutable saved pixels.
use crate::mask::{Crop, Mask};
#[derive(Clone)]
pub(crate) enum Owner {
    Source,
    Timeline { start: f64, end: f64 },
    Plugin { id: String, key: String },
}
pub(crate) struct Editor {
    pub owner: Owner,
    pub source: usize,
    pub return_mode: usize,
    pub original_mask: Option<Mask>,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClipCrop {
    pub start: f64,
    pub end: f64,
    pub crop: Crop,
}
#[derive(Clone, Default)]
pub(crate) struct JobInputs {
    pub bindings: abner_processor_api::ChainBindings,
    pub crops: Vec<ClipCrop>,
}
/// Fit the selected source rectangle into the fixed output canvas. This keeps
/// one stable geometry across differently cropped clips, with black letterboxing.
pub(crate) fn apply_crop(frame: &mut abner_processor_api::Frame, crops: &[ClipCrop]) {
    let t = frame.pts as f64 * frame.time_base[0] as f64 / frame.time_base[1] as f64;
    let Some(c) = crops.iter().rev().find(|c| t >= c.start && t < c.end) else {
        return;
    };
    let (x, y, w, h) = c.crop.pixels(frame.width as u32, frame.height as u32);
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 {
        return;
    }
    let scale = (frame.width as f64 / w as f64).min(frame.height as f64 / h as f64);
    let dw = (w as f64 * scale).round() as usize;
    let dh = (h as f64 * scale).round() as usize;
    let ox = (frame.width - dw) / 2;
    let oy = (frame.height - dh) / 2;
    let mut out = vec![0; frame.pixels.len()];
    for row in 0..dh {
        for col in 0..dw {
            let sx = ((col as f64 + 0.5) / scale - 0.5).clamp(0., (w - 1) as f64);
            let sy = ((row as f64 + 0.5) / scale - 0.5).clamp(0., (h - 1) as f64);
            let (ix, iy) = (sx.floor() as usize, sy.floor() as usize);
            for channel in 0..3 {
                let get = |xx: usize, yy: usize| {
                    frame.pixels[((y as usize + yy) * frame.width + x as usize + xx) * 3 + channel]
                        as f64
                };
                let a = get(ix, iy) * (1. - sx.fract()) + get((ix + 1).min(w - 1), iy) * sx.fract();
                let b = get(ix, (iy + 1).min(h - 1)) * (1. - sx.fract())
                    + get((ix + 1).min(w - 1), (iy + 1).min(h - 1)) * sx.fract();
                out[((oy + row) * frame.width + ox + col) * 3 + channel] =
                    (a * (1. - sy.fract()) + b * sy.fract()).round() as u16;
            }
        }
    }
    frame.pixels = out;
}

static NEXT_SOURCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
pub(crate) fn next_source_id() -> usize {
    NEXT_SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
pub(crate) fn crop_rgba(
    rgba: &[u8],
    w: u32,
    h: u32,
    t: f64,
    crops: &[ClipCrop],
) -> std::sync::Arc<Vec<u8>> {
    let mut f = abner_processor_api::Frame {
        width: w as usize,
        height: h as usize,
        pixels: rgba
            .chunks_exact(4)
            .flat_map(|p| [p[0] as u16 * 257, p[1] as u16 * 257, p[2] as u16 * 257])
            .collect(),
        pts: (t * 1e6).round() as i64,
        duration: 1,
        time_base: [1, 1_000_000],
    };
    apply_crop(&mut f, crops);
    std::sync::Arc::new(
        f.pixels
            .chunks_exact(3)
            .flat_map(|p| {
                [
                    (p[0] / 257) as u8,
                    (p[1] / 257) as u8,
                    (p[2] / 257) as u8,
                    255,
                ]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_affects_only_its_interval_and_preserves_canvas() {
        let mut f = abner_processor_api::Frame {
            width: 4,
            height: 2,
            pixels: (0..24).collect(),
            pts: 0,
            duration: 1,
            time_base: [1, 1],
        };
        let before = f.pixels.clone();
        let crops = vec![ClipCrop {
            start: 1.,
            end: 2.,
            crop: Crop {
                x: 1.,
                y: 0.,
                w: 2.,
                h: 2.,
            },
        }];
        apply_crop(&mut f, &crops);
        assert_eq!(f.pixels, before);
        f.pts = 1;
        apply_crop(&mut f, &crops);
        assert_eq!(f.width, 4);
        assert_eq!(f.height, 2);
        assert_eq!(&f.pixels[0..3], &[0, 0, 0]);
        assert_eq!(&f.pixels[3..9], &before[3..9]);
        assert_eq!(&f.pixels[9..12], &[0, 0, 0]);
    }
}

impl JobInputs {
    pub fn validate_geometry(&self, width: u32, height: u32) -> Result<(), String> {
        for c in &self.crops {
            let r = c.crop;
            if ![r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite())
                || r.x < 0.
                || r.y < 0.
                || r.w < 1.
                || r.h < 1.
                || r.x + r.w > width as f32
                || r.y + r.h > height as f32
            {
                return Err("A saved crop no longer fits the source; edit it again".into());
            }
        }
        Ok(())
    }
}
