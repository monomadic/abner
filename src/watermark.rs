//! Built-in mask proposals from stationary overlays over changing nearby frames.
//! Owns a background decoder; never seeks the visible player or writes media.
mod static_overlay;
mod stroke_lift;
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
    #[cfg(test)]
    pub fn samples(self,duration:f64,fps:f64)->Result<Vec<f64>,String> {
        self.samples_count(duration,fps,Some(64))
    }
    /// None means every frame in the selected range, not an unlimited sample cap.
    pub fn samples_count(self,duration:f64,fps:f64,count:Option<usize>)->Result<Vec<f64>,String> {
        if !duration.is_finite() || !fps.is_finite() || duration<=0. || fps<=0. || !self.start.is_finite() || !self.end.is_finite() || self.start<0. || self.end>duration || self.start>=self.end {return Err("Select a valid detection range inside the clip".into());}
        let first=(self.start*fps-1e-7).ceil().max(0.) as usize;
        let last=((self.end*fps+1e-7).floor() as usize).min((duration*fps-1e-7).ceil() as usize-1);
        let available=last.saturating_sub(first)+1;
        let n=count.unwrap_or(available).min(available);
        if n<6 || last<first {return Err("Select at least six distinct frames for detection".into());}
        if n>10_000_000 {return Err("Select a shorter detection range".into());}
        let times=(0..n).map(|i|(first+i*(last-first)/(n-1)) as f64/fps).collect();
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
    total: usize,
    timeout: Duration,
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
                total:64, timeout:Duration::from_secs(180),
            },
            sender,
            cancel,
        )
    }

    pub fn status(&self) -> String {
        let n = self.progress.load(Ordering::Relaxed);
        if n <= self.total {
            format!("Autodetect · reading frames {n}/{}…",self.total)
        } else {
            "Autodetect · analysing static pixels…".into()
        }
    }
    pub fn expired(&self) -> bool {
        self.started.elapsed() > self.timeout
    }
    pub fn spawn(
        info: VideoInfo,
        source: usize,
        mask_id: u64,
        revision: u64,
        region: Option<[usize; 4]>,
        range: Range,
        drawn: bool,
        samples: Option<usize>,
    ) -> Result<Self, String> {
        let region = detection_region(region, info.width as usize, info.height as usize)?;
        let times = range.samples_count(info.duration, info.fps, samples)?;
        let total=times.len();
        let timeout=Duration::from_secs((total as u64*3).max(180));
        let context=context_region(region,info.width as usize,info.height as usize);
        let n=context[2]*context[3];
        // All-frame scans combine bounded batches spread across the entire range.
        let batch=if samples.is_none(){64.min(total)}else{total};
        if n.checked_mul(batch*3+112).is_none_or(|v|v>1536*1024*1024) {
            return Err("Detection needs more memory: reduce the detection area or frame count".into());
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
                        if samples.is_some() || total<=64 {
                            analyze(info.path, info.width, info.height, &times, &flag, &count, Some(region), drawn)
                        } else {
                            let groups=total.div_ceil(64);
                            let mut votes=vec![0usize;info.width as usize*info.height as usize];
                            let mut used=0;
                            for group in 0..groups {
                                if flag.load(Ordering::Relaxed) {return Err("Watermark detection stopped".into());}
                                let batch:Vec<_>=times.iter().skip(group).step_by(groups).copied().collect();
                                let local=AtomicUsize::new(0);
                                let proposal=analyze(info.path.clone(),info.width,info.height,&batch,&flag,&local,Some(region),drawn)?;
                                for (vote,pixel) in votes.iter_mut().zip(proposal.pixels) {if pixel>0 {*vote+=1;}}
                                used+=proposal.samples;
                                count.store(((group+1)*total/groups).min(total),Ordering::Relaxed);
                            }
                            Ok(Proposal{pixels:votes.into_iter().map(|n|if n*5>=groups*3 {255}else{0}).collect(),samples:used})
                        }
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
            total,timeout,
        })
    }
}

#[cfg(test)]
fn sample_times(at:f64,duration:f64,fps:f64)->Result<Vec<f64>,String> {
    Range::near(at.clamp(0.,duration.max(0.)),duration).samples_count(duration,fps,Some(16))
}

fn detection_region(region: Option<[usize;4]>, w:usize, h:usize) -> Result<[usize;4],String> {
    let [x,y,rw,rh] = region.unwrap_or([0,0,w,h]);
    if x>=w || y>=h || rw==0 || rh==0 {return Err("The processing region is empty or outside the image".into());}
    Ok([x,y,rw.min(w-x),rh.min(h-y)])
}
fn context_region([x,y,w,h]:[usize;4],fw:usize,fh:usize)->[usize;4] {
    let left=x.saturating_sub(96);let top=y.saturating_sub(96);
    [left,top,(x+w+96).min(fw)-left,(y+h+96).min(fh)-top]
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
    drawn: bool,
) -> Result<Proposal, String> {
    let region = detection_region(region, w as usize, h as usize)?;
    let context=context_region(region,w as usize,h as usize);
    let local_region=[region[0]-context[0],region[1]-context[1],region[2],region[3]];
    let timeout=Duration::from_secs((times.len() as u64*3).max(180));
    let started = Instant::now();
    let mut check = || {
        if cancel.load(Ordering::Relaxed) || started.elapsed() > timeout {
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
        let mut rgb=Vec::with_capacity(context[2]*context[3]*3);
        for y in context[1]..context[1]+context[3] {
            for x in context[0]..context[0]+context[2] {
                let at=(y*w as usize+x)*4;rgb.extend_from_slice(&rgba[at..at+3]);
            }
        }
        // Codec frames can leave the informational PTS unset. Use actual pixels
        // to avoid counting identical decoded observations as independent evidence.
        if !frames.iter().any(|f: &Frame| f.rgb == rgb) {
            frames.push(Frame::new(context[2], context[3], rgb)?);
        }
        progress.store(i + 1, Ordering::Relaxed);
    }
    drop(source);
    drop(rgba);
    if frames.len() < 6 {
        return Err("Too few distinct decoded frames to detect a watermark".into());
    }
    progress.store(times.len()+1, Ordering::Relaxed);
    let maps =
        static_overlay::detect_checked(&frames, local_region, 32, &mut check)?;
    check()?;
    let mut pixels = region_mask(w as usize, h as usize, region, &maps.opacity, &maps.confidence);
    // A box drawn tightly around an overlay is the user's claim that one is there,
    // which licenses weaker evidence, including over scenery that never changes.
    if drawn && !pixels.iter().any(|&p|p>0) && region[2] * region[3] * 4 <= w as usize * h as usize {
        let strokes = stroke_lift::detect_checked(&frames, local_region, &mut check)?;
        // Static-scene evidence is a fallback, never a replacement for a temporal mask.
        if strokes.iter().any(|s| *s) {
            pixels.fill(0);
            for (i, _) in strokes.iter().enumerate().filter(|(_, s)| **s) {
                pixels[(region[1] + i / region[2]) * w as usize + region[0] + i % region[2]] = 255;
            }
        }
    }
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
        assert_eq!(times.len(),64);assert_eq!(times[0],12.);assert_eq!(*times.last().unwrap(),18.);
        assert!(times.iter().all(|&t|t>=12.&&t<=18.));
        let mut selected=range;selected.set_edge(false,90.,100.,30.);
        assert!(selected.end-selected.start>=6./30.-1e-9);
        selected.set_edge(true,0.,100.,30.);assert!(selected.end>selected.start);
        assert!(Range{start:0.,end:0.1}.samples(10.,30.).is_err());
        let full=Range{start:0.,end:2.};assert!(*full.samples(2.,30.).unwrap().last().unwrap()<2.);
    }
    #[test]
    fn selectable_counts_and_all_frames_respect_non_grid_boundaries() {
        let r=Range{start:12.01,end:18.02};
        for count in [Some(16),Some(64),Some(128),None] {
            let t=r.samples_count(100.,30.,count).unwrap();
            assert_eq!(t.len(),count.unwrap_or(180));
            assert!(t.iter().all(|&v|v>=r.start && v<=r.end));
            assert!(t.windows(2).all(|w|w[0]<w[1]));
        }
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
    /// Explicit local diagnostic: no media path or output is built into the app.
    #[test]
    #[ignore = "set ABNER_DETECT_INPUT, ABNER_DETECT_OUTPUT, ABNER_DETECT_COUNT and ABNER_DETECT_BOX"]
    fn inspect_detection_clip() {
        let path=PathBuf::from(std::env::var("ABNER_DETECT_INPUT").unwrap());
        let info=crate::probe::probe(&path).unwrap();
        let count=std::env::var("ABNER_DETECT_COUNT").unwrap().parse().unwrap();
        let r:Vec<usize>=std::env::var("ABNER_DETECT_BOX").unwrap().split(',').map(|v|v.parse().unwrap()).collect();
        let region=[r[0],r[1],r[2],r[3]];
        let times=Range{start:0.,end:info.duration}.samples_count(info.duration,info.fps,Some(count)).unwrap();
        let proposal=analyze(path,info.width,info.height,&times,&AtomicBool::new(false),&AtomicUsize::new(0),Some(region),true).unwrap();
        let file=std::fs::File::create(std::env::var("ABNER_DETECT_OUTPUT").unwrap()).unwrap();
        let mut encoder=png::Encoder::new(file,info.width,info.height);
        encoder.set_color(png::ColorType::Grayscale);encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&proposal.pixels).unwrap();
        eprintln!("Native detector: {} samples, {} selected pixels, {:.6}–{:.6}s",proposal.samples,proposal.pixels.iter().filter(|&&p|p>0).count(),times[0],times.last().unwrap());
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
            false,
        )
        .err()
        .unwrap();
        assert_eq!(err, "Watermark detection stopped");
    }
}
