//! Apple VideoToolbox frame interpolation for Abner's processor strip.
//!
//! - `retime`: a real frame-rate conversion (2x/3x/4x, or a fixed target such as
//!   60 fps). It declares the new rate through `Factory::output_rate` and emits
//!   frames on that grid, counted in a 1/rate time base; frames that land on a
//!   source frame are the source frame, untouched.
//!
//! - `leave-one-out`: each interior frame is replaced by the frame VideoToolbox
//!   synthesizes from its two neighbours. A/B against the source (or the
//!   difference view) is the interpolation error on real motion.
//! - `in-between`: each frame is replaced by the synthesized frame `phase` of the
//!   way to the next one, which is what a retime would insert there.
//!
//! The first/last frames have no neighbour on one side and pass through unchanged
//! (retime holds the last frame for the rest of its duration).
use abner_processor_api::{Capabilities, Category, InputRequirement, Role};
use abner_processor_api::{
    Cancellation, Descriptor, Factory, Frame, Preset, Progress, Rate, Result, Session,
};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, c_char};

pub fn factories() -> Vec<Box<dyn Factory>> {
    vec![Box::new(Interpolate)]
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Mode {
    LeaveOneOut,
    InBetween,
    Retime,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    mode: Mode,
    phase: f32,
    /// Retime only: `2x`/`3x`/`4x` multiply the source rate, a number is a
    /// fixed rate (`59.94` and `29.97` are the NTSC rationals).
    target: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self { mode: Mode::LeaveOneOut, phase: 0.5, target: "2x".into() }
    }
}
/// The (title, description) for each retime target, in cycling order.
const TARGETS: [(&str, &str, &str); 11] = [
    ("2x", "2x frame rate", "Doubles the source rate: one new frame between each pair."),
    ("3x", "3x frame rate", "Two new frames between each pair, at thirds."),
    ("4x", "4x frame rate", "Three new frames between each pair, at quarters."),
    ("24", "24 fps", "Fixed 24 fps. From 23.976 this is a speed-neutral re-grid, every frame re-synthesized."),
    ("25", "25 fps", "Fixed 25 fps (PAL)."),
    ("29.97", "29.97 fps", "Fixed 30000/1001 fps (NTSC)."),
    ("30", "30 fps", "Fixed 30 fps."),
    ("50", "50 fps", "Fixed 50 fps."),
    ("59.94", "59.94 fps", "Fixed 60000/1001 fps (NTSC). From 23.976 that is 2.5x: phases cycle 0, .4, .8, .2, .6."),
    ("60", "60 fps", "Fixed 60 fps. From 24 that is 2.5x: phases cycle 0, .4, .8, .2, .6."),
    ("120", "120 fps", "Fixed 120 fps."),
];
fn target_rate(target: &str, input: Rate) -> Result<Rate> {
    let times = |k: u32| input[0].checked_mul(k).map(|n| [n, input[1]]).ok_or("rate overflow".to_string());
    match target {
        "2x" => times(2),
        "3x" => times(3),
        "4x" => times(4),
        "29.97" => Ok([30000, 1001]),
        "59.94" => Ok([60000, 1001]),
        "24" | "25" | "30" | "50" | "60" | "120" => Ok([target.parse().unwrap(), 1]),
        _ => Err(format!("unknown retime target {target:?}")),
    }
}

struct Interpolate;
impl Factory for Interpolate {
    fn capabilities(&self) -> Capabilities {
        Capabilities { role: Role::Processor, input: InputRequirement::Window { before: 1, after: 1 } }
    }
    fn categories(&self) -> Vec<Category> { vec![Category::Interpolation] }
    fn output_rate(&self, settings: &serde_json::Value, input: Rate) -> Result<Rate> {
        let settings: Settings = serde_json::from_value(settings.clone()).map_err(|e| e.to_string())?;
        match settings.mode {
            Mode::Retime => target_rate(&settings.target, input),
            _ => Ok(input),
        }
    }
    fn descriptor(&self) -> Descriptor {
        let targets: Vec<_> = TARGETS
            .iter()
            .map(|(v, t, d)| serde_json::json!({"const": v, "title": t, "description": d}))
            .collect();
        Descriptor {
            id: "videotoolbox.interpolate".into(),
            name: "VT Frame Interpolation".into(),
            settings_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "mode": {
                        "type": "string", "title": "Mode", "default": "leave-one-out",
                        "description": "What the interpolator's output is compared against.",
                        "oneOf": [
                            {"const": "leave-one-out", "title": "Leave-one-out",
                             "description": "Drops each frame and rebuilds it from its two neighbours. The real frame exists, so A/B (or Difference) is the interpolation error on real motion. Frame count and rate are unchanged."},
                            {"const": "in-between", "title": "In-between",
                             "description": "Replaces each frame with the frame synthesized Phase of the way to the next one: a single inserted frame per source frame, shown at the source frame's time against the original it follows. Rate unchanged."},
                            {"const": "retime", "title": "Retime",
                             "description": "Real frame-rate conversion to Target. The preview plays and steps at the new rate; the original side holds the latest source frame at or before each new one. Frames that land on a source frame are passed through untouched."}
                        ]
                    },
                    "phase": {
                        "type": "number", "title": "Phase", "minimum": 0.05, "maximum": 0.95, "default": 0.5,
                        "description": "In-between only: how far towards the next frame the synthesized frame sits (0.5 = midpoint)."
                    },
                    "target": {
                        "type": "string", "title": "Target", "default": "2x",
                        "description": "Retime only: the output frame rate, a multiple of the source's or a fixed rate.",
                        "oneOf": targets
                    }
                }
            }),
            presets: vec![
                Preset {
                    id: "leave-one-out".into(),
                    name: "Leave-one-out error".into(),
                    settings: serde_json::to_value(Settings::default()).unwrap(),
                },
                Preset {
                    id: "midpoint".into(),
                    name: "Midpoint frames".into(),
                    settings: serde_json::json!({"mode": "in-between", "phase": 0.5}),
                },
                Preset {
                    id: "retime-2x".into(),
                    name: "2x frame rate".into(),
                    settings: serde_json::json!({"mode": "retime", "target": "2x"}),
                },
                Preset {
                    id: "retime-60".into(),
                    name: "60 fps".into(),
                    settings: serde_json::json!({"mode": "retime", "target": "60"}),
                },
            ],
        }
    }
    fn create(&self, w: usize, h: usize, settings: &serde_json::Value) -> Result<Box<dyn Session>> {
        let settings: Settings = serde_json::from_value(settings.clone()).map_err(|e| e.to_string())?;
        if !settings.phase.is_finite() || !(0.05..=0.95).contains(&settings.phase) {
            return Err("phase must be 0.05..0.95".into());
        }
        target_rate(&settings.target, [24, 1])?;
        if !(16..=8192).contains(&w) || !(16..=4320).contains(&h) {
            return Err("VideoToolbox interpolation needs 16..8192 x 16..4320 frames".into());
        }
        Ok(Box::new(VtSession {
            vt: Vt::open(w, h)?,
            settings,
            w,
            h,
            held: Vec::new(),
            next: None,
            started: false,
            finished: false,
            failed: false,
            completed: 0,
            reporter: None,
            cancel: Cancellation::default(),
        }))
    }
}

#[repr(C)]
struct Raw {
    _private: [u8; 0],
}
unsafe extern "C" {
    fn vtfrc_open(w: i32, h: i32, err: *mut c_char, len: usize) -> *mut Raw;
    fn vtfrc_close(s: *mut Raw);
    fn vtfrc_run(
        s: *mut Raw,
        a: *const u16,
        ta: f64,
        b: *const u16,
        tb: f64,
        phase: f32,
        out: *mut u16,
        err: *mut c_char,
        len: usize,
    ) -> i32;
}
fn message(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}
/// One VTFrameProcessor session. Created and used on the session's thread only.
struct Vt(*mut Raw);
impl Vt {
    fn open(w: usize, h: usize) -> Result<Self> {
        let mut err = [0 as c_char; 512];
        let raw = unsafe { vtfrc_open(w as i32, h as i32, err.as_mut_ptr(), err.len()) };
        if raw.is_null() {
            return Err(message(&err));
        }
        if err[0] != 0 {
            unsafe { vtfrc_close(raw) };
            return Err(message(&err));
        }
        Ok(Self(raw))
    }
    fn run(&mut self, a: &Frame, b: &Frame, phase: f32) -> Result<Vec<u16>> {
        let seconds = |f: &Frame| f.pts as f64 * f.time_base[0] as f64 / f.time_base[1] as f64;
        let mut out = vec![0u16; a.pixels.len()];
        let mut err = [0 as c_char; 512];
        let ok = unsafe {
            vtfrc_run(
                self.0,
                a.pixels.as_ptr(),
                seconds(a),
                b.pixels.as_ptr(),
                seconds(b),
                phase,
                out.as_mut_ptr(),
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if ok == 0 { Err(message(&err)) } else { Ok(out) }
    }
}
impl Drop for Vt {
    fn drop(&mut self) {
        unsafe { vtfrc_close(self.0) }
    }
}

struct VtSession {
    vt: Vt,
    settings: Settings,
    w: usize,
    h: usize,
    /// Leave-one-out keeps the last two frames, in-between and retime the last one.
    held: Vec<Frame>,
    /// Retime: the output rate (fixed on the first frame, from its duration
    /// for multiples) and the next output index on its grid.
    next: Option<(Rate, i64)>,
    started: bool,
    finished: bool,
    failed: bool,
    completed: u64,
    cancel: Cancellation,
    reporter: Option<abner_processor_api::ProgressReporter>,
}
fn seconds(f: &Frame, pts: i64) -> f64 {
    pts as f64 * f.time_base[0] as f64 / f.time_base[1] as f64
}
/// The first index n on the 1/rate grid with n/rate >= `pts` in `f`'s time base, exactly.
fn first_at_or_after(f: &Frame, pts: i64, rate: Rate) -> i64 {
    let n = pts as i128 * f.time_base[0] as i128 * rate[0] as i128;
    let d = f.time_base[1] as i128 * rate[1] as i128;
    (n.div_euclid(d) + i128::from(n.rem_euclid(d) != 0)) as i64
}
/// The source rate a frame implies, as the rational its duration names.
fn implied_rate(f: &Frame) -> Rate {
    let n = f.time_base[1] as u64;
    let d = f.duration as u64 * f.time_base[0] as u64;
    let g = gcd(n, d);
    [(n / g) as u32, (d / g) as u32]
}
fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}
fn same(f: &Frame, pixels: Vec<u16>) -> Frame {
    Frame { width: f.width, height: f.height, pixels, pts: f.pts, duration: f.duration, time_base: f.time_base }
}
impl VtSession {
    fn push(&mut self, input: Frame) -> Result<Vec<Frame>> {
        self.cancel.check()?;
        if self.failed {
            return Err("processor session failed; create a new session".into());
        }
        if self.finished {
            return Err("processor stream already finished".into());
        }
        input.validate()?;
        if (input.width, input.height) != (self.w, self.h) {
            return Err("session frame dimensions changed".into());
        }
        if self.held.last().is_some_and(|l| l.time_base != input.time_base || input.pts <= l.pts) {
            return Err("frames must arrive in order with one time base; recreate the session at cuts".into());
        }
        let mut out = Vec::new();
        match self.settings.mode {
            Mode::LeaveOneOut => {
                if !self.started {
                    // The first frame has no left neighbour: it passes through.
                    self.started = true;
                    out.push(same(&input, input.pixels.clone()));
                } else if self.held.len() == 2 {
                    let (a, mid) = (&self.held[0], &self.held[1]);
                    let span = (input.pts - a.pts) as f32;
                    let phase = (mid.pts - a.pts) as f32 / span;
                    if let Some(report) = &self.reporter {
                        report(Progress { phase: "Interpolating frame".into(), completed: 0, total: None });
                    }
                    let pixels = self.vt.run(a, &input, phase)?;
                    if let Some(report) = &self.reporter {
                        report(Progress { phase: "Interpolating frame".into(), completed: 1, total: Some(1) });
                    }
                    out.push(same(mid, pixels));
                    self.held.remove(0);
                }
                self.held.push(input);
            }
            Mode::InBetween => {
                if let Some(prev) = self.held.pop() {
                    if let Some(report) = &self.reporter {
                        report(Progress { phase: "Interpolating frame".into(), completed: 0, total: None });
                    }
                    let pixels = self.vt.run(&prev, &input, self.settings.phase)?;
                    if let Some(report) = &self.reporter {
                        report(Progress { phase: "Interpolating frame".into(), completed: 1, total: Some(1) });
                    }
                    out.push(same(&prev, pixels));
                }
                self.held.push(input);
            }
            Mode::Retime => {
                let (rate, next) = match self.next {
                    Some(n) => n,
                    None => {
                        let rate = target_rate(&self.settings.target, implied_rate(&input))?;
                        (rate, first_at_or_after(&input, input.pts, rate))
                    }
                };
                let end = first_at_or_after(&input, input.pts, rate);
                if let Some(prev) = self.held.pop() {
                    out = self.retime_span(&prev, Some(&input), rate, next, end)?;
                }
                self.next = Some((rate, end));
                self.held.push(input);
            }
        }
        self.cancel.check()?;
        self.completed += out.len() as u64;
        Ok(out)
    }
}
impl VtSession {
    /// Output frames n in [from, to) on the 1/rate grid, between `a` and `b`
    /// (`None` = after the last frame: hold it). A frame on `a`'s time is `a`.
    fn retime_span(&mut self, a: &Frame, b: Option<&Frame>, rate: Rate, from: i64, to: i64) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
        let ta = seconds(a, a.pts);
        for n in from..to {
            self.cancel.check()?;
            let t = n as f64 * rate[1] as f64 / rate[0] as f64;
            let pixels = match b {
                Some(b) => {
                    let phase = (t - ta) / (seconds(b, b.pts) - ta);
                    if phase < 1e-6 { a.pixels.clone() } else { self.vt.run(a, b, phase as f32)? }
                }
                None => a.pixels.clone(),
            };
            out.push(Frame { width: a.width, height: a.height, pixels, pts: n, duration: 1, time_base: [rate[1], rate[0]] });
        }
        Ok(out)
    }
}
impl Session for VtSession {
    fn set_progress_reporter(&mut self, reporter: abner_processor_api::ProgressReporter) {
        self.reporter = Some(reporter);
    }
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>> {
        let result = self.push(input);
        if result.is_err() {
            self.failed = true;
            self.held.clear();
        }
        if let Some(report) = &self.reporter { report(self.progress()); }
        result
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        self.cancel.check()?;
        if self.failed {
            return Err("processor session failed; create a new session".into());
        }
        if self.finished {
            return Ok(vec![]);
        }
        self.finished = true;
        // The last frame has no right neighbour: it passes through (retime
        // holds it for the rest of its duration).
        let last = self.held.drain(..).last();
        let out: Vec<Frame> = match (self.settings.mode, last, self.next) {
            (Mode::Retime, Some(last), Some((rate, next))) => {
                let end = first_at_or_after(&last, last.pts + last.duration, rate);
                self.retime_span(&last, None, rate, next, end)?
            }
            (_, last, _) => last.into_iter().collect(),
        };
        self.completed += out.len() as u64;
        if let Some(report) = &self.reporter { report(self.progress()); }
        Ok(out)
    }
    fn output_dimensions(&self) -> [usize; 2] {
        [self.w, self.h]
    }
    fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    fn progress(&self) -> Progress {
        Progress {
            phase: if self.cancel.check().is_err() {
                "cancelled"
            } else if self.failed {
                "failed"
            } else if self.finished {
                "complete"
            } else if self.held.len() < 2 {
                "Waiting for neighbouring frames"
            } else {
                "Processing frames"
            }
            .into(),
            completed: self.completed,
            total: self.finished.then_some(self.completed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A bar sliding right two pixels per frame: a midpoint frame should put it
    /// halfway, which neither neighbour does.
    fn frame(i: i64, w: usize, h: usize) -> Frame {
        let mut pixels = vec![8000u16; w * h * 3];
        let x0 = 8 + 4 * i as usize;
        for y in 0..h {
            for x in x0..x0 + 12 {
                for c in 0..3 {
                    pixels[(y * w + x) * 3 + c] = 60000;
                }
            }
        }
        Frame { width: w, height: h, pixels, pts: i, duration: 1, time_base: [1, 24] }
    }
    fn session(mode: &str) -> Box<dyn Session> {
        Interpolate
            .create(128, 64, &serde_json::json!({"mode": mode}))
            .expect("VideoToolbox frame rate conversion on this Mac")
    }
    #[test]
    fn progress_reports_actual_frame_work() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let mut s = session("leave-one-out");
        s.set_progress_reporter(std::sync::Arc::new(move |p| captured.lock().unwrap().push(p)));
        for i in 0..3 { s.process(frame(i, 128, 64)).unwrap(); }
        // These events arrived from inside process(), before finish().
        let events = events.lock().unwrap();
        assert!(events.iter().any(|p| p.phase == "Interpolating frame" && p.total.is_none()));
        assert!(events.iter().any(|p| p.phase == "Interpolating frame" && p.completed == 1 && p.total == Some(1)));
        assert!(matches!(Interpolate.capabilities().input, InputRequirement::Window { before: 1, after: 1 }));
    }
    fn run(mut s: Box<dyn Session>, n: i64) -> Vec<Frame> {
        let mut out = Vec::new();
        for i in 0..n {
            out.extend(s.process(frame(i, 128, 64)).unwrap());
        }
        out.extend(s.finish().unwrap());
        out
    }
    #[test]
    fn leave_one_out_keeps_count_timing_and_endpoints() {
        let out = run(session("leave-one-out"), 5);
        assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        assert_eq!(out[0].pixels, frame(0, 128, 64).pixels);
        assert_eq!(out[4].pixels, frame(4, 128, 64).pixels);
        for f in &out {
            f.validate().unwrap();
        }
        // Reconstructed interior frames resemble the real ones far more than a neighbour does.
        let err = |a: &[u16], b: &[u16]| a.iter().zip(b).map(|(x, y)| (*x as i64 - *y as i64).abs()).sum::<i64>();
        let real = frame(2, 128, 64).pixels;
        assert!(err(&out[2].pixels, &real) * 3 < err(&frame(1, 128, 64).pixels, &real));
    }
    #[test]
    fn in_between_shifts_each_frame_towards_the_next() {
        let out = run(session("in-between"), 3);
        assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), [0, 1, 2]);
        assert_ne!(out[0].pixels, frame(0, 128, 64).pixels);
        assert_eq!(out[2].pixels, frame(2, 128, 64).pixels);
    }
    #[test]
    fn retime_declares_its_rate_and_fills_the_grid() {
        let s = |t: &str| serde_json::json!({"mode": "retime", "target": t});
        assert_eq!(Interpolate.output_rate(&s("2x"), [24000, 1001]).unwrap(), [48000, 1001]);
        assert_eq!(Interpolate.output_rate(&s("59.94"), [24, 1]).unwrap(), [60000, 1001]);
        assert_eq!(Interpolate.output_rate(&serde_json::json!({"mode": "in-between"}), [24, 1]).unwrap(), [24, 1]);
        assert!(Interpolate.create(128, 64, &s("7x")).is_err());

        // 2x: 4 source frames -> 8 output frames at 1/48 s, originals on even slots.
        let out = run(Interpolate.create(128, 64, &s("2x")).unwrap(), 4);
        assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), (0..8).collect::<Vec<_>>());
        assert!(out.iter().all(|f| f.time_base == [1, 48] && f.duration == 1));
        assert_eq!(out[2].pixels, frame(1, 128, 64).pixels, "on-grid frames are the source");
        assert_ne!(out[3].pixels, frame(1, 128, 64).pixels, "between frames are synthesized");
        assert_eq!(out[7].pixels, frame(3, 128, 64).pixels, "the last frame is held");

        // 24 -> 60: 2.5 output frames per source frame.
        let out = run(Interpolate.create(128, 64, &s("60")).unwrap(), 4);
        assert_eq!(out.len(), 10, "4 frames of 1/24 s = 10 frames of 1/60 s");
        assert!(out.windows(2).all(|w| w[1].pts == w[0].pts + 1));
        assert_eq!(out[5].pixels, frame(2, 128, 64).pixels, "5/60 s is source frame 2 exactly");
    }
    #[test]
    fn rejects_bad_settings_and_order() {
        assert!(Interpolate.create(128, 64, &serde_json::json!({"phase": 1.5})).is_err());
        assert!(Interpolate.create(128, 64, &serde_json::json!({"bogus": 1})).is_err());
        let mut s = session("in-between");
        s.process(frame(1, 128, 64)).unwrap();
        assert!(s.process(frame(0, 128, 64)).is_err());
    }
}
