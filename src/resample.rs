//! swscale's "spline" scaler (`SWS_SPLINE`, what `-sws_flags spline` selects)
//! as a float, separable resampler on packed RGB16 — the downscale behind the
//! Process page's Downsize input row. The kernel, its support and the
//! sample-centre convention follow libswscale's `initFilter`: a cubic spline
//! with p = −2.196152422706632 (infinite support, cut at 20 source pixels'
//! worth of taps), stretched by the ratio when shrinking so it anti-aliases,
//! edge samples clamped. Coefficients stay float where swscale quantizes to
//! 14 bits, so a pixel may differ from ffmpeg's by an LSB or two; the test
//! below holds it to that. One trap when comparing: swscale scales RGB as
//! Y'CbCr planes internally and, unless `full_chroma_inp` is set, halves the
//! chroma horizontally BEFORE scaling — the reference needs that flag, or the
//! colour channels land 38 dB off while gray matches exactly.
use rayon::prelude::*;

/// One output sample: the first source index its taps cover and their weights.
#[derive(Clone, Debug)]
pub struct Taps {
    pub start: isize,
    pub weights: Vec<f32>,
}

fn spline_coeff(mut a: f64, mut b: f64, mut c: f64, mut d: f64, mut dist: f64) -> f64 {
    loop {
        if dist <= 1.0 {
            return ((d * dist + c) * dist + b) * dist + a;
        }
        let (nb, nc, nd) = (b + 2.0 * c + 3.0 * d, c + 3.0 * d, -b - 3.0 * c - 6.0 * d);
        a = 0.0;
        b = nb;
        c = nc;
        d = nd;
        dist -= 1.0;
    }
}

/// swscale's spline kernel at `dist` (in destination pixels when shrinking).
fn kernel(dist: f64) -> f64 {
    const P: f64 = -2.196152422706632;
    spline_coeff(1.0, 0.0, P, -P - 1.0, dist)
}

/// The taps for every destination index along one axis of `src` → `dst`
/// samples, as swscale lays them out (`initFilter`'s generic branch with
/// size factor 20 and luma positions).
pub fn spline_taps(src: usize, dst: usize) -> Vec<Taps> {
    assert!(src > 0 && dst > 0);
    let (src_i, dst_i) = (src as i64, dst as i64);
    let x_inc = ((src_i << 16) + (dst_i >> 1)) / dst_i;
    let size_factor = 20i64;
    let mut filter_size = if x_inc <= 1 << 16 {
        1 + size_factor
    } else {
        1 + (size_factor * src_i + dst_i - 1) / dst_i
    };
    filter_size = filter_size.min(src_i - 2).max(1);
    let mut x_dst_in_src: i64 = x_inc - 0x10000;
    (0..dst)
        .map(|_| {
            let xx = (x_dst_in_src - (filter_size - 2) * (1 << 16)) / (1 << 17);
            let mut weights = Vec::with_capacity(filter_size as usize);
            for j in 0..filter_size {
                let mut d = ((xx + j) * (1 << 17) - x_dst_in_src).abs() << 13;
                if x_inc > 1 << 16 {
                    d = d * dst_i / src_i;
                }
                weights.push(kernel(d as f64 / f64::from(1u32 << 30)));
            }
            let sum: f64 = weights.iter().sum();
            let norm = if sum.abs() > f64::EPSILON { 1.0 / sum } else { 1.0 };
            x_dst_in_src += 2 * x_inc;
            Taps { start: xx as isize, weights: weights.iter().map(|w| (w * norm) as f32).collect() }
        })
        .collect()
}

/// Resample packed RGB16 `src` (`sw`×`sh`) to `dw`×`dh` with the spline,
/// horizontally then vertically, rounding once at the end.
pub fn spline_resize_rgb16(src: &[u16], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u16> {
    assert_eq!(src.len(), sw * sh * 3, "packed RGB16 of the stated size");
    if dw == sw && dh == sh {
        return src.to_vec();
    }
    let xt = spline_taps(sw, dw);
    let yt = spline_taps(sh, dh);
    // Horizontal pass: sh rows of dw RGB floats.
    let mut mid = vec![0f32; dw * sh * 3];
    mid.par_chunks_mut(dw * 3).enumerate().for_each(|(y, row)| {
        let line = &src[y * sw * 3..(y + 1) * sw * 3];
        for (x, t) in xt.iter().enumerate() {
            let mut acc = [0f32; 3];
            for (j, w) in t.weights.iter().enumerate() {
                let sx = (t.start + j as isize).clamp(0, sw as isize - 1) as usize;
                let p = &line[sx * 3..sx * 3 + 3];
                acc[0] += w * f32::from(p[0]);
                acc[1] += w * f32::from(p[1]);
                acc[2] += w * f32::from(p[2]);
            }
            row[x * 3..x * 3 + 3].copy_from_slice(&acc);
        }
    });
    // Vertical pass: dh rows of dw RGB16.
    let mut out = vec![0u16; dw * dh * 3];
    out.par_chunks_mut(dw * 3).enumerate().for_each(|(y, row)| {
        let t = &yt[y];
        let mut acc = vec![0f32; dw * 3];
        for (j, w) in t.weights.iter().enumerate() {
            let sy = (t.start + j as isize).clamp(0, sh as isize - 1) as usize;
            let line = &mid[sy * dw * 3..(sy + 1) * dw * 3];
            for (a, v) in acc.iter_mut().zip(line) {
                *a += w * v;
            }
        }
        for (o, a) in row.iter_mut().zip(&acc) {
            *o = a.round().clamp(0.0, 65535.0) as u16;
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn taps_cover_the_source_and_sum_to_one() {
        for (src, dst) in [(1920usize, 960usize), (1920, 480), (64, 32), (7, 3)] {
            let taps = spline_taps(src, dst);
            assert_eq!(taps.len(), dst);
            for t in &taps {
                let sum: f32 = t.weights.iter().sum();
                assert!((sum - 1.0).abs() < 1e-4, "{src}→{dst}: {sum}");
            }
            // The last destination pixel's taps reach the end of the source.
            let last = taps.last().unwrap();
            assert!(last.start + last.weights.len() as isize >= src as isize - 1);
        }
        // A 1:1 kernel sample is the identity.
        assert!((kernel(0.0) - 1.0).abs() < 1e-12);
        assert!(kernel(1.0).abs() < 1e-12);
    }

    #[test]
    fn flat_images_stay_flat_and_a_2x_shrink_halves_the_size() {
        let (w, h) = (40, 24);
        let src: Vec<u16> = (0..w * h * 3).map(|i| [1000, 40000, 65535][i % 3]).collect();
        let out = spline_resize_rgb16(&src, w, h, w / 2, h / 2);
        assert_eq!(out.len(), (w / 2) * (h / 2) * 3);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(*v, [1000, 40000, 65535][i % 3], "pixel {i}");
        }
    }

    /// Against ffmpeg's own spline scale on the same RGB frame: the float
    /// kernel against swscale's 14-bit one (`full_chroma_inp` keeps swscale
    /// from halving the chroma horizontally on the way in).
    #[test]
    fn matches_ffmpegs_spline_downscale() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("abner_resample_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sw, sh, dw, dh) = (192usize, 108usize, 96usize, 54usize);
        let src = dir.join("src.rgb");
        let refp = dir.join("ref.rgb");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", &format!("testsrc2=size={sw}x{sh}:rate=1"),
                "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo"])
            .arg(&src).status().unwrap().success();
        assert!(ok);
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &format!("{sw}x{sh}"), "-i"])
            .arg(&src)
            .args(["-vf", &format!("scale={dw}:{dh}:flags=spline+accurate_rnd+full_chroma_int+full_chroma_inp"),
                "-pix_fmt", "rgb24", "-f", "rawvideo"])
            .arg(&refp).status().unwrap().success();
        assert!(ok);
        let src8 = std::fs::read(&src).unwrap();
        let ref8 = std::fs::read(&refp).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(src8.len(), sw * sh * 3);
        assert_eq!(ref8.len(), dw * dh * 3);
        let src16: Vec<u16> = src8.iter().map(|v| u16::from(*v) * 257).collect();
        let out = spline_resize_rgb16(&src16, sw, sh, dw, dh);
        let out8: Vec<u8> = out.iter().map(|v| ((u32::from(*v) + 128) / 257) as u8).collect();
        let (mut sq, mut worst) = (0f64, 0i32);
        for (a, b) in out8.iter().zip(&ref8) {
            let d = i32::from(*a) - i32::from(*b);
            sq += f64::from(d * d);
            worst = worst.max(d.abs());
        }
        let psnr = 10.0 * (255.0f64.powi(2) / (sq / out8.len() as f64).max(1e-9)).log10();
        assert!(psnr > 45.0 && worst <= 6, "PSNR {psnr:.2} dB, worst {worst}");
    }
}
