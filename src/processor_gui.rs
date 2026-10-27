//! Runtime-schema controls and cancellable, bounded display previews.
//! No processor identity or implementation belongs in this module.
use crate::{
    app::{
        ACCENT, CUT_HEADER_BLUE, CUT_STATUS_H, Canvas, DETAIL_SLIDE_S, HEADER_H, INSPECTOR_BG,
        INSPECTOR_PATH, Icon, Key, MONO_ADV, RAIL_ACTIONS_H, RAIL_FOOTER_H, RAIL_GRIP_H, RAIL_HEAD_H,
        RAIL_INSPECTOR_H, SOURCE_BORDER, SOURCE_EDGE, SOURCE_EDGE_HOVER, SOURCE_INK, SOURCE_RING,
        SOURCE_SECONDARY, SOURCE_SUBTLE, SOURCE_SURFACE, WORKSPACE_DIM, WORKSPACE_MUTED,
        WORKSPACE_PANEL, WORKSPACE_RULE, WORKSPACE_TEXT, draw_cut_footer_button, draw_dashed_hairline,
        draw_icon, draw_keycap, draw_rail_grip, footer_export_rect, hex_color, mix, rail_full_width,
        status_build_width, ui_label,
    },
    player::{Player, Rgba},
    probe::VideoInfo,
    render::{Align, Item, RectItem, RectPx, TextItem, Upload, VAlign, VideoMode},
};
use abner_processor_api::{
    Cancellation, Capabilities, Category, Descriptor, Frame, InputRequirement, Progress, Registry, Role,
    Session,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex, mpsc},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// `processors.preview_cache_mb`'s default, for a `Panel` nobody configured.
pub const DEFAULT_PREVIEW_BYTES: usize = 1024 * 1024 * 1024;
const FRAMES: usize = 24;
const INK: [f32; 4] = [0.8, 0.8, 0.8, 1.0];
const DIM: [f32; 4] = [0.43, 0.43, 0.43, 1.0];
const BLUE: [f32; 4] = [0.04, 0.33, 0.7, 1.0];
/// Compact processor rows: 48px of content plus an 8px gap.
const CARD_PITCH: f32 = 56.0;
/// The page's footer bar: the Timeline's height, since it carries the same
/// kind of button (Render frame, drawn as the cut footer's Split).
const FOOTER_H: f32 = CUT_STATUS_H;
/// A frame being rendered: the source palette's amber, pulsed while busy.
const RENDERING: u32 = 0xe6ac69;
/// Transport buttons: five icon squares on a 6px gap, centred under the viewer.
const TRANSPORT_W: f32 = 40.0;
const TRANSPORT_GAP: f32 = 6.0;

#[derive(Clone, Debug)]
enum Kind {
    Number { min: f64, max: f64, integer: bool },
    Enum(Vec<Value>),
    Bool,
    Text,
    Unsupported,
}
#[derive(Clone, Debug)]
struct Field {
    key: String,
    title: String,
    kind: Kind,
    default: Option<Value>,
    required: bool,
}
fn fields(schema: &Value) -> Vec<Field> {
    schema["properties"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.iter())
        .map(|(key, s)| {
            let kind = if let Some(e) = s["enum"].as_array().filter(|v| !v.is_empty()) {
                Kind::Enum(e.clone())
            } else {
                match s["type"].as_str() {
                    Some("number" | "integer") => {
                        match (s["minimum"].as_f64(), s["maximum"].as_f64()) {
                            (Some(min), Some(max))
                                if min.is_finite()
                                    && max.is_finite()
                                    && min < max
                                    && (s["type"] != "integer" || min.ceil() <= max.floor()) =>
                            {
                                Kind::Number {
                                    min,
                                    max,
                                    integer: s["type"] == "integer",
                                }
                            }
                            _ => Kind::Text,
                        }
                    }
                    Some("boolean") => Kind::Bool,
                    Some("string") => Kind::Text,
                    _ => Kind::Unsupported,
                }
            };
            Field {
                key: key.clone(),
                title: s["title"].as_str().unwrap_or(key).into(),
                kind,
                default: s.get("default").cloned(),
                required: schema["required"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == key)),
            }
        })
        .collect()
}
fn defaults(fields: &[Field]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|f| f.default.clone().map(|v| (f.key.clone(), v)))
            .collect(),
    )
}
fn numeric(min: f64, max: f64, integer: bool, fraction: f64) -> Value {
    let v = min + fraction.clamp(0.0, 1.0) * (max - min);
    if integer {
        json!(v.round().clamp(min.ceil(), max.floor()) as i64)
    } else {
        json!((v * 10000.0).round() / 10000.0)
    }
}
fn text_value(field: &Field, schema: &Value, text: &str) -> Result<Option<Value>, String> {
    if text.is_empty() && !field.required {
        return Ok(None);
    }
    let value = match schema["properties"][&field.key]["type"].as_str() {
        Some("integer") => json!(text.parse::<i64>().map_err(|_| "Enter an integer")?),
        Some("number") => {
            let n = text.parse::<f64>().map_err(|_| "Enter a number")?;
            if !n.is_finite() {
                return Err("Enter a finite number".into());
            }
            json!(n)
        }
        _ => json!(text),
    };
    let s = &schema["properties"][&field.key];
    if let Some(n) = value.as_f64()
        && (s["minimum"].as_f64().is_some_and(|min| n < min)
            || s["maximum"].as_f64().is_some_and(|max| n > max))
    {
        return Err("Value is outside the allowed range".into());
    }
    Ok(Some(value))
}

struct Request {
    generation: u64,
    chain: Vec<(String, Value)>,
    target: Option<f64>,
    info: VideoInfo,
    start: f64,
    end: Option<f64>,
    cancel: Cancellation,
    /// Bytes allowed for the section's paired before/after frames
    /// (`processors.preview_cache_mb`); the frame count follows from it.
    budget: usize,
}
struct Mailbox {
    pending: Option<Request>,
    stop: bool,
}
struct Worker {
    active: Arc<Mutex<Vec<Cancellation>>>,
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    rx: mpsc::Receiver<Event>,
    live: LiveProgress,
}
type LiveProgress = Arc<Mutex<Option<(u64, StageFeedback)>>>;
struct StageFeedback {
    name: String,
    index: usize,
    count: usize,
    progress: Progress,
    rates: RateSnapshot,
}
#[derive(Clone, Copy)]
struct RateSnapshot {
    frames: u64,
    started: Instant,
    phase_started: Instant,
    baseline: u64,
}
impl RateSnapshot {
    fn output_fps(&self, now: Instant) -> Option<f64> {
        let seconds = now.saturating_duration_since(self.started).as_secs_f64();
        (self.frames > 0 && seconds > 0.0).then(|| self.frames as f64 / seconds)
    }
    fn eta(&self, p: &Progress, now: Instant) -> Option<f64> {
        let total = p.total.filter(|t| *t > 0)?;
        if p.completed >= total { return Some(0.0); }
        let done = p.completed.checked_sub(self.baseline)?;
        let seconds = now.saturating_duration_since(self.phase_started).as_secs_f64();
        (done > 0 && seconds >= 0.25).then(|| seconds / done as f64 * (total - p.completed) as f64)
    }
}
struct StageClock {
    rates: RateSnapshot,
    phase: String,
    completed: u64,
    total: Option<u64>,
}
impl StageClock {
    fn new(now: Instant) -> Self {
        Self { rates: RateSnapshot { frames: 0, started: now, phase_started: now, baseline: 0 },
            phase: String::new(), completed: 0, total: None }
    }
    fn sample(&mut self, p: &Progress, frames: u64, now: Instant) -> RateSnapshot {
        if p.phase != self.phase || p.total != self.total || p.completed < self.completed {
            self.rates.phase_started = now;
            self.rates.baseline = p.completed;
        }
        self.phase.clone_from(&p.phase);
        self.completed = p.completed;
        self.total = p.total;
        self.rates.frames = frames;
        self.rates
    }
}
fn remaining_label(seconds: f64) -> String {
    let seconds = seconds.ceil().max(0.0) as u64;
    if seconds >= 3600 { format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60) }
    else if seconds >= 60 { format!("{}m {:02}s", seconds / 60, seconds % 60) }
    else { format!("{seconds}s") }
}
impl StageFeedback {
    fn fraction(&self) -> Option<f32> {
        self.progress.total.filter(|total| *total > 0).map(|total| {
            (self.progress.completed.min(total) as f64 / total as f64) as f32
        })
    }
}
enum Event {
    Frame(u64, Preview),
    Progress(u64, usize, usize),
    Done(u64, Result<(), String>),
}
fn stage_status(name: &str, progress: &Progress) -> String {
    let phase = if progress.phase.trim().is_empty() {
        "Processing"
    } else {
        &progress.phase
    };
    match progress.total.filter(|total| *total > 0) {
        Some(total) => format!(
            "{name} · {phase} · {:.0}% of stage",
            (progress.completed.min(total) as f64 / total as f64 * 100.0).floor()
        ),
        None => format!("{name} · {phase} · working"),
    }
}
fn role_label(c: Capabilities) -> &'static str {
    match c.role {
        Role::Generator => "Generator",
        Role::Processor => "Processor",
        Role::PostProcessor => "Post-processor",
    }
}
fn input_label(c: Capabilities) -> String {
    match c.input {
        InputRequirement::Unspecified => "input needs undeclared".into(),
        InputRequirement::IndependentFrame => "one frame".into(),
        InputRequirement::Window { before, after } => {
            format!("context −{before} / +{after} frames")
        }
        InputRequirement::WholeClip => "needs entire clip".into(),
        InputRequirement::Recurrent { startup_frames } => {
            format!("segment history · {startup_frames}-frame startup")
        }
        InputRequirement::Batch { min_frames, max_frames } => {
            format!("batches {min_frames}–{max_frames} frames · padded tails")
        }
    }
}
fn capability_label(c: Capabilities) -> String {
    format!("{} · {}", role_label(c), input_label(c))
}
fn category_label(categories: &[Category]) -> String {
    if categories.is_empty() {
        "Uncategorized".into()
    } else {
        categories
            .iter()
            .map(Category::label)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}
/// Stage boundaries remain visible even for legacy sessions without callbacks.
struct ReportingSession {
    inner: Box<dyn Session>,
    report: abner_processor_api::ProgressReporter,
    frames: Arc<AtomicU64>,
}
impl Session for ReportingSession {
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>, String> {
        (self.report)(Progress {
            phase: "Processing".into(),
            completed: 0,
            total: None,
        });
        let result = self.inner.process(input);
        if let Ok(frames) = &result { self.frames.fetch_add(frames.len() as u64, Ordering::Relaxed); }
        (self.report)(self.inner.progress());
        result
    }
    fn finish(&mut self) -> Result<Vec<Frame>, String> {
        (self.report)(Progress {
            phase: "Finishing buffered frames".into(),
            completed: 0,
            total: None,
        });
        let result = self.inner.finish();
        if let Ok(frames) = &result { self.frames.fetch_add(frames.len() as u64, Ordering::Relaxed); }
        (self.report)(self.inner.progress());
        result
    }
    fn output_dimensions(&self) -> [usize; 2] {
        self.inner.output_dimensions()
    }
    fn cancellation(&self) -> Cancellation {
        self.inner.cancellation()
    }
    fn progress(&self) -> Progress {
        self.inner.progress()
    }
}
struct Preview {
    before: Rgba,
    after: Rgba,
    input: [u32; 2],
    output: [u32; 2],
    pts: f64,
    elapsed: f64,
}
impl Worker {
    fn new(registry: Registry) -> Self {
        let mailbox = Arc::new((
            Mutex::new(Mailbox {
                pending: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        // At most one section in flight plus its completion; the UI clears stale events.
        let (tx, rx) = mpsc::sync_channel(FRAMES + 2);
        let shared = mailbox.clone();
        let active = Arc::new(Mutex::new(Vec::new()));
        let active_worker = active.clone();
        let live = LiveProgress::default();
        let live_worker = live.clone();
        std::thread::spawn(move || {
            loop {
                let request = {
                    let (lock, cv) = &*shared;
                    let mut m = lock.lock().unwrap();
                    while m.pending.is_none() && !m.stop {
                        m = cv.wait(m).unwrap();
                    }
                    if m.stop {
                        break;
                    }
                    m.pending.take().unwrap()
                };
                let generation = request.generation;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render(&registry, &request, &tx, Some(&active_worker), Some(&live_worker))
                }))
                .unwrap_or_else(|_| Err("Processor panicked; preview stopped".into()));
                active_worker.lock().unwrap().clear();
                if tx.send(Event::Done(generation, result)).is_err() {
                    break;
                }
            }
        });
        Self {
            mailbox,
            rx,
            active,
            live,
        }
    }
    fn cancel(&self) {
        for c in self.active.lock().unwrap().iter() {
            c.cancel();
        }
    }
    fn submit(&self, request: Request) {
        let (lock, cv) = &*self.mailbox;
        lock.lock().unwrap().pending = Some(request);
        cv.notify_one();
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let (lock, cv) = &*self.mailbox;
        let mut m = lock.lock().unwrap();
        m.stop = true;
        m.pending = None;
        cv.notify_one();
    }
}
fn render(
    registry: &Registry,
    r: &Request,
    tx: &mpsc::SyncSender<Event>,
    active: Option<&Mutex<Vec<Cancellation>>>,
    live: Option<&LiveProgress>,
) -> Result<(), String> {
    r.cancel.check()?;
    // Plugins see the source at its NATIVE size (display dimensions, rotation
    // applied, the same the Sources decoder uses) — pixels are the product here,
    // and a proxy made every setting read as a loss. The display decoder emits
    // RGBA8, so the preview is still 8-bit in and out, not a color-managed
    // HDR conversion or a full-precision export.
    check_sdr(&r.info.path)?;
    let [w, h] = [r.info.width, r.info.height];
    let mut sessions = Vec::new();
    let mut dims = [w as usize, h as usize];
    for (index, (id, settings)) in r.chain.iter().enumerate() {
        let factory = registry.get(id)?;
        let caps = factory.capabilities();
        if !matches!(caps.role, Role::Processor)
            || matches!(caps.input, InputRequirement::WholeClip)
        {
            return Err(format!(
                "{}: generator / whole-clip execution is not supported by frame previews",
                factory.descriptor().name
            ));
        }
        if !factory.supports_preview() {
            return Err(format!(
                "{} does not support frame previews",
                factory.descriptor().name
            ));
        }
        let name = factory.descriptor().name;
        let generation = r.generation;
        let count = r.chain.len();
        let live = live.cloned();
        let frames = Arc::new(AtomicU64::new(0));
        let counted = frames.clone();
        let clock = Mutex::new(StageClock::new(Instant::now()));
        let report: abner_processor_api::ProgressReporter = Arc::new(move |progress| {
            if let Some(live) = &live {
                // One latest snapshot: tile callbacks never fill the frame channel
                // or leave the UI displaying a queue of obsolete percentages.
                *live.lock().unwrap() = Some((generation, StageFeedback {
                    name: name.clone(), index, count,
                    rates: clock.lock().unwrap().sample(&progress, counted.load(Ordering::Relaxed), Instant::now()),
                    progress,
                }));
            }
        });
        report(Progress { phase: "Preparing processor".into(), completed: 0, total: None });
        let mut session = factory.create(dims[0], dims[1], settings)?;
        session.set_progress_reporter(report.clone());
        let session: Box<dyn Session> = Box::new(ReportingSession {
            inner: session,
            report,
            frames,
        });
        dims = session.output_dimensions();
        if dims.contains(&0)
            || dims.iter().any(|d| *d > 8192)
            || dims[0] * dims[1] * 8 > r.budget
        {
            return Err(
                "Processor output exceeds the preview memory budget (processors.preview_cache_mb)"
                    .into(),
            );
        }
        if let Some(active) = active {
            active.lock().unwrap().push(session.cancellation());
        }
        r.cancel.check()?;
        sessions.push(session);
    }
    let [ow, oh] = dims;
    if ow < 2 || oh < 2 || ow > 8192 || oh > 8192 {
        return Err("Unsupported preview output dimensions".into());
    }
    let per_frame = (w as usize * h as usize + ow * oh) * 4;
    if per_frame > r.budget {
        return Err(
            "A native-size before/after pair exceeds the preview memory budget (processors.preview_cache_mb)"
                .into(),
        );
    }
    let count = (r.budget / per_frame).clamp(1, FRAMES);
    let fps = r.info.fps;
    if !fps.is_finite() || fps <= 0.0 || fps > 1000.0 {
        return Err("Preview needs a valid frame rate".into());
    }
    let player = Player::spawn(
        &r.info.path,
        w,
        h,
        crate::probe::vt_accel(&r.info.codec),
        r.info.rotation,
        8 * 1024 * 1024,
    )
    .ok_or("Could not open preview decoder")?;
    player.seek(r.start, true);
    let started = Instant::now();
    let mut originals = BTreeMap::new();
    let mut received = 0;
    let mut emit = |frames: Vec<Frame>,
                    originals: &mut BTreeMap<i64, (Rgba, f64)>|
     -> Result<bool, String> {
        let mut delivered = false;
        for frame in frames {
            r.cancel.check()?;
            frame.validate()?;
            if frame.width != ow || frame.height != oh || frame.time_base != [1, 1_000_000] {
                return Err("Preview processor changed its declared geometry or time base".into());
            }
            let (before, source_pts) = originals
                .remove(&frame.pts)
                .ok_or("Preview requires timestamp-preserving output frames")?;
            let mut rgba = Vec::with_capacity(ow * oh * 4);
            for rgb in frame.pixels.as_chunks::<3>().0 {
                rgba.extend([
                    ((rgb[0] as u32 + 128) / 257) as u8,
                    ((rgb[1] as u32 + 128) / 257) as u8,
                    ((rgb[2] as u32 + 128) / 257) as u8,
                    255,
                ]);
            }
            if r.target
                .is_some_and(|target| (source_pts - target).abs() > 0.51 / fps)
            {
                continue;
            }
            received += 1;
            delivered = true;
            tx.send(Event::Frame(
                r.generation,
                Preview {
                    before,
                    after: Arc::new(rgba),
                    input: [w, h],
                    output: [ow as u32, oh as u32],
                    pts: source_pts,
                    elapsed: started.elapsed().as_secs_f64(),
                },
            ))
            .map_err(|_| "Preview closed")?;
        }
        Ok(delivered)
    };
    let read_frame = || -> Result<Option<(f64, Rgba)>, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            r.cancel.check()?;
            if let Some(frame) = player.take_next() {
                return Ok(Some(frame));
            }
            if player.failed() {
                return Err("Preview decoder failed".into());
            }
            if player.exhausted() {
                return Ok(None);
            }
            if Instant::now() > deadline {
                return Err("Preview decoder timed out".into());
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    };
    let mut next = read_frame()?;
    let decode_count = r.target.map_or(count, |target| {
        (((target - r.start).max(0.0) * fps).ceil() as usize).saturating_add(FRAMES)
    });
    for decoded in 0..decode_count {
        if r.target.is_some() {
            let _ = tx.try_send(Event::Progress(r.generation, decoded, decode_count));
        }
        let Some((source_pts, rgba)) = next.take() else {
            break;
        };
        if r.end.is_some_and(|end| source_pts >= end - 1e-6) {
            break;
        }
        next = read_frame()?;
        let pts = (source_pts * 1e6).round() as i64;
        let nominal = (1e6 / fps).round().max(1.0) as i64;
        // One-frame lookahead preserves real source timestamps and rounding,
        // including fractional frame rates, while keeping sessions contiguous.
        let duration = next
            .as_ref()
            .map_or(nominal, |(t, _)| (*t * 1e6).round() as i64 - pts);
        if duration <= 0 || (duration - nominal).abs() > 2000 {
            return Err("This preview currently requires constant-rate video".into());
        }
        let pixels = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0] as u16 * 257, p[1] as u16 * 257, p[2] as u16 * 257])
            .collect();
        if originals.len() >= count {
            return Err("Processor buffered too many frames for preview".into());
        }
        originals.insert(pts, (rgba, source_pts));
        let delivered = emit(
            process_chain(
                &mut sessions,
                0,
                vec![Frame {
                    width: w as usize,
                    height: h as usize,
                    pixels,
                    pts,
                    duration,
                    time_base: [1, 1_000_000],
                }],
                &r.cancel,
                r.budget,
            )?,
            &mut originals,
        )?;
        // A targeted preview is complete once its output arrives. Dropping the
        // sessions here avoids starting another batch or flushing unwanted tails.
        if delivered && r.target.is_some() { return Ok(()); }
    }
    r.cancel.check()?;
    for i in 0..sessions.len() {
        let tail = sessions[i].finish()?;
        let delivered = emit(
            process_chain(&mut sessions, i + 1, tail, &r.cancel, r.budget)?,
            &mut originals,
        )?;
        if delivered && r.target.is_some() { return Ok(()); }
    }
    if received == 0 {
        return Err("No preview frames at this position".into());
    }
    if !originals.is_empty() {
        return Err("Processor did not return all preview frames".into());
    }
    Ok(())
}
fn process_chain(
    sessions: &mut [Box<dyn abner_processor_api::Session>],
    from: usize,
    mut frames: Vec<Frame>,
    cancel: &Cancellation,
    budget: usize,
) -> Result<Vec<Frame>, String> {
    for session in &mut sessions[from..] {
        let mut outputs = Vec::new();
        for frame in frames {
            cancel.check()?;
            outputs.extend(session.process(frame)?);
            let bytes: usize = outputs
                .iter()
                .map(|f| f.pixels.len().saturating_mul(2))
                .sum();
            if bytes > budget {
                return Err("Processor batch exceeds the preview memory budget".into());
            }
        }
        frames = outputs;
    }
    Ok(frames)
}
fn check_sdr(path: &std::path::Path) -> Result<(), String> {
    crate::probe::metadata(path, Duration::from_secs(5))
        .map_err(|e| e.to_string())?
        .check_sdr()
}


pub struct Panel {
    pub open: bool,
    descriptors: Vec<Descriptor>,
    selected: usize,
    saved: Vec<Value>,
    enabled: Vec<bool>,
    supported: Vec<bool>,
    capabilities: Vec<Capabilities>,
    categories: Vec<Vec<Category>>,
    /// Last pointer position, for the rail's hover states.
    cursor: (f32, f32),
    /// The focused-processor inspector under the list, folded by its grip
    /// like the Sources rail's; `detail_at` times the 150 ms slide.
    show_detail: bool,
    detail_at: Option<Instant>,
    pub seek: Option<f64>,
    pub toggle_play: bool,
    time: f64,
    pub segment_start: f64,
    pub timeline_revision: u64,
    duration: f64,
    fps: f64,
    source: usize,
    source_dims: [u32; 2],
    list_scroll: f32,
    scrubbing: bool,
    filename: String,
    fields: Vec<Field>,
    settings: Value,
    worker: Option<Worker>,
    /// `processors.preview_cache_mb` in bytes: bounds each section's paired
    /// frames on the worker and the rendered frames kept across the timeline.
    budget: usize,
    generation: u64,
    cancel: Cancellation,
    dirty: Option<Instant>,
    context: Option<(std::path::PathBuf, u64, Option<u64>)>,
    frames: Vec<Preview>,
    status: String,
    busy: bool,
    feedback: Option<StageFeedback>,
    render_result: Option<bool>,
    show_rendered: bool,
    render_elapsed: Duration,
    preview_count: u64,
    /// The frame the running render was asked for, and the worker's context
    /// progress (frames decoded, frames to decode): the amber marks.
    pending: Option<f64>,
    progress: Option<(usize, usize)>,
    busy_since: Instant,
    selected_frame: usize,
    playback: bool,
    editing: Option<(usize, String)>,
    scroll: f32,
    dragging: Option<usize>,
    preset: usize,
    text_selected: bool,
}
impl Default for Panel {
    fn default() -> Self {
        Self {
            open: false,
            descriptors: vec![],
            selected: 0,
            saved: vec![],
            enabled: vec![],
            supported: vec![],
            capabilities: vec![],
            categories: vec![],
            cursor: (-1.0, -1.0),
            show_detail: true,
            detail_at: None,
            seek: None,
            toggle_play: false,
            time: 0.0,
            segment_start: 0.0,
            timeline_revision: 0,
            duration: 1.0,
            fps: 24.0,
            source: 0,
            source_dims: [16, 9],
            list_scroll: 0.0,
            scrubbing: false,
            filename: String::new(),
            fields: vec![],
            settings: json!({}),
            worker: None,
            budget: DEFAULT_PREVIEW_BYTES,
            generation: 0,
            cancel: Cancellation::default(),
            dirty: None,
            context: None,
            frames: vec![],
            status: String::new(),
            busy: false,
            feedback: None,
            render_result: None,
            show_rendered: true,
            render_elapsed: Duration::ZERO,
            preview_count: 0,
            pending: None,
            progress: None,
            busy_since: Instant::now(),
            selected_frame: 0,
            playback: true,
            editing: None,
            scroll: 0.0,
            dragging: None,
            preset: 0,
            text_selected: false,
        }
    }
}
impl Panel {
    pub fn new(registry: Registry) -> Self {
        let descriptors = registry.descriptors();
        let capabilities = descriptors
            .iter()
            .map(|d| registry.get(&d.id).unwrap().capabilities())
            .collect();
        let categories = descriptors
            .iter()
            .map(|d| {
                let mut c = registry.get(&d.id).unwrap().categories();
                c.sort();
                c.dedup();
                c
            })
            .collect();
        let supported = descriptors
            .iter()
            .map(|d| {
                registry.get(&d.id).is_ok_and(|f| {
                    let c = f.capabilities();
                    f.supports_preview()
                        && matches!(c.role, Role::Processor)
                        && !matches!(c.input, InputRequirement::WholeClip)
                })
            })
            .collect();
        let saved = descriptors
            .iter()
            .map(|d| defaults(&fields(&d.settings_schema)))
            .collect();
        // Plugins start bypassed: running one is always the user's choice.
        let enabled = vec![false; descriptors.len()];
        let worker = (!descriptors.is_empty()).then(|| Worker::new(registry));
        let mut s = Self::default();
        s.descriptors = descriptors;
        s.saved = saved;
        s.enabled = enabled;
        s.supported = supported;
        s.capabilities = capabilities;
        s.categories = categories;
        s.worker = worker;
        s.select(0);
        s
    }
    fn select(&mut self, index: usize) {
        if self.descriptors.is_empty() {
            return;
        }
        self.dragging = None;
        self.selected = index;
        self.preset = 0;
        self.fields = fields(&self.descriptors[index].settings_schema);
        self.settings = self.saved[index].clone();
        self.scroll = 0.0;
        self.editing = None;
        self.changed();
    }
    /// Apply the configured preview budget; the next render uses it.
    pub fn set_budget(&mut self, bytes: usize) {
        self.budget = bytes.max(1);
    }
    fn changed(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
        self.generation += 1;
        self.frames.clear();
        self.selected_frame = 0;
        self.busy = false;
        self.feedback = None;
        self.render_result = None;
        self.preview_count = 0;
        self.render_elapsed = Duration::ZERO;
        self.dirty = None;
        if let Some(saved) = self.saved.get_mut(self.selected) {
            *saved = self.settings.clone();
        }
        self.status = "Render the current frame to inspect the enabled chain.".into();
    }
    fn toggle(&mut self) {
        self.dragging = None;
        self.open = !self.open;
        if self.open {
            self.changed();
        } else {
            self.cancel.cancel();
            if let Some(worker) = &self.worker {
                worker.cancel();
            }
            self.generation += 1;
            self.busy = false;
            self.editing = None;
            self.dirty = None;
        }
    }
    pub fn start(&mut self, id: &str, settings: Value) -> Result<(), String> {
        let index = self
            .descriptors
            .iter()
            .position(|d| d.id == id)
            .ok_or_else(|| format!("Processor not registered: {id}"))?;
        if !settings.is_object() {
            return Err("Processor settings must be an object".into());
        }
        self.select(index);
        // Naming it on the command line is the opt-in.
        self.enabled[index] = true;
        for (k, v) in settings.as_object().unwrap() {
            self.settings[k] = v.clone();
        }
        self.open = true;
        self.changed();
        Ok(())
    }
    pub fn shortcut(&mut self, c: char) -> bool {
        if !self.open || self.editing.is_none() {
            return false;
        }
        match c.to_ascii_lowercase() {
            'a' => self.text_selected = true,
            'v' => {
                if let Some(text) = crate::open::paste_text() {
                    self.text_input(text.trim_end_matches(['\n', '\r']));
                }
            }
            _ => {}
        }
        true
    }
    pub fn text_input(&mut self, text: &str) -> bool {
        if self.open
            && let Some((_, value)) = &mut self.editing
        {
            if self.text_selected {
                value.clear();
                self.text_selected = false;
            }
            value.push_str(text);
            return true;
        }
        false
    }
    pub fn pointer(&mut self, x: f32, y: f32, vp: (f32, f32)) -> bool {
        if !self.open {
            return false;
        }
        self.cursor = (x, y);
        if self.scrubbing {
            let t = Self::timeline(vp);
            self.seek = Some(
                (((x - t.x) / t.w).clamp(0.0, 1.0) as f64 * self.duration * self.fps).floor()
                    / self.fps,
            );
            return true;
        }
        if let Some(i) = self.dragging {
            let row = self.row(i, vp);
            let f = &self.fields[i];
            if let Kind::Number { min, max, integer } = f.kind {
                let value = numeric(min, max, integer, ((x - row.x) / row.w) as f64);
                if self.settings[&f.key] != value {
                    self.settings[&f.key] = value;
                    self.changed();
                }
            }
            return true;
        }
        hit(Self::bounds(vp), x, y)
    }
    pub fn release(&mut self) -> bool {
        let scrub = std::mem::take(&mut self.scrubbing);
        self.dragging.take().is_some() || scrub
    }
    pub fn close(&mut self) {
        if self.open {
            self.toggle();
        }
    }
    /// Only the inspector's fold slide runs hot; the host polls the worker
    /// at 100ms while paused.
    pub fn animating(&self) -> bool {
        self.busy
            || self.detail_at
                .is_some_and(|t| t.elapsed().as_secs_f32() < DETAIL_SLIDE_S)
    }
    /// 0 folded … 1 open.
    fn detail_slide(&self) -> f32 {
        let p = self.detail_at.map_or(1.0, |t| {
            (t.elapsed().as_secs_f32() / DETAIL_SLIDE_S).clamp(0.0, 1.0)
        });
        if self.show_detail { p } else { 1.0 - p }
    }
    fn toggle_detail(&mut self) {
        // Reversing mid-slide starts from where the panel is: back-date the
        // start so the new direction's progress equals the distance covered.
        let open = self.detail_slide();
        self.show_detail = !self.show_detail;
        let done = if self.show_detail { open } else { 1.0 - open };
        self.detail_at = Some(Instant::now() - Duration::from_secs_f32(done * DETAIL_SLIDE_S));
    }
    pub fn enter(&mut self) {
        self.open = true;
    }
    pub fn source(&mut self, slot: usize, dims: [u32; 2], playing: bool) {
        self.source = slot;
        self.source_dims = dims;
        self.playback = playing;
    }
    pub fn take_seek(&mut self) -> Option<f64> {
        self.seek.take()
    }
    fn request_preview(&mut self) {
        self.cancel.cancel();
        if let Some(w) = &self.worker {
            w.cancel();
        }
        self.generation += 1;
        self.dirty = Some(Instant::now());
        self.toggle_play = self.playback;
        self.feedback = None;
        self.render_result = None;
        self.show_rendered = true;
        self.preview_count = 0;
        self.status = "Preparing render…".into();
    }
    fn cancel_preview(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = &self.worker {
            worker.cancel();
            worker.mailbox.0.lock().unwrap().pending = None;
        }
        self.generation += 1;
        self.dirty = None;
        self.busy = false;
        self.pending = None;
        self.progress = None;
        self.feedback = None;
        self.render_elapsed = self.busy_since.elapsed();
        self.render_result = Some(false);
        self.status = "Frame preview cancelled".into();
    }
    /// One source frame back or forward, clamped to the clip.
    fn step(&mut self, dir: i32) {
        self.seek = Some(if dir < 0 {
            (self.time - 1.0 / self.fps).max(0.0)
        } else {
            (self.time + 1.0 / self.fps).min((self.duration - 1.0 / self.fps).max(0.0))
        });
    }
    fn jump(&mut self, next: bool) {
        let tolerance = 0.5 / self.fps.max(1.0);
        self.seek = if next {
            self.frames.iter().find(|f| f.pts > self.time + tolerance)
        } else {
            self.frames
                .iter()
                .rev()
                .find(|f| f.pts < self.time - tolerance)
        }
        .map(|f| f.pts);
    }
    fn chain(&self) -> Vec<(String, Value)> {
        self.descriptors
            .iter()
            .enumerate()
            .take(self.selected + 1)
            .filter(|(i, _)| self.enabled[*i])
            .map(|(i, d)| (d.id.clone(), self.saved[i].clone()))
            .collect()
    }
    fn move_selected(&mut self, delta: isize) {
        if self.descriptors.is_empty() {
            return;
        }
        let to = self
            .selected
            .saturating_add_signed(delta)
            .min(self.descriptors.len() - 1);
        self.saved[self.selected] = self.settings.clone();
        self.descriptors.swap(self.selected, to);
        self.saved.swap(self.selected, to);
        self.enabled.swap(self.selected, to);
        self.supported.swap(self.selected, to);
        self.capabilities.swap(self.selected, to);
        self.categories.swap(self.selected, to);
        self.select(to);
    }
    pub fn key(&mut self, key: Key) -> bool {
        if self.open && self.editing.is_some() {
            match key {
                Key::Escape => self.editing = None,
                Key::Backspace => {
                    if self.text_selected {
                        self.editing.as_mut().unwrap().1.clear();
                        self.text_selected = false;
                    } else {
                        self.editing.as_mut().unwrap().1.pop();
                    }
                }
                Key::Enter => {
                    let (i, text) = self.editing.clone().unwrap();
                    match text_value(
                        &self.fields[i],
                        &self.descriptors[self.selected].settings_schema,
                        &text,
                    ) {
                        Ok(v) => {
                            let key = &self.fields[i].key;
                            match v {
                                Some(v) => {
                                    self.settings[key] = v;
                                }
                                None => {
                                    self.settings.as_object_mut().unwrap().remove(key);
                                }
                            }
                            self.editing = None;
                            self.changed();
                        }
                        Err(e) => self.status = e,
                    }
                }
                Key::Char(c) => {
                    self.text_input(&c.to_string());
                }
                Key::Space => {
                    self.text_input(" ");
                }
                _ => {}
            }
            return true;
        }
        if !self.open {
            return false;
        }
        match key {
            Key::Tab => {
                if self.frames.iter().any(|f| (f.pts - self.time).abs() <= 0.51 / self.fps) {
                    self.show_rendered = !self.show_rendered;
                } else {
                    self.status = "No rendered frame at this position · Render frame first".into();
                }
                true
            }
            Key::Space => {
                self.toggle_play = true;
                true
            }
            Key::KeyLeft => {
                self.jump(false);
                true
            }
            Key::KeyRight => {
                self.jump(true);
                true
            }
            Key::Char('r' | 'R') => {
                self.request_preview();
                true
            }
            Key::Left => {
                self.step(-1);
                true
            }
            Key::Right => {
                self.step(1);
                true
            }
            Key::Char('m' | 'M' | 'c' | 'C' | 't' | 'T' | 'p' | 'P') => true,
            Key::Render | Key::SaveProject => {
                self.status =
                    "Process previews do not write media. Use Timeline for editing exports.".into();
                true
            }
            _ => false,
        }
    }
    fn bounds(vp: (f32, f32)) -> RectPx {
        RectPx {
            x: 0.0,
            y: 44.0,
            w: vp.0,
            h: (vp.1 - 44.0).max(1.0),
        }
    }
    /// Where the columns end: the footer bar's top.
    fn bottom(vp: (f32, f32)) -> f32 {
        vp.1 - FOOTER_H
    }
    fn footer(vp: (f32, f32)) -> RectPx {
        RectPx { x: 0.0, y: Self::bottom(vp), w: vp.0, h: FOOTER_H }
    }
    /// Render frame, where the Timeline's footer keeps Split.
    pub(crate) fn render_button(vp: (f32, f32)) -> RectPx {
        RectPx { x: 90.0, y: Self::bottom(vp) + 6.0, w: 132.0, h: 26.0 }
    }
    /// The Sources rail's width, so switching modes doesn't move the edge.
    fn left(vp: (f32, f32)) -> f32 {
        rail_full_width(vp.0)
    }
    fn list_top() -> f32 {
        HEADER_H + RAIL_HEAD_H + 10.0
    }
    /// The grip strip and the visible part of the focused inspector, as
    /// the Sources rail lays them out: above the Move up / down row, and
    /// only when the list keeps room for one card and its footer line.
    fn detail(&self, vp: (f32, f32)) -> (RectPx, RectPx) {
        let l = Self::left(vp);
        let bottom = Self::bottom(vp) - RAIL_ACTIONS_H;
        let none = RectPx { x: 0.0, y: bottom, w: l, h: 0.0 };
        let needed = CARD_PITCH + RAIL_FOOTER_H + RAIL_GRIP_H + RAIL_INSPECTOR_H;
        if self.descriptors.is_empty() || bottom - Self::list_top() < needed {
            return (none, none);
        }
        let t = self.detail_slide();
        let shown = (RAIL_INSPECTOR_H * t * t * (3.0 - 2.0 * t)).round();
        let inspector = RectPx { x: 0.0, y: bottom - shown, w: l, h: shown };
        let grip = RectPx { x: 0.0, y: inspector.y - RAIL_GRIP_H, w: l, h: RAIL_GRIP_H };
        (grip, inspector)
    }
    fn list(&self, vp: (f32, f32)) -> RectPx {
        let (grip, _) = self.detail(vp);
        let y = Self::list_top();
        let end = if grip.h > 0.0 { grip.y } else { Self::bottom(vp) - RAIL_ACTIONS_H };
        RectPx { x: 0.0, y, w: Self::left(vp), h: (end - RAIL_FOOTER_H - y).max(CARD_PITCH) }
    }
    fn card(&self, i: usize, vp: (f32, f32)) -> RectPx {
        RectPx {
            x: 12.0,
            y: Self::list_top() + i as f32 * CARD_PITCH - self.list_scroll,
            w: Self::left(vp) - 24.0,
            h: CARD_PITCH - 8.0,
        }
    }
    /// Switch geometry is shared by painting and hit testing.
    fn card_switch(&self, i: usize, vp: (f32, f32)) -> RectPx {
        let r = self.card(i, vp);
        RectPx { x: r.x + r.w - 74.0, y: r.y + (r.h - 28.0) / 2.0, w: 64.0, h: 28.0 }
    }
    fn move_button(vp: (f32, f32), down: bool) -> RectPx {
        let w = (Self::left(vp) - 32.0) / 2.0;
        RectPx { x: if down { 20.0 + w } else { 12.0 }, y: Self::bottom(vp) - 34.0, w, h: 28.0 }
    }
    fn right(vp: (f32, f32)) -> f32 {
        (vp.0 * 0.25).clamp(200.0, 310.0)
    }
    pub(crate) fn viewer(vp: (f32, f32)) -> RectPx {
        let l = Self::left(vp);
        let r = Self::right(vp);
        RectPx {
            x: l + 16.0,
            y: 88.0,
            w: (vp.0 - l - r - 32.0).max(1.0),
            h: (Self::transport_y(vp) - 124.0 - 88.0).max(1.0),
        }
    }
    fn progress_button(vp: (f32, f32)) -> RectPx {
        let v = Self::viewer(vp);
        RectPx { x: v.x + v.w - 104.0, y: Self::transport_y(vp) - 90.0, w: 92.0, h: 26.0 }
    }
    fn timeline(vp: (f32, f32)) -> RectPx {
        // The viewer's span, computed directly: the viewer's height asks
        // for the transport, which sits on this rect.
        let (l, r) = (Self::left(vp), Self::right(vp));
        RectPx {
            x: l + 16.0,
            y: Self::bottom(vp) - 60.0,
            w: (vp.0 - l - r - 32.0).max(1.0),
            h: 28.0,
        }
    }
    fn transport_y(vp: (f32, f32)) -> f32 {
        Self::timeline(vp).y - 46.0
    }
    fn transport(vp: (f32, f32), i: usize) -> RectPx {
        let v = Self::viewer(vp);
        let total = 5.0 * TRANSPORT_W + 4.0 * TRANSPORT_GAP;
        RectPx {
            x: v.x + ((v.w - total) / 2.0).max(0.0) + i as f32 * (TRANSPORT_W + TRANSPORT_GAP),
            y: Self::transport_y(vp),
            w: TRANSPORT_W,
            h: 30.0,
        }
    }
    fn row(&self, i: usize, vp: (f32, f32)) -> RectPx {
        let r = Self::bounds(vp);
        RectPx {
            x: vp.0 - Self::right(vp) + 14.0,
            y: r.y + 88.0 + i as f32 * 51.0 - self.scroll,
            w: Self::right(vp) - 28.0,
            h: 45.0,
        }
    }
    pub fn scroll(&mut self, dy: f32, cursor: (f32, f32), vp: (f32, f32)) -> bool {
        let r = Self::bounds(vp);
        if self.open && hit(r, cursor.0, cursor.1) {
            if cursor.0 < Self::left(vp) {
                let list = self.list(vp);
                self.list_scroll = (self.list_scroll - dy).clamp(
                    0.0,
                    (self.descriptors.len() as f32 * CARD_PITCH - list.h).max(0.0),
                );
                return true;
            }
            if cursor.0 < vp.0 - Self::right(vp) {
                // Over the canvas the app pans a zoomed picture, as on the
                // timeline; the rest of the middle column swallows it.
                return !hit(Self::viewer(vp), cursor.0, cursor.1);
            }
            self.scroll = (self.scroll - dy).clamp(
                0.0,
                (self.fields.len() as f32 * 51.0 - (Self::bottom(vp) - 151.0)).max(0.0),
            );
            return true;
        }
        false
    }
    pub fn click(&mut self, x: f32, y: f32, vp: (f32, f32)) -> bool {
        if !self.open {
            return false;
        }
        let r = Self::bounds(vp);
        if !hit(r, x, y) {
            return false;
        }
        if hit(Self::footer(vp), x, y) {
            if hit(Self::render_button(vp), x, y) {
                self.request_preview();
            }
            return true;
        }
        if hit(Self::progress_button(vp), x, y) {
            if self.busy { self.cancel_preview(); } else { self.request_preview(); }
            return true;
        }
        if x < Self::left(vp) {
            let (grip, _) = self.detail(vp);
            if grip.h > 0.0 && hit(grip, x, y) {
                self.toggle_detail();
            } else if hit(Self::move_button(vp, false), x, y) {
                self.move_selected(-1);
            } else if hit(Self::move_button(vp, true), x, y) {
                self.move_selected(1);
            } else if hit(self.list(vp), x, y) {
                let index = ((y - Self::list_top() + self.list_scroll) / CARD_PITCH) as usize;
                if index < self.descriptors.len() && hit(self.card(index, vp), x, y) {
                    if hit(self.card_switch(index, vp), x, y) {
                        self.enabled[index] = !self.enabled[index];
                        self.changed();
                    } else if index != self.selected {
                        self.select(index);
                    }
                }
            }
            return true;
        }
        for i in 0..5 {
            if hit(Self::transport(vp, i), x, y) {
                match i {
                    0 => self.jump(false),
                    1 => self.step(-1),
                    2 => self.toggle_play = true,
                    3 => self.step(1),
                    _ => self.jump(true),
                }
                return true;
            }
        }
        let timeline = Self::timeline(vp);
        if hit(timeline, x, y) {
            self.scrubbing = true;
            self.seek = Some(
                (((x - timeline.x) / timeline.w).clamp(0.0, 1.0) as f64 * self.duration * self.fps)
                    .floor()
                    / self.fps,
            );
            return true;
        }
        let right = vp.0 - Self::right(vp);
        if (50.0..86.0).contains(&y) && x >= right && !self.descriptors.is_empty() {
            let d = &self.descriptors[self.selected];
            if let Some(p) = d.presets.get(self.preset % d.presets.len().max(1)) {
                self.preset += 1;
                self.settings = defaults(&self.fields);
                if let Some(map) = p.settings.as_object() {
                    for (k, v) in map {
                        self.settings[k] = v.clone();
                    }
                }
                self.changed();
            }
            return true;
        }
        if y < r.y + r.h - 60.0 && y >= r.y + 102.0 {
            for i in 0..self.fields.len() {
                let row = self.row(i, vp);
                if !hit(row, x, y) {
                    continue;
                }
                let f = &self.fields[i];
                if matches!(f.kind, Kind::Number { .. }) && y < row.y + 17.0 {
                    self.text_selected = true;
                    self.editing = Some((i, self.settings[&f.key].to_string()));
                    return true;
                }
                match &f.kind {
                    Kind::Number { min, max, integer } => {
                        self.dragging = Some(i);
                        self.settings[&f.key] =
                            numeric(*min, *max, *integer, ((x - row.x) / row.w) as f64)
                    }
                    Kind::Enum(values) => {
                        let n = values
                            .iter()
                            .position(|v| v == &self.settings[&f.key])
                            .map_or(0, |i| (i + 1) % values.len());
                        self.settings[&f.key] = values[n].clone();
                    }
                    Kind::Bool => {
                        self.settings[&f.key] =
                            json!(!self.settings[&f.key].as_bool().unwrap_or(false))
                    }
                    Kind::Text => {
                        self.text_selected = true;
                        self.editing = Some((
                            i,
                            self.settings[&f.key]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| {
                                    if self.settings.get(&f.key).is_some() {
                                        self.settings[&f.key].to_string()
                                    } else {
                                        String::new()
                                    }
                                }),
                        ));
                        return true;
                    }
                    Kind::Unsupported => {
                        self.status =
                            "This input type is not supported by the preview editor".into();
                        return true;
                    }
                }
                self.editing = None;
                self.changed();
                return true;
            }
        }
        true
    }
    pub fn tick(
        &mut self,
        info: &VideoInfo,
        start: f64,
        end: Option<f64>,
        uploads: &mut Vec<Upload>,
        slot: usize,
    ) {
        if !self.open {
            if let Some(w) = &self.worker {
                while w.rx.try_recv().is_ok() {}
            }
            return;
        }
        self.time = start;
        self.filename = info
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.duration = info.duration;
        self.fps = info.fps.max(1.0);
        let context = (
            info.path.clone(),
            info.duration.to_bits(),
            Some(self.timeline_revision),
        );
        if self.context.as_ref() != Some(&context) {
            self.context = Some(context);
            self.changed();
        }
        if self.dirty.is_some() && self.editing.is_none() {
            self.dirty = None;
            let chain = self.chain();
            if chain.is_empty() && !self.descriptors.is_empty() {
                self.status = "Nothing enabled · switch a processor on to run it".into();
            } else if self
                .descriptors
                .iter()
                .enumerate()
                .take(self.selected + 1)
                .any(|(i, _)| self.enabled[i] && !self.supported[i])
            {
                self.status = "An enabled processor does not support frame previews.".into();
            } else if let Some(worker) = &self.worker {
                self.cancel = Cancellation::default();
                self.busy = true;
                self.status = "Rendering current frame through the enabled chain…".into();
                self.pending = Some(start);
                self.progress = None;
                self.busy_since = Instant::now();
                worker.submit(Request {
                    generation: self.generation,
                    chain,
                    target: Some(start),
                    info: info.clone(),
                    start: self.segment_start,
                    end,
                    cancel: self.cancel.clone(),
                    budget: self.budget,
                });
            } else {
                self.status = "No processors installed in this build.".into();
            }
        }
        if let Some(worker) = &self.worker {
            while let Ok(event) = worker.rx.try_recv() {
                match event {
                    Event::Frame(g, f) if g == self.generation => {
                        self.preview_count += 1;
                        self.frames
                            .retain(|old| (old.pts - f.pts).abs() > 0.5 / self.fps);
                        let needed = f.before.len() + f.after.len();
                        while !self.frames.is_empty()
                            && self
                                .frames
                                .iter()
                                .map(|f| f.before.len() + f.after.len())
                                .sum::<usize>()
                                + needed
                                > self.budget
                        {
                            self.frames.remove(0);
                        }
                        self.status =
                            format!("Rendered {:.3}s · {:.2}s processing", f.pts, f.elapsed);
                        self.frames.push(f);
                        self.frames.sort_by(|a, b| a.pts.total_cmp(&b.pts));
                    }
                    Event::Progress(g, done, total) if g == self.generation => {
                        self.progress = Some((done, total));
                        self.feedback = None;
                        self.status = format!("Preparing temporal context · {done}/{total} frames");
                    }
                    Event::Done(g, result) if g == self.generation => {
                        self.busy = false;
                        self.render_result = Some(result.is_ok());
                        self.render_elapsed = self.busy_since.elapsed();
                        self.pending = None;
                        self.progress = None;
                        if let Err(e) = result {
                            self.status = e;
                        } else {
                            self.status =
                                format!("Render ready · {} saved frames", self.frames.len());
                        }
                    }
                    _ => {}
                }
            }
            if let Some((g, feedback)) = worker.live.lock().unwrap().take()
                && g == self.generation && self.busy {
                self.status = stage_status(&feedback.name, &feedback.progress);
                self.feedback = Some(feedback);
            }
        }
        if let Some(i) = self
            .frames
            .iter()
            .position(|f| (f.pts - start).abs() <= 0.51 / self.fps)
        {
            self.selected_frame = i;
            let f = &self.frames[i];
            uploads.push(Upload {
                idx: slot + 1,
                w: f.output[0],
                h: f.output[1],
                buf: f.after.clone(),
            });
        }
    }
    /// The left column as the Sources rail draws it (278:584): a heading
    /// strip, compact processor rows (name, categories, OFF/ON switch) ringed
    /// orange on the focused one, then the folding focused inspector and the
    /// Move up / down row in the Duplicate / Add files strip.
    fn draw_rail(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let l = Self::left(vp);
        let ink = hex_color(SOURCE_INK);
        let secondary = hex_color(SOURCE_SECONDARY);
        let subtle = hex_color(SOURCE_SUBTLE);
        let over = |r: RectPx| hit(r, self.cursor.0, self.cursor.1);
        items.push(Item::Rect(RectItem::new(
            RectPx { x: 0.0, y: HEADER_H, w: l, h: Self::bottom(vp) - HEADER_H },
            WORKSPACE_PANEL,
        )));
        items.push(Item::Rect(RectItem::new(
            RectPx { x: l - 1.0, y: HEADER_H, w: 1.0, h: Self::bottom(vp) - HEADER_H },
            WORKSPACE_RULE,
        )));
        let (hx, hy) = (14.0, HEADER_H + RAIL_HEAD_H / 2.0);
        items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(l - 110.0),
            ..TextItem::new(hx, hy, 13.0, ink, "Processors") }));
        items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(40.0),
            ..TextItem::new(hx + 11.0 * 13.0 * MONO_ADV, hy, 13.0, secondary, self.descriptors.len().to_string()) }));
        let on = self.enabled.iter().filter(|e| **e).count();
        if !self.descriptors.is_empty() {
            items.push(Item::Text(TextItem { regular: true, align: Align::Right, valign: VAlign::Middle, max_width: Some(70.0),
                ..TextItem::new(l - 14.0, hy, 11.0, if on > 0 { hex_color(CUT_HEADER_BLUE) } else { subtle }, format!("{on} on")) }));
        }
        let list = self.list(vp);
        items.push(Item::Clip(Some(list)));
        for (i, d) in self.descriptors.iter().enumerate() {
            let c = self.card(i, vp);
            if c.y + c.h < list.y || c.y > list.y + list.h {
                continue;
            }
            let active = i == self.selected;
            let hover = over(c) && over(list);
            items.push(Item::Rect(RectItem { radius: 10.0, border_w: 2.0,
                border_color: hex_color(if active { SOURCE_RING } else if hover { SOURCE_EDGE_HOVER } else { SOURCE_EDGE }),
                ..RectItem::new(c, if active { WORKSPACE_PANEL } else { hex_color(0x000000) }) }));
            let on = self.enabled[i];
            let switch = self.card_switch(i, vp);
            let tx = c.x + 12.0;
            let tw = switch.x - tx - 10.0;
            items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(tw),
                ..TextItem::new(tx, c.y + 16.0, 12.0, if on { ink } else { secondary }, d.name.as_str()) }));
            let meta = if on && !self.supported[i] {
                "No frame preview".to_string()
            } else {
                category_label(&self.categories[i])
            };
            items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(tw),
                ..TextItem::new(tx, c.y + 33.0, 10.0, if on { secondary } else { subtle }, meta) }));
            // The reference's sliding OFF/ON control: only the active side
            // carries a label, with a blue ON thumb and a white outer contour.
            items.push(Item::Rect(RectItem { radius: 9.0, border_w: 1.5,
                border_color: if over(switch) { hex_color(0xffffff) } else { ink },
                ..RectItem::new(switch, hex_color(0x0c0d0e)) }));
            let thumb = RectPx { x: switch.x + if on { 33.0 } else { 3.0 },
                y: switch.y + 3.0, w: 28.0, h: 22.0 };
            items.push(Item::Rect(RectItem { radius: 6.0, border_w: 1.0,
                border_color: hex_color(if on { 0x61b5ee } else { 0x606b79 }),
                ..RectItem::new(thumb, hex_color(if on { CUT_HEADER_BLUE } else { 0x292e34 })) }));
            items.push(Item::Text(TextItem { header: true, align: Align::Center, valign: VAlign::Middle,
                max_width: Some(thumb.w), ..TextItem::new(thumb.x + thumb.w / 2.0,
                    thumb.y + thumb.h / 2.0, 11.0, hex_color(0xffffff), if on { "ON" } else { "OFF" }) }));
        }
        items.push(Item::Clip(None));
        let footer = if self.descriptors.is_empty() {
            "No processors in this build"
        } else {
            "Enabled processors run top to bottom"
        };
        items.push(Item::Text(TextItem { interface: true, valign: VAlign::Middle, max_width: Some(l - 24.0),
            ..TextItem::new(12.0, list.y + list.h + 16.0, 11.0, subtle, footer) }));
        self.draw_detail(items, vp);
        for down in [false, true] {
            let b = Self::move_button(vp, down);
            let hover = over(b) && !self.descriptors.is_empty();
            items.push(Item::Rect(RectItem { radius: 8.0, border_w: 1.0,
                border_color: hex_color(SOURCE_BORDER), ..RectItem::new(b, hex_color(SOURCE_SURFACE)) }));
            items.push(Item::Text(TextItem { interface: true, align: Align::Center, valign: VAlign::Middle, max_width: Some(b.w - 8.0),
                ..TextItem::new(b.x + b.w / 2.0, b.y + b.h / 2.0, 11.0, if hover { WORKSPACE_TEXT } else { WORKSPACE_DIM },
                    if down { "Move down" } else { "Move up" }) }));
        }
    }
    /// Focused inspector (278:609), for the focused processor: keycap and
    /// name as the title row, five mono rows between dashed hairlines
    /// (categories first) and the registry ID where Sources puts the path.
    fn draw_detail(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let (grip, inspector) = self.detail(vp);
        if grip.h <= 0.0 {
            return;
        }
        let l = Self::left(vp);
        if inspector.h > 0.0 {
            let i = self.selected;
            let d = &self.descriptors[i];
            let c = self.capabilities[i];
            let (y, x0, x1) = (inspector.y, 16.0, l - 16.0);
            // Full layout anchored at the top, clipped while it folds.
            items.push(Item::Clip(Some(inspector)));
            items.push(Item::Rect(RectItem::new(RectPx { h: RAIL_INSPECTOR_H, ..inspector }, hex_color(INSPECTOR_BG))));
            draw_keycap(items, RectPx { x: x0, y: y + 16.0, w: 24.0, h: 24.0 }, i + 1, mix(INSPECTOR_BG, 0xffffff, 0.5));
            items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(l - 80.0),
                ..TextItem::new(x0 + 32.0, y + 28.0, 13.0, hex_color(SOURCE_INK), d.name.as_str()) }));
            for ry in [y + 49.0, y + 178.0] {
                draw_dashed_hairline(items, x0, x1, ry);
            }
            let settings = format!("{} fields · {} presets", self.fields.len(), d.presets.len());
            for (k, (key, value)) in [
                ("Category", category_label(&self.categories[i])),
                ("Role", role_label(c).to_string()),
                ("Input", input_label(c)),
                ("Preview", if self.supported[i] { "Frame preview".into() } else { "Not supported".into() }),
                ("Settings", settings),
            ].into_iter().enumerate() {
                let ry = y + 68.5 + k as f32 * 23.0;
                items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(78.0),
                    ..TextItem::new(x0, ry, 11.0, WORKSPACE_DIM, key) }));
                items.push(Item::Text(TextItem { regular: true, align: Align::Right, valign: VAlign::Middle, max_width: Some(l - 110.0),
                    ..TextItem::new(x1, ry, 11.0, WORKSPACE_TEXT, value) }));
            }
            items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(l - 32.0),
                ..TextItem::new(x0, y + 195.0, 10.0, hex_color(INSPECTOR_PATH), d.id.as_str()) }));
            items.push(Item::Clip(None));
        }
        draw_rail_grip(items, grip, l, hit(grip, self.cursor.0, self.cursor.1));
    }
    /// `view` is the app's shared zoom/center, which the viewer's canvas
    /// applies exactly as the timeline's does.
    pub fn draw(&self, items: &mut Vec<Item>, vp: (f32, f32), slot: usize, view: (f32, (f32, f32)), show_ui: bool) {
        if !self.open {
            return;
        }
        if !show_ui {
            let cached = self.frames.iter()
                .find(|f| (f.pts - self.time).abs() <= 0.51 / self.fps)
                .filter(|_| self.show_rendered);
            let dims = cached.map_or(self.source_dims, |f| f.output);
            let source = if cached.is_some() { slot + 1 } else { self.source };
            let viewport = RectPx { x: 0.0, y: 0.0, w: vp.0, h: vp.1 };
            let picture = crate::app::App::zoomed(
                crate::app::App::fit_rect((dims[0], dims[1]), viewport), view.0, view.1);
            items.push(Item::Clip(Some(viewport)));
            items.push(Item::Rect(RectItem::new(viewport, [0.0, 0.0, 0.0, 1.0])));
            items.push(Item::Video { a: source, b: source, r: picture,
                uv: [0.0, 0.0, 1.0, 1.0], mode: VideoMode::Tex, p0: 0.0, p1: 0.0 });
            items.push(Item::Clip(None));
            return;
        }
        let r = Self::bounds(vp);
        let right = vp.0 - Self::right(vp);
        items.push(Item::Clip(Some(r)));
        items.push(Item::Rect(RectItem::new(r, [0.012, 0.012, 0.012, 1.0])));
        items.push(Item::Rect(RectItem::new(
            RectPx {
                x: right,
                y: 44.0,
                w: vp.0 - right,
                h: r.h,
            },
            [0.006, 0.006, 0.006, 1.0],
        )));
        self.draw_rail(items, vp);
        let v = Self::viewer(vp);
        let cached = self
            .frames
            .iter()
            .find(|f| (f.pts - self.time).abs() <= 0.51 / self.fps)
            .filter(|_| self.show_rendered);
        let title = if cached.is_some() {
            "RENDERED FRAME · Tab for source"
        } else {
            "SOURCE FRAME · Tab for render"
        };
        text(
            items,
            v.x,
            61.0,
            11.0,
            title,
            if cached.is_some() { BLUE } else { DIM },
            v.w,
        );
        text(items, v.x, 78.0, 10.0, &self.filename, DIM, v.w);
        if let Some(f) = cached {
            text(
                items,
                v.x,
                v.y + v.h + 10.0,
                10.0,
                &format!(
                    "{}×{} → {}×{} · through focused processor",
                    f.input[0], f.input[1], f.output[0], f.output[1]
                ),
                DIM,
                v.w,
            );
        }
        let dims = cached.map_or(self.source_dims, |f| f.output);
        let source = if cached.is_some() {
            slot + 1
        } else {
            self.source
        };
        let board = Canvas { r: v };
        board.ground(items);
        items.push(Item::Clip(Some(v)));
        let picture = board.picture((dims[0], dims[1]), view);
        Canvas::lift(items, picture);
        items.push(Item::Video {
            a: source,
            b: source,
            r: picture,
            uv: [0.0, 0.0, 1.0, 1.0],
            mode: VideoMode::Tex,
            p0: 0.0,
            p1: 0.0,
        });
        board.recess(items);
        items.push(Item::Clip(Some(r)));
        self.draw_progress(items, vp);
        self.draw_transport(items, vp);
        let t = Self::timeline(vp);
        items.push(Item::Rect(RectItem::new(
            RectPx {
                y: t.y + 12.0,
                h: 4.0,
                ..t
            },
            [0.08, 0.08, 0.08, 1.0],
        )));
        let at = |pts: f64| t.x + t.w * (pts / self.duration.max(0.001)).clamp(0.0, 1.0) as f32;
        for f in &self.frames {
            let x = at(f.pts);
            items.push(Item::Rect(RectItem::new(
                RectPx {
                    x: x - 2.0,
                    y: t.y + 6.0,
                    w: 4.0,
                    h: 16.0,
                },
                BLUE,
            )));
        }
        // The frame being rendered: amber, breathing while the worker runs,
        // with the temporal context it is decoding sweeping in from the
        // segment start behind it.
        if self.busy && let Some(pending) = self.pending {
            let phase = self.busy_since.elapsed().as_secs_f32() * 4.5;
            let pulse = 0.45 + 0.55 * (0.5 + 0.5 * phase.sin());
            let amber = hex_color(RENDERING);
            if let Some((done, total)) = self.progress && total > 0 {
                let from = self.segment_start.min(pending);
                let to = (from + done as f64 / self.fps).min(pending);
                items.push(Item::Rect(RectItem::new(
                    RectPx { x: at(from), y: t.y + 12.0, w: (at(to) - at(from)).max(0.0), h: 4.0 },
                    [amber[0], amber[1], amber[2], 0.45],
                )));
            }
            items.push(Item::Rect(RectItem::new(
                RectPx { x: at(pending) - 2.0, y: t.y + 6.0, w: 4.0, h: 16.0 },
                [amber[0], amber[1], amber[2], pulse],
            )));
        }
        let x = t.x + t.w * (self.time / self.duration.max(0.001)).clamp(0.0, 1.0) as f32;
        items.push(Item::Rect(RectItem::new(
            RectPx {
                x,
                y: t.y + 1.0,
                w: 2.0,
                h: 26.0,
            },
            INK,
        )));
        text(
            items,
            t.x,
            t.y + 32.0,
            11.0,
            &format!(
                "{:.3}s / {:.3}s · {} rendered frames",
                self.time,
                self.duration,
                self.frames.len()
            ),
            INK,
            t.w,
        );
        self.draw_footer(items, vp);
        button(
            items,
            RectPx {
                x: right + 14.0,
                y: 53.0,
                w: Self::right(vp) - 28.0,
                h: 29.0,
            },
            "Cycle processor presets",
            false,
        );
        let name = self
            .descriptors
            .get(self.selected)
            .map_or("Settings", |d| d.name.as_str());
        text(
            items,
            right + 14.0,
            94.0,
            11.0,
            name,
            INK,
            Self::right(vp) - 28.0,
        );
        if let Some(c) = self.capabilities.get(self.selected) {
            text(
                items,
                right + 14.0,
                110.0,
                9.0,
                &capability_label(*c),
                DIM,
                Self::right(vp) - 28.0,
            );
        }
        let clip = RectPx {
            x: right,
            y: 129.0,
            w: Self::right(vp),
            h: (Self::bottom(vp) - 151.0).max(1.0),
        };
        items.push(Item::Clip(Some(clip)));
        for (i, f) in self.fields.iter().enumerate() {
            let row = self.row(i, vp);
            if row.y + row.h < clip.y || row.y > clip.y + clip.h {
                continue;
            }
            text(items, row.x, row.y, 11.0, &f.title, INK, row.w);
            let value = &self.settings[&f.key];
            let editing = self.editing.as_ref().filter(|(n, _)| *n == i);
            let shown = if let Some((_, s)) = editing {
                format!("{s}▏")
            } else if matches!(f.kind, Kind::Unsupported) {
                "Unsupported input".into()
            } else if value.is_null() {
                "Automatic (click to edit)".into()
            } else {
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| display_value(value))
            };
            let boxr = RectPx {
                x: row.x,
                y: row.y + 17.0,
                w: row.w,
                h: 24.0,
            };
            items.push(Item::Rect(RectItem::new(boxr, [0.04, 0.04, 0.04, 1.0])));
            if let Kind::Number { min, max, .. } = f.kind {
                let amount =
                    ((value.as_f64().unwrap_or(min) - min) / (max - min)).clamp(0.0, 1.0) as f32;
                items.push(Item::Rect(RectItem::new(
                    RectPx {
                        w: row.w * amount,
                        h: 3.0,
                        y: boxr.y + 21.0,
                        ..boxr
                    },
                    BLUE,
                )));
            }
            text(
                items,
                row.x + 5.0,
                row.y + 20.0,
                11.0,
                &shown,
                if editing.is_some() { BLUE } else { INK },
                row.w - 10.0,
            );
        }
        items.push(Item::Clip(None));
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = &self.worker {
            worker.cancel();
        }
    }
}
fn display_value(v: &Value) -> String {
    if let Some(n) = v.as_f64() {
        format!("{n:.4}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    } else {
        v.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string())
    }
}
impl Panel {
    /// Five icon squares: previous rendered frame, a frame back, play /
    /// pause, a frame on, next rendered frame — the cut toolbar's step and
    /// keyframe glyphs, a triangle / two bars for the transport. The one
    /// under the pointer names itself beneath the row.
    /// Kept outside the video canvas so zoom/pan cannot cover render feedback.
    fn draw_progress(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let v = Self::viewer(vp);
        let r = RectPx { x: v.x, y: Self::transport_y(vp) - 98.0, w: v.w, h: 88.0 };
        let ink = hex_color(SOURCE_INK);
        let secondary = hex_color(SOURCE_SECONDARY);
        let accent = hex_color(CUT_HEADER_BLUE);
        let elapsed = if self.busy { self.busy_since.elapsed() } else { self.render_elapsed };
        let title = if self.busy {
            self.feedback.as_ref().map_or("Preparing frame preview", |f| f.name.as_str())
        } else {
            match self.render_result {
                Some(true) => "Frame preview ready",
                Some(false) => "Frame preview stopped",
                None => "Frame preview",
            }
        };
        let fraction = if self.busy {
            self.feedback.as_ref().and_then(StageFeedback::fraction)
        } else {
            self.render_result.filter(|ok| *ok).map(|_| 1.0)
        };
        let phase = if self.busy {
            self.feedback.as_ref().map_or_else(|| self.progress.map_or_else(
                || "Preparing input and processors".into(),
                |(done, total)| format!("Preparing input · {done}/{total} frames")), |f| {
                format!("{}/{} · {}", f.index + 1, f.count,
                    if f.progress.phase.trim().is_empty() { "Processing" } else { &f.progress.phase })
            })
        } else if self.render_result.is_some() {
            self.status.clone()
        } else {
            "Render frame to preview the enabled chain".into()
        };
        items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0,
            border_color: hex_color(SOURCE_BORDER),
            ..RectItem::new(r, hex_color(0x0c1015)) }));
        let heading = if self.busy || self.render_result.is_some() {
            format!("{title} · {:.1}s", elapsed.as_secs_f64())
        } else { title.into() };
        ui_label(items, r.x + 12.0, r.y + 16.0, 12.0, ink, &heading, Align::Left, (r.w - 128.0).max(1.0));
        let control = Self::progress_button(vp);
        let hover = hit(control, self.cursor.0, self.cursor.1);
        items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0,
            border_color: hex_color(if hover { 0x858585 } else { 0x606b79 }),
            ..RectItem::new(control, if self.busy { accent } else { hex_color(SOURCE_SURFACE) }) }));
        ui_label(items, control.x + control.w / 2.0, control.y + control.h / 2.0, 12.0,
            ink, if self.busy { "Cancel" } else { "Render" }, Align::Center, control.w - 12.0);
        ui_label(items, r.x + 12.0, r.y + 35.0, 10.0, secondary, &phase, Align::Left,
            (r.w - if fraction.is_some() { 116.0 } else { 24.0 }).max(1.0));
        if let Some(fraction) = fraction {
            let label = if self.busy { format!("{:.0}% of stage", (fraction * 100.0).floor()) } else { "100%".into() };
            ui_label(items, r.x + r.w - 12.0, r.y + 35.0, 10.0, ink, &label, Align::Right, 90.0);
        }
        let metrics = if self.busy {
            let now = Instant::now();
            let fps = self.feedback.as_ref().and_then(|f| f.rates.output_fps(now))
                .map_or_else(|| "Output FPS —".into(), |fps| format!("{fps:.2} output FPS"));
            let eta = self.feedback.as_ref().and_then(|f| f.rates.eta(&f.progress, now))
                .map_or_else(|| {
                    if self.feedback.as_ref().is_some_and(|f| f.progress.total.is_none_or(|n| n == 0)) {
                        "Stage ETA unavailable".into()
                    } else { "Stage ETA estimating…".into() }
                }, |seconds| format!("Stage ETA ~{}", remaining_label(seconds)));
            format!("{fps} · {eta}")
        } else if self.preview_count > 0 && self.render_elapsed.as_secs_f64() > 0.0 {
            format!("{:.2} preview FPS · {}", self.preview_count as f64 / self.render_elapsed.as_secs_f64(),
                if self.render_result == Some(true) { "Remaining 0s" } else { "Stopped" })
        } else { "Output FPS — · Stage ETA —".into() };
        ui_label(items, r.x + 12.0, r.y + 55.0, 10.0, secondary, &metrics, Align::Left, (r.w - 24.0).max(1.0));
        let track = RectPx { x: r.x + 12.0, y: r.y + 73.0, w: (r.w - 24.0).max(1.0), h: 6.0 };
        items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(track, hex_color(0x252b33)) }));
        let fill = if let Some(fraction) = fraction {
            Some(RectPx { w: track.w * fraction, ..track })
        } else if self.busy {
            let w = track.w * 0.25;
            let sweep = 0.5 + 0.5 * (elapsed.as_secs_f32() * 2.5).sin();
            Some(RectPx { x: track.x + (track.w - w) * sweep, w, ..track })
        } else { None };
        if let Some(fill) = fill && fill.w > 0.0 {
            items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(fill, accent) }));
        }
    }
    fn draw_transport(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        const NAMES: [&str; 5] = ["Prev rendered", "Frame back", "Play", "Frame on", "Next rendered"];
        for i in 0..5 {
            let b = Self::transport(vp, i);
            let hover = hit(b, self.cursor.0, self.cursor.1);
            items.push(Item::Rect(RectItem {
                radius: 5.0,
                border_w: 1.0,
                border_color: hex_color(if hover { 0x555555 } else { 0x303030 }),
                ..RectItem::new(b, hex_color(0x050506))
            }));
            let ink = hex_color(if hover { 0xffffff } else { 0xc8c8c8 });
            let (cx, cy) = (b.x + b.w / 2.0, b.y + b.h / 2.0);
            match i {
                0 => draw_icon(items, Icon::KeyBack, cx, cy, ink),
                1 => draw_icon(items, Icon::StepBack, cx, cy, ink),
                2 if self.playback => {
                    for x in [-5.0, 1.5] {
                        items.push(Item::Rect(RectItem { radius: 0.75,
                            ..RectItem::new(RectPx { x: cx + x, y: cy - 6.0, w: 3.5, h: 12.0 }, ink) }));
                    }
                }
                2 => items.push(Item::Triangle {
                    r: RectPx { x: cx - 4.0, y: cy - 6.5, w: 10.0, h: 13.0 },
                    color: ink,
                    left: false,
                    radius: 0.75,
                }),
                3 => draw_icon(items, Icon::StepFwd, cx, cy, ink),
                _ => draw_icon(items, Icon::KeyFwd, cx, cy, ink),
            }
            if hover {
                let name = if i == 2 && self.playback { "Pause" } else { NAMES[i] };
                ui_label(items, cx, b.y + b.h + 9.0, 10.0, WORKSPACE_DIM, name, Align::Center, 120.0);
            }
        }
    }
    /// The footer bar every page has, at the Timeline's height: the mode
    /// label, Render frame drawn as the cut footer's Split, the status line
    /// and the key hints; `App` adds Export and the build stamp.
    fn draw_footer(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let f = Self::footer(vp);
        let cy = f.y + f.h / 2.0;
        items.push(Item::Rect(RectItem::new(f, WORKSPACE_PANEL)));
        items.push(Item::Rect(RectItem::new(RectPx { h: 1.0, ..f }, WORKSPACE_RULE)));
        ui_label(items, 16.0, cy, 10.0, ACCENT, "PROCESS", Align::Left, 68.0);
        let b = Self::render_button(vp);
        draw_cut_footer_button(
            items,
            b,
            if self.busy { "Rendering…" } else { "Render frame" },
            "R",
            false,
            hit(b, self.cursor.0, self.cursor.1),
            false,
        );
        // Export and the build stamp are `App::draw_footer_end`'s, painted after.
        let export = footer_export_rect(vp, b.x + b.w + 8.0);
        let hint = "[ ] source   + duplicate   ← → frame   native 8-bit SDR";
        let hint_w = if vp.0 >= 1220.0 + status_build_width() { 400.0 } else { 0.0 };
        let x = b.x + b.w + 14.0;
        ui_label(items, x, cy, 10.0, WORKSPACE_DIM, self.status.as_str(), Align::Left,
            (export.x - 12.0 - x - hint_w).max(40.0));
        if hint_w > 0.0 {
            ui_label(items, export.x - 12.0, cy, 10.0, WORKSPACE_MUTED, hint, Align::Right, hint_w);
        }
    }
}
fn hit(r: RectPx, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}
fn text(items: &mut Vec<Item>, x: f32, y: f32, size: f32, s: &str, color: [f32; 4], width: f32) {
    let mut t = TextItem::new(x, y, size, color, s);
    t.max_width = Some(width.max(1.0));
    items.push(Item::Text(t));
}
fn button(items: &mut Vec<Item>, r: RectPx, label: &str, selected: bool) {
    items.push(Item::Rect(RectItem::new(
        r,
        if selected {
            BLUE
        } else {
            [0.055, 0.055, 0.055, 1.0]
        },
    )));
    text(items, r.x + 7.0, r.y + 6.0, 11.0, label, INK, r.w - 14.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fullscreen_preview_covers_header_and_omits_panel_controls() {
        let mut panel = Panel::new(Registry::default());
        panel.open = true;
        panel.source_dims = [720, 720];
        panel.source = 2;
        let mut items = Vec::new();
        panel.draw(&mut items, (1920.0, 1080.0), 3, (1.0, (0.5, 0.5)), false);
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[1], Item::Rect(r) if r.r.x == 0.0 && r.r.y == 0.0 && r.r.h == 1080.0));
        assert!(matches!(&items[2], Item::Video { a: 2, r, .. } if r.y == 0.0 && r.h == 1080.0));
        assert!(matches!(items.last(), Some(Item::Clip(None))));
    }

    #[test]
    fn schema_defaults_do_not_invent_optional_values() {
        let f = fields(
            &json!({"properties":{"path":{"type":"string"},"amount":{"type":"number","minimum":0,"maximum":1,"default":0.4},"mode":{"enum":["a","b"],"default":"b"},"mask":{"type":"object"}}}),
        );
        assert_eq!(defaults(&f), json!({"amount":0.4,"mode":"b"}));
        assert!(matches!(
            f.iter().find(|f| f.key == "mask").unwrap().kind,
            Kind::Unsupported
        ));
    }
    #[test]
    fn numeric_controls_clamp_and_keep_integer_types() {
        assert_eq!(numeric(1.0, 4.0, true, 0.5), json!(3));
        assert_eq!(numeric(-1.0, 1.0, false, 2.0), json!(1.0));
        assert_eq!(numeric(-1.0, 1.0, false, -2.0), json!(-1.0));
    }
    #[test]
    fn text_edit_can_restore_discovery_and_reject_invalid_numbers() {
        let s = json!({"properties":{"file":{"type":"string"},"n":{"type":"integer","minimum":0}},"required":["n"]});
        let f = fields(&s);
        assert_eq!(text_value(&f[0], &s, "").unwrap(), None);
        assert!(text_value(&f[1], &s, "1.2").is_err());
        assert!(text_value(&f[1], &s, "-1").is_err());
        assert_eq!(text_value(&f[1], &s, "8").unwrap(), Some(json!(8)));
    }
    #[test]
    fn invalidation_cancels_and_discards_old_frames() {
        let mut p = Panel::default();
        let c = p.cancel.clone();
        p.changed();
        assert!(c.check().is_err());
        assert_eq!(p.generation, 1);
        assert!(p.frames.is_empty());
    }
    #[test]
    fn preview_controls_cancel_without_losing_cached_frames_and_tab_compares() {
        let mut p = Panel::default();
        p.enter();
        p.time = 1.0;
        p.frames = vec![mark(1.0)];
        assert!(p.key(Key::Tab));
        assert!(!p.show_rendered);
        assert!(p.key(Key::Tab));
        assert!(p.show_rendered);
        assert!(p.seek.is_none(), "A/B does not seek or switch sources");
        let vp = (1280.0, 800.0);
        let button = Panel::progress_button(vp);
        p.click(button.x + 5.0, button.y + 5.0, vp);
        assert!(p.dirty.is_some(), "the panel's Render button requests a preview");
        p.busy = true;
        let token = p.cancel.clone();
        let generation = p.generation;
        p.click(button.x + 5.0, button.y + 5.0, vp);
        assert!(token.check().is_err());
        assert!(p.generation > generation, "late output must be rejected");
        assert!(!p.busy && p.dirty.is_none() && p.pending.is_none());
        assert_eq!(p.frames.len(), 1, "Cancel preserves completed previews");
        p.time = 2.0;
        p.key(Key::Tab);
        assert!(p.show_rendered, "no cached output must not toggle to an unrelated frame");
    }
    #[test]
    fn stage_feedback_never_invents_a_percentage() {
        let mut progress = Progress {
            phase: "Tiles".into(),
            completed: 1,
            total: None,
        };
        assert_eq!(
            stage_status("Example", &progress),
            "Example · Tiles · working"
        );
        progress.total = Some(0);
        assert!(!stage_status("Example", &progress).contains('%'));
        progress.total = Some(4);
        assert!(stage_status("Example", &progress).ends_with("25% of stage"));
        progress.completed = 9;
        assert!(stage_status("Example", &progress).ends_with("100% of stage"));
        progress.phase.clear();
        assert!(stage_status("Example", &progress).contains("Processing"));
    }
    #[test]
    fn throughput_and_eta_use_measured_work_and_reset_at_phase_boundaries() {
        let start = Instant::now();
        let mut clock = StageClock::new(start);
        let mut p = Progress { phase: "Tiles".into(), completed: 0, total: Some(10) };
        assert!(clock.sample(&p, 0, start).eta(&p, start).is_none());
        p.completed = 2;
        let now = start + Duration::from_secs(2);
        let rate = clock.sample(&p, 0, now);
        assert_eq!(rate.eta(&p, now), Some(8.0));
        assert!(rate.output_fps(now).is_none(), "tiles are not output frames");
        p.completed = 4;
        let now = start + Duration::from_secs(4);
        let rate = clock.sample(&p, 1, now);
        assert_eq!(rate.output_fps(now), Some(0.25));
        assert_eq!(rate.eta(&p, now), Some(6.0));
        // A batch restarting the same phase must not inherit the previous rate.
        p.completed = 0;
        let now = start + Duration::from_secs(5);
        assert!(clock.sample(&p, 1, now).eta(&p, now).is_none());
        p.completed = 1;
        let now = start + Duration::from_secs(6);
        assert_eq!(clock.sample(&p, 1, now).eta(&p, now), Some(9.0));
        p.phase = "Opaque device call".into();
        p.total = None;
        assert!(clock.sample(&p, 1, now).eta(&p, now).is_none());
        p.phase = "Next pass".into();
        p.total = Some(100);
        p.completed = 50;
        assert!(clock.sample(&p, 1, now).eta(&p, now).is_none(), "first observed count has no measured rate");
        p.completed = 100;
        assert_eq!(clock.sample(&p, 2, now).eta(&p, now), Some(0.0));
    }
    #[test]
    fn whole_clip_capability_is_visible_but_not_previewable() {
        struct WholeClip;
        impl abner_processor_api::Factory for WholeClip {
            fn descriptor(&self) -> Descriptor {
                BufferedFactory.descriptor()
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities {
                    role: Role::PostProcessor,
                    input: InputRequirement::WholeClip,
                }
            }
            fn create(&self, _: usize, _: usize, _: &Value) -> Result<Box<dyn Session>, String> {
                panic!("whole-clip processor must not be opened as a frame processor")
            }
        }
        let mut registry = Registry::default();
        registry.register(Box::new(WholeClip)).unwrap();
        let p = Panel::new(registry);
        assert!(!p.supported[0]);
        assert_eq!(
            capability_label(p.capabilities[0]),
            "Post-processor · needs entire clip"
        );
    }
    #[test]
    fn rail_cards_select_switch_and_fold_like_sources() {
        struct Tagged(&'static str, Vec<Category>);
        impl abner_processor_api::Factory for Tagged {
            fn categories(&self) -> Vec<Category> {
                self.1.clone()
            }
            fn descriptor(&self) -> Descriptor {
                Descriptor { id: self.0.into(), ..BufferedFactory.descriptor() }
            }
            fn create(&self, w: usize, h: usize, s: &Value) -> Result<Box<dyn Session>, String> {
                BufferedFactory.create(w, h, s)
            }
        }
        let mut r = Registry::default();
        r.register(Box::new(Tagged("example.a", vec![Category::Upscaling, Category::Denoising, Category::Upscaling]))).unwrap();
        r.register(Box::new(Tagged("example.b", vec![]))).unwrap();
        let mut p = Panel::new(r);
        p.enter();
        // Sorted, deduplicated, and labelled; none declared reads as such.
        assert_eq!(category_label(&p.categories[0]), "Upscaling · Denoising");
        assert_eq!(category_label(&p.categories[1]), "Uncategorized");
        let vp = (1280.0, 900.0);
        let card = p.card(1, vp);
        assert!(p.click(card.x + 60.0, card.y + 20.0, vp));
        assert_eq!(p.selected, 1);
        assert_eq!(p.enabled, [false, false], "a card click focuses, never enables");
        let check = p.card_switch(0, vp);
        p.click(check.x + 5.0, check.y + 5.0, vp);
        assert_eq!(p.enabled, [true, false]);
        assert_eq!(p.selected, 1, "the enable switch doesn't move focus");
        let (grip, inspector) = p.detail(vp);
        assert!(inspector.h > 0.0);
        p.click(grip.x + 20.0, grip.y + 5.0, vp);
        assert!(!p.show_detail && p.animating());
        p.move_selected(-1);
        assert_eq!(category_label(&p.categories[0]), "Uncategorized");
        assert_eq!(p.enabled, [false, true], "flags travel with their processor");
    }
    struct BufferedFactory;
    impl abner_processor_api::Factory for BufferedFactory {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "example.buffered".into(),
                name: "Buffered example".into(),
                settings_schema: json!({"properties":{"strength":{"type":"number","minimum":0.0,"maximum":1.0,"default":0.5},"mode":{"type":"string","enum":["a","b"],"default":"a"},"path":{"type":"string"}}}),
                presets: vec![],
            }
        }
        fn create(
            &self,
            w: usize,
            h: usize,
            _: &Value,
        ) -> Result<Box<dyn abner_processor_api::Session>, String> {
            Ok(Box::new(Buffered {
                pending: None,
                dimensions: [w, h],
                next: None,
                cancel: Cancellation::default(),
            }))
        }
    }
    struct Buffered {
        pending: Option<Frame>,
        dimensions: [usize; 2],
        next: Option<i64>,
        cancel: Cancellation,
    }
    impl abner_processor_api::Session for Buffered {
        fn process(&mut self, f: Frame) -> Result<Vec<Frame>, String> {
            self.cancel.check()?;
            if self.next.is_some_and(|pts| f.pts != pts) {
                return Err("discontinuous time".into());
            }
            self.next = Some(f.pts + f.duration);
            Ok(self.pending.replace(f).into_iter().collect())
        }
        fn finish(&mut self) -> Result<Vec<Frame>, String> {
            Ok(self.pending.take().into_iter().collect())
        }
        fn output_dimensions(&self) -> [usize; 2] {
            self.dimensions
        }
        fn cancellation(&self) -> Cancellation {
            self.cancel.clone()
        }
        fn progress(&self) -> abner_processor_api::Progress {
            abner_processor_api::Progress {
                phase: "example".into(),
                completed: 0,
                total: None,
            }
        }
    }
    fn registry() -> Registry {
        let mut r = Registry::default();
        r.register(Box::new(BufferedFactory)).unwrap();
        r
    }
    #[test]
    fn gui_controls_edit_runtime_settings_and_cancel_on_drag() {
        let mut panel = Panel::new(registry());
        panel.open = true;
        let vp = (1280.0, 900.0);
        let i = panel
            .fields
            .iter()
            .position(|f| f.key == "strength")
            .unwrap();
        let row = panel.row(i, vp);
        let old = panel.cancel.clone();
        assert!(panel.click(row.x + row.w * 0.75, row.y + 25.0, vp));
        assert_eq!(panel.settings["strength"], json!(0.75));
        assert!(old.check().is_err());
        assert!(panel.pointer(row.x + row.w, row.y + 25.0, vp));
        assert_eq!(panel.settings["strength"], json!(1.0));
        assert!(panel.release());
        let i = panel.fields.iter().position(|f| f.key == "mode").unwrap();
        let row = panel.row(i, vp);
        panel.click(row.x + 20.0, row.y + 25.0, vp);
        assert_eq!(panel.settings["mode"], json!("b"));
        let i = panel.fields.iter().position(|f| f.key == "path").unwrap();
        let row = panel.row(i, vp);
        panel.click(row.x + 20.0, row.y + 25.0, vp);
        assert!(panel.text_input("/tmp/example"));
        panel.key(Key::Enter);
        assert_eq!(panel.settings["path"], json!("/tmp/example"));
        panel.close();
        assert!(panel.cancel.check().is_err());
    }
    #[test]
    fn preview_drains_buffered_tail_and_matches_fractional_rate_source() {
        let dir = std::env::temp_dir().join(format!(
            "abner-preview-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let path = dir.join("synthetic.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x64:rate=24000/1001",
                "-frames:v",
                "6",
                // lossless H.264 (FFV1 is not a codec the FilmCraft player decodes)
                "-c:v",
                "libx264",
                "-qp",
                "0",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let info = crate::probe::probe(&path).unwrap();
        let r = Request {
            generation: 7,
            chain: vec![("example.buffered".into(), json!({}))],
            target: None,
            info: info.clone(),
            start: 0.0,
            end: None,
            cancel: Cancellation::default(),
            budget: DEFAULT_PREVIEW_BYTES,
        };
        let (tx, rx) = mpsc::sync_channel(FRAMES + 2);
        render(&registry(), &r, &tx, None, None).unwrap();
        let mut frames = vec![];
        while let Ok(Event::Frame(g, f)) = rx.try_recv() {
            assert_eq!(g, 7);
            assert_eq!(f.before, f.after);
            // Plugins get the source at its native size, never a proxy.
            assert_eq!(f.input, [info.width, info.height]);
            assert_eq!(f.before.len(), (info.width * info.height * 4) as usize);
            frames.push(f);
        }
        assert_eq!(frames.len(), 6);
        assert_eq!(frames[0].pts, 0.0);
        assert!(frames.windows(2).all(|f| f[1].pts > f[0].pts));
        // A budget too small for one native pair refuses, naming the config key.
        let starved = Request {
            generation: 8,
            chain: r.chain.clone(),
            target: None,
            info: info.clone(),
            start: 0.0,
            end: None,
            cancel: Cancellation::default(),
            budget: 1024,
        };
        let err = render(&registry(), &starved, &tx, None, None).unwrap_err();
        assert!(err.contains("preview_cache_mb"), "{err}");
        let limited = Request {
            generation: 9,
            chain: r.chain.clone(),
            target: None,
            info: info.clone(),
            start: 0.0,
            end: Some(0.1),
            cancel: Cancellation::default(),
            budget: DEFAULT_PREVIEW_BYTES,
        };
        render(&registry(), &limited, &tx, None, None).unwrap();
        let mut limited_count = 0;
        while let Ok(Event::Frame(_, f)) = rx.try_recv() {
            assert!(f.pts < 0.1);
            limited_count += 1;
        }
        assert_eq!(
            limited_count, 3,
            "a preview session must stop and drain at a timeline boundary"
        );
        // Two buffering stages must flush in order and publish ONLY the requested frame.
        let single = Request {
            generation: 12,
            chain: vec![
                ("example.buffered".into(), json!({})),
                ("example.buffered".into(), json!({})),
            ],
            target: Some(frames[2].pts),
            info: info.clone(),
            start: 0.0,
            end: None,
            cancel: Cancellation::default(),
            budget: DEFAULT_PREVIEW_BYTES,
        };
        render(&registry(), &single, &tx, None, None).unwrap();
        let mut selected = vec![];
        while let Ok(event) = rx.try_recv() {
            if let Event::Frame(_, f) = event {
                selected.push(f);
            }
        }
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].pts, frames[2].pts);
        assert_eq!(selected[0].before, selected[0].after);
        // A buffered stage must not start more work or flush after the target
        // has arrived. This models the expensive extra-batch regression.
        struct GuardFactory(Arc<std::sync::atomic::AtomicUsize>);
        struct GuardSession(Box<dyn Session>, Arc<std::sync::atomic::AtomicUsize>);
        impl abner_processor_api::Factory for GuardFactory {
            fn descriptor(&self) -> Descriptor { BufferedFactory.descriptor() }
            fn create(&self, w: usize, h: usize, settings: &Value) -> Result<Box<dyn Session>, String> {
                Ok(Box::new(GuardSession(BufferedFactory.create(w, h, settings)?, self.0.clone())))
            }
        }
        impl Session for GuardSession {
            fn process(&mut self, frame: Frame) -> Result<Vec<Frame>, String> {
                self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.0.process(frame)
            }
            fn finish(&mut self) -> Result<Vec<Frame>, String> { panic!("target already delivered; do not flush") }
            fn output_dimensions(&self) -> [usize; 2] { self.0.output_dimensions() }
            fn cancellation(&self) -> Cancellation { self.0.cancellation() }
            fn progress(&self) -> Progress { self.0.progress() }
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut guarded = Registry::default();
        guarded.register(Box::new(GuardFactory(calls.clone()))).unwrap();
        let first = Request {
            generation: 12, chain: r.chain.clone(), target: Some(0.0),
            info: info.clone(), start: 0.0, end: None, cancel: Cancellation::default(), budget: r.budget,
        };
        let (guard_tx, guard_rx) = mpsc::sync_channel(FRAMES + 2);
        render(&guarded, &first, &guard_tx, None, None).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        let delivered: Vec<_> = guard_rx.try_iter().filter_map(|event| match event {
            Event::Frame(g, f) => Some((g, f.pts)), _ => None,
        }).collect();
        assert_eq!(delivered, [(12, 0.0)]);
        // Already-cancelled requests must never publish output.
        r.cancel.cancel();
        assert!(render(&registry(), &r, &tx, None, None).is_err());
        assert!(rx.try_recv().is_err());
        // Late events from an old settings generation must never become uploads.
        let mut panel = Panel::default();
        panel.open = true;
        panel.generation = 8;
        panel.context = Some((info.path.clone(), 0.0f64.to_bits(), None));
        panel.worker = Some(Worker {
            active: Arc::new(Mutex::new(Vec::new())),
            mailbox: Arc::new((
                Mutex::new(Mailbox {
                    pending: None,
                    stop: false,
                }),
                Condvar::new(),
            )),
            rx,
            live: LiveProgress::default(),
        });
        tx.send(Event::Frame(7, frames.remove(0))).unwrap();
        let mut uploads = vec![];
        panel.tick(&info, 0.0, None, &mut uploads, 1);
        assert!(uploads.is_empty());
        assert!(panel.frames.is_empty());
        panel.frames = vec![mark(0.0), mark(0.1)];
        panel.tick(&info, 0.2, None, &mut uploads, 1);
        assert_eq!(panel.frames.len(), 2, "seeking must preserve valid marks");
        let generation = panel.generation;
        let live = panel.worker.as_ref().unwrap().live.clone();
        let feedback = |done, total| StageFeedback {
            name: "Example stage".into(), index: 1, count: 2,
            progress: Progress { phase: "Tiles".into(), completed: done, total },
            rates: StageClock::new(Instant::now()).rates,
        };
        panel.busy = true;
        *live.lock().unwrap() = Some((generation - 1, feedback(10, Some(10))));
        panel.tick(&info, 0.2, None, &mut uploads, 1);
        assert!(panel.feedback.is_none(), "cancelled jobs must not replace live progress");
        for done in 0..=50 {
            *live.lock().unwrap() = Some((generation, feedback(done, Some(100))));
        }
        // Input-channel updates cannot hide a newer in-flight tile update.
        tx.send(Event::Progress(generation, 1, 24)).unwrap();
        panel.tick(&info, 0.2, None, &mut uploads, 1);
        assert_eq!(panel.feedback.as_ref().unwrap().fraction(), Some(0.5));
        *live.lock().unwrap() = Some((generation, feedback(0, None)));
        panel.tick(&info, 0.2, None, &mut uploads, 1);
        assert!(panel.feedback.as_ref().unwrap().fraction().is_none());
        tx.send(Event::Done(generation, Err("example failure".into()))).unwrap();
        *live.lock().unwrap() = Some((generation, feedback(100, Some(100))));
        panel.tick(&info, 0.2, None, &mut uploads, 1);
        assert_eq!(panel.render_result, Some(false), "a failed preview is never 100% complete");
        assert!(!panel.busy);
        panel.request_preview();
        assert!(panel.feedback.is_none() && panel.render_result.is_none());
        panel.close();
        panel.enter();
        assert_eq!(
            panel.frames.len(),
            2,
            "changing workspace must preserve valid marks"
        );
    }
    struct Named(&'static str);
    impl abner_processor_api::Factory for Named {
        fn descriptor(&self) -> Descriptor {
            let mut d = BufferedFactory.descriptor();
            d.id = self.0.into();
            d
        }
        fn create(
            &self,
            w: usize,
            h: usize,
            s: &Value,
        ) -> Result<Box<dyn abner_processor_api::Session>, String> {
            BufferedFactory.create(w, h, s)
        }
    }
    fn mark(pts: f64) -> Preview {
        Preview {
            before: Arc::new(vec![0; 16]),
            after: Arc::new(vec![0; 16]),
            input: [2, 2],
            output: [2, 2],
            pts,
            elapsed: 1.0,
        }
    }
    #[test]
    fn ordered_chain_bypass_settings_and_frame_navigation() {
        let mut r = Registry::default();
        r.register(Box::new(Named("example.a"))).unwrap();
        r.register(Box::new(Named("example.b"))).unwrap();
        let mut p = Panel::new(r);
        // Plugins start bypassed; nothing runs until it is ticked.
        assert_eq!(p.enabled, [false, false]);
        assert!(p.chain().is_empty());
        p.enabled = vec![true, true];
        p.enter();
        p.settings["strength"] = json!(0.8);
        p.changed();
        p.select(1);
        assert_eq!(
            p.chain()
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["example.a", "example.b"]
        );
        p.enabled[0] = false;
        p.changed();
        assert_eq!(p.chain().len(), 1);
        p.move_selected(-1);
        assert_eq!(p.descriptors[0].id, "example.b");
        assert!(p.enabled[0]);
        assert!(!p.enabled[1]);
        p.select(1);
        assert_eq!(p.settings["strength"], json!(0.8));
        p.frames = vec![mark(1.0), mark(3.0), mark(7.0)];
        p.time = 3.0;
        p.jump(true);
        assert_eq!(p.take_seek(), Some(7.0));
        p.jump(false);
        assert_eq!(p.take_seek(), Some(1.0));
        p.time = 0.0;
        p.jump(false);
        assert_eq!(p.take_seek(), None);
        p.time = 8.0;
        p.jump(true);
        assert_eq!(p.take_seek(), None);
        let old = p.cancel.clone();
        p.changed();
        assert!(p.frames.is_empty());
        assert!(old.check().is_err());
        assert!(
            p.dirty.is_none(),
            "settings changes must not start expensive inference automatically"
        );
    }
    struct Arithmetic {
        multiply: u16,
        add: u16,
    }
    impl abner_processor_api::Session for Arithmetic {
        fn process(&mut self, mut f: Frame) -> Result<Vec<Frame>, String> {
            for p in &mut f.pixels {
                *p = *p * self.multiply + self.add;
            }
            Ok(vec![f])
        }
        fn finish(&mut self) -> Result<Vec<Frame>, String> {
            Ok(vec![])
        }
        fn output_dimensions(&self) -> [usize; 2] {
            [2, 2]
        }
        fn cancellation(&self) -> Cancellation {
            Cancellation::default()
        }
        fn progress(&self) -> abner_processor_api::Progress {
            abner_processor_api::Progress {
                phase: "test".into(),
                completed: 0,
                total: None,
            }
        }
    }
    #[test]
    fn chain_order_changes_pixels_and_cancellation_prevents_execution() {
        let make = || Frame {
            width: 2,
            height: 2,
            pixels: vec![10; 12],
            pts: 0,
            duration: 1,
            time_base: [1, 24],
        };
        let mut sessions: Vec<Box<dyn abner_processor_api::Session>> = vec![
            Box::new(Arithmetic {
                multiply: 2,
                add: 0,
            }),
            Box::new(Arithmetic {
                multiply: 1,
                add: 3,
            }),
        ];
        let cancel = Cancellation::default();
        assert_eq!(
            process_chain(&mut sessions, 0, vec![make()], &cancel, DEFAULT_PREVIEW_BYTES).unwrap()[0].pixels[0],
            23
        );
        sessions.swap(0, 1);
        assert_eq!(
            process_chain(&mut sessions, 0, vec![make()], &cancel, DEFAULT_PREVIEW_BYTES).unwrap()[0].pixels[0],
            26
        );
        cancel.cancel();
        assert!(process_chain(&mut sessions, 0, vec![make()], &cancel, DEFAULT_PREVIEW_BYTES).is_err());
    }
}
