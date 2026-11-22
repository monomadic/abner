//! Retained detector scores for interactive thresholding.
use super::{Check, Region, mark_region};

pub(crate) const DEFAULT_SENSITIVITY: u8 = 5;
pub(crate) const MAX_SENSITIVITY: u8 = 10;
const BASE_CUTOFF: u8 = 128;
pub(super) const OPACITY: f32 = 0.35;
pub(super) const CONFIDENCE: f32 = 0.25;

fn cutoff(level: u8) -> u8 {
    BASE_CUTOFF + 10 * DEFAULT_SENSITIVITY - 10 * level
}

pub(super) fn selected_by_default(score: u8) -> bool {
    score >= BASE_CUTOFF
}

pub(super) fn threshold_factor(level: u8) -> f32 {
    f32::from(cutoff(level)) / f32::from(BASE_CUTOFF)
}

pub(super) fn relative_score(value: f32, baseline: f32) -> u8 {
    // Flooring in baseline units preserves the default threshold exactly.
    (f64::from(value) / f64::from(baseline) * f64::from(BASE_CUTOFF)) as u8
}

struct Window {
    region: Region,
    min_component: usize,
    scores: Vec<[u8; 2]>,
}

#[derive(Default)]
pub(crate) struct Evidence {
    windows: Vec<Window>,
    strokes: Option<(Region, Vec<u8>)>,
}

impl Evidence {
    pub(crate) fn is_empty(&self) -> bool {
        self.windows.is_empty() && self.strokes.is_none()
    }

    pub(super) fn add_window(
        &mut self,
        [x, y, w, h]: Region,
        opacity: Vec<f32>,
        confidence: Vec<f32>,
        min_component: usize,
    ) {
        // Strip empty borders. They cannot connect islands at any slider setting.
        let (mut left, mut top, mut right, mut bottom) = (w, h, 0, 0);
        let minimum = f64::from(cutoff(MAX_SENSITIVITY)) / f64::from(BASE_CUTOFF);
        let min_opacity = f64::from(OPACITY) * minimum;
        let min_confidence = f64::from(CONFIDENCE) * minimum;
        for (i, (&a, &c)) in opacity.iter().zip(&confidence).enumerate() {
            if f64::from(a) >= min_opacity && f64::from(c) >= min_confidence {
                left = left.min(i % w);
                right = right.max(i % w + 1);
                top = top.min(i / w);
                bottom = bottom.max(i / w + 1);
            }
        }
        if left >= right || top >= bottom {
            return;
        }
        let scores = (top..bottom)
            .flat_map(|row| {
                let a = &opacity[row * w + left..row * w + right];
                let c = &confidence[row * w + left..row * w + right];
                a.iter()
                    .zip(c)
                    .map(|(&a, &c)| [relative_score(a, OPACITY), relative_score(c, CONFIDENCE)])
            })
            .collect();
        self.windows.push(Window {
            region: [x + left, y + top, right - left, bottom - top],
            min_component,
            scores,
        });
    }

    pub(super) fn set_strokes(&mut self, region: Region, scores: Vec<u8>) {
        self.strokes = Some((region, scores));
    }

    pub(crate) fn select(&self, size: [usize; 2], level: u8) -> Vec<u8> {
        let limit = cutoff(level);
        let mut pixels = vec![0; size[0] * size[1]];
        for window in &self.windows {
            let selected = select_islands(
                window.region[2],
                window.scores.len(),
                window.min_component,
                |i| window.scores[i].iter().all(|&score| score >= limit),
                &|| Ok(()),
            )
            .unwrap();
            mark_region(&mut pixels, size[0], window.region, &selected);
        }
        if let Some((region, scores)) = &self.strokes {
            let selected: Vec<_> = scores.iter().map(|&p| u8::from(p >= limit) * 255).collect();
            mark_region(&mut pixels, size[0], *region, &selected);
        }
        pixels
    }

    #[cfg(test)]
    pub(crate) fn from_scores(region: Region, opacity: Vec<f32>, confidence: Vec<f32>) -> Self {
        let mut evidence = Self::default();
        evidence.add_window(region, opacity, confidence, 32);
        evidence
    }
}

/// Remove small connected islands after thresholding. 1 marks pixels already
/// visited, so a single output buffer also serves as the flood-fill state.
pub(super) fn select_islands(
    width: usize,
    len: usize,
    min_component: usize,
    contains: impl Fn(usize) -> bool,
    check: Check<'_>,
) -> Result<Vec<u8>, String> {
    let mut pixels: Vec<_> = (0..len)
        .map(|i| if contains(i) { 255 } else { 0 })
        .collect();
    let mut component = Vec::new();
    for start in 0..len {
        if start % 4096 == 0 {
            check()?;
        }
        if pixels[start] != 255 {
            continue;
        }
        component.clear();
        component.push(start);
        pixels[start] = 1;
        let mut cursor = 0;
        while cursor < component.len() {
            if cursor % 4096 == 0 {
                check()?;
            }
            let p = component[cursor];
            cursor += 1;
            let x = p % width;
            for q in [
                (x > 0).then(|| p - 1),
                (x + 1 < width).then(|| p + 1),
                (p >= width).then(|| p - width),
                (p + width < len).then(|| p + width),
            ]
            .into_iter()
            .flatten()
            {
                if pixels[q] == 255 {
                    pixels[q] = 1;
                    component.push(q);
                }
            }
        }
        if component.len() < min_component {
            for &p in &component {
                pixels[p] = 0;
            }
        }
    }
    pixels.iter_mut().for_each(|p| *p *= 255);
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_both_scores_and_removes_small_islands_at_each_level() {
        let [w, h] = [26, 8];
        let (mut opacity, mut confidence) = (vec![0.; w * h], vec![0.; w * h]);
        for y in 1..7 {
            for x in 1..7 {
                opacity[y * w + x] = 0.80;
                confidence[y * w + x] = 0.20;
            }
            for x in 9..15 {
                opacity[y * w + x] = 0.40;
                confidence[y * w + x] = 0.80;
            }
            for x in 17..23 {
                opacity[y * w + x] = OPACITY;
                confidence[y * w + x] = CONFIDENCE;
            }
        }
        opacity[25] = 1.;
        confidence[25] = 1.;
        let evidence = Evidence::from_scores([0, 0, w, h], opacity, confidence);
        let count = |level| {
            evidence
                .select([w, h], level)
                .into_iter()
                .filter(|&p| p != 0)
                .count()
        };
        assert_eq!(count(0), 0);
        assert_eq!(count(DEFAULT_SENSITIVITY), 72);
        assert_eq!(count(MAX_SENSITIVITY), 108);
    }
}
