//! Screen-space overlay proposals. No registration or scene trajectories are used.
use super::Frame;

pub(super) struct Maps {
    pub opacity: Vec<f32>,
    pub confidence: Vec<f32>,
    pub edge: Vec<f32>,
    pub variation: Vec<f32>,
}

/// Robust central temporal range, averaged over RGB. A fixed alpha overlay
/// scales this range by (1-alpha), provided local background distributions agree.
fn temporal(frames: &[Frame], x: usize, y: usize) -> ([f32; 3], f32) {
    let mut median = [0.; 3];
    let mut spread = 0.;
    for c in 0..3 {
        let mut values: Vec<_> = frames.iter().map(|f| f.pixel(x, y)[c]).collect();
        values.sort_by(f32::total_cmp);
        median[c] = values[values.len() / 2];
        spread += (values[values.len() * 7 / 8] - values[values.len() / 8]) / 3.;
    }
    (median, spread)
}

#[cfg(test)]
fn detect(frames: &[Frame], region: [usize; 4], radius: usize) -> Maps {
    detect_checked(frames, region, radius, &mut || Ok(())).unwrap()
}

pub(super) fn detect_checked(
    frames: &[Frame],
    region: [usize; 4],
    radius: usize,
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<Maps, String> {
    checkpoint()?;
    let [ox, oy, w, h] = region;
    let fw = frames[0].width;
    let fh = frames[0].height;
    // Include a halo of unselected pixels so tightly drawn boxes have clean context.
    let halo = radius * 3;
    let x0 = ox.saturating_sub(halo);
    let y0 = oy.saturating_sub(halo);
    let x1 = (ox + w + halo).min(fw);
    let y1 = (oy + h + halo).min(fh);
    let cw = x1 - x0;
    let ch = y1 - y0;
    let mut ranges = vec![0.; cw * ch];
    let mut medians = vec![[0.; 3]; cw * ch];
    for y in y0..y1 {
        checkpoint()?;
        for x in x0..x1 {
            let (m, s) = temporal(frames, x, y);
            medians[(y - y0) * cw + x - x0] = m;
            ranges[(y - y0) * cw + x - x0] = s;
        }
    }
    let mut maps = Maps {
        opacity: vec![0.; w * h],
        confidence: vec![0.; w * h],
        edge: vec![0.; w * h],
        variation: vec![0.; w * h],
    };
    let mut wider_opacity = vec![0.; w * h];
    let mut wider_variation = vec![0.; w * h];
    for y in 0..h {
        checkpoint()?;
        for x in 0..w {
            let px = ox + x;
            let py = oy + y;
            let cx = px - x0;
            let cy = py - y0;
            // Spatially subsample context only; output and temporal measurements are native resolution.
            let mut context = Vec::new();
            for yy in (cy.saturating_sub(radius)..=(cy + radius).min(ch - 1)).step_by(4) {
                for xx in (cx.saturating_sub(radius)..=(cx + radius).min(cw - 1)).step_by(4) {
                    context.push(ranges[yy * cw + xx]);
                }
            }
            context.sort_by(f32::total_cmp);
            let local_reference = context[context.len() * 3 / 4];
            // A broad solid emblem can fill the local window. Estimate background
            // variation in a wider annulus as well, excluding its stable interior.
            let mut ring = Vec::new();
            for yy in (cy.saturating_sub(halo)..=(cy + halo).min(ch - 1)).step_by(8) {
                for xx in (cx.saturating_sub(halo)..=(cx + halo).min(cw - 1)).step_by(8) {
                    if xx.abs_diff(cx) > radius || yy.abs_diff(cy) > radius {
                        ring.push(ranges[yy * cw + xx]);
                    }
                }
            }
            ring.sort_by(f32::total_cmp);
            let outer_reference = if ring.is_empty() {
                0.
            } else {
                ring[ring.len() / 2]
            };
            let reference = local_reference.max(outer_reference);
            let spread = ranges[cy * cw + cx];
            let p = y * w + x;
            wider_opacity[p] = if reference >= 0.06 {
                (1. - spread / reference).clamp(0., 1.)
            } else {
                0.
            };
            wider_variation[p] = reference;
            maps.variation[p] = local_reference;
            if local_reference < 0.06 {
                continue;
            }
            maps.opacity[p] = (1. - spread / local_reference).clamp(0., 1.);
            // Repeated signed RGB differences distinguish a persistent boundary from
            // edges passing through a pixel. Test both axes; avoid luminance-only failure on colored logos.
            let mut coherence = 0f32;
            for (dx, dy) in [(2usize, 0usize), (0, 2)] {
                let ax = px.saturating_sub(dx).max(x0);
                let ay = py.saturating_sub(dy).max(y0);
                let bx = (px + dx).min(x1 - 1);
                let by = (py + dy).min(y1 - 1);
                // A gray overlay reverses contrast over light versus dark scenery.
                // Require strong temporal attenuation as well as an edge present in
                // at least three quarters of the observations; sign alone is not evidence.
                if maps.opacity[p] >= 0.55 {
                    let mut magnitudes: Vec<f32> = frames.iter().map(|f| {
                        let a = f.pixel(ax, ay);
                        let b = f.pixel(bx, by);
                        (0..3).map(|c| (b[c] - a[c]).powi(2)).sum::<f32>().sqrt()
                    }).collect();
                    magnitudes.sort_by(f32::total_cmp);
                    let persistent = magnitudes[magnitudes.len() / 4];
                    coherence = coherence.max((persistent / 0.08).clamp(0., 1.));
                }
                let a = medians[(ay - y0) * cw + ax - x0];
                let b = medians[(by - y0) * cw + bx - x0];
                let direction: [f32; 3] = std::array::from_fn(|c| b[c] - a[c]);
                let norm = direction.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm < 0.04 {
                    continue;
                }
                let consistent = frames
                    .iter()
                    .filter(|f| {
                        let a = f.pixel(ax, ay);
                        let b = f.pixel(bx, by);
                        let dot = (0..3).map(|c| (b[c] - a[c]) * direction[c]).sum::<f32>() / norm;
                        dot > 0.025
                    })
                    .count() as f32
                    / frames.len() as f32;
                coherence = coherence
                    .max(((consistent - 0.65) / 0.35).clamp(0., 1.) * (norm / 0.12).min(1.));
            }
            maps.edge[p] = coherence;
        }
    }
    // Use wider context only for enclosed interiors, not open background.
    // A one-pixel closing seals sampling gaps in a persistent outline. Actual
    // holes remain clean unless their temporal evidence also supports an overlay.
    let solid: Vec<_> = maps.opacity.iter().map(|&a| a >= 0.35).collect();
    let mut expanded = vec![false; w * h];
    let mut closed = vec![false; w * h];
    for y in 0..h {
        checkpoint()?;
        for x in 0..w {
            expanded[y * w + x] = (y.saturating_sub(1)..=(y + 1).min(h - 1))
                .any(|yy| (x.saturating_sub(1)..=(x + 1).min(w - 1)).any(|xx| solid[yy * w + xx]));
        }
    }
    for y in 0..h {
        checkpoint()?;
        for x in 0..w {
            closed[y * w + x] = (y.saturating_sub(1)..=(y + 1).min(h - 1)).all(|yy| {
                (x.saturating_sub(1)..=(x + 1).min(w - 1)).all(|xx| expanded[yy * w + xx])
            });
        }
    }
    let mut outside = vec![false; w * h];
    let mut queue = Vec::new();
    for y in 0..h {
        checkpoint()?;
        for x in 0..w {
            if (x == 0 || y == 0 || x + 1 == w || y + 1 == h) && !closed[y * w + x] {
                outside[y * w + x] = true;
                queue.push(y * w + x);
            }
        }
    }
    let mut cursor = 0;
    while cursor < queue.len() {
        if cursor % 4096 == 0 {
            checkpoint()?;
        }
        let p = queue[cursor];
        cursor += 1;
        let (x, y) = (p % w, p / w);
        for q in [
            if x > 0 { Some(p - 1) } else { None },
            if x + 1 < w { Some(p + 1) } else { None },
            if y > 0 { Some(p - w) } else { None },
            if y + 1 < h { Some(p + w) } else { None },
        ]
        .into_iter()
        .flatten()
        {
            if !outside[q] && !closed[q] {
                outside[q] = true;
                queue.push(q);
            }
        }
    }
    for p in 0..w * h {
        if !outside[p] && wider_opacity[p] >= 0.6 {
            maps.opacity[p] = maps.opacity[p].max(wider_opacity[p]);
            maps.variation[p] = wider_variation[p];
        }
    }
    // Retain spatially connected stable interiors only if supported by persistent
    // boundaries. This fills strokes rather than returning just their outlines.
    let mut visited = vec![false; w * h];
    for start in 0..w * h {
        if start % 4096 == 0 {
            checkpoint()?;
        }
        if visited[start] || maps.opacity[start] < 0.35 {
            continue;
        }
        let mut component = vec![start];
        visited[start] = true;
        let mut cursor = 0;
        let mut seeds = 0;
        while cursor < component.len() {
            if cursor % 4096 == 0 {
                checkpoint()?;
            }
            let p = component[cursor];
            cursor += 1;
            if maps.edge[p] > 0.85 {
                seeds += 1;
            }
            let (x, y) = (p % w, p / w);
            for q in [
                if x > 0 { Some(p - 1) } else { None },
                if x + 1 < w { Some(p + 1) } else { None },
                if y > 0 { Some(p - w) } else { None },
                if y + 1 < h { Some(p + w) } else { None },
            ]
            .into_iter()
            .flatten()
            {
                if !visited[q] && maps.opacity[q] >= 0.35 {
                    visited[q] = true;
                    component.push(q);
                }
            }
        }
        let edge_evidence = (seeds as f32 / 6.).min(1.);
        if component.len() < (radius * radius / 32).max(12) || seeds < 3 {
            for p in component {
                maps.opacity[p] = 0.;
            }
            continue;
        }
        for p in component {
            let dynamic = ((maps.variation[p] - 0.06) / 0.12).clamp(0., 1.);
            maps.confidence[p] = edge_evidence
                * dynamic
                * ((maps.opacity[p] - 0.25) / 0.55).clamp(0., 1.)
                * (frames.len() as f32 / 16.).min(1.);
        }
    }
    // Confidence thresholding can split tiny islands off a larger soft component.
    // Filter at the exported mask threshold too, so those islands do not survive
    // merely because weak low-confidence pixels connected them upstream.
    let mut seen = vec![false; w * h];
    for start in 0..w * h {
        if start % 4096 == 0 {
            checkpoint()?;
        }
        if seen[start] || maps.confidence[start] < 0.25 {
            continue;
        }
        let mut component = vec![start];
        seen[start] = true;
        let mut cursor = 0;
        while cursor < component.len() {
            if cursor % 4096 == 0 {
                checkpoint()?;
            }
            let p = component[cursor];
            cursor += 1;
            let (x, y) = (p % w, p / w);
            for q in [
                if x > 0 { Some(p - 1) } else { None },
                if x + 1 < w { Some(p + 1) } else { None },
                if y > 0 { Some(p - w) } else { None },
                if y + 1 < h { Some(p + w) } else { None },
            ]
            .into_iter()
            .flatten()
            {
                if !seen[q] && maps.confidence[q] >= 0.25 {
                    seen[q] = true;
                    component.push(q);
                }
            }
        }
        if component.len() < (radius * radius / 32).max(12) {
            for p in component {
                maps.confidence[p] = 0.;
            }
        }
    }
    for p in 0..w * h {
        if maps.confidence[p] < 0.25 {
            maps.opacity[p] = 0.;
            maps.confidence[p] = 0.;
        }
    }
    Ok(maps)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_static_strokes_over_unregistered_backgrounds() {
        let mut state = 17u32;
        let frames: Vec<_> = (0..32)
            .map(|_| {
                let rgb = (0..80 * 64)
                    .flat_map(|p| {
                        let alpha = if (34..42).contains(&(p % 80)) && (12..52).contains(&(p / 80))
                        {
                            0.7
                        } else {
                            0.
                        };
                        std::array::from_fn::<_, 3, _>(|_| {
                            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                            (((state >> 24) as f32) * (1. - alpha) + 220. * alpha).round() as u8
                        })
                    })
                    .collect();
                Frame::new(80, 64, rgb).unwrap()
            })
            .collect();
        let m = detect(&frames, [20, 6, 40, 52], 16);
        let center = 26 * 40 + 18;
        assert!(m.confidence[center] > 0.5);
        assert!((m.opacity[center] - 0.7).abs() < 0.15);
        let false_positives = m
            .confidence
            .iter()
            .enumerate()
            .filter(|(p, c)| **c > 0.5 && !(14..22).contains(&(p % 40)))
            .count();
        assert!(false_positives < 20, "{false_positives} false positives");
    }
    #[test]
    fn recovers_broad_colored_interior_and_rejects_isolated_speckles() {
        let frames: Vec<_> = (0..24)
            .map(|t| {
                let bg = [
                    40 + (t * 71 % 180) as u8,
                    30 + (t * 43 % 190) as u8,
                    20 + (t * 97 % 200) as u8,
                ];
                let mut rgb = Vec::new();
                for y in 0..64 {
                    for x in 0..72 {
                        let emblem = (20..52).contains(&x) && (12..44).contains(&y);
                        let hole = (34..38).contains(&x) && (25..29).contains(&y);
                        let speck = (7..9).contains(&x) && (7..9).contains(&y);
                        rgb.extend_from_slice(if (emblem && !hole) || speck {
                            &[210, 12, 8]
                        } else {
                            &bg
                        });
                    }
                }
                Frame::new(72, 64, rgb).unwrap()
            })
            .collect();
        let maps = detect(&frames, [0, 0, 72, 64], 8);
        // This interior has no changing pixels within the original radius.
        assert!(maps.opacity[28 * 72 + 30] > 0.95);
        assert!(maps.confidence[28 * 72 + 30] > 0.7);
        // Actual background visible through the emblem is not filled geometrically.
        assert_eq!(maps.opacity[26 * 72 + 35], 0.);
        for y in 7..9 {
            for x in 7..9 {
                assert_eq!(maps.opacity[y * 72 + x], 0.);
            }
        }
    }

    #[test]
    fn gray_strokes_survive_contrast_reversals_without_selecting_background() {
        let frames: Vec<_> = (0..16).map(|t| {
            let bg = if t % 2 == 0 { 32. } else { 224. };
            let mut rgb = Vec::new();
            for y in 0..48 {
                for x in 0..64 {
                    let stroke = (22..26).contains(&x) && (8..40).contains(&y);
                    let value = if stroke { bg * 0.3 + 128. * 0.7 } else { bg };
                    rgb.extend_from_slice(&[value as u8; 3]);
                }
            }
            Frame::new(64, 48, rgb).unwrap()
        }).collect();
        let maps = detect(&frames, [0, 0, 64, 48], 12);
        for y in 0..48 {
            for x in 0..64 {
                if (22..26).contains(&x) && (8..40).contains(&y) {
                    assert!(maps.confidence[y*64+x] >= 0.5, "missed stroke at {x},{y}");
                } else {
                    assert_eq!(maps.confidence[y*64+x], 0., "background at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn entirely_static_scenery_is_unknown() {
        let f = Frame::new(32, 32, vec![120; 32 * 32 * 3]).unwrap();
        let maps = detect(&vec![f; 16], [4, 4, 24, 24], 8);
        assert!(maps.confidence.iter().all(|&v| v == 0.));
    }
}
