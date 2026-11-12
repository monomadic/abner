//! Proposal for a search box the user drew around an overlay, built for scenes
//! that barely change. Two independent cues are fused:
//! - lift: stroke-shaped areas uniformly lighter (or darker) than their
//!   surroundings in the temporal median;
//! - attenuation: grain and flicker are damped under a translucent overlay.
//! Busy texture fakes lift but not attenuation; dark static objects fake
//! attenuation but not lift. Together the cues may be weak; alone, attenuation
//! must be strong, and lift counts only where there is no flicker to measure.
//! It still cannot tell an opaque overlay from scenery of the same
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

/// How much less each pixel flickers than is typical nearby (0 none, 1 completely),
/// and whether the surroundings flicker measurably at all. Where they do not, the
/// value is zero and nothing can be inferred from it.
fn attenuation(
    spread: &[f32],
    w: usize,
    h: usize,
    reach: usize,
    check: Check,
) -> Result<(Vec<f32>, Vec<bool>), String> {
    const G: usize = 4;
    let (gw, gh) = ((w - 1) / G + 2, (h - 1) / G + 2);
    let mut grid = vec![0f32; gw * gh];
    let mut window = Vec::new();
    for gy in 0..gh {
        check()?;
        for gx in 0..gw {
            let (cx, cy) = ((gx * G).min(w - 1), (gy * G).min(h - 1));
            window.clear();
            for y in (cy.saturating_sub(reach)..=(cy + reach).min(h - 1)).step_by(3) {
                for x in (cx.saturating_sub(reach)..=(cx + reach).min(w - 1)).step_by(3) {
                    window.push(spread[y * w + x]);
                }
            }
            let k = window.len() * 6 / 10;
            grid[gy * gw + gx] = *window.select_nth_unstable_by(k, f32::total_cmp).1;
        }
    }
    let mut out = vec![0f32; w * h];
    let mut known = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let (gx, gy) = (x / G, y / G);
            let (tx, ty) = ((x % G) as f32 / G as f32, (y % G) as f32 / G as f32);
            let at = |a: usize, b: usize| grid[b * gw + a];
            let typical = (at(gx, gy) * (1. - tx) + at(gx + 1, gy) * tx) * (1. - ty)
                + (at(gx, gy + 1) * (1. - tx) + at(gx + 1, gy + 1) * tx) * ty;
            // Below about two 8-bit levels the ratio is quantisation, not evidence.
            if typical >= 0.008 {
                known[y * w + x] = true;
                out[y * w + x] = (1. - spread[y * w + x] / typical).clamp(-1., 1.);
            }
        }
    }
    Ok((box_mean(&box_mean(&out, w, h, 2), w, h, 2), known))
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
    let mut spread = vec![0f32; cw * ch];
    let mut values = vec![0f32; frames.len()];
    for y in 0..ch {
        check()?;
        for x in 0..cw {
            let mut median = [0.; 3];
            for (c, slot) in median.iter_mut().enumerate() {
                for (value, frame) in values.iter_mut().zip(frames) {
                    *value = frame.pixel(x0 + x, y0 + y)[c];
                }
                values.sort_unstable_by(f32::total_cmp);
                *slot = values[values.len() / 2];
                spread[y * cw + x] +=
                    (values[values.len() * 7 / 8] - values[values.len() / 8]) / 3.;
            }
            darkest[y * cw + x] = median[0].min(median[1]).min(median[2]);
            lightest[y * cw + x] = 1. - median[0].max(median[1]).max(median[2]);
        }
    }
    let (attenuation, flickers) = attenuation(&spread, cw, ch, coarse, check)?;
    let mut best: (f32, Vec<bool>) = (0., vec![false; w * h]);
    // A light overlay lifts the darkest channel; a dark one lowers the lightest.
    for image in [darkest, lightest] {
        let kept = open(&image, cw, ch, fine, check)?;
        let background = open(&image, cw, ch, coarse, check)?;
        let detail: Vec<f32> = image.iter().zip(&kept).map(|(a, b)| a - b).collect();
        let texture = box_mean(&detail, cw, ch, coarse);
        // Busy texture produces lift of its own; demand more than it explains.
        let lift: Vec<f32> = (0..cw * ch)
            .map(|p| kept[p] - background[p] - 2.5 * texture[p])
            .collect();
        let lift = box_mean(&lift, cw, ch, 1);
        // Each cue in units of "clearly present"; 1.0 means both are. Strong attenuation
        // suffices alone. Lift suffices alone only where flicker cannot be measured:
        // where it can, undamped flicker says the brightness belongs to the scene.
        let score = |p: usize| {
            let lift = (lift[p] / 0.06).clamp(-1., 2.);
            if flickers[p] {
                0.5 * (lift.min(1.25) + (attenuation[p] / 0.17).clamp(-1., 2.))
            } else {
                0.5 * lift
            }
        };
        let at = |q: usize| (oy + q / w - y0) * cw + ox + q % w - x0;
        let mut mask = vec![false; w * h];
        let mut seen = vec![false; w * h];
        let mut weight = 0.;
        for start in 0..w * h {
            if seen[start] || score(at(start)) < 0.75 {
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
                    if !seen[n] && score(at(n)) >= 0.75 {
                        seen[n] = true;
                        component.push(n);
                    }
                }
            }
            if component.len() >= (coarse * coarse / 3).max(24) && peak >= 1. {
                weight += total;
                for q in component {
                    mask[q] = true;
                }
            }
        }
        // Thresholding bites into soft edges; let accepted strokes reclaim adjacent
        // pixels with weaker evidence, a few pixels at most. Smoothed attenuation
        // spills past an edge, so some lift is required as well.
        for _ in 0..fine {
            check()?;
            let grown: Vec<usize> = (0..w * h)
                .filter(|&q| {
                    let (x, y) = (q % w, q / w);
                    !mask[q]
                        && score(at(q)) >= 0.4
                        && lift[at(q)] >= 0.015
                        && ((x > 0 && mask[q - 1])
                            || (x + 1 < w && mask[q + 1])
                            || (y > 0 && mask[q - w])
                            || (y + 1 < h && mask[q + w]))
                })
                .collect();
            if grown.is_empty() {
                break;
            }
            for q in grown {
                mask[q] = true;
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

    /// Bright scenery with fresh grain every frame. `overlay` blends 35% white, which
    /// lifts only slightly here; `painted` bakes the same brightness into the scenery.
    fn grainy(overlay: bool, painted: bool) -> Vec<Frame> {
        let mut state = 23u32;
        (0..16)
            .map(|_| {
                let mut rgb = Vec::new();
                for y in 0..96 {
                    for x in 0..160 {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        let grain = ((state >> 24) as f32 / 255. - 0.5) * 0.1;
                        let mut value = 0.8 + grain;
                        if bar(x, y) {
                            if overlay {
                                value = value * 0.65 + 0.35;
                            } else if painted {
                                value += 0.07;
                            }
                        }
                        rgb.extend_from_slice(&[(value.clamp(0., 1.) * 255.) as u8; 3]);
                    }
                }
                Frame::new(160, 96, rgb).unwrap()
            })
            .collect()
    }

    #[test]
    fn weak_lift_needs_damped_flicker_to_count() {
        let found = detect_checked(&grainy(true, false), REGION, &mut || Ok(())).unwrap();
        let (hit, total, stray) = score(&found);
        assert!(hit * 10 >= total * 8, "{hit}/{total} stroke pixels");
        assert!(stray * 50 <= found.len(), "{stray} stray pixels");
        // The same brightness as part of the scene flickers like its surroundings.
        let scenery = detect_checked(&grainy(false, true), REGION, &mut || Ok(())).unwrap();
        assert!(scenery.iter().all(|m| !m));
    }

    #[test]
    fn plain_scenery_yields_no_proposal() {
        let mask = detect_checked(&still(None), REGION, &mut || Ok(())).unwrap();
        assert!(mask.iter().all(|m| !m));
    }
}
