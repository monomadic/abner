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

/// Detection uses source time, independently of the visible playhead.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Range { pub start:f64, pub end:f64 }
impl Range {
    #[cfg(test)]
    pub fn near(at:f64,duration:f64)->Self { Self{start:(at-30.).max(0.),end:(at+30.).min(duration)} }
    pub fn samples(self,duration:f64,fps:f64)->Result<Vec<f64>,String> {
        if !duration.is_finite() || !fps.is_finite() || duration<=0. || fps<=0. || !self.start.is_finite() || !self.end.is_finite() || self.start<0. || self.end>duration || self.start>=self.end {return Err("Select a valid detection range inside the clip".into());}
        let hi=self.end.min((duration-1./fps).max(0.));
        let mut times:Vec<_>=(0..16).map(|i|((self.start+(hi-self.start)*i as f64/15.)*fps).floor()/fps).collect();
        times.dedup_by(|a,b|(*a-*b).abs()<0.5/fps);
        if times.len()<6 {return Err("Select at least six distinct frames for detection".into());}
        Ok(times)
    }
    pub fn set_edge(&mut self,end:bool,at:f64,duration:f64,fps:f64) {
        let minimum=(6./fps.max(1.)).min(duration);
        let t=(at*fps.max(1.)).round()/fps.max(1.);
        if end {self.end=t.clamp((self.start+minimum).min(duration),duration);}else{self.start=t.clamp(0.,(self.end-minimum).max(0.));}
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
        source: usize,
        mask_id: u64,
        revision: u64,
        region: Option<[usize; 4]>,
        range: Range,
    ) -> Result<Self, String> {
        let region = detection_region(region, info.width as usize, info.height as usize)?;
        let times = range.samples(info.duration, info.fps)?;
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
                        analyze(info.path, info.width, info.height, &times, &flag, &count, Some(region))
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

#[cfg(test)]
fn sample_times(at:f64,duration:f64,fps:f64)->Result<Vec<f64>,String> {
    Range::near(at.clamp(0.,duration.max(0.)),duration).samples(duration,fps)
}

fn detection_region(region: Option<[usize;4]>, w:usize, h:usize) -> Result<[usize;4],String> {
    let [x,y,rw,rh] = region.unwrap_or([0,0,w,h]);
    if x>=w || y>=h || rw==0 || rh==0 {return Err("The processing region is empty or outside the image".into());}
    Ok([x,y,rw.min(w-x),rh.min(h-y)])
}
fn region_mask(w:usize,h:usize,region:[usize;4],opacity:&[f32],confidence:&[f32]) -> Vec<u8> {
    let [x,y,rw,rh] = region;
    let mut pixels = vec![0;w*h];
    for row in 0..rh { for col in 0..rw {
        let i = row*rw+col;
        if opacity[i]>=0.35 && confidence[i]>=0.25 {pixels[(y+row)*w+x+col]=255;}
    }}
    pixels
}

fn analyze(
    path: PathBuf,
    w: u32,
    h: u32,
    times: &[f64],
    cancel: &AtomicBool,
    progress: &AtomicUsize,
    region: Option<[usize;4]>,
) -> Result<Proposal, String> {
    let region = detection_region(region, w as usize, h as usize)?;
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
        static_overlay::detect_checked(&frames, region, 32, &mut check)?;
    check()?;
    let pixels = region_mask(w as usize, h as usize, region, &maps.opacity, &maps.confidence);
    Ok(Proposal {
        pixels,
        samples: frames.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_detection_range_samples_only_its_frames_and_clamps_handles() {
        let range=Range{start:12.,end:18.};let times=range.samples(100.,30.).unwrap();
        assert_eq!(times.len(),16);assert_eq!(times[0],12.);assert_eq!(*times.last().unwrap(),18.);
        assert!(times.iter().all(|&t|t>=12.&&t<=18.));
        let mut selected=range;selected.set_edge(false,90.,100.,30.);
        assert!(selected.end-selected.start>=6./30.-1e-9);
        selected.set_edge(true,0.,100.,30.);assert!(selected.end>selected.start);
        assert!(Range{start:0.,end:0.1}.samples(10.,30.).is_err());
        let full=Range{start:0.,end:2.};assert!(*full.samples(2.,30.).unwrap().last().unwrap()<2.);
    }
    #[test]
    fn region_detection_only_marks_pixels_inside_the_processing_region() {
        let region = detection_region(Some([3,2,99,2]),8,6).unwrap();
        assert_eq!(region,[3,2,5,2]);
        let pixels = region_mask(8,6,region,&vec![1.;10],&vec![1.;10]);
        for y in 0..6 { for x in 0..8 {
            assert_eq!(pixels[y*8+x],if (2..4).contains(&y) && x>=3 {255}else{0});
        }}
        assert!(detection_region(Some([8,0,1,1]),8,6).is_err());
        assert!(detection_region(Some([0,0,0,1]),8,6).is_err());
        assert_eq!(detection_region(None,8,6).unwrap(),[0,0,8,6]);
    }
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
            None,
        )
        .err()
        .unwrap();
        assert_eq!(err, "Watermark detection stopped");
    }
}
