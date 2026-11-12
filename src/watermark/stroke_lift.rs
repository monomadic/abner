//! Appearance-only proposal for a search box the user drew around an overlay.
//! Finds stroke-shaped areas that are uniformly lighter (or darker) than their
//! surroundings in the temporal median, so it works when the scene never changes.
//! Unlike the temporal detector it cannot tell an overlay from scenery of the same
//! shape, which is why it is only used inside an explicit search area.
use super::Frame;

type Check<'a> = &'a mut dyn FnMut() -> Result<(), String>;

fn pass(src: &[f32], w: usize, h: usize, r: usize, horizontal: bool, min: bool) -> Vec<f32> {
    let mut out = vec![0.; w * h];
    for y in 0..h {
        for x in 0..w {
            let (lo, hi) = if horizontal {
                (x.saturating_sub(r), (x + r).min(w - 1))
            } else {
                (y.saturating_sub(r), (y + r).min(h - 1))
            };
            let values = (lo..=hi).map(|k| if horizontal { src[y * w + k] } else { src[k * w + x] });
            out[y * w + x] = if min {
                values.fold(f32::INFINITY, f32::min)
            } else {
                values.fold(f32::NEG_INFINITY, f32::max)
            };
        }
    }
    out
}

/// Greyscale opening with a (2r+1) square: removes light detail narrower than the square.
fn open(src: &[f32], w: usize, h: usize, r: usize, check: Check) -> Result<Vec<f32>, String> {
    let mut image = src.to_vec();
    for (horizontal, min) in [(true, true), (false, true), (true, false), (false, false)] {
        check()?;
        image = pass(&image, w, h, r, horizontal, min);
    }
    Ok(image)
}

fn box_mean(src: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut rows = vec![0.; w * h];
    for y in 0..h {
        let mut sum = 0.;
        let mut prefix = Vec::with_capacity(w + 1);
        prefix.push(0.);
        for x in 0..w {
            sum += src[y * w + x];
            prefix.push(sum);
        }
        for x in 0..w {
            let (lo, hi) = (x.saturating_sub(r), (x + r).min(w - 1));
            rows[y * w + x] = (prefix[hi + 1] - prefix[lo]) / (hi - lo + 1) as f32;
        }
    }
    let mut out = vec![0.; w * h];
    for x in 0..w {
        let mut sum = 0.;
        let mut prefix = Vec::with_capacity(h + 1);
        prefix.push(0.);
        for y in 0..h {
            sum += rows[y * w + x];
            prefix.push(sum);
        }
        for y in 0..h {
            let (lo, hi) = (y.saturating_sub(r), (y + r).min(h - 1));
            out[y * w + x] = (prefix[hi + 1] - prefix[lo]) / (hi - lo + 1) as f32;
        }
    }
    out
}

/// Mask over `region` (row-major, region-sized). Empty when nothing stroke-like stands out.
pub(super) fn detect_checked(
    frames: &[Frame],
    region: [usize; 4],
    check: Check,
) -> Result<Vec<bool>, String> {
    let [ox, oy, w, h] = region;
    let (fw, fh) = (frames[0].width, frames[0].height);
    // Widest stroke considered, and the finest detail kept, scale with the box.
    let coarse = (w.min(h) / 8).clamp(8, 40);
    let fine = (coarse / 7).max(1);
    let pad = coarse * 2;
    let (x0, y0) = (ox.saturating_sub(pad), oy.saturating_sub(pad));
    let (x1, y1) = ((ox + w + pad).min(fw), (oy + h + pad).min(fh));
    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut darkest = vec![0.; cw * ch];
    let mut lightest = vec![0.; cw * ch];
    let mut values = vec![0f32; frames.len()];
    for y in 0..ch {
        check()?;
        for x in 0..cw {
            let mut median = [0.; 3];
            for (c, slot) in median.iter_mut().enumerate() {
                for (value, frame) in values.iter_mut().zip(frames) {
                    *value = frame.pixel(x0 + x, y0 + y)[c];
                }
                let middle = values.len() / 2;
                *slot = *values.select_nth_unstable_by(middle, f32::total_cmp).1;
            }
            darkest[y * cw + x] = median[0].min(median[1]).min(median[2]);
            lightest[y * cw + x] = 1. - median[0].max(median[1]).max(median[2]);
        }
    }
    let mut best: (f32, Vec<bool>) = (0., vec![false; w * h]);
    // A light overlay lifts the darkest channel; a dark one lowers the lightest.
    for image in [darkest, lightest] {
        let kept = open(&image, cw, ch, fine, check)?;
        let background = open(&image, cw, ch, coarse, check)?;
        let detail: Vec<f32> = image.iter().zip(&kept).map(|(a, b)| a - b).collect();
        let texture = box_mean(&detail, cw, ch, coarse);
        // Busy texture produces lift of its own; demand more than it explains.
        let score = |p: usize| kept[p] - background[p] - 2.5 * texture[p];
        let at = |q: usize| (oy + q / w - y0) * cw + ox + q % w - x0;
        let mut mask = vec![false; w * h];
        let mut seen = vec![false; w * h];
        let mut weight = 0.;
        for start in 0..w * h {
            if seen[start] || score(at(start)) < 0.04 {
                continue;
            }
            if start % 4096 == 0 {
                check()?;
            }
            let mut component = vec![start];
            seen[start] = true;
            let (mut cursor, mut peak, mut total) = (0, 0f32, 0.);
            while cursor < component.len() {
                let q = component[cursor];
                cursor += 1;
                let value = score(at(q));
                peak = peak.max(value);
                total += value;
                let (x, y) = (q % w, q / w);
                for n in [
                    if x > 0 { Some(q - 1) } else { None },
                    if x + 1 < w { Some(q + 1) } else { None },
                    if y > 0 { Some(q - w) } else { None },
                    if y + 1 < h { Some(q + w) } else { None },
                ]
                .into_iter()
                .flatten()
                {
                    if !seen[n] && score(at(n)) >= 0.04 {
                        seen[n] = true;
                        component.push(n);
                    }
                }
            }
            if component.len() >= (coarse * coarse / 3).max(24) && peak >= 0.09 {
                weight += total;
                for q in component {
                    mask[q] = true;
                }
            }
        }
        if weight > best.0 {
            best = (weight, mask);
        }
    }
    Ok(best.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eight identical frames: nothing ever changes, so only appearance can help.
    fn still(overlay: Option<f32>) -> Vec<Frame> {
        let mut state = 5u32;
        let mut rgb = Vec::new();
        for y in 0..96 {
            for x in 0..160 {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let grain = (state >> 24) as f32 / 255. * 0.08;
                let base = 0.45 + grain;
                let value = match overlay {
                    Some(tone) if bar(x, y) => base * 0.6 + tone * 0.4,
                    _ => base,
                };
                rgb.extend_from_slice(&[(value * 255.) as u8; 3]);
            }
        }
        vec![Frame::new(160, 96, rgb).unwrap(); 8]
    }
    fn bar(x: usize, y: usize) -> bool {
        (24..76).contains(&y) && (40..120).contains(&x) && (x - 40) % 28 < 8
    }
    fn score(mask: &[bool]) -> (usize, usize, usize) {
        let [ox, oy, w, _] = REGION;
        let (mut hit, mut total, mut stray) = (0, 0, 0);
        for (i, &m) in mask.iter().enumerate() {
            let inside = bar(ox + i % w, oy + i / w);
            total += usize::from(inside);
            hit += usize::from(inside && m);
            stray += usize::from(!inside && m);
        }
        (hit, total, stray)
    }
    const REGION: [usize; 4] = [20, 10, 120, 76];

    #[test]
    fn finds_translucent_light_and_dark_strokes_over_unchanging_scenery() {
        for tone in [1., 0.] {
            let mask = detect_checked(&still(Some(tone)), REGION, &mut || Ok(())).unwrap();
            let (hit, total, stray) = score(&mask);
            assert!(hit * 10 >= total * 9, "tone {tone}: {hit}/{total} stroke pixels");
            assert!(stray * 50 <= mask.len(), "tone {tone}: {stray} stray pixels");
        }
    }

    #[test]
    fn plain_scenery_yields_no_proposal() {
        let mask = detect_checked(&still(None), REGION, &mut || Ok(())).unwrap();
        assert!(mask.iter().all(|m| !m));
    }
}
