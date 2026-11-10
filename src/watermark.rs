//! Built-in mask proposals from stationary overlays over changing nearby frames.
//! Owns a background decoder; never seeks the visible player or writes media.
mod static_overlay;
use crate::probe::VideoInfo;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Frame {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
}
impl Frame {
    fn new(width: usize, height: usize, rgb: Vec<u8>) -> Result<Self, String> {
        if width.checked_mul(height).and_then(|v| v.checked_mul(3)) != Some(rgb.len())
            || width == 0
            || height == 0
        {
            return Err("Invalid sampled frame dimensions".into());
        }
        Ok(Self { width, height, rgb })
    }
    fn pixel(&self, x: usize, y: usize) -> [f32; 3] {
        let p = (y * self.width + x) * 3;
        std::array::from_fn(|c| self.rgb[p + c] as f32 / 255.)
    }
}

pub(crate) struct Proposal {
    pub pixels: Vec<u8>,
    pub samples: usize,
}
pub(crate) struct Task {
    pub source: usize,
    pub mask_id: u64,
    pub revision: u64,
    pub receiver: Receiver<Result<Proposal, String>>,
    pub progress: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
impl Task {
    #[cfg(test)]
    pub fn test_pending(
        source: usize,
        mask_id: u64,
        revision: u64,
    ) -> (
        Self,
        std::sync::mpsc::Sender<Result<Proposal, String>>,
        Arc<AtomicBool>,
    ) {
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        (
            Self {
                source,
                mask_id,
                revision,
                receiver,
                progress: Arc::new(AtomicUsize::new(0)),
                cancel: cancel.clone(),
                started: Instant::now(),
            },
            sender,
            cancel,
        )
    }

    pub fn status(&self) -> String {
        let n = self.progress.load(Ordering::Relaxed);
        if n <= 16 {
            format!("Detect watermark · reading nearby frames {n}/16…")
        } else {
            "Detect watermark · finding static shapes…".into()
        }
    }
    pub fn expired(&self) -> bool {
        self.started.elapsed() > Duration::from_secs(180)
    }
    pub fn spawn(
        info: VideoInfo,
        at: f64,
        source: usize,
        mask_id: u64,
        revision: u64,
    ) -> Result<Self, String> {
        let times = sample_times(at, info.duration, info.fps)?;
        let n = info.width as usize * info.height as usize;
        // Samples plus statistical maps, graph queues and one display conversion.
        if n.checked_mul(16 * 3 + 112)
            .is_none_or(|v| v > 1536 * 1024 * 1024)
        {
            return Err("This clip exceeds the memory limit for watermark detection".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let progress = Arc::new(AtomicUsize::new(0));
        let count = progress.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("watermark detection".into())
            .spawn(move || {
                let result = filmcraft_media::cancel::with_cancel(&flag, || {
                    filmcraft_media::cancel::with_background(true, || {
                        analyze(info.path, info.width, info.height, &times, &flag, &count)
                    })
                });
                let _ = sender.send(result);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            source,
            mask_id,
            revision,
            receiver,
            progress,
            cancel,
            started: Instant::now(),
        })
    }
}

fn sample_times(at: f64, duration: f64, fps: f64) -> Result<Vec<f64>, String> {
    if !at.is_finite() || !duration.is_finite() || !fps.is_finite() || duration <= 0. || fps <= 0. {
        return Err("Watermark detection needs a clip with known duration and frame rate".into());
    }
    let last = (duration - 1. / fps).max(0.);
    let center = at.clamp(0., last);
    let lo = (center - 30.).max(0.);
    let hi = (center + 30.).min(last);
    let mut times: Vec<_> = (0..16)
        .map(|i| ((lo + (hi - lo) * i as f64 / 15.) * fps).floor() / fps)
        .collect();
    times.dedup_by(|a, b| (*a - *b).abs() < 0.5 / fps);
    if times.len() < 6 {
        return Err("At least six distinct frames are needed to detect a watermark".into());
    }
    Ok(times)
}

fn analyze(
    path: PathBuf,
    w: u32,
    h: u32,
    times: &[f64],
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> Result<Proposal, String> {
    let started = Instant::now();
    let mut check = || {
        if cancel.load(Ordering::Relaxed) || started.elapsed() > Duration::from_secs(180) {
            Err("Watermark detection stopped".into())
        } else {
            Ok(())
        }
    };
    check()?;
    let source = crate::fc::open(&path)?;
    let mut frames = Vec::new();
    let frame_s = crate::fc::timing(&source).map_or(0., |v| v.0);
    let mut rgba = Vec::new();
    for (i, &t) in times.iter().enumerate() {
        check()?;
        let picture = source
            .video_frame(filmcraft_media::FrameRequest::full(
                filmcraft_time::Tick::from_seconds_f64(t + frame_s * 0.25),
            ))
            .map_err(|e| format!("Could not read a nearby frame: {e}"))?;
        if (picture.width, picture.height) != (w, h) {
            return Err("Sampled frame geometry does not match the mask".into());
        }
        crate::fc::fit_rgba_into(&picture, crate::fc::Fit::Exact { w, h }, &mut rgba);
        let rgb: Vec<u8> = rgba
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        // Codec frames can leave the informational PTS unset. Use actual pixels
        // to avoid counting identical decoded observations as independent evidence.
        if !frames.iter().any(|f: &Frame| f.rgb == rgb) {
            frames.push(Frame::new(w as usize, h as usize, rgb)?);
        }
        progress.store(i + 1, Ordering::Relaxed);
    }
    drop(source);
    drop(rgba);
    if frames.len() < 6 {
        return Err("Too few distinct decoded frames to detect a watermark".into());
    }
    progress.store(17, Ordering::Relaxed);
    let maps =
        static_overlay::detect_checked(&frames, [0, 0, w as usize, h as usize], 32, &mut check)?;
    check()?;
    let pixels = maps
        .opacity
        .iter()
        .zip(&maps.confidence)
        .map(|(&a, &c)| if a >= 0.35 && c >= 0.25 { 255 } else { 0 })
        .collect();
    Ok(Proposal {
        pixels,
        samples: frames.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn samples_are_near_playhead_distinct_and_inside_clip() {
        assert_eq!(
            sample_times(60., 200., 30.).unwrap(),
            (0..16).map(|i| 30. + i as f64 * 4.).collect::<Vec<_>>()
        );
        for at in [0., 9.99, 99.] {
            let t = sample_times(at, 10., 30.).unwrap();
            assert!(t.windows(2).all(|w| w[1] > w[0]));
            assert!(t.iter().all(|&t| t >= 0. && t < 10.));
        }
        assert!(sample_times(0., 0.1, 30.).is_err());
        assert!(sample_times(0., f64::NAN, 30.).is_err());
    }
    #[test]
    fn cancelled_analysis_never_opens_the_source() {
        let err = analyze(
            PathBuf::from("missing-video"),
            32,
            32,
            &[0.; 16],
            &AtomicBool::new(true),
            &AtomicUsize::new(0),
        )
        .err()
        .unwrap();
        assert_eq!(err, "Watermark detection stopped");
    }
}
