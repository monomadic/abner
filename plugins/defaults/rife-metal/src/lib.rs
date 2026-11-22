//! In-process RIFE Metal comparison previews. No media files or child processes.
//! `retime` is a real frame-rate conversion: the rate is declared through
//! `Factory::output_rate` and frames are emitted on that grid, counted in a 1/rate
//! time base; frames that land on a source frame are the source frame, untouched.
use abner_processor_api::{
    Cancellation, Descriptor, Factory, Frame, Preset, Progress, Rate, Result, Session,
};
use abner_processor_api::{Capabilities, Category, InputRequirement, Role};
use libloading::Library;
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString, c_char};

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
    tier: String,
    /// Retime only: `2x`/`3x`/`4x` multiply the source rate, a number is a
    /// fixed rate (`59.94` and `29.97` are the NTSC rationals).
    target: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::LeaveOneOut,
            phase: 0.5,
            tier: "hq".into(),
            target: "2x".into(),
        }
    }
}
/// (value, title, description) for each retime target, in cycling order.
const TARGETS: [(&str, &str, &str); 11] = [
    (
        "2x",
        "2x frame rate",
        "Doubles the source rate: one new frame between each pair.",
    ),
    (
        "3x",
        "3x frame rate",
        "Two new frames between each pair, at thirds.",
    ),
    (
        "4x",
        "4x frame rate",
        "Three new frames between each pair, at quarters.",
    ),
    (
        "24",
        "24 fps",
        "Fixed 24 fps. From 23.976 this is a speed-neutral re-grid, every frame re-synthesized.",
    ),
    ("25", "25 fps", "Fixed 25 fps (PAL)."),
    ("29.97", "29.97 fps", "Fixed 30000/1001 fps (NTSC)."),
    ("30", "30 fps", "Fixed 30 fps."),
    ("50", "50 fps", "Fixed 50 fps."),
    (
        "59.94",
        "59.94 fps",
        "Fixed 60000/1001 fps (NTSC). From 23.976 that is 2.5x: phases cycle 0, .4, .8, .2, .6.",
    ),
    (
        "60",
        "60 fps",
        "Fixed 60 fps. From 24 that is 2.5x: phases cycle 0, .4, .8, .2, .6.",
    ),
    ("120", "120 fps", "Fixed 120 fps."),
];
fn target_rate(target: &str, input: Rate) -> Result<Rate> {
    let input = checked_rate(input)?;
    let times = |k: u32| {
        input[0]
            .checked_mul(k)
            .map(|n| [n, input[1]])
            .ok_or("rate overflow".to_string())
    };
    checked_rate(match target {
        "2x" => times(2),
        "3x" => times(3),
        "4x" => times(4),
        "29.97" => Ok([30000, 1001]),
        "59.94" => Ok([60000, 1001]),
        "24" | "25" | "30" | "50" | "60" | "120" => Ok([target.parse().unwrap(), 1]),
        _ => Err(format!("unknown retime target {target:?}")),
    }?)
}
// Exact integer scheduling; floating point is used only for the model's phase.
fn first_at_or_after(f: &Frame, pts: i64, rate: Rate) -> Result<i64> {
    let n = i128::from(pts) * i128::from(f.time_base[0]) * i128::from(rate[0]);
    let d = i128::from(f.time_base[1]) * i128::from(rate[1]);
    if d == 0 {
        return Err("invalid output rate".into());
    }
    i64::try_from(n.div_euclid(d) + i128::from(n.rem_euclid(d) != 0))
        .map_err(|_| "output timestamp overflow".into())
}
fn checked_rate(rate: Rate) -> Result<Rate> {
    if rate.contains(&0) || u64::from(rate[0]) > 240 * u64::from(rate[1]) {
        return Err("rate must be positive and at most 240 fps".into());
    }
    let g = gcd(rate[0], rate[1]);
    Ok([rate[0] / g, rate[1] / g])
}
fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}
fn batch_limit(w: usize, h: usize, from: i64, to: i64) -> Result<()> {
    let count = to.checked_sub(from).ok_or("output interval overflow")?;
    let bytes = (w as u128) * (h as u128) * 6;
    // Bound both phase count and returned pixel allocations before doing GPU work.
    if !(0..=16).contains(&count) || (count as u128) * bytes > 256 * 1024 * 1024 {
        return Err(
            "timestamp gap exceeds bounded output batch (16 frames / 256 MiB); split the segment"
                .into(),
        );
    }
    Ok(())
}

/// Keep the source end exact when it falls partway through the final output cell.
fn clip_tail(frame: &mut Frame, source: &Frame) -> Result<()> {
    let end = source
        .pts
        .checked_add(source.duration)
        .ok_or("source end overflow")?;
    let end_cross =
        i128::from(end) * i128::from(source.time_base[0]) * i128::from(frame.time_base[1]);
    let frame_end = frame
        .pts
        .checked_add(frame.duration)
        .ok_or("output end overflow")?;
    let frame_cross =
        i128::from(frame_end) * i128::from(frame.time_base[0]) * i128::from(source.time_base[1]);
    if frame_cross <= end_cross {
        return Ok(());
    }
    let g = gcd(source.time_base[1], frame.time_base[1]);
    let scale = u64::from(source.time_base[1] / g) * u64::from(frame.time_base[1]);
    let scale = u32::try_from(scale).map_err(|_| "final frame time base overflow")?;
    let pts = i128::from(frame.pts)
        * i128::from(frame.time_base[0])
        * i128::from(scale / frame.time_base[1]);
    let end =
        i128::from(end) * i128::from(source.time_base[0]) * i128::from(scale / source.time_base[1]);
    frame.pts = i64::try_from(pts).map_err(|_| "final timestamp overflow")?;
    frame.duration = i64::try_from(end - pts).map_err(|_| "final duration overflow")?;
    frame.time_base = [1, scale];
    frame.validate()
}

struct Interpolate;
impl Factory for Interpolate {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            role: Role::Processor,
            input: InputRequirement::Window {
                before: 1,
                after: 1,
            },
        }
    }
    fn categories(&self) -> Vec<Category> {
        vec![Category::Interpolation]
    }
    fn output_rate(&self, settings: &serde_json::Value, input: Rate) -> Result<Rate> {
        let settings: Settings =
            serde_json::from_value(settings.clone()).map_err(|e| e.to_string())?;
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
            id: "rife.metal".into(),
            name: "RIFE Metal".into(),
            settings_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "tier": {
                        "type": "string", "title": "Quality", "default": "hq",
                        "description": "The resolution RIFE estimates motion at. Output dimensions never change.",
                        "oneOf": [
                            {"const": "hq", "title": "HQ", "description": "Motion estimated at full resolution: slowest, best on fine detail."},
                            {"const": "balanced", "title": "Balanced", "description": "Motion estimated at half resolution."},
                            {"const": "fast", "title": "Fast", "description": "Motion estimated at quarter resolution: fastest, can miss small or thin motion."}
                        ]
                    },
                    "mode": {
                        "type": "string", "title": "Mode", "default": "leave-one-out",
                        "description": "What the interpolator's output is compared against.",
                        "oneOf": [
                            {"const": "leave-one-out", "title": "Leave-one-out",
                             "description": "Rebuild each interior frame from its neighbours. Use Difference to see the error against the real frame. The first and last frames pass through."},
                            {"const": "in-between", "title": "In-between",
                             "description": "Preview a generated frame between each source pair. Phase sets its position and the frame rate stays the same."},
                            {"const": "retime", "title": "Retime",
                             "description": "Play and step at the Target rate. Source frames at matching timestamps pass through. The Source view holds the most recent original frame."}
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
                    name: "Midpoint preview".into(),
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
        let settings: Settings =
            serde_json::from_value(settings.clone()).map_err(|e| e.to_string())?;
        if !["hq", "balanced", "fast"].contains(&settings.tier.as_str()) {
            return Err("tier must be hq, balanced or fast".into());
        }
        if !settings.phase.is_finite() || !(0.05..=0.95).contains(&settings.phase) {
            return Err("phase must be 0.05..0.95".into());
        }
        target_rate(&settings.target, [24, 1])?;
        if !(16..=8192).contains(&w) || !(16..=4320).contains(&h) {
            return Err("RIFE needs 16..8192 x 16..4320 frames".into());
        }
        Ok(Box::new(RifeSession {
            rife: Rife::open(w, h, &settings.tier)?,
            settings,
            w,
            h,
            held: Vec::new(),
            next: None,
            input_rate: None,
            started: false,
            finished: false,
            failed: false,
            completed: 0,
            reporter: None,
            cancel: Cancellation::default(),
        }))
    }
}

type Open =
    unsafe extern "C" fn(i32, i32, *const c_char, *mut c_char, usize) -> *mut std::ffi::c_void;
#[cfg(test)]
type Run = unsafe extern "C" fn(
    *mut std::ffi::c_void,
    *const u16,
    *const u16,
    f32,
    *mut u16,
    *mut c_char,
    usize,
) -> i32;
type Batch = unsafe extern "C" fn(
    *mut std::ffi::c_void,
    *const u16,
    *const u16,
    *const f32,
    usize,
    i32,
    *const *mut u16,
    *mut c_char,
    usize,
) -> i32;
type Reset = unsafe extern "C" fn(*mut std::ffi::c_void);
type Close = unsafe extern "C" fn(*mut std::ffi::c_void);
fn message(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
// The library must outlive both the context and its function pointers. Sessions
// stay on their creating thread, as required by Abner's processor contract.
struct Rife {
    context: *mut std::ffi::c_void,
    #[cfg(test)]
    run: Run,
    batch: Batch,
    reset: Reset,
    cached_right: Option<(i64, [u32; 2])>,
    close: Close,
    _library: Library,
}
impl Rife {
    fn open(w: usize, h: usize, tier: &str) -> Result<Self> {
        let path = match std::env::var_os("RIFE_METAL_LIBRARY") {
            Some(p) => std::path::PathBuf::from(p),
            None => std::path::PathBuf::from(
                std::env::var_os("HOME").ok_or("HOME is unset; set RIFE_METAL_LIBRARY")?,
            )
            .join(".local/libexec/rife-metal-rs/libRifeNative.dylib"),
        };
        let parent = path
            .parent()
            .ok_or("runtime path needs a parent directory")?;
        for (bundle, resource) in [
            ("RifeMetal_RifeMetal", "rife-v4.26.rmw"),
            ("RifeMetal_RifeMetalCore", "default.metallib"),
        ] {
            let dir = parent.join(format!("{bundle}.bundle"));
            if !dir.join("Contents/Resources").join(resource).is_file()
                && !dir.join(resource).is_file()
            {
                return Err(format!(
                    "Missing {resource}; reinstall the complete rife-metal-rs runtime beside {}",
                    path.display()
                ));
            }
        }
        let library = unsafe { Library::new(&path) }.map_err(|e| {
            format!(
                "Install rife-metal-rs with ./install.sh ({}): {e}",
                path.display()
            )
        })?;
        unsafe {
            let open: Open = *library
                .get(b"rm_pair_open_v1\0")
                .map_err(|e| format!("Update the rife-metal-rs runtime: {e}"))?;
            #[cfg(test)]
            let run: Run = *library
                .get(b"rm_pair_run_v1\0")
                .map_err(|e| e.to_string())?;
            let close: Close = *library
                .get(b"rm_pair_close_v1\0")
                .map_err(|e| e.to_string())?;
            let batch: Batch = *library
                .get(b"rm_pair_batch_v2\0")
                .map_err(|e| format!("Update rife-metal-rs with ./install.sh: {e}"))?;
            let reset: Reset = *library
                .get(b"rm_pair_reset_v2\0")
                .map_err(|e| e.to_string())?;
            let tier = CString::new(tier).map_err(|e| e.to_string())?;
            let mut error = [0 as c_char; 1024];
            let context = open(
                w as i32,
                h as i32,
                tier.as_ptr(),
                error.as_mut_ptr(),
                error.len(),
            );
            if context.is_null() {
                return Err(message(&error));
            }
            Ok(Self {
                context,
                #[cfg(test)]
                run,
                batch,
                reset,
                cached_right: None,
                close,
                _library: library,
            })
        }
    }
    fn reset(&mut self) {
        unsafe { (self.reset)(self.context) };
        self.cached_right = None;
    }
    fn run(&mut self, a: &Frame, b: &Frame, phase: f32) -> Result<Vec<u16>> {
        Ok(self.run_many(a, b, &[phase])?.remove(0))
    }
    fn run_many(&mut self, a: &Frame, b: &Frame, phases: &[f32]) -> Result<Vec<Vec<u16>>> {
        if phases.is_empty() {
            self.cached_right = None;
            return Ok(vec![]);
        }
        let mut outputs: Vec<Vec<u16>> = phases.iter().map(|_| vec![0; a.pixels.len()]).collect();
        let pointers: Vec<_> = outputs.iter_mut().map(|p| p.as_mut_ptr()).collect();
        let reuse = self.cached_right == Some((a.pts, a.time_base));
        let mut error = [0 as c_char; 1024];
        let ok = unsafe {
            (self.batch)(
                self.context,
                a.pixels.as_ptr(),
                b.pixels.as_ptr(),
                phases.as_ptr(),
                phases.len(),
                i32::from(reuse),
                pointers.as_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if ok == 0 {
            self.cached_right = None;
            return Err(message(&error));
        }
        self.cached_right = Some((b.pts, b.time_base));
        Ok(outputs)
    }
    #[cfg(test)]
    fn reference(&mut self, a: &Frame, b: &Frame, phase: f32) -> Result<Vec<u16>> {
        let mut output = vec![0u16; a.pixels.len()];
        let mut error = [0 as c_char; 1024];
        let ok = unsafe {
            (self.run)(
                self.context,
                a.pixels.as_ptr(),
                b.pixels.as_ptr(),
                phase,
                output.as_mut_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if ok == 0 {
            Err(message(&error))
        } else {
            Ok(output)
        }
    }
}
impl Drop for Rife {
    fn drop(&mut self) {
        unsafe { (self.close)(self.context) }
    }
}

struct RifeSession {
    rife: Rife,
    settings: Settings,
    w: usize,
    h: usize,
    /// Leave-one-out keeps the last two frames, in-between and retime the last one.
    held: Vec<Frame>,
    /// Retime: the output rate (fixed on the first frame, from its duration
    /// for multiples) and the next output index on its grid.
    next: Option<(Rate, i64)>,
    input_rate: Option<Rate>,
    started: bool,
    finished: bool,
    failed: bool,
    completed: u64,
    cancel: Cancellation,
    reporter: Option<abner_processor_api::ProgressReporter>,
}
fn same(f: &Frame, pixels: Vec<u16>) -> Frame {
    Frame {
        width: f.width,
        height: f.height,
        pixels,
        pts: f.pts,
        duration: f.duration,
        time_base: f.time_base,
    }
}
impl RifeSession {
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
        if self
            .held
            .last()
            .is_some_and(|l| l.time_base != input.time_base || input.pts <= l.pts)
        {
            return Err(
                "frames must arrive in order with one time base; recreate the session at cuts"
                    .into(),
            );
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
                    let span = (i128::from(input.pts) - i128::from(a.pts)) as f64;
                    let phase = ((i128::from(mid.pts) - i128::from(a.pts)) as f64 / span) as f32;
                    if !(0.0 < phase && phase < 1.0) {
                        return Err(
                            "timestamp spacing cannot be represented as a RIFE phase".into()
                        );
                    }
                    if let Some(report) = &self.reporter {
                        report(Progress {
                            phase: "Interpolating frame".into(),
                            completed: 0,
                            total: None,
                        });
                    }
                    let pixels = self.rife.run(a, &input, phase)?;
                    if let Some(report) = &self.reporter {
                        report(Progress {
                            phase: "Interpolating frame".into(),
                            completed: 1,
                            total: Some(1),
                        });
                    }
                    out.push(same(mid, pixels));
                    self.held.remove(0);
                }
                self.held.push(input);
            }
            Mode::InBetween => {
                if let Some(prev) = self.held.pop() {
                    if let Some(report) = &self.reporter {
                        report(Progress {
                            phase: "Interpolating frame".into(),
                            completed: 0,
                            total: None,
                        });
                    }
                    let pixels = self.rife.run(&prev, &input, self.settings.phase)?;
                    if let Some(report) = &self.reporter {
                        report(Progress {
                            phase: "Interpolating frame".into(),
                            completed: 1,
                            total: Some(1),
                        });
                    }
                    out.push(same(&prev, pixels));
                }
                self.held.push(input);
            }
            Mode::Retime => {
                let (rate, next) = match self.next {
                    Some(n) => n,
                    None => {
                        let rate = target_rate(
                            &self.settings.target,
                            self.input_rate.ok_or(
                                "host must configure the nominal input rate before retiming",
                            )?,
                        )?;
                        (rate, first_at_or_after(&input, input.pts, rate)?)
                    }
                };
                let end = first_at_or_after(&input, input.pts, rate)?;
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
impl RifeSession {
    /// Output frames n in [from, to) on the 1/rate grid, between `a` and `b`
    /// (`None` = after the last frame: hold it). A frame on `a`'s time is `a`.
    fn retime_span(
        &mut self,
        a: &Frame,
        b: Option<&Frame>,
        rate: Rate,
        from: i64,
        to: i64,
    ) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
        batch_limit(a.width, a.height, from, to)?;
        let mut phases = Vec::new();
        let mut generated = Vec::new();
        for n in from..to {
            let numerator = (i128::from(n) * i128::from(rate[1]) * i128::from(a.time_base[1]))
                .checked_sub(i128::from(a.pts) * i128::from(a.time_base[0]) * i128::from(rate[0]))
                .ok_or("phase timestamp overflow")?;
            let phase = if let Some(b) = b {
                let denominator = (i128::from(b.pts) - i128::from(a.pts))
                    .checked_mul(i128::from(a.time_base[0]))
                    .and_then(|v| v.checked_mul(i128::from(rate[0])))
                    .ok_or("phase span overflow")?;
                if numerator == 0 {
                    None
                } else {
                    if numerator < 0 || numerator >= denominator {
                        return Err("output outside source interval".into());
                    }
                    Some(
                        ((numerator as f64 / denominator as f64) as f32)
                            .clamp(f32::from_bits(1), f32::from_bits(1.0f32.to_bits() - 1)),
                    )
                }
            } else {
                None
            };
            generated.push(phase.is_some());
            if let Some(p) = phase {
                phases.push(p);
            }
        }
        self.cancel.check()?;
        let mut pixels = if let Some(b) = b {
            self.rife.run_many(a, b, &phases)?
        } else {
            vec![]
        }
        .into_iter();
        self.cancel.check()?;
        for (n, generated) in (from..to).zip(generated) {
            out.push(Frame {
                width: a.width,
                height: a.height,
                pixels: if generated {
                    pixels.next().ok_or("missing generated frame")?
                } else {
                    a.pixels.clone()
                },
                pts: n,
                duration: 1,
                time_base: [rate[1], rate[0]],
            });
        }
        Ok(out)
    }
}
impl Session for RifeSession {
    fn configure_input_rate(&mut self, rate: Rate) -> Result<()> {
        if self.started || !self.held.is_empty() || self.finished || self.failed {
            return Err("configure rate before input".into());
        }
        self.input_rate = Some(checked_rate(rate)?);
        Ok(())
    }
    fn reset(&mut self) -> bool {
        self.rife.reset();
        self.held.clear();
        self.next = None;
        self.input_rate = None;
        self.started = false;
        self.finished = false;
        self.failed = false;
        self.completed = 0;
        self.reporter = None;
        self.cancel = Cancellation::default();
        true
    }

    fn set_progress_reporter(&mut self, reporter: abner_processor_api::ProgressReporter) {
        self.reporter = Some(reporter);
    }
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>> {
        let result = self.push(input);
        if result.is_err() {
            self.failed = true;
            self.held.clear();
        }
        if let Some(report) = &self.reporter {
            report(self.progress());
        }
        result
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        let result = self.finish_inner();
        if result.is_err() {
            self.failed = true;
            self.held.clear();
        }
        result
    }
    fn output_dimensions(&self) -> [usize; 2] {
        [self.w, self.h]
    }
    fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    fn progress(&self) -> Progress {
        self.status()
    }
}
impl RifeSession {
    fn finish_inner(&mut self) -> Result<Vec<Frame>> {
        self.cancel.check()?;
        if self.failed {
            return Err("processor session failed; create a new session".into());
        }
        if self.finished {
            return Ok(vec![]);
        }
        self.finished = true;
        if self.settings.mode == Mode::Retime {
            // The last frame has no right neighbour: it is held for the rest
            // of its duration.
            let out = match (self.held.pop(), self.next) {
                (Some(last), Some((rate, next))) => {
                    let end = first_at_or_after(
                        &last,
                        last.pts
                            .checked_add(last.duration)
                            .ok_or("source end overflow")?,
                        rate,
                    )?;
                    let mut out = self.retime_span(&last, None, rate, next, end)?;
                    if let Some(frame) = out.last_mut() {
                        clip_tail(frame, &last)?;
                    }
                    out
                }
                _ => vec![],
            };
            self.completed += out.len() as u64;
            return Ok(out);
        }
        // The last frame has no right neighbour: it passes through.
        let single_passthrough = self.settings.mode == Mode::LeaveOneOut && self.held.len() == 1;
        let last = self
            .held
            .drain(..)
            .next_back()
            .filter(|_| !single_passthrough);
        let out: Vec<Frame> = last.into_iter().collect();
        self.completed += out.len() as u64;
        if let Some(report) = &self.reporter {
            report(self.progress());
        }
        Ok(out)
    }
    fn status(&self) -> Progress {
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
    /// A bar sliding right four pixels per frame: a midpoint frame should put it
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
        Frame {
            width: w,
            height: h,
            pixels,
            pts: i,
            duration: 1,
            time_base: [1, 24],
        }
    }
    fn session(mode: &str) -> Box<dyn Session> {
        Interpolate
            .create(128, 64, &serde_json::json!({"mode": mode}))
            .expect("RIFE Metal installed on this Mac")
    }
    #[test]
    fn progress_reports_actual_frame_work() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let mut s = session("leave-one-out");
        s.set_progress_reporter(std::sync::Arc::new(move |p| {
            captured.lock().unwrap().push(p)
        }));
        for i in 0..3 {
            s.process(frame(i, 128, 64)).unwrap();
        }
        // These events arrived from inside process(), before finish().
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|p| p.phase == "Interpolating frame" && p.total.is_none())
        );
        assert!(
            events.iter().any(|p| p.phase == "Interpolating frame"
                && p.completed == 1
                && p.total == Some(1))
        );
        assert!(matches!(
            Interpolate.capabilities().input,
            InputRequirement::Window {
                before: 1,
                after: 1
            }
        ));
    }
    fn run(mut s: Box<dyn Session>, n: i64) -> Vec<Frame> {
        s.configure_input_rate([24, 1]).unwrap();
        let mut out = Vec::new();
        for i in 0..n {
            out.extend(s.process(frame(i, 128, 64)).unwrap());
        }
        out.extend(s.finish().unwrap());
        out
    }
    #[test]
    fn retime_declares_its_rate_and_fills_the_grid() {
        let s = |t: &str| serde_json::json!({"mode": "retime", "target": t});
        assert_eq!(
            Interpolate.output_rate(&s("2x"), [24000, 1001]).unwrap(),
            [48000, 1001]
        );
        assert_eq!(
            Interpolate.output_rate(&s("59.94"), [24, 1]).unwrap(),
            [60000, 1001]
        );
        assert_eq!(
            Interpolate
                .output_rate(&serde_json::json!({"mode": "in-between"}), [24, 1])
                .unwrap(),
            [24, 1]
        );
        assert!(Interpolate.create(128, 64, &s("7x")).is_err());
        let out = run(
            Interpolate
                .create(128, 64, &s("2x"))
                .expect("RIFE Metal installed on this Mac"),
            4,
        );
        assert_eq!(
            out.iter().map(|f| f.pts).collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>()
        );
        assert!(
            out.iter()
                .all(|f| f.time_base == [1, 48] && f.duration == 1)
        );
        assert_eq!(
            out[2].pixels,
            frame(1, 128, 64).pixels,
            "on-grid frames are the source"
        );
        assert_ne!(
            out[3].pixels,
            frame(1, 128, 64).pixels,
            "between frames are synthesized"
        );
        assert_eq!(
            out[7].pixels,
            frame(3, 128, 64).pixels,
            "the last frame is held"
        );
        let out = run(Interpolate.create(128, 64, &s("60")).unwrap(), 4);
        assert_eq!(out.len(), 10, "4 frames of 1/24 s = 10 frames of 1/60 s");
        assert_eq!(
            out[5].pixels,
            frame(2, 128, 64).pixels,
            "5/60 s is source frame 2 exactly"
        );
    }
    #[test]
    fn leave_one_out_keeps_count_timing_and_endpoints() {
        let out = run(session("leave-one-out"), 5);
        assert_eq!(
            out.iter().map(|f| f.pts).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(out[0].pixels, frame(0, 128, 64).pixels);
        assert_eq!(out[4].pixels, frame(4, 128, 64).pixels);
        for f in &out {
            f.validate().unwrap();
        }
        // Reconstructed interior frames resemble the real ones far more than a neighbour does.
        let err = |a: &[u16], b: &[u16]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (*x as i64 - *y as i64).abs())
                .sum::<i64>()
        };
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
    fn rejects_bad_settings_and_order() {
        assert!(
            Interpolate
                .create(128, 64, &serde_json::json!({"phase": 1.5}))
                .is_err()
        );
        assert!(
            Interpolate
                .create(128, 64, &serde_json::json!({"bogus": 1}))
                .is_err()
        );
        let mut s = session("in-between");
        s.process(frame(1, 128, 64)).unwrap();
        assert!(s.process(frame(0, 128, 64)).is_err());
    }
    #[test]
    fn empty_single_and_two_frame_clips_flush_once() {
        for mode in ["leave-one-out", "in-between"] {
            for n in 0..=2 {
                let mut s = session(mode);
                let mut out = vec![];
                for i in 0..n {
                    out.extend(s.process(frame(i, 128, 64)).unwrap());
                }
                out.extend(s.finish().unwrap());
                assert_eq!(out.len(), n as usize);
                assert!(s.finish().unwrap().is_empty());
                assert_eq!(s.progress().completed, n as u64);
                assert!(s.process(frame(3, 128, 64)).is_err());
            }
        }
    }
    #[test]
    fn cancellation_and_invalid_frames_are_terminal() {
        let mut s = session("in-between");
        s.cancellation().cancel();
        assert!(s.process(frame(0, 128, 64)).is_err());
        assert!(s.finish().is_err());
        assert_eq!(s.progress().phase, "cancelled");
        let mut s = session("in-between");
        let mut bad = frame(0, 128, 64);
        bad.pixels.pop();
        assert!(s.process(bad).is_err());
        assert!(s.process(frame(1, 128, 64)).is_err());
        assert!(s.finish().is_err());
        let mut s = session("in-between");
        s.process(frame(0, 128, 64)).unwrap();
        let mut bad = frame(1, 128, 64);
        bad.time_base = [1, 30];
        assert!(s.process(bad).is_err());
    }
    #[test]
    fn all_tiers_preserve_rgb_channels_and_output_size() {
        for tier in ["hq", "balanced", "fast"] {
            let mut s = Interpolate
                .create(
                    130,
                    66,
                    &serde_json::json!({"tier":tier,"mode":"in-between"}),
                )
                .unwrap();
            let mut a = frame(0, 130, 66);
            a.pixels = [65535, 0, 0].repeat(130 * 66);
            let mut b = frame(1, 130, 66);
            b.pixels.clone_from(&a.pixels);
            assert!(s.process(a).unwrap().is_empty());
            let out = s.process(b).unwrap().remove(0);
            out.validate().unwrap();
            assert_eq!(s.output_dimensions(), [130, 66]);
            assert!(
                out.pixels
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .all(|p| p[0] > 64000 && p[1] < 1000 && p[2] < 1000)
            );
        }
    }
    #[test]
    fn vfr_uses_timestamp_phase_and_keeps_duration() {
        let mut s = session("leave-one-out");
        let a = frame(0, 128, 64);
        let mut mid = frame(1, 128, 64);
        mid.pts = 2;
        mid.duration = 4;
        let mut b = frame(2, 128, 64);
        b.pts = 6;
        let expected = Rife::open(128, 64, "hq")
            .unwrap()
            .run(&a, &b, 1.0 / 3.0)
            .unwrap();
        s.process(a).unwrap();
        s.process(mid).unwrap();
        let out = s.process(b).unwrap().remove(0);
        assert_eq!((out.pts, out.duration, out.time_base), (2, 4, [1, 24]));
        assert_eq!(out.pixels, expected);
    }
    #[test]
    fn nominal_rate_is_independent_of_rounded_or_vfr_durations() {
        for rate in [
            [24, 1],
            [25, 1],
            [30, 1],
            [50, 1],
            [24000, 1001],
            [30000, 1001],
            [60000, 1001],
        ] {
            let settings = serde_json::json!({"mode":"retime","target":"2x"});
            let declared = Interpolate.output_rate(&settings, rate).unwrap();
            let mut s = Interpolate.create(128, 64, &settings).unwrap();
            s.configure_input_rate(rate).unwrap();
            let mut a = frame(0, 128, 64);
            a.time_base = [1, 1000];
            a.pts = 1000;
            a.duration = 17;
            let mut b = frame(1, 128, 64);
            b.time_base = [1, 1000];
            b.pts = 1033;
            b.duration = 45;
            s.process(a).unwrap();
            let mut out = s.process(b).unwrap();
            out.extend(s.finish().unwrap());
            assert!(!out.is_empty());
            for f in out {
                assert!(u64::from(f.time_base[1]) % u64::from(declared[0]) == 0);
                f.validate().unwrap();
            }
        }
    }
    #[test]
    fn bounds_and_integer_timestamp_edges() {
        let mut a = frame(0, 128, 64);
        a.pts = i64::MAX;
        assert!(first_at_or_after(&a, a.pts, [240, 1]).is_err());
        assert!(checked_rate([0, 1]).is_err());
        assert!(checked_rate([1, 0]).is_err());
        assert!(target_rate("4x", [u32::MAX, 1]).is_err());
        assert!(batch_limit(3840, 2160, 0, 6).is_err());
        assert!(batch_limit(128, 64, 0, 17).is_err());
        let settings = serde_json::json!({"mode":"retime","target":"60"});
        let mut s = Interpolate.create(128, 64, &settings).unwrap();
        s.configure_input_rate([30, 1]).unwrap();
        s.process(frame(0, 128, 64)).unwrap();
        let mut gap = frame(1, 128, 64);
        gap.pts = 1000;
        assert!(s.process(gap).err().unwrap().contains("bounded output"));
        assert!(s.finish().is_err());
    }
    #[test]
    fn reset_keeps_engine_and_replaces_cancelled_token() {
        for mode in ["leave-one-out", "in-between", "retime"] {
            let settings = serde_json::json!({"mode":mode,"target":"60"});
            let mut s = Interpolate.create(128, 64, &settings).unwrap();
            s.configure_input_rate([24, 1]).unwrap();
            s.process(frame(0, 128, 64)).unwrap();
            let old = s.cancellation();
            old.cancel();
            assert!(s.process(frame(1, 128, 64)).is_err());
            assert!(s.reset());
            assert!(old.check().is_err());
            assert!(s.cancellation().check().is_ok());
            assert_eq!(s.progress().completed, 0);
            s.configure_input_rate([24, 1]).unwrap();
            let mut got = vec![];
            for i in 0..4 {
                got.extend(s.process(frame(i, 128, 64)).unwrap());
            }
            got.extend(s.finish().unwrap());
            let expected = run(Interpolate.create(128, 64, &settings).unwrap(), 4);
            assert_eq!(got.len(), expected.len());
            for (a, b) in got.iter().zip(&expected) {
                assert_eq!(a.pts, b.pts);
                assert_eq!(a.pixels, b.pixels);
            }
            assert!(s.reset());
            assert!(s.finish().unwrap().is_empty());
        }
    }

    #[test]
    fn cached_multi_phase_stream_matches_pair_reference() {
        for tier in ["hq", "balanced", "fast"] {
            let mut engine = Rife::open(130, 66, tier).unwrap();
            for i in 0..3 {
                let mut a = frame(i, 130, 66);
                let mut b = frame(i + 1, 130, 66);
                for (n, p) in a.pixels.iter_mut().enumerate() {
                    *p = ((n * 7919 + i as usize * 313) % 65536) as u16;
                }
                for (n, p) in b.pixels.iter_mut().enumerate() {
                    *p = ((n * 7919 + (i + 1) as usize * 313) % 65536) as u16;
                }
                let out = engine.run_many(&a, &b, &[0.2, 0.5, 0.8]).unwrap();
                for (pixels, phase) in out.iter().zip([0.2, 0.5, 0.8]) {
                    let reference = engine.reference(&a, &b, phase).unwrap();
                    let max = pixels
                        .iter()
                        .zip(reference)
                        .map(|(a, b)| a.abs_diff(b))
                        .max()
                        .unwrap();
                    assert!(
                        max <= 257,
                        "stream/reference differs by more than one 8-bit step: {max}"
                    );
                }
            }
            engine.reset();
            let a = frame(0, 130, 66);
            let b = frame(1, 130, 66);
            assert!(!engine.run_many(&a, &b, &[0.5]).unwrap().is_empty());
        }
    }

    #[test]
    fn missing_nominal_rate_and_tail_overflow_fail_cleanly() {
        let settings = serde_json::json!({"mode":"retime","target":"60"});
        let mut s = Interpolate.create(128, 64, &settings).unwrap();
        assert!(s.process(frame(0, 128, 64)).is_err());
        assert!(s.reset());
        s.configure_input_rate([30, 1]).unwrap();
        let mut last = frame(0, 128, 64);
        last.pts = i64::MAX;
        last.time_base = [1, 60];
        s.process(last).unwrap();
        assert!(s.finish().is_err());
        assert_eq!(s.progress().phase, "failed");
        assert!(s.finish().is_err());
    }
    #[test]
    fn fractional_retime_ends_exactly_at_the_source_end() {
        for target in ["60", "59.94"] {
            let settings = serde_json::json!({"mode":"retime","target":target});
            let mut s = Interpolate.create(128, 64, &settings).unwrap();
            s.configure_input_rate([24000, 1001]).unwrap();
            let mut out = vec![];
            for i in 0..2 {
                let mut f = frame(i, 128, 64);
                f.time_base = [1001, 24000];
                out.extend(s.process(f).unwrap());
            }
            out.extend(s.finish().unwrap());
            let last = out.last().unwrap();
            assert_eq!(
                i128::from(last.pts + last.duration) * i128::from(last.time_base[0]) * 24000,
                2 * 1001 * i128::from(last.time_base[1])
            );
            for pair in out.windows(2) {
                assert_eq!(
                    i128::from(pair[0].pts + pair[0].duration)
                        * i128::from(pair[0].time_base[0])
                        * i128::from(pair[1].time_base[1]),
                    i128::from(pair[1].pts)
                        * i128::from(pair[1].time_base[0])
                        * i128::from(pair[0].time_base[1])
                );
            }
        }
    }
}
