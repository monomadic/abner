//! A translucent screen-space overlay transmits nearby background variation.
use super::{Check, Frame, native_luma};

const SAMPLE_POINTS: usize = 256;
const OPAQUE: f32 = 0.90;
const MIN_BAND_RMS: f64 = 0.035;
const MIN_BAND_R2: f64 = 0.85;
const MIN_BLEND_R2: f64 = 0.45;

type Point = [usize; 2];
type Rgb = [f64; 3];

/// Reject a translucent candidate when a coherent, changing band predicts
/// transmission inconsistent with the component. Opaque candidates and ambiguous
/// neighbourhoods carry no reliable information about the background under them.
pub(super) fn contradicts(
    frames: &[Frame],
    component: &[usize],
    opacity: &[f32],
    [x, y, width]: [usize; 3],
    band: impl Iterator<Item = Point> + Clone,
    check: Check<'_>,
) -> Result<bool, String> {
    if frames.len() < 8 {
        return Ok(false);
    }
    let stride = component.len().div_ceil(SAMPLE_POINTS);
    let mut opacities: Vec<_> = component
        .iter()
        .step_by(stride)
        .map(|&p| opacity[p])
        .collect();
    let middle = opacities.len() / 2;
    let expected_opacity = *opacities.select_nth_unstable_by(middle, f32::total_cmp).1;
    if expected_opacity >= OPAQUE {
        return Ok(false);
    }

    let points: Vec<_> = component
        .iter()
        .step_by(stride)
        .map(|&p| [x + p % width, y + p / width])
        .collect();
    let mut lo = points[0];
    let mut hi = points[0];
    for point in &points {
        for axis in 0..2 {
            lo[axis] = lo[axis].min(point[axis]);
            hi[axis] = hi[axis].max(point[axis]);
        }
    }
    let axis = usize::from(hi[1] - lo[1] > hi[0] - lo[0]);
    let middle = lo[axis] + (hi[axis] - lo[axis]) / 2;
    let side = |point: Point| usize::from(point[axis] > middle);
    let counts = band.clone().fold([0usize; 2], |mut counts, point| {
        counts[side(point)] += 1;
        counts
    });
    if counts.iter().any(|&n| n < 16) {
        return Ok(false);
    }
    let steps = counts.map(|n| n.div_ceil(SAMPLE_POINTS / 2));
    let mut seen = [0usize; 2];
    let mut points_by_side = [Vec::new(), Vec::new()];
    for point in band {
        let i = side(point);
        if seen[i].is_multiple_of(steps[i]) {
            points_by_side[i].push(point);
        }
        seen[i] += 1;
    }
    let [near, far] = points_by_side;
    let native = native_luma(frames);
    let mut near_signal = Vec::with_capacity(frames.len());
    let mut far_signal = Vec::with_capacity(frames.len());
    for (i, frame) in frames.iter().enumerate() {
        if i % 32 == 0 {
            check()?;
        }
        near_signal.push(mean(frame, near.iter(), native));
        far_signal.push(mean(frame, far.iter(), native));
    }

    // The band is only a proxy for the hidden background. Independent motion
    // on opposite sides cannot justify a rejection.
    let agreement = regression(&near_signal, &far_signal);
    if agreement.rms < MIN_BAND_RMS
        || agreement.r2 < MIN_BAND_R2
        || !(0.7..=1.4).contains(&agreement.slope)
    {
        return Ok(false);
    }

    let background: Vec<_> = near_signal
        .into_iter()
        .zip(far_signal)
        .map(|(a, b)| std::array::from_fn(|c| (a[c] + b[c]) * 0.5))
        .collect();
    let mut foreground = Vec::with_capacity(frames.len());
    for (i, frame) in frames.iter().enumerate() {
        if i % 32 == 0 {
            check()?;
        }
        foreground.push(mean(frame, points.iter(), native));
    }

    let fit = regression(&background, &foreground);
    let expected = f64::from(1. - expected_opacity);
    // Display-encoded RGB and a spatial background proxy both introduce error.
    let tolerance = (expected * 0.55).max(0.10);
    Ok(fit.rms >= MIN_BAND_RMS
        && (fit.r2 < MIN_BLEND_R2 || (fit.slope - expected).abs() > tolerance))
}

fn mean<'a>(frame: &Frame, points: impl Iterator<Item = &'a Point>, native: bool) -> Rgb {
    let mut sum = [0.; 3];
    let mut count = 0;
    for &[x, y] in points {
        let values = if native {
            [frame.luma(x, y); 3]
        } else {
            frame.rgb_pixel(x, y)
        };
        for (sum, value) in sum.iter_mut().zip(values) {
            *sum += f64::from(value);
        }
        count += 1;
    }
    sum.map(|v| v / f64::from(count))
}

struct Fit {
    slope: f64,
    r2: f64,
    rms: f64,
}

/// Fit one transmission coefficient with a separate intercept for each channel.
fn regression(x: &[Rgb], y: &[Rgb]) -> Fit {
    let n = x.len() as f64;
    let (mut xx, mut yy, mut xy) = (0., 0., 0.);
    for c in 0..3 {
        let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0., 0., 0., 0., 0.);
        for (x, y) in x.iter().zip(y) {
            let (x, y) = (x[c], y[c]);
            sx += x;
            sy += y;
            sxx += x * x;
            syy += y * y;
            sxy += x * y;
        }
        xx += (sxx - sx * sx / n).max(0.);
        yy += (syy - sy * sy / n).max(0.);
        xy += sxy - sx * sy / n;
    }
    Fit {
        slope: if xx > 0. { xy / xx } else { 0. },
        r2: if xx > 0. && yy > 0. && xy > 0. {
            (xy * xy / (xx * yy)).min(1.)
        } else {
            0.
        },
        rms: (xx / (n * 3.)).sqrt(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independently_moving_sides_cannot_veto_a_candidate() {
        let [w, h] = [40, 8];
        let frames: Vec<_> = (0..32)
            .map(|t| {
                let left = 24 + (t * 53 % 208) as u8;
                let right = 24 + (t * 89 % 208) as u8;
                let rgb = (0..w * h)
                    .flat_map(|p| match p % w {
                        0..=9 => [left; 3],
                        10..=23 => [120; 3],
                        _ => [right; 3],
                    })
                    .collect();
                Frame::new(w, h, rgb).unwrap()
            })
            .collect();
        let component: Vec<_> = (0..w * h).filter(|p| (10..24).contains(&(p % w))).collect();
        let band = (0..w * h)
            .filter(|p| !(10..24).contains(&(p % w)))
            .map(|p| [p % w, p / w]);
        assert!(
            !contradicts(
                &frames,
                &component,
                &vec![0.65; w * h],
                [0, 0, w],
                band,
                &|| Ok(())
            )
            .unwrap()
        );
    }
}
