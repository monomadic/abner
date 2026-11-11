//! Runtime-schema controls and cancellable, bounded display previews.
//! No processor identity or implementation belongs in this module.
use abner_processor_api::{VideoFrame, FramePixels, PixelFormat};
use crate::{
    app::{
        ACCENT, CUT_HEADER_BLUE, CUT_STATUS_H, Canvas, DETAIL_SLIDE_S, HEADER_H, INSPECTOR_BG,
        INSPECTOR_PATH, Icon, Key, MONO_ADV, RAIL_ACTIONS_H, RAIL_FOOTER_H, RAIL_GRIP_H, RAIL_HEAD_H,
        RAIL_INSPECTOR_H, SOURCE_BORDER, SOURCE_INK, SOURCE_RING,
        SOURCE_SUBTLE, SOURCE_SURFACE, WORKSPACE_DIM, WORKSPACE_MUTED,
        WORKSPACE_PANEL, WORKSPACE_RULE, WORKSPACE_TEXT, draw_cut_footer_button,
        draw_icon, draw_rail_grip, footer_export_rect, hex_color, mix, rail_full_width,
        status_build_width, ui_label, fmt_time, TRACK, FRAME_INK,
    },
    player::{Player, Rgba},
    probe::VideoInfo,
    render::{Align, Item, RectItem, RectPx, TextBg, TextItem, Upload, VAlign, VideoMode},
};
use abner_processor_api::{
    Cancellation, Capabilities, Category, Descriptor, Frame, InputRequirement, Progress, Rate,
    Registry, Role, ScaleOption, Session,
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
const CANVAS_HEADER_H: f32 = 36.0;
const DIM: [f32; 4] = [0.43, 0.43, 0.43, 1.0];
const BLUE: [f32; 4] = [0.04, 0.33, 0.7, 1.0];
/// Compact processor rows (Figma rail): 30px rows on a 3px gap, inside a
/// rounded group box with 10px padding, under the category chips.
const CARD_PITCH: f32 = 33.0;
const ROW_H: f32 = 30.0;
const GROUP_PAD: f32 = 10.0;
/// Category chips: 18px pills, wrapping on a 24px pitch from the rail's
/// 13px gutter; mono 10px labels, never narrower than ALL's 62px pill.
const CHIP_TOP: f32 = 67.0;
const CHIP_H: f32 = 18.0;
const CHIP_PITCH: f32 = 24.0;
const CHIP_PX: f32 = 10.0;
const CHIP_TRACK: f32 = 0.4;
/// The host's own setting in a processor's settings object: how much to
/// shrink the frame before the stage (1 = off; 2 and 4 pick the stage's 2x /
/// 4x model, declared through `Factory::scales`). Stripped before the plugin
/// sees the settings. Topaz's "Focus Fix", for any processor that upscales.
const DOWNSIZE_KEY: &str = "$downsize";
/// The plugin's view of a settings object and the host's downsize factor.
fn split_downsize(settings: &Value) -> (Value, usize) {
    let mut plain = settings.clone();
    let factor = plain
        .as_object_mut()
        .and_then(|m| m.remove(DOWNSIZE_KEY))
        .and_then(|v| v.as_u64())
        .map_or(1, |f| f.max(1) as usize);
    (plain, factor)
}
/// The Downsize input row for a processor declaring `scales`: Off plus one
/// choice per 2x / 4x model it offers. None when it offers neither.
fn downsize_field(scales: &[ScaleOption]) -> Option<Field> {
    let mut choices = vec![Choice {
        value: json!(1),
        title: Some("Off".into()),
        description: Some("The stage sees the frame at its native size.".into()),
    }];
    for (factor, label) in [(2usize, "1/2 · 2x model"), (4, "1/4 · 4x model")] {
        if scales.iter().any(|s| s.factor == factor) {
            choices.push(Choice {
                value: json!(factor),
                title: Some(label.into()),
                description: Some(format!(
                    "Shrink the frame to 1/{factor} with swscale's spline, run the {factor}x model, and land back at the source size."
                )),
            });
        }
    }
    (choices.len() > 1).then(|| Field {
        key: DOWNSIZE_KEY.into(),
        title: "Downsize input".into(),
        kind: Kind::Enum(choices),
        default: Some(json!(1)),
        required: false,
        description: Some(
            "Topaz's Focus Fix, for any upscaling processor: the frame goes in smaller so the model's upscale has to invent the detail, instead of preserving the blur. The Scale setting is overridden to match.".into(),
        ),
    })
}
/// The page's footer bar: the Timeline's height, since it carries the same
/// kind of button (Render frame, drawn as the cut footer's Split).
const FOOTER_H: f32 = CUT_STATUS_H;
/// A frame being rendered: the source palette's amber, pulsed while busy.
const RENDERING: u32 = 0xe6ac69;
/// Transport buttons: five icon squares on a 6px gap, centred under the viewer.
const TRANSPORT_W: f32 = 40.0;
const TRANSPORT_GAP: f32 = 6.0;
/// Figma Switch: a 27px thumb travel, settling in 180ms with ease-out.
const SWITCH_SLIDE_S: f32 = 0.18;
#[derive(Clone, Copy)]
struct SwitchSlide {
    from: f32,
    at: Instant,
}
impl SwitchSlide {
    fn position(self, on: bool, now: Instant) -> f32 {
        let t = (now.duration_since(self.at).as_secs_f32() / SWITCH_SLIDE_S).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.from + (f32::from(on) - self.from) * eased
    }
}

/// One choice of a selector: `enum` gives bare values, `oneOf` of
/// `{"const", "title", "description"}` names and explains each.
#[derive(Clone, Debug)]
struct Choice {
    value: Value,
    title: Option<String>,
    description: Option<String>,
}
#[derive(Clone, Debug)]
enum Kind {
    Number { min: f64, max: f64, integer: bool },
    Enum(Vec<Choice>),
    Bool,
    Text,
    Spatial { key:String, crop:bool },
    Unsupported,
}
#[derive(Clone, Debug)]
struct Field {
    key: String,
    title: String,
    kind: Kind,
    default: Option<Value>,
    required: bool,
    /// The schema's `description`: the setting's hover tooltip.
    description: Option<String>,
}
impl Field {
    fn choice(&self, value: &Value) -> Option<&Choice> {
        match &self.kind {
            Kind::Enum(choices) => choices.iter().find(|c| &c.value == value),
            _ => None,
        }
    }
    /// The tooltip for this setting at `value`: what the setting does, then
    /// what the current choice does. None when the schema explains neither.
    fn tooltip(&self, value: &Value) -> Option<String> {
        let choice = self.choice(value).and_then(|c| {
            c.description.as_ref().map(|d| match &c.title {
                Some(t) => format!("{t}: {d}"),
                None => format!("{}: {d}", display_value(&c.value)),
            })
        });
        match (&self.description, choice) {
            (Some(d), Some(c)) => Some(format!("{d}\n\n{c}")),
            (Some(d), None) => Some(d.clone()),
            (None, c) => c,
        }
    }
    /// The value as the editor shows it: a choice's title when it has one.
    fn shown(&self, value: &Value) -> String {
        self.choice(value)
            .and_then(|c| c.title.clone())
            .unwrap_or_else(|| value.as_str().map(str::to_owned).unwrap_or_else(|| display_value(value)))
    }
}
// Small coverage cells keep previews independent of the editor's full-size mask texture.
fn slider_value(row:RectPx)->RectPx {RectPx{x:row.x+row.w-40.,y:row.y,w:40.,h:20.}}
fn spatial_button(row:RectPx)->RectPx { RectPx{x:row.x,y:row.y+if row.w<122. {33.}else{21.},w:row.w.min(97.),h:62.} }
fn draw_mask_preview(items: &mut Vec<Item>, r: RectPx, dims: [usize;2], pixels: &[u8]) {
    let [w,h] = dims;
    if w == 0 || h == 0 || w.checked_mul(h) != Some(pixels.len()) { return; }
    let cols = w.min(64); let rows = h.min(36);
    for y in 0..rows { for x in 0..cols {
        let selected = (y*h/rows..(y+1)*h/rows).any(|py|
            pixels[py*w+x*w/cols..py*w+(x+1)*w/cols].iter().any(|&p|p!=0));
        if selected { items.push(Item::Rect(RectItem::new(RectPx {x:r.x+x as f32*r.w/cols as f32,y:r.y+y as f32*r.h/rows as f32,w:r.w/cols as f32,h:r.h/rows as f32},[0.,0.35,1.,0.65]))); }
    }}
}
pub(crate) fn draw_region_preview(items: &mut Vec<Item>, r: RectPx, dims: [usize;2], region: [usize;4]) {
    let [w,h] = dims; if w==0 || h==0 {return;}
    let [x,y,rw,rh] = region;
    let area = RectPx { x:r.x+x as f32/w as f32*r.w,y:r.y+y as f32/h as f32*r.h,w:rw as f32/w as f32*r.w,h:rh as f32/h as f32*r.h };
    for dim in [RectPx{x:r.x,y:r.y,w:r.w,h:area.y-r.y},RectPx{x:r.x,y:area.y+area.h,w:r.w,h:r.y+r.h-area.y-area.h},RectPx{x:r.x,y:area.y,w:area.x-r.x,h:area.h},RectPx{x:area.x+area.w,y:area.y,w:r.x+r.w-area.x-area.w,h:area.h}] {
        if dim.w>0. && dim.h>0. { items.push(Item::Rect(RectItem::new(dim,[0.,0.,0.,0.7]))); }
    }
    items.push(Item::Rect(RectItem {border_w:1.,border_color:[0.,0.6,1.,1.],..RectItem::new(area,[0.;4])}));
}
fn choices(s: &Value) -> Option<Vec<Choice>> {
    let text = |v: &Value| v.as_str().map(str::to_owned);
    if let Some(e) = s["enum"].as_array().filter(|v| !v.is_empty()) {
        return Some(e.iter().map(|v| Choice { value: v.clone(), title: None, description: None }).collect());
    }
    let one_of = s["oneOf"].as_array().filter(|v| !v.is_empty())?;
    one_of
        .iter()
        .map(|c| {
            c.get("const").map(|v| Choice {
                value: v.clone(),
                title: text(&c["title"]),
                description: text(&c["description"]),
            })
        })
        .collect()
}
fn fields(schema: &Value) -> Vec<Field> {
    schema["properties"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.iter())
        .map(|(key, s)| {
            let kind = if let Some(c) = choices(s) {
                Kind::Enum(c)
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
                description: s["description"].as_str().filter(|d| !d.trim().is_empty()).map(str::to_owned),
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
/// A probed rate as the rational the API speaks: integer and NTSC rates
/// exactly, anything else to a thousandth.
fn rational(fps: f64) -> Rate {
    for den in [1u32, 1001, 1000] {
        let num = (fps * den as f64).round();
        if num >= 1.0 && num < u32::MAX as f64 && (num / den as f64 - fps).abs() < 1e-6 * fps.max(1.0) {
            return [num as u32, den];
        }
    }
    [((fps * 1000.0).round() as u32).max(1), 1000]
}
fn rate_fps(r: Rate) -> f64 {
    r[0] as f64 / r[1] as f64
}
fn same_rate(a: Rate, b: Rate) -> bool {
    u64::from(a[0]) * u64::from(b[1]) == u64::from(b[0]) * u64::from(a[1])
}
/// The rate the chain delivers for a source at `fps`, each stage declaring
/// its output for the one before it.
fn chain_rate(registry: &Registry, chain: &[(String, Value)], fps: f64) -> Result<Rate, String> {
    let mut rate = rational(fps);
    for (id, settings) in chain {
        let factory = registry.get(id)?;
        rate = factory.output_rate(&split_downsize(settings).0, rate)?;
        if rate.contains(&0) || rate_fps(rate) > 1000.0 {
            return Err(format!("{}: unsupported output frame rate", factory.descriptor().name));
        }
    }
    Ok(rate)
}
/// A frame rate as the UI writes it: 60, 29.97, 23.976.
fn fps_label(fps: f64) -> String {
    format!("{fps:.3}").trim_end_matches('0').trim_end_matches('.').to_owned()
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
    inputs: crate::spatial::JobInputs,
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
#[cfg(test)]
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
    callbacks: Arc<AtomicU64>,
}
impl Session for ReportingSession {
    fn format_pairs(&self) -> Vec<abner_processor_api::FormatPair> { self.inner.format_pairs() }
    fn configure_format(&mut self, pair: abner_processor_api::FormatPair) -> Result<(), String> { self.inner.configure_format(pair) }
    fn process_video(&mut self, input: VideoFrame) -> Result<Vec<VideoFrame>, String> {
        let before = self.callbacks.load(Ordering::Relaxed);
        if before == 0 { (self.report)(Progress { phase: "Processing".into(), completed: 0, total: None }); }
        let result = self.inner.process_video(input);
        if let Ok(frames) = &result { self.frames.fetch_add(frames.len() as u64, Ordering::Relaxed); }
        if self.callbacks.load(Ordering::Relaxed) == before { (self.report)(self.inner.progress()); }
        result
    }
    fn finish_video(&mut self) -> Result<Vec<VideoFrame>, String> {
        let before = self.callbacks.load(Ordering::Relaxed);
        if before == 0 { (self.report)(Progress { phase: "Finishing buffered frames".into(), completed: 0, total: None }); }
        let result = self.inner.finish_video();
        if let Ok(frames) = &result { self.frames.fetch_add(frames.len() as u64, Ordering::Relaxed); }
        if self.callbacks.load(Ordering::Relaxed) == before { (self.report)(self.inner.progress()); }
        result
    }
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>, String> {
        let before = self.callbacks.load(Ordering::Relaxed);
        if before == 0 { (self.report)(Progress {
            phase: "Processing".into(),
            completed: 0,
            total: None,
        }); }
        let result = self.inner.process(input);
        if let Ok(frames) = &result { self.frames.fetch_add(frames.len() as u64, Ordering::Relaxed); }
        if self.callbacks.load(Ordering::Relaxed) == before { (self.report)(self.inner.progress()); }
        result
    }
    fn finish(&mut self) -> Result<Vec<Frame>, String> {
        let before = self.callbacks.load(Ordering::Relaxed);
        if before == 0 { (self.report)(Progress {
            phase: "Finishing buffered frames".into(),
            completed: 0,
            total: None,
        }); }
        let result = self.inner.finish();
        if let Ok(frames) = &result { self.frames.fetch_add(frames.len() as u64, Ordering::Relaxed); }
        if self.callbacks.load(Ordering::Relaxed) == before { (self.report)(self.inner.progress()); }
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
    /// Some stage was fed a downsized copy of its input.
    downsized: bool,
    /// The output frame's own time and length, seconds: a retimed chain
    /// puts new frames between the source's.
    pts: f64,
    duration: f64,
    /// The original paired with it: the latest source frame at or before
    /// `pts` (the same frame unless the chain retimes).
    source_pts: f64,
    elapsed: f64,
}
impl Worker {
    fn new(registry: Arc<Mutex<Registry>>) -> Self {
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
                    let registry = registry.lock().unwrap_or_else(|e| e.into_inner());
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
    r.inputs.validate_geometry(w,h)?;
    let mut sessions = Vec::new();
    let mut dims = [w as usize, h as usize];
    let downsized = r.chain.iter().any(|(_, s)| split_downsize(s).1 > 1);
    for (index, (id, raw)) in r.chain.iter().enumerate() {
        let factory = registry.get(id)?;
        if index>0 && r.inputs.bindings.get(id).is_some_and(|b|!b.is_empty()) {return Err("Spatial inputs currently require the first enabled processor; move this processor first".into());}
        let (mut settings, downsize) = split_downsize(raw);
        let mut stage_dims = dims;
        if downsize > 1 {
            if r.inputs.bindings.get(id).is_some_and(|b| !b.is_empty()) {
                return Err("Downsize input cannot be combined with spatial inputs".into());
            }
            let option = factory
                .scales()
                .into_iter()
                .find(|s| s.factor == downsize)
                .ok_or_else(|| format!("{}: no {downsize}x model to bring a 1/{downsize} input back to size", factory.descriptor().name))?;
            if let (Some(map), Some(over)) = (settings.as_object_mut(), option.settings.as_object()) {
                for (k, v) in over {
                    map.insert(k.clone(), v.clone());
                }
            }
            stage_dims = [(dims[0] / downsize).max(1), (dims[1] / downsize).max(1)];
        }
        let settings = &settings;
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
        let mut session = abner_processor_api::create_bound(factory,stage_dims[0],stage_dims[1],settings,r.inputs.bindings.get(id).cloned().unwrap_or_default())?;
        if let Some(target) = r.target {
            session.set_preview_target((target * 1e6).round() as i64, [1, 1_000_000]);
        }
        let callbacks = Arc::new(AtomicU64::new(0));
        let observed = callbacks.clone();
        let detailed = report.clone();
        session.set_progress_reporter(Arc::new(move |progress| {
            observed.fetch_add(1, Ordering::Relaxed);
            detailed(progress);
        }));
        let session: Box<dyn Session> = Box::new(ReportingSession {
            inner: session,
            report,
            frames,
            callbacks,
        });
        let session: Box<dyn Session> = if downsize > 1 {
            Box::new(Downsized { inner: Box::new(abner_processing::negotiation::Rgb16Boundary::new(session, r.budget)?), factor: downsize })
        } else {
            session
        };
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
    // Decoder formats are discovered on the first native frame, then fixed for
    // the segment. The display's RGBA8 format never becomes the processor input.
    let mut format_plan: Option<abner_processing::negotiation::FormatPlan> = None;
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
    let count = (r.budget / per_frame).max(1);
    let fps = r.info.fps;
    if !fps.is_finite() || fps <= 0.0 || fps > 1000.0 {
        return Err("Preview needs a valid frame rate".into());
    }
    // A retimed chain emits frames on its own grid; each is paired with the
    // latest original at or before it instead of the original it replaces.
    let retimed = !same_rate(chain_rate(registry, &r.chain, fps)?, rational(fps));
    let mut last_out: Option<i64> = None;
    let player = Player::spawn_native(&r.info.path, w, h, 8 * 1024 * 1024)
    .ok_or("Could not open preview decoder")?;
    // A frame preview is an isolated input, including for recurrent/batch models.
    // finish() lets implementations pad their own short input without replaying
    // the segment. Segment rendering retains the normal streaming contract.
    let decode_start = r.target.unwrap_or(r.start);
    player.seek(decode_start, true);
    let started = Instant::now();
    let mut originals = BTreeMap::new();
    let received = std::cell::Cell::new(0usize);
    let mut emit = |frames: Vec<VideoFrame>,
                    originals: &mut BTreeMap<i64, (Rgba, f64)>|
     -> Result<bool, String> {
        let mut delivered = false;
        for frame in frames {
            r.cancel.check()?;
            if r.target.is_none() && received.get() >= count { break; }
            frame.validate()?;
            if frame.width != ow || frame.height != oh || (!retimed && frame.time_base != [1, 1_000_000]) {
                return Err("Preview processor changed its declared geometry or time base".into());
            }
            // A retimer may count in its own time base (1/rate is natural);
            // everything here keys on the microseconds the inputs carry.
            let micros = |t: i64| {
                (t as i128 * frame.time_base[0] as i128 * 1_000_000 + frame.time_base[1] as i128 / 2)
                    .div_euclid(frame.time_base[1] as i128) as i64
            };
            let frame = VideoFrame {
                pts: micros(frame.pts),
                duration: micros(frame.pts + frame.duration) - micros(frame.pts),
                time_base: [1, 1_000_000],
                ..frame
            };
            let (before, source_pts) = if retimed {
                if last_out.is_some_and(|l| frame.pts <= l) {
                    return Err("Retimed output timestamps must strictly increase".into());
                }
                last_out = Some(frame.pts);
                let (&key, (before, source_pts)) = originals
                    .range(..=frame.pts)
                    .next_back()
                    .ok_or("Retimed output frame precedes every source frame")?;
                let paired = (before.clone(), *source_pts);
                // Outputs arrive in order: nothing older can pair again.
                *originals = originals.split_off(&key);
                paired
            } else {
                originals
                    .remove(&frame.pts)
                    .ok_or("Preview requires timestamp-preserving output frames (or a declared output rate)")?
            };
            let FramePixels::Rgba8(rgba) = frame.pixels else { return Err("preview format mismatch".into()); };
            // Targeted requests feed exactly one decoded source frame. Its
            // timestamp may differ from the requested grid (container timing,
            // rounded rates or a clamped end seek). Never discard its completed
            // output using the old history-replay timestamp filter.
            received.set(received.get() + 1);
            delivered = true;
            tx.send(Event::Frame(
                r.generation,
                Preview {
                    before,
                    after: rgba,
                    downsized,
                    input: [w, h],
                    output: [ow as u32, oh as u32],
                    // Keep an isolated, timestamp-preserving preview attached
                    // to the playhead position that requested it. source_pts
                    // still records the actual decoded frame for inspection.
                    pts: if retimed { frame.pts as f64 / 1e6 }
                        else { r.target.unwrap_or(frame.pts as f64 / 1e6) },
                    duration: frame.duration as f64 / 1e6,
                    source_pts,
                    elapsed: started.elapsed().as_secs_f64(),
                },
            ))
            .map_err(|_| "Preview closed")?;
        }
        Ok(delivered)
    };
    let read_frame = || -> Result<Option<(f64, crate::player::NativeFrame)>, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            r.cancel.check()?;
            if let Some(frame) = player.take_next_native() {
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
    let decode_count = if r.target.is_some() { 1 } else { count };
    for decoded in 0..decode_count {
        if r.target.is_some() {
            let _ = tx.try_send(Event::Progress(r.generation, decoded, decode_count));
        }
        let Some((source_pts, decoded_frame)) = next.take() else {
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
        if originals.len() >= count { return Err("Processor buffered too many frames for preview".into()); }
        let mut input = abner_processing::color::from_decoded(&decoded_frame, pts, duration, [1,1_000_000])?;
        // Before/after presentation is a separate display branch of the native frame.
        let before = abner_processing::color::rgba8(&input)?;
        originals.insert(pts, (before, source_pts));
        if !r.inputs.crops.is_empty() {
            let mut rgb = abner_processing::color::convert(input, PixelFormat::Rgb16)?.into_rgb16()?;
            crate::spatial::apply_crop(&mut rgb,&r.inputs.crops);
            input = rgb.into();
        }
        if format_plan.is_none() {
            format_plan = Some(abner_processing::negotiation::FormatPlan::new(&mut sessions, input.format, PixelFormat::Rgba8)?);
        }
        let delivered = emit(
            format_plan.as_ref().unwrap().process(
                &mut sessions,
                0,
                vec![input],
                &r.cancel,
                r.budget,
            )?,
            &mut originals,
        )?;
        // A targeted preview is complete once its output arrives. Dropping the
        // sessions here avoids starting another batch or flushing unwanted tails.
        if delivered && (r.target.is_some() || received.get() >= count) { return Ok(()); }
    }
    r.cancel.check()?;
    for i in 0..sessions.len() {
        let Some(plan) = &format_plan else { break; };
        let delivered = emit(
            plan.finish_stage(&mut sessions, i, &r.cancel, r.budget)?,
            &mut originals,
        )?;
        if delivered && (r.target.is_some() || received.get() >= count) { return Ok(()); }
    }
    if received.get() == 0 {
        return Err("No preview frames at this position".into());
    }
    if !retimed && !originals.is_empty() {
        return Err("Processor did not return all preview frames".into());
    }
    Ok(())
}
/// A stage fed a spline-shrunk copy of its input (the Downsize input row):
/// the frame goes in at 1/factor and the stage's own factor-x model brings
/// it back to size. Spatial inputs are refused upstream, so `process` is
/// the only way in.
struct Downsized {
    inner: Box<dyn Session>,
    factor: usize,
}
impl Session for Downsized {
    fn set_preview_target(&mut self, pts: i64, time_base: [u32; 2]) {
        self.inner.set_preview_target(pts, time_base);
    }
    fn set_progress_reporter(&mut self, reporter: abner_processor_api::ProgressReporter) {
        self.inner.set_progress_reporter(reporter);
    }
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>, String> {
        let (w, h) = ((input.width / self.factor).max(1), (input.height / self.factor).max(1));
        let pixels = crate::resample::spline_resize_rgb16(&input.pixels, input.width, input.height, w, h);
        self.inner.process(Frame { width: w, height: h, pixels, ..input })
    }
    fn finish(&mut self) -> Result<Vec<Frame>, String> {
        self.inner.finish()
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
#[cfg(test)]
pub(crate) use abner_processing::process_chain;

fn check_sdr(path: &std::path::Path) -> Result<(), String> {
    crate::probe::metadata(path, Duration::from_secs(5))
        .map_err(|e| e.to_string())?
        .check_sdr()
}


pub struct Panel {
    declarations: BTreeMap<String,Vec<abner_processor_api::SpatialInputDescriptor>>,
    inputs: crate::spatial::JobInputs,
    input_owner:usize,
    source_bindings: BTreeMap<usize,abner_processor_api::ChainBindings>,
    pub editor_request: Option<(String,String,bool)>,
    preset_dir: Option<std::path::PathBuf>,
    user_presets: BTreeMap<String,std::path::PathBuf>,
    preset_name: Option<String>,
    caret_at: Instant,
    /// Shared with the worker (locked for a whole render), so the page can
    /// ask the chain's output rate without waiting on one, and Export can
    /// hand the same registry to its own job.
    registry: Arc<Mutex<Registry>>,
    pub open: bool,
    descriptors: Vec<Descriptor>,
    selected: usize,
    saved: Vec<Value>,
    enabled: Vec<bool>,
    switch_slides: Vec<Option<SwitchSlide>>,
    supported: Vec<bool>,
    capabilities: Vec<Capabilities>,
    categories: Vec<Vec<Category>>,
    /// Each processor's declared integer upscales (`Factory::scales`).
    scales: Vec<Vec<ScaleOption>>,
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
    /// The category chip picked above the list; `None` is ALL.
    filter: Option<Category>,
    scrubbing: bool,
    filename: String,
    fields: Vec<Field>,
    settings: Value,
    worker: Option<Worker>,
    /// The chain's output rate, the cadence the page steps and scrubs at,
    /// and what it was computed for (source rate + chain).
    rate: f64,
    retimed: bool,
    rate_key: String,
    /// `processors.preview_cache_mb` in bytes: bounds each section's paired
    /// frames on the worker and the rendered frames kept across the timeline.
    budget: usize,
    generation: u64,
    cancel: Cancellation,
    dirty: Option<Instant>,
    render_segment: bool,
    context: Option<(std::path::PathBuf, u64, Option<u64>)>,
    frames: Vec<Preview>,
    /// The chain `frames` was rendered through (`chain_key`), and earlier
    /// chains' frames: switching a stage off and on again, or reselecting,
    /// brings its renders back. Shares `budget` with `frames`, oldest out.
    frames_key: String,
    stash: Vec<(String, Vec<Preview>)>,
    status: String,
    busy: bool,
    feedback: Option<StageFeedback>,
    render_result: Option<bool>,
    show_rendered: bool,
    /// `--processor-render`: a Render press waiting for the first tick.
    render_queued: bool,
    render_elapsed: Duration,
    preview_count: u64,
    changed_count: u64,
    /// The frame the running render was asked for, and the worker's context
    /// progress (frames decoded, frames to decode): the amber marks.
    pending: Option<f64>,
    progress: Option<(usize, usize)>,
    busy_since: Instant,
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
            input_owner:usize::MAX, declarations: BTreeMap::new(), inputs: Default::default(), source_bindings: BTreeMap::new(), editor_request: None,
            preset_dir: None,
            user_presets: BTreeMap::new(),
            preset_name: None,
            caret_at: Instant::now(),
            registry: Arc::new(Mutex::new(Registry::default())),
            open: false,
            descriptors: vec![],
            selected: 0,
            saved: vec![],
            enabled: vec![],
            switch_slides: vec![],
            supported: vec![],
            capabilities: vec![],
            categories: vec![],
            scales: vec![],
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
            filter: None,
            scrubbing: false,
            filename: String::new(),
            fields: vec![],
            settings: json!({}),
            worker: None,
            rate: 24.0,
            retimed: false,
            rate_key: String::new(),
            budget: DEFAULT_PREVIEW_BYTES,
            generation: 0,
            cancel: Cancellation::default(),
            dirty: None,
            render_segment: false,
            context: None,
            frames: vec![],
            frames_key: String::new(),
            stash: vec![],
            status: String::new(),
            busy: false,
            feedback: None,
            render_result: None,
            show_rendered: true,
            render_queued: false,
            render_elapsed: Duration::ZERO,
            preview_count: 0,
            changed_count: 0,
            pending: None,
            progress: None,
            busy_since: Instant::now(),
            playback: true,
            editing: None,
            scroll: 0.0,
            dragging: None,
            preset: usize::MAX,
            text_selected: false,
        }
    }
}
impl Panel {
    pub fn new(registry: Registry) -> Self {
        Self::with_presets(registry, crate::processor_presets::directory())
    }
    fn with_presets(registry: Registry, preset_dir: Option<std::path::PathBuf>) -> Self {
        let mut descriptors = registry.descriptors();
        let mut user_presets=BTreeMap::new();
        if let Some(root)=&preset_dir {for d in &mut descriptors {
            let (presets,errors)=crate::processor_presets::load(root,&d.id);
            for error in errors {log::warn!("Preset skipped: {error}");}
            for (path,preset) in presets {let id=format!("user:{}",path.display());user_presets.insert(id.clone(),path);d.presets.push(abner_processor_api::Preset{id,name:preset.name,settings:preset.settings});}
        }}
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
        let declarations=descriptors.iter().map(|d|(d.id.clone(),registry.get(&d.id).unwrap().spatial_inputs())).collect();
        let scales: Vec<Vec<ScaleOption>> = descriptors.iter().map(|d| registry.get(&d.id).unwrap().scales()).collect();
        let registry = Arc::new(Mutex::new(registry));
        let worker = (!descriptors.is_empty()).then(|| Worker::new(registry.clone()));
        let mut s = Self::default();
        s.preset_dir = preset_dir;
        s.user_presets = user_presets;
        s.declarations = declarations;
        s.registry = registry;
        s.descriptors = descriptors;
        s.saved = saved;
        s.enabled = enabled;
        s.switch_slides = vec![None; s.descriptors.len()];
        s.supported = supported;
        s.capabilities = capabilities;
        s.categories = categories;
        s.scales = scales;
        s.worker = worker;
        s.select(0);
        s.editor_request = None;
        s
    }
    fn select(&mut self, index: usize) {
        if self.descriptors.is_empty() {
            return;
        }
        self.dragging = None;
        self.selected = index;
        self.preset = usize::MAX;
        self.preset_name = None;
        self.fields = fields(&self.descriptors[index].settings_schema);
        // Keep discovery overrides available to presets/CLI, but out of the UI.
        let schema = &self.descriptors[index].settings_schema;
        self.fields.retain(|f| !matches!(schema["properties"][&f.key]["format"].as_str(), Some("file-path" | "directory-path")));
        if let Some(inputs)=self.declarations.get(&self.descriptors[index].id) {
            use abner_processor_api::SpatialInputKind as K;
            for d in inputs {
                let tools: &[bool]=match d.kind {K::Image|K::Region=>&[true],K::Mask=>&[false],K::MaskedImage=>&[true,false]};
                for &crop in tools {
                    self.fields.push(Field { key:format!("__spatial_{}_{}",d.key,crop), title:if d.kind==K::Region {"Search Area (Crop)".into()} else if !crop {"Removal (Mask)".into()} else {format!("{} (Crop)",d.title)},
                        kind:Kind::Spatial{key:d.key.clone(),crop},default:None,required:d.required,
                        description:Some(if d.kind==K::Region {"Processing region; keeps the complete input image"} else {"Edit this input without changing other inputs"}.into()) });
                }
            }
        }
        self.fields.sort_by_key(|f| match f.kind {
            Kind::Spatial { crop:false, .. } if f.required => 0,
            Kind::Spatial { .. } => 1,
            _ => 2,
        });
        if let Some(field) = downsize_field(self.scales.get(index).map_or(&[][..], Vec::as_slice)) {
            let position = self.fields.iter().take_while(|f| matches!(f.kind, Kind::Spatial { .. })).count();
            self.fields.insert(position, field);
        }
        self.settings = self.saved[index].clone();
        if self.fields.iter().any(|f| f.key == DOWNSIZE_KEY) && self.settings.get(DOWNSIZE_KEY).is_none() {
            self.settings[DOWNSIZE_KEY] = json!(1);
        }
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
        self.busy = false;
        self.feedback = None;
        self.render_result = None;
        self.preview_count = 0;
        self.changed_count = 0;
        self.render_elapsed = Duration::ZERO;
        self.dirty = None;
        if let Some(saved) = self.saved.get_mut(self.selected) {
            *saved = self.settings.clone();
        }
        let key = self.chain_key();
        let old = std::mem::replace(&mut self.frames_key, key.clone());
        let frames = std::mem::take(&mut self.frames);
        if old != key {
            if !frames.is_empty() {
                self.stash.retain(|(k, _)| *k != old);
                self.stash.push((old, frames));
            }
            if let Some(at) = self.stash.iter().position(|(k, _)| *k == key) {
                self.frames = self.stash.remove(at).1;
            }
        } else {
            self.frames = frames;
        }
        Self::trim_stash(&mut self.stash, Self::frame_bytes(&self.frames), self.budget);
        self.status = if self.frames.is_empty() {
            "Render the current frame to inspect the enabled chain.".into()
        } else {
            format!("{} rendered frames kept for this chain", self.frames.len())
        };
    }
    /// Everything a render's pixels depend on: the previewed chain with its
    /// settings, the spatial inputs and crops, and the source and its edits.
    fn chain_key(&self) -> String {
        let bindings: Vec<_> = self.inputs.bindings.iter().flat_map(|(stage, b)| {
            b.iter().map(move |(k, v)| (stage, k, v.dimensions, v.crop, v.revision,
                v.mask.as_ref().map(|m| Arc::as_ptr(m) as usize)))
        }).collect();
        format!("{}|{bindings:?}|{:?}|{:?}|{}",
            serde_json::to_string(&self.chain()).unwrap_or_default(),
            self.inputs.crops, self.context, self.input_owner)
    }
    fn frame_bytes(frames: &[Preview]) -> usize {
        frames.iter().map(|f| f.before.len() + f.after.len()).sum()
    }
    /// Drop the oldest stashed chains until they, the live frames and
    /// `extra` more bytes fit the preview budget.
    fn trim_stash(stash: &mut Vec<(String, Vec<Preview>)>, live: usize, budget: usize) {
        while !stash.is_empty()
            && live + stash.iter().map(|(_, f)| Self::frame_bytes(f)).sum::<usize>() > budget
        {
            stash.remove(0);
        }
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
            self.preset_name = None;
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
        if !self.open || (self.editing.is_none() && self.preset_name.is_none()) {
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
        self.caret_at = Instant::now();
        if self.open && let Some(value)=&mut self.preset_name {
            if self.text_selected {value.clear();self.text_selected=false;}
            if value.len()+text.len()<=160 {value.push_str(text);}
            return true;
        }
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
    fn pointer_time(&self, x: f32, timeline: RectPx) -> f64 {
        let t = ((x - timeline.x) / timeline.w).clamp(0.0, 1.0) as f64 * self.duration;
        (t * self.rate).floor().min((self.duration * self.rate).ceil() - 1.0).max(0.0) / self.rate
    }
    /// Markers select their exact cached timestamp; the rest of the track
    /// seeks on the output frame grid, never the container's keyframe grid.
    fn timeline_time(&self, x: f32, timeline: RectPx) -> f64 {
        self.frames.iter()
            .map(|f| (f, (timeline.x + timeline.w * (f.pts / self.duration.max(0.001)) as f32 - x).abs()))
            .filter(|(_, distance)| *distance <= 4.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or_else(|| self.pointer_time(x, timeline), |(f, _)| f.pts)
    }

    pub fn pointer(&mut self, x: f32, y: f32, vp: (f32, f32)) -> bool {
        if !self.open {
            return false;
        }
        self.cursor = (x, y);
        if self.scrubbing {
            let t = Self::timeline(vp);
            self.seek = Some(self.timeline_time(x, t));
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
    pub(crate) fn scrubbing(&self) -> bool { self.open && self.scrubbing }
    pub fn release(&mut self) -> bool {
        let scrub = std::mem::take(&mut self.scrubbing);
        self.dragging.take().is_some() || scrub
    }
    pub fn close(&mut self) {
        if self.open {
            self.toggle();
        }
    }
    /// Keep redraws hot only while work or a visible UI transition is running.
    pub fn animating(&self) -> bool {
        self.open && (self.busy
            || self.switch_slides.iter().flatten().any(|s| s.at.elapsed().as_secs_f32() < SWITCH_SLIDE_S)
            || self.detail_at
                .is_some_and(|t| t.elapsed().as_secs_f32() < DETAIL_SLIDE_S))
    }
    fn switch_position(&self, index: usize, now: Instant) -> f32 {
        self.switch_slides[index].map_or(f32::from(self.enabled[index]), |s| s.position(self.enabled[index], now))
    }
    fn toggle_enabled(&mut self, index: usize) {
        let now = Instant::now();
        let from = self.switch_position(index, now);
        self.enabled[index] = !self.enabled[index];
        self.switch_slides[index] = Some(SwitchSlide { from, at: now });
        self.changed();
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
    pub(crate) fn input_owner(&mut self,id:usize) {
        if self.input_owner!=id {
            self.source_bindings.insert(self.input_owner,std::mem::take(&mut self.inputs.bindings));
            self.inputs.bindings=self.source_bindings.remove(&id).unwrap_or_default();self.input_owner=id;self.changed();
        }
    }
    pub(crate) fn job_inputs(&self)->crate::spatial::JobInputs {self.inputs.clone()}
    pub(crate) fn set_crops(&mut self,crops:Vec<crate::spatial::ClipCrop>) {
        if self.inputs.crops!=crops {self.inputs.crops=crops;self.changed();}
    }
    pub(crate) fn focused_id(&self) -> Option<&str> {
        self.descriptors.get(self.selected).map(|d| d.id.as_str())
    }
    pub(crate) fn processing_region(&self, id: &str) -> Option<[usize; 4]> {
        self.declarations.get(id)?.iter()
            .filter(|d| d.kind == abner_processor_api::SpatialInputKind::Region)
            .find_map(|d| self.binding(id, &d.key)?.crop)
    }
    pub(crate) fn binding(&self,id:&str,key:&str)->Option<abner_processor_api::SpatialBinding> {self.inputs.bindings.get(id)?.get(key).cloned()}
    pub(crate) fn set_binding(&mut self,id:String,key:String,b:abner_processor_api::SpatialBinding) {
        self.inputs.bindings.entry(id).or_default().insert(key,b);self.changed();
    }
    pub fn source(&mut self, slot: usize, dims: [u32; 2], playing: bool) {

        self.source = slot;
        self.source_dims = dims;
        self.playback = playing;
    }
    pub fn take_seek(&mut self) -> Option<f64> {
        self.seek.take()
    }
    fn request_segment(&mut self) {
        self.request_preview();
        self.render_segment = true;
        self.seek = Some(self.segment_start);
    }
    /// Render once the first tick has bound the clip: binding it resets
    /// the panel, which would drop a request made before.
    pub(crate) fn render_frame(&mut self) {
        self.render_queued = true;
    }
    fn request_preview(&mut self) {
        self.render_segment = false;
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
        self.changed_count = 0;
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
    /// One frame back or forward on the chain's OUTPUT grid (the source's
    /// unless a stage retimes), clamped to the clip.
    fn step(&mut self, dir: i32) {
        let r = self.rate;
        let n = (self.time * r + 0.1).floor() + dir as f64;
        self.seek = Some((n / r).clamp(0.0, (self.duration - 1.0 / r).max(0.0)));
    }
    /// The source frame the original shows at output time `t`: the latest
    /// one at or before it.
    fn source_frame(&self, t: f64) -> f64 {
        (t * self.fps + 0.1 * self.fps / self.rate).floor().max(0.0) / self.fps
    }
    /// Where the app seeks its players for output time `t`: half a source
    /// frame before `source_frame`, so the exact seek ("first frame at or
    /// after") lands on it whatever the pts rounding.
    pub fn source_seek(&self, t: f64) -> f64 {
        (self.source_frame(t) - 0.5 / self.fps).max(0.0)
    }
    /// The rendered frame on screen at the playhead: the output frame whose
    /// span holds it (a tenth of a frame early, so grid rounding can't fall
    /// into the previous frame's span).
    fn shown(&self) -> Option<&Preview> {
        let lead = 0.1 / self.rate;
        self.frames
            .iter()
            .rev()
            .find(|f| f.pts - lead <= self.time && self.time < f.pts + f.duration - lead)
    }
    fn jump(&mut self, next: bool) {
        let tolerance = 0.5 / self.rate.max(1.0);
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
    /// Export includes every enabled stage, regardless of preview selection.
    pub fn modifies_video(&self) -> bool {
        self.enabled.iter().any(|enabled| *enabled)
    }

    /// Export runs every enabled stage in its displayed order, not just the
    /// chain up to the selected one the preview shows.
    pub(crate) fn export_setup(&self) -> (Arc<Mutex<Registry>>, Vec<(String, Value)>) {
        let chain = self.descriptors.iter().enumerate().filter(|(i, _)| self.enabled[*i])
            .map(|(i,d)| (d.id.clone(), if i == self.selected { self.settings.clone() } else { self.saved[i].clone() })).collect();
        (self.registry.clone(), chain)
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
    /// Move the focused processor one visible row up or down the chain;
    /// rows a category filter hides keep their order relative to each other.
    fn move_selected(&mut self, delta: isize) {
        if self.descriptors.is_empty() {
            return;
        }
        let visible = self.visible();
        let Some(at) = visible.iter().position(|i| *i == self.selected) else { return };
        let to = visible[at.saturating_add_signed(delta).min(visible.len() - 1)];
        if to == self.selected {
            return;
        }
        self.saved[self.selected] = self.settings.clone();
        fn shift<T>(v: &mut Vec<T>, from: usize, to: usize) {
            let item = v.remove(from);
            v.insert(to, item);
        }
        let from = self.selected;
        shift(&mut self.descriptors, from, to);
        shift(&mut self.saved, from, to);
        shift(&mut self.enabled, from, to);
        shift(&mut self.switch_slides, from, to);
        shift(&mut self.supported, from, to);
        shift(&mut self.capabilities, from, to);
        shift(&mut self.categories, from, to);
        self.select(to);
    }
    /// Descriptor indices the category filter shows, in chain order.
    fn visible(&self) -> Vec<usize> {
        (0..self.descriptors.len())
            .filter(|i| self.filter.as_ref().is_none_or(|f| self.categories[*i].contains(f)))
            .collect()
    }
    /// Every category some processor declares, in the API's order.
    fn chip_categories(&self) -> Vec<Category> {
        let mut all: Vec<Category> = self.categories.iter().flatten().cloned().collect();
        all.sort();
        all.dedup();
        all
    }
    /// The chip row (ALL, then each category), wrapped to the rail's width:
    /// one geometry for drawing and hit testing.
    fn chips(&self, vp: (f32, f32)) -> Vec<(RectPx, Option<Category>, String)> {
        if self.descriptors.is_empty() {
            return Vec::new();
        }
        let (x0, x1) = (13.0, Self::left(vp) - 13.0);
        let (mut x, mut y) = (x0, HEADER_H + CHIP_TOP);
        std::iter::once(None)
            .chain(self.chip_categories().into_iter().map(Some))
            .map(|c| {
                let label = c.as_ref().map_or("ALL".to_string(), |c| c.label().to_uppercase());
                let n = label.chars().count() as f32;
                let w = (n * (CHIP_PX * MONO_ADV + CHIP_TRACK) + 20.0).max(62.0).min(x1 - x0);
                if x > x0 && x + w > x1 {
                    x = x0;
                    y += CHIP_PITCH;
                }
                let r = RectPx { x, y, w, h: CHIP_H };
                x += w + 2.0;
                (r, c, label)
            })
            .collect()
    }
    fn set_filter(&mut self, filter: Option<Category>) {
        self.filter = filter;
        self.list_scroll = 0.0;
        if let Some(&first) = self.visible().first()
            && !self.visible().contains(&self.selected)
        {
            self.select(first);
        }
    }
    pub fn key(&mut self, key: Key) -> bool {
        self.caret_at = Instant::now();
        if self.open && self.preset_name.is_some() {
            match key {
                Key::Escape=>self.preset_name=None,
                Key::Enter=>self.save_preset(),
                Key::Backspace=>{let value=self.preset_name.as_mut().unwrap();if self.text_selected{value.clear();self.text_selected=false;}else{value.pop();}},
                Key::Char(c)=>{self.text_input(&c.to_string());},
                Key::Space=>{self.text_input(" ");},
                _=>{}
            }
            return true;
        }
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
                if self.shown().is_some() {
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
            Key::Char('R') => {
                self.request_segment();
                true
            }
            Key::Char('r') => {
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
    pub(crate) fn segment_button(vp: (f32, f32)) -> RectPx {
        RectPx { x: 230.0, y: Self::bottom(vp) + 6.0, w: 166.0, h: 26.0 }
    }
    /// The Sources rail's width, so switching modes doesn't move the edge.
    fn left(vp: (f32, f32)) -> f32 {
        rail_full_width(vp.0)
    }
    /// The rounded box the rows sit in starts 14px under the last chip row.
    fn group_top(&self, vp: (f32, f32)) -> f32 {
        self.chips(vp).last().map_or(HEADER_H + RAIL_HEAD_H, |(r, ..)| r.y + r.h + 14.0)
    }
    fn list_top(&self, vp: (f32, f32)) -> f32 {
        self.group_top(vp) + GROUP_PAD
    }
    /// The grip strip and the visible part of the focused inspector, as
    /// the Sources rail lays them out: above the Move up / down row, and
    /// only when the list keeps room for one card and its footer line.
    fn detail(&self, vp: (f32, f32)) -> (RectPx, RectPx) {
        let l = Self::left(vp);
        let bottom = Self::bottom(vp) - RAIL_ACTIONS_H;
        let none = RectPx { x: 0.0, y: bottom, w: l, h: 0.0 };
        let needed = CARD_PITCH + RAIL_FOOTER_H + RAIL_GRIP_H + RAIL_INSPECTOR_H;
        if self.descriptors.is_empty() || bottom - self.list_top(vp) < needed {
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
        let y = self.list_top(vp);
        let end = if grip.h > 0.0 { grip.y } else { Self::bottom(vp) - RAIL_ACTIONS_H };
        RectPx { x: 0.0, y, w: Self::left(vp), h: (end - RAIL_FOOTER_H - GROUP_PAD - y).max(CARD_PITCH) }
    }
    /// Processor `i`'s row; a row the filter hides has no area.
    fn card(&self, i: usize, vp: (f32, f32)) -> RectPx {
        let Some(at) = self.visible().iter().position(|v| *v == i) else {
            return RectPx { x: 0.0, y: -1e4, w: 0.0, h: 0.0 };
        };
        RectPx {
            x: 13.0 + GROUP_PAD,
            y: self.list_top(vp) + at as f32 * CARD_PITCH - self.list_scroll,
            w: Self::left(vp) - 2.0 * (13.0 + GROUP_PAD),
            h: ROW_H,
        }
    }
    /// The row's pill switch: shared by painting and hit testing.
    fn card_switch(&self, i: usize, vp: (f32, f32)) -> RectPx {
        let r = self.card(i, vp);
        RectPx { x: r.x + r.w - 46.0, y: r.y + (r.h - 18.0) / 2.0, w: 34.0, h: 18.0 }
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
            x: l,
            y: HEADER_H + CANVAS_HEADER_H,
            w: (vp.0 - l - r).max(1.0),
            h: (Self::transport_y(vp) - 20.0 - HEADER_H - CANVAS_HEADER_H).max(1.0),
        }
    }
    /// The Source | Rendered switch at the viewer's top-right, level with
    /// the format summary. Tab flips the same flag; the zoom is shared, so both sides
    /// show the same region of the picture.
    fn compare_switch(vp: (f32, f32)) -> RectPx {
        let v = Self::viewer(vp);
        RectPx { x: v.x + v.w - 12.0 - 144.0, y: HEADER_H + 6.0, w: 144.0, h: 24.0 }
    }
    fn compare_half(vp: (f32, f32), rendered: bool) -> RectPx {
        let s = Self::compare_switch(vp);
        RectPx { x: s.x + if rendered { s.w / 2.0 } else { 0.0 }, w: s.w / 2.0, ..s }
    }
    fn draw_compare_switch(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let available = self.shown().is_some();
        let s = Self::compare_switch(vp);
        items.push(Item::Rect(RectItem { radius: 6.0, border_w: 1.0,
            border_color: hex_color(0x303235), ..RectItem::new(s, hex_color(0x1a1a1a)) }));
        for (rendered, label) in [(false, "Source"), (true, "Rendered")] {
            let h = Self::compare_half(vp, rendered);
            let on = available && self.show_rendered == rendered;
            if on {
                let inset = RectPx { x: h.x + 2.0, y: h.y + 2.0, w: h.w - 4.0, h: h.h - 4.0 };
                items.push(Item::Rect(RectItem { radius: 4.0, ..RectItem::new(inset, hex_color(0x303235)) }));
            }
            let ink = if !available { 0x595959 } else if on { 0xd1d1d1 } else if hit(h, self.cursor.0, self.cursor.1) { 0xffffff } else { 0x919191 };
            ui_label(items, h.x + h.w / 2.0, h.y + h.h / 2.0, 10.0, hex_color(ink), label, Align::Center, h.w - 8.0);
        }
    }
    fn progress_panel(vp: (f32, f32)) -> RectPx {
        let (l, r) = (Self::left(vp), Self::right(vp));
        RectPx { x: l + 16.0, y: Self::bottom(vp) - 79.0,
            w: (vp.0 - l - r - 32.0).max(1.0), h: 67.0 }
    }
    fn progress_button(vp: (f32, f32)) -> RectPx {
        let r = Self::progress_panel(vp);
        RectPx { x: r.x + r.w - 103.0, y: r.y + 8.0, w: 91.0, h: 26.0 }
    }
    fn timeline(vp: (f32, f32)) -> RectPx {
        let last = Self::transport(vp, 4);
        let x = last.x + last.w + 16.0;
        RectPx { x, y: Self::transport_y(vp) + 1.0,
            w: (vp.0 - Self::right(vp) - 16.0 - x).max(1.0), h: 28.0 }
    }
    fn transport_y(vp: (f32, f32)) -> f32 {
        Self::bottom(vp) - 145.0
    }
    fn transport(vp: (f32, f32), i: usize) -> RectPx {
        let l = Self::left(vp);
        let span = vp.0 - l - Self::right(vp) - 32.0;
        // Keep room for the seekbar even at the minimum window width.
        let w = ((span * 0.40 - 4.0 * TRANSPORT_GAP) / 5.0).clamp(20.0, TRANSPORT_W);
        RectPx { x: l + 16.0 + i as f32 * (w + TRANSPORT_GAP),
            y: Self::transport_y(vp), w, h: 30.0 }
    }
    fn settings_switch(vp: (f32, f32)) -> RectPx {
        RectPx { x: vp.0 - 78.0, y: HEADER_H + 20.0, w: 61.0, h: 27.0 }
    }
    /// Preset controls use the same bounds for drawing and pointer input.
    fn preset_buttons(vp: (f32, f32)) -> [RectPx; 5] {
        let x = vp.0 - Self::right(vp) + 16.;
        let y = HEADER_H + 100.;
        let w = Self::right(vp) - 32.;
        let name = (w - 128.).max(40.);
        let widths = [name, 18., 18., 34., 42.];
        let mut at = x;
        std::array::from_fn(|i| {
            let r = RectPx {
                x: at,
                y,
                w: widths[i],
                h: 23.,
            };
            at += widths[i] + 4.;
            r
        })
    }
    fn begin_save_preset(&mut self) {
        if let Some((i, value)) = &self.editing {
            match text_value(
                &self.fields[*i],
                &self.descriptors[self.selected].settings_schema,
                value,
            ) {
                Ok(Some(value)) => self.settings[&self.fields[*i].key] = value,
                Ok(None) => {
                    self.settings
                        .as_object_mut()
                        .unwrap()
                        .remove(&self.fields[*i].key);
                }
                Err(e) => {
                    self.status = e;
                    return;
                }
            }
            self.changed();
        }
        let d = &self.descriptors[self.selected];
        self.editing = None;
        self.preset_name = Some(
            d.presets
                .get(self.preset)
                .filter(|p| self.user_presets.contains_key(&p.id))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| {
                    let mut n = 1;
                    loop {
                        let name = format!("Custom {n}");
                        if !d.presets.iter().any(|p| p.name == name) {
                            break name;
                        }
                        n += 1;
                    }
                }),
        );
        self.text_selected = false;
        self.status = "Name the preset, then press Enter or Save. Escape cancels.".into();
    }
    fn save_preset(&mut self) {
        let Some(name) = self.preset_name.clone() else {
            self.begin_save_preset();
            return;
        };
        let Some(root) = &self.preset_dir else {
            self.status = "Config directory unavailable; preset was not saved.".into();
            return;
        };
        let d = &mut self.descriptors[self.selected];
        match crate::processor_presets::save(root, &d.id, &name, &self.settings) {
            Ok(path) => {
                let id = format!("user:{}", path.display());
                let preset = abner_processor_api::Preset {
                    id: id.clone(),
                    name: name.trim().into(),
                    settings: self.settings.clone(),
                };
                self.preset = if let Some(i) = d.presets.iter().position(|p| p.id == id) {
                    d.presets[i] = preset;
                    i
                } else {
                    d.presets.push(preset);
                    d.presets.len() - 1
                };
                self.user_presets.insert(id, path);
                self.preset_name = None;
                self.status = format!("Saved preset {}", name.trim());
            }
            Err(e) => self.status = format!("Could not save preset: {e}"),
        }
    }
    fn delete_preset(&mut self) {
        let d = &mut self.descriptors[self.selected];
        let Some(p) = d.presets.get(self.preset) else {
            return;
        };
        let Some(path) = self.user_presets.get(&p.id) else {
            self.status = "Built-in presets cannot be deleted.".into();
            return;
        };
        let Some(root) = &self.preset_dir else {
            return;
        };
        match crate::processor_presets::delete(root, &d.id, path) {
            Ok(()) => {
                let p = d.presets.remove(self.preset);
                self.user_presets.remove(&p.id);
                self.preset = if d.presets.is_empty() {
                    usize::MAX
                } else {
                    self.preset.min(d.presets.len() - 1)
                };
                self.preset_name = None;
                self.apply_preset();
                self.status = format!("Deleted preset {}", p.name);
            }
            Err(e) => self.status = format!("Could not delete preset: {e}"),
        }
    }
    fn apply_preset(&mut self) {
        self.settings = defaults(&self.fields);
        if let Some(p) = self.descriptors[self.selected].presets.get(self.preset)
            && let Some(map) = p.settings.as_object()
        {
            for (k, v) in map {
                self.settings[k] = v.clone();
            }
        }
        self.preset_name = None;
        self.changed();
    }
    fn draw_settings(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let x = vp.0 - Self::right(vp) + 16.0;
        let w = Self::right(vp) - 32.0;
        let top = HEADER_H;
        let ink = hex_color(0xe6e6e6);
        let muted = hex_color(0xa3a5a8);
        let dim = hex_color(0x777b82);
        let blue = hex_color(0x007be5);
        let caret = self.caret_at.elapsed().as_millis() % 1000 < 500;
        text(items, x, top + 20.0, 9.0, "PROCESSOR SETTINGS", dim, w - 70.0);
        let Some(d) = self.descriptors.get(self.selected) else { return; };
        text(items, x, top + 40.0, 15.0, &d.name, ink, w);
        text(items, x, top + 67.0, 10.0,
            &format!("{} · {}", category_label(&self.categories[self.selected]), input_label(self.capabilities[self.selected])), muted, w);
        let switch = Self::settings_switch(vp);
        draw_processor_switch(items, switch, self.enabled[self.selected], self.switch_position(self.selected, Instant::now()));
        text(items, x, top + 88.0, 9.0, "PRESET", dim, w);
        let name=self.preset_name.as_deref();
        let preset = name.unwrap_or_else(||d.presets.get(self.preset).map_or("Default", |p| p.name.as_str()));
        let custom=d.presets.get(self.preset).is_some_and(|p|self.user_presets.contains_key(&p.id));
        for (index,(r, label)) in Self::preset_buttons(vp).into_iter().zip([preset, "‹", "›", "Save", "Delete"]).enumerate() {
            items.push(Item::Rect(RectItem { radius: 6.0, border_w: 1.0, border_color: hex_color(0x303234), ..RectItem::new(r, hex_color(0x0a0a0a)) }));
            ui_label(items, r.x + r.w / 2.0, r.y + r.h / 2.0, 10.0, if index==4&&!custom {dim} else {muted}, label, Align::Center, r.w - 4.0);
            if index == 0 && name.is_some() { set_caret(items, caret); }
        }
        items.push(Item::Rect(RectItem::new(RectPx { x, y: top + 136.0, w, h: 1.0 }, hex_color(0x303234))));
        let clip = RectPx { x: x - 16.0, y: top + 158.0, w: Self::right(vp), h: (Self::bottom(vp) - top - 158.0).max(1.0) };
        items.push(Item::Clip(Some(clip)));
        for (i, f) in self.fields.iter().enumerate() {
            let row = self.row(i, vp);
            if row.y + row.h < clip.y || row.y > clip.y + clip.h { continue; }
            if matches!(f.kind,Kind::Spatial{..}) && row.w<122. {
                let (label,kind)=f.title.rsplit_once(" (").unwrap_or((&f.title,""));
                text(items,row.x,row.y,11.,label,ink,row.w);
                text(items,row.x,row.y+12.,11.,&format!("({kind}"),ink,row.w);
            } else if !matches!(f.kind,Kind::Number{..}|Kind::Bool) { text(items, row.x, row.y, 11.0, &f.title, ink, row.w); }
            let value = &self.settings[&f.key];
            let editing = self.editing.as_ref().filter(|(n, _)| *n == i);
            let shown = if let Kind::Spatial{key,crop}=&f.kind {
                let configured=self.binding(&self.descriptors[self.selected].id,key).is_some_and(|b| if *crop {b.crop.is_some()}else{b.mask.is_some()});
                if configured {"Edit…".into()} else {"Choose…".into()}
            } else { editing.map_or_else(|| if value.is_null() { "Automatic".into() } else { f.shown(value) }, |(_, s)| s.clone()) };
            if let Kind::Number { min, max, .. } = f.kind {
                let value_rect=slider_value(row);
                let hovered=hit(row,self.cursor.0,self.cursor.1);
                let focused=editing.is_some();
                items.push(Item::Text(TextItem{valign:VAlign::Middle,max_width:Some((row.w-48.).max(1.)),..TextItem::new(row.x,row.y+10.,11.,ink,&f.title)}));
                items.push(Item::Rect(RectItem{radius:4.,border_w:if focused||!hovered{1.}else{0.},border_color:if focused{blue}else{hex_color(0x4e4e4e)},
                    ..RectItem::new(value_rect,hex_color(if focused||hovered{0x1a1a1a}else{0x0a0a0a}))}));
                ui_label(items,value_rect.x+value_rect.w/2.,row.y+10.,10.,ink,&shown,Align::Center,value_rect.w-4.);
                if focused {set_caret(items,caret);}
                let amount=((value.as_f64().unwrap_or(min)-min)/(max-min)).clamp(0.,1.) as f32;
                let track=RectPx{x:row.x,y:row.y+26.,w:row.w,h:4.};
                items.push(Item::Rect(RectItem{radius:2.,..RectItem::new(track,hex_color(0x303234))}));
                let cx=track.x+track.w*amount;
                if hovered||focused||self.dragging==Some(i) {
                    items.push(Item::Rect(RectItem{radius:10.,..RectItem::new(RectPx{x:cx-10.,y:row.y+18.,w:20.,h:20.},blue)}));
                }
                items.push(Item::Rect(RectItem{radius:2.,..RectItem::new(RectPx{w:track.w*amount,..track},blue)}));
                items.push(Item::Rect(RectItem{radius:6.,..RectItem::new(RectPx{x:cx-6.,y:row.y+22.,w:12.,h:12.},ink)}));
            } else if let Kind::Spatial { key, crop } = &f.kind {
                let r = spatial_button(row);
                let hovered=hit(r,self.cursor.0,self.cursor.1);
                let binding = self.binding(&d.id,key);
                let configured = binding.as_ref().is_some_and(|b| if *crop { b.crop.is_some() } else { b.mask.is_some() });
                items.push(Item::Rect(RectItem { radius:6.,border_w:1.,border_color:hex_color(0x303234),..RectItem::new(r,hex_color(if hovered {0x1a1a1a}else{0x0a0a0a})) }));
                let thumb=RectPx{x:r.x+4.,y:r.y+4.,w:r.w-8.,h:54.};
                if configured {
                    let uv = if *crop {
                        let b=binding.as_ref().unwrap();let [x,y,w,h]=b.crop.unwrap();
                        [x as f32/b.dimensions[0].max(1) as f32,y as f32/b.dimensions[1].max(1) as f32,(x+w) as f32/b.dimensions[0].max(1) as f32,(y+h) as f32/b.dimensions[1].max(1) as f32]
                    } else {[0.,0.,1.,1.]};
                    items.push(Item::Video{a:self.source,b:self.source,r:thumb,uv,mode:VideoMode::Tex,p0:0.,p1:0.});
                    if !crop {let b=binding.as_ref().unwrap();if let Some(mask)=&b.mask {draw_mask_preview(items,thumb,b.dimensions,mask);}}
                    if hovered {items.push(Item::Rect(RectItem::new(thumb,[1.,1.,1.,0.025])));}
                } else {items.push(Item::Rect(RectItem::new(thumb,hex_color(if hovered {0x353535}else{0x232323}))));}
                items.push(Item::Rect(RectItem{radius:6.,..RectItem::new(RectPx{x:thumb.x+(thumb.w-40.)/2.,y:thumb.y+7.,w:40.,h:40.},[0.,0.,0.,0.7])}));
                items.push(Item::FrameIcon{x:thumb.x+(thumb.w-20.)/2.,y:thumb.y+17.,index:if *crop {5}else{4}});
            } else if matches!(f.kind,Kind::Bool) {
                items.push(Item::Text(TextItem{valign:VAlign::Middle,max_width:Some((row.w-48.).max(1.)),..TextItem::new(row.x,row.y+10.,11.,ink,&f.title)}));
                let on=value.as_bool().unwrap_or(false);
                draw_rail_switch(items,RectPx{x:row.x+row.w-32.,y:row.y+2.,w:32.,h:16.},if on{1.}else{0.});
            } else {
                let r = RectPx { x: row.x, y: row.y + 16.0, w: row.w, h: 20.0 };
                let hovered=hit(r,self.cursor.0,self.cursor.1);
                items.push(Item::Rect(RectItem { radius: 4.0, border_w: 1.0, border_color: if editing.is_some(){blue}else{hex_color(0x303234)}, ..RectItem::new(r, hex_color(if hovered||editing.is_some(){0x1a1a1a}else{0x0a0a0a})) }));
                ui_label(items, r.x + r.w / 2.0, r.y + r.h / 2.0, 10.0, ink, &shown, Align::Center, r.w - 12.0);
                if editing.is_some() { set_caret(items, caret); }
            }
        }
        items.push(Item::Clip(None));
        self.draw_tooltip(items, vp, clip);
    }
    fn spatial_count(&self) -> usize {
        self.fields.iter().take_while(|f|matches!(f.kind,Kind::Spatial{..})).count()
    }
    #[cfg(test)]
    fn settings_rows(&self) -> usize {
        let spatial=self.spatial_count();
        spatial.div_ceil(2)+self.fields.len()-spatial
    }
    fn field_pitch(&self,i:usize)->f32 {
        if matches!(self.fields[i].kind,Kind::Spatial{..}){104.}else{44.}
    }
    fn settings_height(&self)->f32 {
        let spatial=self.spatial_count();
        spatial.div_ceil(2) as f32*104.+(spatial..self.fields.len()).map(|i|self.field_pitch(i)).sum::<f32>()
    }
    fn row(&self, i: usize, vp: (f32, f32)) -> RectPx {
        let r = Self::bounds(vp);
        let spatial=self.spatial_count();
        let width=Self::right(vp)-32.;
        let paired=i<spatial;
        let column=if paired {i%2}else{0};
        let offset=if paired {(i/2) as f32*104.}else{spatial.div_ceil(2) as f32*104.+(spatial..i).map(|j|self.field_pitch(j)).sum::<f32>()};
        let w=if paired {(width-12.)/2.}else{width};
        RectPx {
            x: vp.0 - Self::right(vp) + 16.0 + column as f32*(w+12.),
            y: r.y + 164.0 + offset - self.scroll,
            w,
            h: if matches!(self.fields[i].kind,Kind::Spatial{..}){96.}else{36.},
        }
    }
    pub fn scroll(&mut self, dy: f32, cursor: (f32, f32), vp: (f32, f32)) -> bool {
        let r = Self::bounds(vp);
        if self.open && hit(r, cursor.0, cursor.1) {
            if cursor.0 < Self::left(vp) {
                let list = self.list(vp);
                self.list_scroll = (self.list_scroll - dy).clamp(
                    0.0,
                    (self.visible().len() as f32 * CARD_PITCH - 3.0 - list.h).max(0.0),
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
                (self.settings_height() - (Self::bottom(vp) - 210.0)).max(0.0),
            );
            return true;
        }
        false
    }
    pub fn click(&mut self, x: f32, y: f32, vp: (f32, f32)) -> bool {
        self.caret_at = Instant::now();
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
            } else if hit(Self::segment_button(vp), x, y) {
                self.request_segment();
            }
            return true;
        }
        if hit(Self::progress_button(vp), x, y) {
            if self.busy { self.cancel_preview(); } else { self.request_preview(); }
            return true;
        }
        if self.shown().is_some() && hit(Self::compare_switch(vp), x, y) {
            self.show_rendered = hit(Self::compare_half(vp, true), x, y);
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
            } else if let Some((_, c, _)) = self.chips(vp).into_iter().find(|(r, ..)| hit(*r, x, y)) {
                self.set_filter(c);
            } else if hit(self.list(vp), x, y) {
                let row = ((y - self.list_top(vp) + self.list_scroll) / CARD_PITCH) as usize;
                if let Some(&index) = self.visible().get(row) && hit(self.card(index, vp), x, y) {
                    if hit(self.card_switch(index, vp), x, y) {
                        self.toggle_enabled(index);
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
            self.seek = Some(self.timeline_time(x, timeline));
            return true;
        }
        if !self.descriptors.is_empty() && hit(Self::settings_switch(vp), x, y) {
            self.toggle_enabled(self.selected);
            return true;
        }
        if let Some(button) = Self::preset_buttons(vp).iter().position(|r| hit(*r, x, y))
            && !self.descriptors.is_empty()
        {
            match button {
                3=>self.save_preset(),
                4=>self.delete_preset(),
                0 if self.preset_name.is_some()=>self.text_selected=false,
                1|2=>{let count=self.descriptors[self.selected].presets.len()+1;let at=if self.preset<count-1{self.preset+1}else{0};let next=if button==1{(at+count-1)%count}else{(at+1)%count};self.preset=if next==0{usize::MAX}else{next-1};self.apply_preset();},
                _=>{}
            }
            return true;
        }
        if y < Self::bottom(vp) && y >= HEADER_H + 158.0 {
            self.preset_name=None;
            for i in 0..self.fields.len() {
                let row = self.row(i, vp);
                if !hit(row, x, y) {
                    continue;
                }
                let f = &self.fields[i];
                if matches!(f.kind, Kind::Number { .. }) && hit(slider_value(row),x,y) {
                    self.text_selected = false;
                    self.editing = Some((i, self.settings[&f.key].to_string()));
                    return true;
                }
                match &f.kind {
                    Kind::Number { min, max, integer } => {
                        if y<row.y+20. {return true;}
                        self.dragging = Some(i);
                        self.settings[&f.key] =
                            numeric(*min, *max, *integer, ((x - row.x) / row.w) as f64)
                    }
                    Kind::Enum(choices) => {
                        let n = choices
                            .iter()
                            .position(|c| c.value == self.settings[&f.key])
                            .map_or(0, |i| (i + 1) % choices.len());
                        self.settings[&f.key] = choices[n].value.clone();
                    }
                    Kind::Bool => {
                        self.settings[&f.key] =
                            json!(!self.settings[&f.key].as_bool().unwrap_or(false))
                    }
                    Kind::Text => {
                        self.text_selected = false;
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
                    Kind::Spatial{key,crop} => {
                        if !hit(spatial_button(row),x,y) {return true;}
                        if self.enabled.iter().take(self.selected).any(|e|*e) {self.status="Move this processor first to edit its spatial inputs".into();return true;}
                        self.editor_request=Some((self.descriptors[self.selected].id.clone(),key.clone(),*crop));return true;
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
    /// Re-ask the chain's output rate when the source rate or the chain
    /// changed. The worker holds the registry for a whole render, so this
    /// only tries; a busy registry keeps the old rate until the next tick.
    fn update_rate(&mut self, info: &VideoInfo) {
        let chain = self.chain();
        let key = format!("{}|{chain:?}", info.fps);
        if key == self.rate_key {
            return;
        }
        let source = rational(self.fps);
        // An empty chain declares the source's own rate, so a panel with no
        // processors needs no special case here.
        let rate = match self.registry.try_lock() {
            Ok(registry) => chain_rate(&registry, &chain, self.fps),
            Err(_) => return,
        };
        self.rate_key = key;
        match rate {
            Ok(rate) => {
                self.rate = rate_fps(rate);
                self.retimed = !same_rate(rate, source);
            }
            Err(e) => {
                self.rate = self.fps;
                self.retimed = false;
                self.status = e;
            }
        }
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
        if std::mem::take(&mut self.render_queued) {
            self.request_preview();
        }
        self.update_rate(info);
        if self.dirty.is_some() && self.editing.is_none() && self.preset_name.is_none() {
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
                self.status = if self.render_segment {
                    "Rendering segment · stops at the preview cache limit…"
                } else { "Rendering current frame through the enabled chain…" }.into();
                self.pending = Some(start);
                self.progress = None;
                self.busy_since = Instant::now();
                worker.submit(Request {
                    inputs:self.inputs.clone(),
                    generation: self.generation,
                    chain,
                    // The source frame under the playhead: a retimed chain
                    // returns every output frame paired with it.
                    target: (!self.render_segment).then(|| self.source_frame(start)),
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
                        if f.input != f.output || f.before != f.after { self.changed_count += 1; }
                        self.frames
                            .retain(|old| (old.pts - f.pts).abs() > 0.5 * f.duration);
                        let needed = f.before.len() + f.after.len();
                        Self::trim_stash(&mut self.stash, Self::frame_bytes(&self.frames) + needed, self.budget);
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
                            self.feedback = None;
                            self.status = e;
                        } else {
                            self.status = if self.changed_count == 0 && self.preview_count > 0 {
                                "Preview complete · no pixels changed. Check processor settings and inputs.".into()
                            } else {
                                format!("Render ready · {} frames · {} with changed pixels", self.preview_count, self.changed_count)
                            };
                            // The worker's last report can contain diagnostics about
                            // unresolved output. Preserve it after completion too.
                            let latest = worker.live.lock().unwrap().take();
                            if let Some((g, feedback)) = latest && g == self.generation {
                                self.feedback = Some(feedback);
                            }
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
        if let Some(f) = self.shown() {
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
        let ink = hex_color(0xe6e6e6);
        let secondary = hex_color(0xa3a5a8);
        let subtle = hex_color(SOURCE_SUBTLE);
        let over = |r: RectPx| hit(r, self.cursor.0, self.cursor.1);
        items.push(Item::Rect(RectItem::new(
            RectPx { x: 0.0, y: HEADER_H, w: l, h: Self::bottom(vp) - HEADER_H },
            hex_color(0x050505),
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
                ..TextItem::new(l - 14.0, hy, 11.0, if on > 0 { hex_color(CUT_HEADER_BLUE) } else { subtle }, format!("{on} ACTIVE")) }));
        }
        let chips = self.chips(vp);
        if let Some((first, ..)) = chips.first() {
            items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, tracking: 0.5,
                ..TextItem::new(14.0, first.y - 14.0, 10.0, subtle, "CATEGORY") }));
        }
        for (r, c, label) in &chips {
            let picked = *c == self.filter;
            let colour = if picked { ink } else if over(*r) { hex_color(0xc8c8c8) } else { secondary };
            if picked {
                items.push(Item::Rect(RectItem { radius: CHIP_H / 2.0, border_w: 1.0,
                    border_color: hex_color(SOURCE_RING), ..RectItem::new(*r, hex_color(0x1a1a1a)) }));
            }
            items.push(Item::Text(TextItem { regular: true, align: Align::Center, valign: VAlign::Middle,
                tracking: CHIP_TRACK, max_width: Some(r.w - 8.0),
                ..TextItem::new(r.x + r.w / 2.0, r.y + r.h / 2.0, CHIP_PX, colour, label.as_str()) }));
        }
        let list = self.list(vp);
        let visible = self.visible();
        if !visible.is_empty() {
            // The group box hugs its rows, and stops at the list's end.
            let top = self.group_top(vp);
            let rows = visible.len() as f32 * CARD_PITCH - 3.0;
            let h = (rows.min(list.h) + 2.0 * GROUP_PAD).max(0.0);
            items.push(Item::Rect(RectItem { radius: 14.0, border_w: 1.0, border_color: hex_color(0x2e2e2e),
                ..RectItem::new(RectPx { x: 13.0, y: top, w: l - 26.0, h }, hex_color(0x121212)) }));
        }
        items.push(Item::Clip(Some(list)));
        for &i in &visible {
            let d = &self.descriptors[i];
            let c = self.card(i, vp);
            if c.y + c.h < list.y || c.y > list.y + list.h {
                continue;
            }
            let active = i == self.selected;
            let hover = over(c) && over(list);
            items.push(Item::Rect(RectItem { radius: 8.0, border_w: if active { 2.0 } else { 0.0 },
                border_color: hex_color(SOURCE_RING),
                ..RectItem::new(c, hex_color(if hover && !active { 0x0c0c0c } else { 0x030303 })) }));
            let on = self.enabled[i];
            let switch = self.card_switch(i, vp);
            let tx = c.x + 13.0;
            items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(switch.x - tx - 10.0),
                ..TextItem::new(tx, c.y + c.h / 2.0, 12.0, if on || active { ink } else { hex_color(0x8c8c8c) }, d.name.as_str()) }));
            draw_rail_switch(items, switch, self.switch_position(i, Instant::now()));
        }
        items.push(Item::Clip(None));
        let footer = if self.descriptors.is_empty() { "No processors in this build" } else { "" };
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
    /// Focused inspector (278:609), for the focused processor: the name as
    /// the title row (the position keycap went 2026-10-29), five mono rows between solid hairlines
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
            // The chain position as a round badge, the Sources keycap's place.
            let badge = RectPx { x: x0, y: y + 16.0, w: 24.0, h: 24.0 };
            items.push(Item::Rect(RectItem { radius: 12.0, ..RectItem::new(badge, hex_color(0xb4b4b8)) }));
            items.push(Item::Text(TextItem { inspector: 2, align: Align::Center, valign: VAlign::Middle, max_width: Some(22.0),
                ..TextItem::new(badge.x + 12.0, badge.y + 12.0, 14.0, hex_color(0x161616), (i + 1).to_string()) }));
            items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(l - 80.0),
                ..TextItem::new(x0 + 32.0, y + 28.0, 13.0, hex_color(SOURCE_INK), d.name.as_str()) }));
            for ry in [y + 49.0, y + 178.0] {
                items.push(Item::Rect(RectItem::new(
                    RectPx { x: x0, y: ry, w: x1 - x0, h: 1.0 }, hex_color(0x303234))));
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
                    ..TextItem::new(x1, ry, 11.0, hex_color(0xa3a5a8), value) }));
            }
            items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(l - 32.0),
                ..TextItem::new(x0, y + 195.0, 10.0, hex_color(INSPECTOR_PATH), d.id.as_str()) }));
            items.push(Item::Clip(None));
        }
        draw_rail_grip(items, grip, l, hit(grip, self.cursor.0, self.cursor.1));
    }
    /// Use the dimensions and fit of the image actually displayed, including
    /// A/B source selection and the fullscreen viewer's different fit geometry.
    pub(crate) fn actual_zoom(&self, vp: (f32, f32), show_ui: bool, scale: f32) -> Option<f32> {
        let dims = self.shown().filter(|_| self.show_rendered)
            .map_or(self.source_dims, |f| f.output);
        let dims = (dims[0], dims[1]);
        if dims.0 == 0 || dims.1 == 0 { return None; }
        let base = if show_ui {
            Canvas { r: Self::viewer(vp) }.fit(dims)
        } else {
            crate::app::App::fit_rect(dims, RectPx { x: 0.0, y: 0.0, w: vp.0, h: vp.1 })
        };
        (base.w > 0.0).then(|| dims.0 as f32 / (base.w * scale.max(0.1)))
    }

    /// `view` is the app's shared zoom/center, which the viewer's canvas
    /// applies exactly as the timeline's does.
    pub fn draw(&self, items: &mut Vec<Item>, vp: (f32, f32), slot: usize, view: (f32, (f32, f32)), show_ui: bool) {
        if !self.open {
            return;
        }
        if !show_ui {
            let cached = self.shown().filter(|_| self.show_rendered);
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
            hex_color(0x0f0f0f),
        )));
        self.draw_rail(items, vp);
        let v = Self::viewer(vp);
        let cached = self.shown().filter(|_| self.show_rendered);
        if let Some(f) = cached {
            text(
                items,
                v.x + 16.0,
                v.y + v.h + 10.0,
                10.0,
                &if self.retimed {
                    format!(
                        "{}×{} → {}×{} · frame at {:.3}s · original holds {:.3}s",
                        f.input[0], f.input[1], f.output[0], f.output[1], f.pts, f.source_pts
                    )
                } else {
                    format!(
                        "{}×{} → {}×{} · through focused processor{}",
                        f.input[0], f.input[1], f.output[0], f.output[1],
                        if f.downsized { " · input downsized" } else { "" }
                    )
                },
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
        let header = RectPx { x: v.x, y: HEADER_H, w: v.w, h: CANVAS_HEADER_H };
        items.push(Item::Rect(RectItem::new(header, hex_color(0x000000))));
        let resolution = |dims: [u32; 2]| match dims {
            [3840, 2160] | [4096, 2160] => "4k".to_owned(),
            [7680, 4320] => "8k".to_owned(),
            [w, h] if w as u64 * 9 == h as u64 * 16 => format!("{h}p"),
            [w, h] => format!("{w}×{h}"),
        };
        let describe = |dims| {
            let size = resolution(dims);
            if self.retimed { format!("{size} {}fps", fps_label(self.fps)) } else { size }
        };
        let output = self.shown().map_or_else(|| "—".to_owned(), |f| {
            let size = resolution(f.output);
            if self.retimed { format!("{size} {}fps", fps_label(self.rate)) } else { size }
        });
        ui_label(items, v.x + 12.0, header.y + header.h / 2.0, 10.0,
            hex_color(0xa3a5a8), &format!("{} → {output}", describe(self.source_dims)),
            Align::Left, (Self::compare_switch(vp).x - v.x - 24.0).max(1.0));
        self.draw_compare_switch(items, vp);
        items.push(Item::Clip(Some(r)));
        self.draw_progress(items, vp);
        self.draw_transport(items, vp);
        let t = Self::timeline(vp);
        let track = RectPx { y: t.y + 12.0, h: 4.0, ..t };
        items.push(Item::Rect(RectItem { radius: 2.0, ..RectItem::new(track, TRACK) }));
        let fraction = (self.time / self.duration.max(0.001)).clamp(0.0, 1.0) as f32;
        items.push(Item::Rect(RectItem { radius: 2.0,
            ..RectItem::new(RectPx { w: track.w * fraction, ..track }, ACCENT) }));
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
        let x = at(self.time);
        items.push(Item::Rect(RectItem { radius: 6.0,
            ..RectItem::new(RectPx { x: x - 6.0, y: track.y - 4.0, w: 12.0, h: 12.0 }, ACCENT) }));
        ui_label(items, t.x, t.y + 37.0, 11.0, WORKSPACE_TEXT,
            format!("{} / {}", fmt_time(self.time), fmt_time(self.duration)), Align::Left, t.w * 0.55);
        ui_label(items, t.x + t.w, t.y + 37.0, 10.0, WORKSPACE_DIM,
            format!("frame {} · {} rendered", (self.time * self.rate).round() as i64, self.frames.len()),
            Align::Right, t.w * 0.45);
        self.draw_footer(items, vp);
        self.draw_settings(items, vp);
    }
    /// The hovered setting explains itself: its schema `description`, then
    /// the current choice's, wrapped by the renderer to the left of the
    /// settings column so it covers the picture, never another setting.
    fn draw_tooltip(&self, items: &mut Vec<Item>, vp: (f32, f32), clip: RectPx) {
        if self.dragging.is_some() || !hit(clip, self.cursor.0, self.cursor.1) {
            return;
        }
        let Some((i, f)) = self
            .fields
            .iter()
            .enumerate()
            .find(|(i, _)| hit(self.row(*i, vp), self.cursor.0, self.cursor.1))
        else {
            return;
        };
        if self.editing.as_ref().is_some_and(|(n, _)| *n == i) {
            return;
        }
        let tip=f.tooltip(&self.settings[&f.key]);
        let tip=if let Kind::Number{min,max,..}=f.kind {
            let bounds=format!("Range {}–{} · drag to adjust, click value to type",display_value(&json!(min)),display_value(&json!(max)));
            Some(tip.map_or_else(||bounds.clone(),|tip|format!("{tip}\n\n{bounds}")))
        }else{tip};
        let Some(tip)=tip else{return;};
        let right = vp.0 - Self::right(vp);
        let wrap = (right - Self::left(vp) - 40.0).clamp(120.0, 300.0);
        let row = self.row(i, vp);
        items.push(Item::Text(TextItem {
            interface: true,
            align: Align::Right,
            wrap: Some(wrap),
            bg: Some(TextBg { radius: 5.0, pad_x: 10.0, pad_y: 8.0, shadow:[0.,0.,0.,0.65],shadow_dy:3., ..TextBg::new(hex_color(0xf4f0dd)) }),
            ..TextItem::new(right - 18.0, row.y + 8.0, 11.0, hex_color(0x161a1d), tip)
        }));
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
/// Figma Process Switch (103:2651): one thumb and its label travel together.
fn draw_processor_switch(items: &mut Vec<Item>, r: RectPx, on: bool, position: f32) {
    let ink = hex_color(0xededed);
    items.push(Item::Rect(RectItem { radius: 8.0, border_w: 1.5,
        border_color: hex_color(0xa4a7ad), ..RectItem::new(r, hex_color(0x0a0a0a)) }));
    let thumb = RectPx { x: r.x + 3.0 + (r.w - 34.0) * position, y: r.y + 3.0, w: 28.0, h: 21.0 };
    items.push(Item::Rect(RectItem { radius: 5.0, border_w: if on { 0.0 } else { 0.5 },
        border_color: hex_color(0x757575),
        ..RectItem::new(thumb, mix(0x535353, CUT_HEADER_BLUE, position)) }));
    items.push(Item::Text(TextItem { header: true, align: Align::Center, valign: VAlign::Middle,
        max_width: Some(28.0), ..TextItem::new(thumb.x + 14.0, thumb.y + 10.5, 12.0, ink, if on { "ON" } else { "OFF" }) }));
}
/// The rail's pill switch (Figma rail): a 14px round thumb on a 16px
/// travel, grey off, the focus orange on, outlined white in both states.
fn draw_rail_switch(items: &mut Vec<Item>, r: RectPx, position: f32) {
    items.push(Item::Rect(RectItem { radius: r.h / 2.0, border_w: 1.5,
        border_color: mix(0xdcdcdc, 0xffffff, position),
        ..RectItem::new(r, mix(0x34373b, SOURCE_RING, position)) }));
    // 2.5px in, so a sliver of track shows between the knob and the outline.
    let d = r.h - 5.0;
    let thumb = RectPx { x: r.x + 2.5 + (r.w - d - 5.0) * position, y: r.y + 2.5, w: d, h: d };
    items.push(Item::Rect(RectItem { radius: d / 2.0, ..RectItem::new(thumb, hex_color(0xf7f7f7)) }));
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
        let r = Self::progress_panel(vp);
        let ink = hex_color(0xededed);
        let secondary = hex_color(0xa4a7ad);
        let accent = hex_color(CUT_HEADER_BLUE);
        let elapsed = if self.busy { self.busy_since.elapsed() } else { self.render_elapsed };
        let title = if self.busy {
            self.feedback.as_ref().map_or("Preparing frame preview", |f| f.name.as_str())
        } else {
            match self.render_result {
                Some(true) => "Frame preview ready",
                Some(false) => "Frame preview error",
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
            self.feedback.as_ref().filter(|f| !f.progress.phase.trim().is_empty())
                .map_or_else(|| self.status.clone(), |f| format!("{} · {}", self.status, f.progress.phase))
        } else {
            "Render frame to preview the enabled chain".into()
        };
        items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0,
            border_color: hex_color(0x303235),
            ..RectItem::new(r, hex_color(0x0a0a0a)) }));
        let heading = if self.busy || self.render_result.is_some() {
            format!("{title} · {:.1}s", elapsed.as_secs_f64())
        } else { title.into() };
        ui_label(items, r.x + 12.0, r.y + 16.0, 11.0, ink, &heading, Align::Left, (r.w - 128.0).max(1.0));
        // The panel's primary action: a filled blue button that lifts under
        // the pointer; while rendering it turns into an amber-outlined Cancel.
        let control = Self::progress_button(vp);
        let hover = hit(control, self.cursor.0, self.cursor.1);
        let (fill, border, control_ink) = if self.busy {
            (hex_color(if hover { 0x2a2219 } else { 0x1a1612 }), hex_color(RENDERING), hex_color(RENDERING))
        } else {
            let blue = if hover { 0x2b8cf0 } else { 0x1580de };
            (hex_color(blue), hex_color(blue), hex_color(0xffffff))
        };
        items.push(Item::Rect(RectItem { radius: 5.0, border_w: 1.0, border_color: border,
            ..RectItem::new(control, fill) }));
        ui_label(items, control.x + control.w / 2.0, control.y + control.h / 2.0, 11.0,
            control_ink, if self.busy { "Cancel" } else { "Render" }, Align::Center, control.w - 12.0);
        let mut line = phase;
        if let Some(fraction) = fraction {
            line = format!("{line} · {}", if self.busy {
                format!("{:.0}% of stage", (fraction * 100.0).floor()) } else { "100%".into() });
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
        } else { String::new() };
        // One secondary row: the phase, then the rates while there are any.
        if !metrics.is_empty() {
            line = format!("{line} · {metrics}");
        }
        ui_label(items, r.x + 12.0, r.y + 35.0, 10.0, secondary, &line, Align::Left, (r.w - 24.0).max(1.0));
        let track = RectPx { x: r.x + 12.0, y: r.y + 51.0, w: (r.w - 24.0).max(1.0), h: 6.0 };
        items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(track, hex_color(0x303235)) }));
        let fill = if let Some(fraction) = fraction {
            Some(RectPx { w: track.w * fraction, ..track })
        } else if self.busy {
            // Unknown work must not look like repeatedly completing work.
            // Keep a steady activity mark until a measured fraction arrives.
            Some(RectPx { w: 6.0_f32.min(track.w), ..track })
        } else { None };
        if let Some(fill) = fill && fill.w > 0.0 {
            items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(fill, accent) }));
        }
    }
    fn draw_transport(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        const NAMES: [&str; 5] = ["Prev rendered", "Frame back", "Play", "Frame on", "Next rendered"];
        for i in 0..5 {
            let b = Self::transport(vp, i);
            // Bare glyphs, as the reference draws them: no box, the hover
            // lifts the ink to white and names the control beneath.
            let hover = hit(b, self.cursor.0, self.cursor.1);
            let ink = if i == 2 { FRAME_INK } else if hover { hex_color(0xffffff) } else { WORKSPACE_DIM };
            if i == 2 {
                let d = b.w.min(32.0);
                let circle = RectPx { x: b.x + (b.w - d) / 2.0, y: b.y + (b.h - d) / 2.0, w: d, h: d };
                items.push(Item::Rect(RectItem { radius: 16.0, ..RectItem::new(circle, ACCENT) }));
            }
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
        let span = Self::segment_button(vp);
        draw_cut_footer_button(items, span, "Render segment", "⇧R", false,
            hit(span, self.cursor.0, self.cursor.1), false);
        let export = footer_export_rect(vp, span.x + span.w + 8.0);
        let hint = "[ ] source   + duplicate   ← → frame   native 8-bit SDR";
        let hint_w = if vp.0 >= 1220.0 + status_build_width() { 400.0 } else { 0.0 };
        let x = span.x + span.w + 14.0;
        let status_w = export.x - 12.0 - x - hint_w;
        if status_w > 0.0 {
            ui_label(items, x, cy, 10.0, WORKSPACE_DIM, self.status.as_str(), Align::Left, status_w);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn switches_reverse_without_jumping_and_stop_redrawing() {
        let mut p = Panel::new(registry());
        p.enter();
        let vp = (1280.0, 800.0);
        let card = p.card_switch(0, vp);
        p.click(card.x + 5.0, card.y + 5.0, vp);
        assert!(p.enabled[0] && p.animating());
        let half = Instant::now() - Duration::from_secs_f32(SWITCH_SLIDE_S / 2.0);
        p.switch_slides[0].as_mut().unwrap().at = half;
        let previous = p.switch_slides[0].unwrap();
        let before = previous.position(true, half + Duration::from_secs_f32(SWITCH_SLIDE_S / 2.0));
        assert!(before > 0.5 && before < 1.0);
        let right = Panel::settings_switch(vp);
        p.click(right.x + 5.0, right.y + 5.0, vp);
        assert!(!p.enabled[0]);
        let reversed = p.switch_slides[0].unwrap();
        assert_eq!(reversed.from, previous.position(true, reversed.at));
        assert_eq!(reversed.position(false, reversed.at), reversed.from);
        p.switch_slides[0].as_mut().unwrap().at = Instant::now() - Duration::from_secs_f32(SWITCH_SLIDE_S + 0.01);
        assert_eq!(p.switch_position(0, Instant::now()), 0.0);
        assert!(!p.animating());
        p.toggle_enabled(0);
        p.close();
        assert!(!p.animating(), "hidden controls never keep the loop hot");
    }
    #[test]
    fn switching_a_stage_off_and_on_keeps_its_rendered_frames() {
        let mut p = Panel::new(registry());
        p.enter();
        p.toggle_enabled(0);
        p.frames = vec![mark(0.0), mark(1.0)];
        p.toggle_enabled(0);
        assert!(p.frames.is_empty(), "the stage-off chain has no renders of its own");
        p.toggle_enabled(0);
        assert_eq!(p.frames.len(), 2, "back on: the same chain's frames return");
        p.settings["$probe"] = json!(1);
        p.changed();
        assert!(p.frames.is_empty(), "new settings never show the old pixels");
        p.settings.as_object_mut().unwrap().remove("$probe");
        p.changed();
        assert_eq!(p.frames.len(), 2, "settings put back are the same chain again");
        p.set_budget(16 * 4);
        p.settings["$probe"] = json!(2);
        p.changed();
        p.frames = vec![mark(0.0), mark(1.0)];
        p.changed();
        assert!(p.stash.is_empty(), "the stash shares the preview budget, oldest out");
        p.settings.as_object_mut().unwrap().remove("$probe");
        p.changed();
        assert!(p.frames.is_empty(), "evicted chains render afresh");
        assert_eq!(p.stash.len(), 1);
    }
    #[test]
    fn actual_zoom_matches_displayed_upscaled_pixels() {
        let mut panel = Panel::default();
        panel.open = true;
        panel.source_dims = [1920, 1080];
        let mut preview = mark(0.0);
        preview.input = panel.source_dims;
        preview.output = [3840, 2160];
        panel.frames.push(preview);
        let vp = (1280.0, 800.0);
        for show_ui in [true, false] {
            for scale in [1.0, 2.0] {
                for rendered in [true, false] {
                    panel.show_rendered = rendered;
                    let zoom = panel.actual_zoom(vp, show_ui, scale).unwrap();
                    let mut items = Vec::new();
                    panel.draw(&mut items, vp, 1, (zoom, (0.5, 0.5)), show_ui);
                    let picture = items.iter().find_map(|item| match item {
                        Item::Video { r, .. } => Some(r), _ => None,
                    }).unwrap();
                    let expected = if rendered { 3840.0 } else { 1920.0 };
                    assert!((picture.w * scale - expected).abs() < 0.01);
                }
            }
        }
        panel.frames.clear();
        panel.show_rendered = true;
        let fallback = panel.actual_zoom(vp, true, 2.0);
        panel.show_rendered = false;
        assert_eq!(fallback, panel.actual_zoom(vp, true, 2.0));
    }

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
    fn seekbar_seeks_frames_between_previews_and_selects_exact_markers() {
        let mut p = Panel::default();
        p.enter();
        p.duration = 60.0;
        p.fps = 30.0;
        p.rate = 60.0;
        p.frames = vec![mark(5.123), mark(40.127)];
        let vp = (1280.0, 800.0);
        let track = Panel::timeline(vp);
        let x = |time: f64| track.x + track.w * (time / p.duration) as f32;
        let (gap, marker, drag) = (x(20.0), x(40.127), x(30.0));
        let y = track.y + 12.0;
        assert!(p.click(gap, y, vp));
        let target = p.take_seek().unwrap();
        assert!((target - 20.0).abs() <= 1.0 / p.rate);
        p.time = target;
        assert!(p.shown().is_none(), "uncached positions must show the source");
        p.release();
        assert!(p.click(marker + 1.0, y, vp));
        assert_eq!(p.take_seek(), Some(40.127));
        p.time = 40.127;
        assert_eq!(p.shown().unwrap().pts, p.time);
        assert!(p.pointer(drag, y, vp));
        let target = p.take_seek().unwrap();
        assert!((target - 30.0).abs() <= 1.0 / p.rate);
        p.time = target;
        assert!(p.shown().is_none());
        p.release();
        assert_eq!(p.pointer_time(track.x + track.w, track), 60.0 - 1.0 / 60.0);
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
        // The on-canvas switch sets the same flag as Tab.
        let source = Panel::compare_half(vp, false);
        assert!(p.click(source.x + 5.0, source.y + 5.0, vp));
        assert!(!p.show_rendered);
        let rendered = Panel::compare_half(vp, true);
        assert!(p.click(rendered.x + 5.0, rendered.y + 5.0, vp));
        assert!(p.show_rendered);
        assert!(p.seek.is_none() && p.dirty.is_none(), "the switch neither seeks nor renders");
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
    fn detailed_callbacks_are_not_overwritten_by_frame_counter_fallbacks() {
        struct Detailed(abner_processor_api::ProgressReporter);
        impl Session for Detailed {
            fn process(&mut self, input: Frame) -> Result<Vec<Frame>, String> {
                (self.0)(Progress { phase: "Continuous preview".into(), completed: 45, total: Some(100) });
                Ok(vec![input])
            }
            fn finish(&mut self) -> Result<Vec<Frame>, String> { Ok(vec![]) }
            fn output_dimensions(&self) -> [usize; 2] { [2, 2] }
            fn cancellation(&self) -> Cancellation { Cancellation::default() }
            fn progress(&self) -> Progress { Progress { phase: "legacy frame count".into(), completed: 1, total: None } }
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let report: abner_processor_api::ProgressReporter = Arc::new(move |p| captured.lock().unwrap().push(p));
        let callbacks = Arc::new(AtomicU64::new(0));
        let observed = callbacks.clone();
        let detailed = report.clone();
        let frames = Arc::new(AtomicU64::new(0));
        let mut session = ReportingSession {
            inner: Box::new(Detailed(Arc::new(move |p| { observed.fetch_add(1, Ordering::Relaxed); detailed(p); }))),
            report, callbacks, frames: frames.clone(),
        };
        session.process(Frame { width: 2, height: 2, pixels: vec![0; 12], pts: 0, duration: 1, time_base: [1, 24] }).unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.last().unwrap().phase, "Continuous preview");
        assert_eq!(events.last().unwrap().completed, 45);
        assert!(!events.iter().any(|p| p.phase == "legacy frame count"));
        assert_eq!(frames.load(Ordering::Relaxed), 1);
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
    #[test]
    fn category_chips_filter_the_rows_and_keep_focus_visible() {
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
        r.register(Box::new(Tagged("example.a", vec![Category::Denoising]))).unwrap();
        r.register(Box::new(Tagged("example.b", vec![Category::Interpolation]))).unwrap();
        r.register(Box::new(Tagged("example.c", vec![Category::Denoising]))).unwrap();
        let mut p = Panel::new(r);
        p.enter();
        let vp = (1280.0, 900.0);
        let chips = p.chips(vp);
        let labels: Vec<_> = chips.iter().map(|(_, _, l)| l.as_str()).collect();
        assert_eq!(labels, ["ALL", "INTERPOLATION", "DENOISING"], "ALL, then the categories present, in API order");
        p.select(1);
        // Picking a chip that hides the focused row focuses the first row it shows.
        let (chip, ..) = chips[2];
        p.click(chip.x + chip.w / 2.0, chip.y + chip.h / 2.0, vp);
        assert_eq!(p.filter, Some(Category::Denoising));
        assert_eq!(p.visible(), [0, 2]);
        assert_eq!(p.selected, 0);
        assert_eq!(p.card(1, vp).h, 0.0, "a hidden row has no area");
        let second = p.card(2, vp);
        assert_eq!(second.y, p.card(0, vp).y + CARD_PITCH, "visible rows close up");
        p.click(second.x + 20.0, second.y + 10.0, vp);
        assert_eq!(p.selected, 2);
        // Moving steps over the hidden row, which keeps its place.
        p.move_selected(-1);
        let ids: Vec<_> = p.descriptors.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["example.c", "example.a", "example.b"]);
        assert_eq!(p.selected, 0);
        let (all, ..) = p.chips(vp)[0];
        p.click(all.x + 5.0, all.y + 5.0, vp);
        assert_eq!(p.visible(), [0, 1, 2]);
    }
    #[test]
    fn paired_spatial_controls_stay_separate_and_leave_room_for_settings() {
        let mut p=Panel::default();
        p.fields=vec![
            Field{key:"mask".into(),title:"Removal (Mask)".into(),kind:Kind::Spatial{key:"mask".into(),crop:false},default:None,required:true,description:None},
            Field{key:"region".into(),title:"Search Area (Crop)".into(),kind:Kind::Spatial{key:"region".into(),crop:true},default:None,required:false,description:None},
            Field{key:"strength".into(),title:"Strength".into(),kind:Kind::Bool,default:None,required:false,description:None},
        ];
        for width in [720.,900.,1280.] {
            let vp=(width,800.);let a=p.row(0,vp);let b=p.row(1,vp);let next=p.row(2,vp);
            assert_eq!(a.y,b.y);
            let left=spatial_button(a);let right=spatial_button(b);
            assert!(left.x+left.w<=right.x);
            assert!(right.x+right.w<=width-16.);
            assert!(left.y+left.h<next.y && right.y+right.h<next.y);
            assert_eq!(next.w,Panel::right(vp)-32.);
        }
        assert_eq!(p.settings_rows(),2);
    }
    #[test]
    fn spatial_thumbnails_show_mask_coverage_and_region_bounds() {
        let r = RectPx { x:10.,y:20.,w:64.,h:36. };
        let mut items = Vec::new();
        let mut pixels = vec![0;128*72]; pixels[30*128+40]=255;
        draw_mask_preview(&mut items,r,[128,72],&pixels);
        assert_eq!(items.len(),1,"even a small mask must remain visible in the thumbnail");
        let Item::Rect(cell) = &items[0] else {panic!("mask cell")};
        assert!(cell.r.x>=r.x && cell.r.x+cell.r.w<=r.x+r.w);
        items.clear();
        draw_region_preview(&mut items,r,[128,72],[32,18,64,36]);
        let Some(Item::Rect(outline)) = items.last() else {panic!("region outline")};
        assert_eq!(outline.r.x,26.); assert_eq!(outline.r.y,29.);
        assert_eq!(outline.r.w,32.); assert_eq!(outline.r.h,18.);
    }
    #[test]
    fn model_locations_are_hidden_and_masks_remain_manual() {
        let mut p = Panel::default();
        p.descriptors = vec![abner_processor_api::Factory::descriptor(&BufferedFactory)];
        p.descriptors[0].settings_schema["properties"]["model"] = json!({"type":"string","format":"file-path","default":"automatic"});
        p.saved = vec![defaults(&fields(&p.descriptors[0].settings_schema))];
        p.enabled = vec![false];
        p.supported = vec![true];
        p.declarations.insert("example.buffered".into(),vec![abner_processor_api::SpatialInputDescriptor {
            key:"mask".into(),title:"Removal mask".into(),kind:abner_processor_api::SpatialInputKind::Mask,required:true,
        }]);
        p.select(0);
        assert!(matches!(p.fields[0].kind,Kind::Spatial{crop:false,..}));
        assert!(!p.fields.iter().any(|f| f.key=="model"));
        assert_eq!(p.settings["model"],"automatic");
        assert!(p.editor_request.is_none());
        p.open = true;
        let row = p.row(0,(1280.,800.));
        p.click(row.x+10.,row.y+35.,(1280.,800.));
        assert_eq!(p.editor_request.take(),Some(("example.buffered".into(),"mask".into(),false)));
        p.set_binding("example.buffered".into(),"mask".into(),abner_processor_api::SpatialBinding {mask:Some(Arc::new(vec![255])),..Default::default()});
        p.select(0);
        assert!(p.editor_request.is_none());
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
    /// Nearest-neighbour upscaler declaring a 2x model (`{"scale": 2}`),
    /// the fixture for the Downsize input row.
    struct ScalingFactory;
    impl abner_processor_api::Factory for ScalingFactory {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "example.scaling".into(),
                name: "Scaling example".into(),
                settings_schema: json!({"properties":{"scale":{"type":"integer","minimum":1,"maximum":4,"default":1}}}),
                presets: vec![],
            }
        }
        fn scales(&self) -> Vec<ScaleOption> {
            vec![ScaleOption { factor: 2, settings: json!({"scale": 2}) }]
        }
        fn create(&self, w: usize, h: usize, s: &Value) -> Result<Box<dyn abner_processor_api::Session>, String> {
            let factor = s["scale"].as_u64().unwrap_or(1) as usize;
            Ok(Box::new(Scaling { input: [w, h], factor, cancel: Cancellation::default() }))
        }
    }
    struct Scaling {
        input: [usize; 2],
        factor: usize,
        cancel: Cancellation,
    }
    impl abner_processor_api::Session for Scaling {
        fn process(&mut self, f: Frame) -> Result<Vec<Frame>, String> {
            if [f.width, f.height] != self.input {
                return Err(format!("fed {}x{}, created for {}x{}", f.width, f.height, self.input[0], self.input[1]));
            }
            let (w, h, k) = (f.width, f.height, self.factor);
            let mut pixels = Vec::with_capacity(w * h * k * k * 3);
            for y in 0..h * k {
                for x in 0..w * k {
                    let i = ((y / k) * w + x / k) * 3;
                    pixels.extend_from_slice(&f.pixels[i..i + 3]);
                }
            }
            Ok(vec![Frame { width: w * k, height: h * k, pixels, ..f }])
        }
        fn finish(&mut self) -> Result<Vec<Frame>, String> {
            Ok(vec![])
        }
        fn output_dimensions(&self) -> [usize; 2] {
            [self.input[0] * self.factor, self.input[1] * self.factor]
        }
        fn cancellation(&self) -> Cancellation {
            self.cancel.clone()
        }
        fn progress(&self) -> abner_processor_api::Progress {
            abner_processor_api::Progress { phase: "example".into(), completed: 0, total: None }
        }
    }
    fn registry() -> Registry {
        let mut r = Registry::default();
        r.register(Box::new(BufferedFactory)).unwrap();
        r.register(Box::new(ScalingFactory)).unwrap();
        r
    }
    #[test]
    fn downsize_row_appears_only_for_declared_scales_and_stays_out_of_plugin_settings() {
        let mut panel = Panel::new(registry());
        panel.open = true;
        let scaling = panel.descriptors.iter().position(|d| d.id == "example.scaling").unwrap();
        let buffered = panel.descriptors.iter().position(|d| d.id == "example.buffered").unwrap();
        panel.select(buffered);
        assert!(panel.fields.iter().all(|f| f.key != DOWNSIZE_KEY), "no scales, no row");
        panel.select(scaling);
        let row = panel.fields.iter().position(|f| f.key == DOWNSIZE_KEY).expect("Downsize input row");
        assert_eq!(row, 0, "the host's row leads the plugin's settings");
        assert!(matches!(&panel.fields[row].kind, Kind::Enum(c) if c.len() == 2), "Off and 1/2 only: no 4x model declared");
        assert_eq!(panel.settings[DOWNSIZE_KEY], json!(1));
        let vp = (1280.0, 900.0);
        let r = panel.row(row, vp);
        assert!(panel.click(r.x + r.w / 2.0, r.y + 25.0, vp));
        assert_eq!(panel.settings[DOWNSIZE_KEY], json!(2), "a click cycles to the 2x choice");
        panel.enabled[scaling] = true;
        let chain = panel.chain();
        let stage = chain.iter().find(|(id, _)| id == "example.scaling").unwrap();
        assert_eq!(stage.1[DOWNSIZE_KEY], json!(2), "the chain carries the host key");
        let (plain, factor) = split_downsize(&stage.1);
        assert_eq!(factor, 2);
        assert!(plain.get(DOWNSIZE_KEY).is_none(), "the plugin never sees it");
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
        // The compact value chip edits without starting a track drag.
        let value=slider_value(row);
        panel.click(value.x+value.w/2.,value.y+10.,vp);
        assert!(panel.editing.is_some() && panel.dragging.is_none());
        panel.shortcut('a');panel.text_input("0.25");panel.key(Key::Enter);
        assert_eq!(panel.settings["strength"],json!(0.25));
        assert_eq!(row.h,36.);
        assert_eq!(panel.settings_height(),132.,"all regular settings share the compact pitch");
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
        panel.click(row.x + 20.0, row.y + 25.0, vp);
        panel.text_input("/child");panel.key(Key::Enter);
        assert_eq!(panel.settings["path"], json!("/tmp/example/child"));
        panel.click(row.x + 20.0, row.y + 25.0, vp);
        panel.shortcut('a');panel.text_input("replacement");panel.key(Key::Enter);
        assert_eq!(panel.settings["path"], json!("replacement"));
        panel.close();
        assert!(panel.cancel.check().is_err());
    }
    #[test]
    fn native_preview_preserves_yuv_and_low_rgb16_bits_through_buffered_sessions() {
        use crate::native_decode_tests::{Fixture,Observe,reference};
        let fixture=Fixture::new();let info=crate::probe::probe(&fixture.path).unwrap();
        for format in [PixelFormat::Yuv422p10,PixelFormat::Rgb16] {
            let frames=Arc::new(Mutex::new(Vec::new()));let mut registry=Registry::default();
            registry.register(Box::new(Observe{format,frames:frames.clone()})).unwrap();
            let request=Request {inputs:Default::default(),generation:1,chain:vec![("fixture.observe".into(),json!({}))],target:None,info:info.clone(),start:0.,end:None,cancel:Cancellation::default(),budget:DEFAULT_PREVIEW_BYTES};
            let (tx,rx)=mpsc::sync_channel(FRAMES+2);render(&registry,&request,&tx,None,None).unwrap();
            let mut displayed=0;while let Ok(Event::Frame(_,f))=rx.try_recv(){assert_eq!(f.before,f.after);displayed+=1;}assert_eq!(displayed,12);
            let frames=frames.lock().unwrap();assert_eq!(frames.len(),12);
            for (i,f) in frames.iter().enumerate() {
                let expected=abner_processing::color::convert(reference(&fixture.path,i as u64),format).unwrap();
                match (&f.pixels,expected.pixels) {
                    (FramePixels::Yuv16(a),FramePixels::Yuv16(b))=>{assert_eq!(a,&b);assert!(a[0].iter().any(|v|v%4!=0));},
                    (FramePixels::Rgb16(a),FramePixels::Rgb16(b))=>{assert_eq!(a,&b);assert!(a.iter().any(|v|v%257!=0));},
                    _=>panic!("negotiated storage changed"),
                }
            }
        }
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
                // Supported H.264: lossless transform bypass has no software decode path.
                "-c:v",
                "libx264",
                "-crf",
                "18",
                // Frame 4 is a sync sample: preview seeks must land even
                // without a preceding cached span to verify the target.
                "-g",
                "2",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let info = crate::probe::probe(&path).unwrap();
        let r = Request {
            inputs:Default::default(),
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
        // A native RGBA-only session must survive the spatial/reporting wrappers
        // and reach the display without an intermediate RGB16 plugin contract.
        struct NativeRgba([usize; 2]);
        impl Session for NativeRgba {
            fn format_pairs(&self) -> Vec<abner_processor_api::FormatPair> { vec![abner_processor_api::FormatPair::same(PixelFormat::Rgba8)] }
            fn configure_format(&mut self, p: abner_processor_api::FormatPair) -> Result<(), String> {
                assert_eq!(p, abner_processor_api::FormatPair::same(PixelFormat::Rgba8)); Ok(())
            }
            fn process_video(&mut self, f: VideoFrame) -> Result<Vec<VideoFrame>, String> { Ok(vec![f]) }
            fn finish_video(&mut self) -> Result<Vec<VideoFrame>, String> { Ok(vec![]) }
            fn output_dimensions(&self) -> [usize;2] { self.0 }
            fn cancellation(&self) -> Cancellation { Cancellation::default() }
            fn progress(&self) -> Progress { Progress { phase:"native".into(),completed:0,total:None } }
        }
        struct NativeFactory;
        impl abner_processor_api::Factory for NativeFactory {
            fn descriptor(&self) -> Descriptor { Descriptor {id:"native".into(),name:"Native".into(),settings_schema:json!({}),presets:vec![]} }
            fn create(&self,w:usize,h:usize,_:&serde_json::Value)->Result<Box<dyn Session>,String> {Ok(Box::new(NativeRgba([w,h])))}
        }
        let mut native_registry=Registry::default();native_registry.register(Box::new(NativeFactory)).unwrap();
        let native = Request { chain:vec![("native".into(),json!({}))], inputs:Default::default(),generation:10,target:None,info:info.clone(),start:0.,end:None,cancel:Cancellation::default(),budget:DEFAULT_PREVIEW_BYTES };
        render(&native_registry,&native,&tx,None,None).unwrap();
        let mut native_count=0;
        while let Ok(Event::Frame(_,f))=rx.try_recv() { assert_eq!(f.before,f.after);native_count+=1; }
        assert_eq!(native_count,6);
        // A budget too small for one native pair refuses, naming the config key.
        let starved = Request {
            inputs:Default::default(),
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
        // Downsize input: the stage sees a 1/2 frame and its 2x model lands
        // back at the source size; without it the 2x model doubles the frame.
        for (settings, expect) in [
            (json!({DOWNSIZE_KEY: 2}), [info.width, info.height]),
            (json!({"scale": 2}), [info.width * 2, info.height * 2]),
        ] {
            let scaled = Request {
                inputs: Default::default(),
                generation: 8,
                chain: vec![("example.scaling".into(), settings.clone())],
                target: Some(frames[1].pts),
                info: info.clone(),
                start: 0.0,
                end: None,
                cancel: Cancellation::default(),
                budget: DEFAULT_PREVIEW_BYTES,
            };
            render(&registry(), &scaled, &tx, None, None).unwrap();
            let mut got = vec![];
            while let Ok(event) = rx.try_recv() {
                if let Event::Frame(_, f) = event {
                    got.push(f);
                }
            }
            assert_eq!(got.len(), 1, "{settings}");
            assert_eq!(got[0].output, expect, "{settings}");
            assert_eq!(got[0].input, [info.width, info.height], "the before is the untouched source");
            assert_eq!(got[0].before.len(), (info.width * info.height * 4) as usize);
            assert_eq!(got[0].after.len(), (expect[0] * expect[1] * 4) as usize);
            assert_eq!(got[0].downsized, settings.get(DOWNSIZE_KEY).is_some());
        }
        let limited = Request {
            inputs:Default::default(),
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
            inputs:Default::default(),
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
        // An isolated input is flushed through a buffering stage without
        // feeding preceding history or starting a second batch.
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
            fn finish(&mut self) -> Result<Vec<Frame>, String> { self.0.finish() }
            fn output_dimensions(&self) -> [usize; 2] { self.0.output_dimensions() }
            fn cancellation(&self) -> Cancellation { self.0.cancellation() }
            fn progress(&self) -> Progress { self.0.progress() }
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut guarded = Registry::default();
        guarded.register(Box::new(GuardFactory(calls.clone()))).unwrap();
        let first = Request {
            inputs:Default::default(),
            generation: 12, chain: r.chain.clone(), target: Some(0.0),
            info: info.clone(), start: 0.0, end: None, cancel: Cancellation::default(), budget: r.budget,
        };
        let (guard_tx, guard_rx) = mpsc::sync_channel(FRAMES + 2);
        render(&guarded, &first, &guard_tx, None, None).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let delivered: Vec<_> = guard_rx.try_iter().filter_map(|event| match event {
            Event::Frame(g, f) => Some((g, f.pts)), _ => None,
        }).collect();
        assert_eq!(delivered, [(12, 0.0)]);
        // Every previewable processor late in the source receives only the target,
        // rather than rendering and discarding every preceding frame.
        struct IndependentFactory(Arc<std::sync::atomic::AtomicUsize>, InputRequirement);
        impl abner_processor_api::Factory for IndependentFactory {
            fn descriptor(&self) -> Descriptor { ScalingFactory.descriptor() }
            fn capabilities(&self) -> Capabilities {
                Capabilities { role: Role::Processor, input: self.1 }
            }
            fn create(&self, w: usize, h: usize, settings: &Value) -> Result<Box<dyn Session>, String> {
                Ok(Box::new(GuardSession(ScalingFactory.create(w, h, settings)?, self.0.clone())))
            }
        }
        for requirement in [InputRequirement::IndependentFrame, InputRequirement::Unspecified,
            InputRequirement::Recurrent { startup_frames: 4 },
            InputRequirement::Batch { min_frames: 4, max_frames: 8 },
            InputRequirement::Window { before: 2, after: 2 }] {
            calls.store(0, std::sync::atomic::Ordering::Relaxed);
            let mut independent = Registry::default();
            independent.register(Box::new(IndependentFactory(calls.clone(), requirement))).unwrap();
            let late = Request {
                chain: vec![("example.scaling".into(), json!({}))],
                target: Some(frames[4].pts),
                inputs: Default::default(), generation: 13, info: info.clone(),
                start: 0.0, end: None, cancel: Cancellation::default(), budget: r.budget
            };
            render(&independent, &late, &guard_tx, None, None).unwrap();
            assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
            let delivered: Vec<_> = guard_rx.try_iter().filter_map(|event| match event {
                Event::Frame(_, f) => Some(f.source_pts), _ => None,
            }).collect();
            assert_eq!(delivered.len(), 1);
            assert!((delivered[0] - frames[4].pts).abs() < 1e-6);
        }
        // Render, move, render again with slightly different header/decoder
        // rates. A valid landing can lie outside the old nominal tolerance;
        // its completed output must still be visible at the requested position.
        let mut previews = Vec::new();
        let later = frames[4].pts + 0.499 / info.fps;
        let mut rounded_info = info.clone();
        rounded_info.fps = 25.0;
        for target in [0.0, later] {
            let request = Request {
                inputs: Default::default(), generation: 20, chain: r.chain.clone(),
                target: Some(target), info: rounded_info.clone(), start: 0.0, end: None,
                cancel: Cancellation::default(), budget: r.budget,
            };
            render(&registry(), &request, &guard_tx, None, None).unwrap();
            let mut output: Vec<_> = guard_rx.try_iter().filter_map(|e| match e {
                Event::Frame(_, f) => Some(f), _ => None,
            }).collect();
            assert_eq!(output.len(), 1);
            assert_eq!(output[0].pts, target);
            previews.push(output.remove(0));
        }
        assert!((previews[1].source_pts - later).abs() > 0.51 / rounded_info.fps);
        let mut display = Panel::default();
        display.frames = previews;
        display.rate = info.fps;
        display.time = later;
        assert_eq!(display.shown().unwrap().pts, later);
        display.time = 0.0;
        assert_eq!(display.shown().unwrap().pts, 0.0);
        let mut transport = Panel::default();
        transport.duration = info.duration;
        transport.rate = info.fps;
        transport.time = 0.0;
        let track = Panel::timeline((1280.0, 900.0));
        let expected = (info.duration * 0.6 * info.fps).floor() / info.fps;
        assert_eq!(transport.pointer_time(track.x + track.w * 0.6, track), expected);
        transport.step(1);
        assert!((transport.take_seek().unwrap() - 1.0 / info.fps).abs() < 1e-6);
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
        panel.busy = true;
        let generation = panel.generation;
        let mut unchanged = mark(0.0);
        unchanged.after = unchanged.before.clone();
        tx.send(Event::Frame(generation, unchanged)).unwrap();
        tx.send(Event::Done(generation, Ok(()))).unwrap();
        let mut diagnostic = feedback(1, None);
        diagnostic.progress.phase = "12 unresolved pixels".into();
        *live.lock().unwrap() = Some((generation, diagnostic));
        panel.tick(&info, 0.2, None, &mut uploads, 1);
        assert_eq!(panel.render_result, Some(true));
        assert!(panel.status.contains("no pixels changed"));
        assert_eq!(panel.feedback.as_ref().unwrap().progress.phase, "12 unresolved pixels");
        panel.request_preview();
        assert_eq!(panel.changed_count, 0);
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
            downsized: false,
            before: Arc::new(vec![0; 16]),
            after: Arc::new(vec![0; 16]),
            input: [2, 2],
            output: [2, 2],
            pts,
            duration: 1.0 / 24.0,
            source_pts: pts,
            elapsed: 1.0,
        }
    }
    #[test]
    fn retimed_comparison_uses_output_span_in_window_and_fullscreen() {
        let mut p = Panel::default();
        p.open = true;
        p.fps = 24.0;
        p.rate = 12.0;
        p.retimed = true;
        let mut frame = mark(0.0);
        frame.duration = 1.0 / 12.0;
        p.frames.push(frame);
        // Still inside this output frame, beyond the source-grid tolerance.
        p.time = 0.06;
        for show_ui in [true, false] {
            let mut items = vec![];
            p.draw(&mut items, (1280.0, 720.0), 1, (1.0, (0.5, 0.5)), show_ui);
            assert!(items.iter().any(|i| matches!(i, Item::Video { a: 2, .. })));
        }
        p.key(Key::Tab);
        assert!(!p.show_rendered);
        p.key(Key::Tab);
        assert!(p.show_rendered);
    }
    #[test]
    fn choices_explain_themselves_in_the_tooltip() {
        let f = fields(&json!({"properties":{"mode":{
            "title":"Mode","description":"What the preview shows.","default":"loo",
            "oneOf":[
                {"const":"loo","title":"Leave-one-out","description":"Rebuilds each frame from its neighbours."},
                {"const":"mid","title":"In-between"}
            ]}}}));
        let mode = &f[0];
        assert!(matches!(&mode.kind, Kind::Enum(c) if c.len() == 2));
        assert_eq!(defaults(&f), json!({"mode":"loo"}));
        assert_eq!(mode.shown(&json!("loo")), "Leave-one-out");
        assert_eq!(
            mode.tooltip(&json!("loo")).unwrap(),
            "What the preview shows.\n\nLeave-one-out: Rebuilds each frame from its neighbours."
        );
        // A choice without its own text still shows the setting's.
        assert_eq!(mode.tooltip(&json!("mid")).unwrap(), "What the preview shows.");
        let bare = fields(&json!({"properties":{"n":{"type":"integer","minimum":0,"maximum":4}}}));
        assert_eq!(bare[0].tooltip(&json!(2)), None, "no description, no tooltip");
    }
    #[test]
    fn rates_stay_exact_rationals() {
        assert_eq!(rational(24.0), [24, 1]);
        assert_eq!(rational(24000.0 / 1001.0), [24000, 1001]);
        assert_eq!(rational(30000.0 / 1001.0), [30000, 1001]);
        assert_eq!(rational(12.5), [12500, 1000], "else to a thousandth");
    }
    /// A retimer: every input frame, then a copy half a period later with
    /// its pixels inverted, so a new frame is told from an original.
    struct Doubler;
    impl abner_processor_api::Factory for Doubler {
        fn descriptor(&self) -> Descriptor {
            Descriptor { id: "example.double".into(), name: "Double rate".into(), settings_schema: json!({}), presets: vec![] }
        }
        fn output_rate(&self, _: &Value, r: Rate) -> Result<Rate, String> {
            Ok([r[0] * 2, r[1]])
        }
        fn create(&self, w: usize, h: usize, _: &Value) -> Result<Box<dyn Session>, String> {
            struct S([usize; 2], Cancellation);
            impl Session for S {
                fn process(&mut self, f: Frame) -> Result<Vec<Frame>, String> {
                    let half = f.duration / 2;
                    let between = Frame {
                        width: f.width,
                        height: f.height,
                        pixels: f.pixels.iter().map(|p| u16::MAX - p).collect(),
                        pts: f.pts + half,
                        duration: f.duration - half,
                        time_base: f.time_base,
                    };
                    Ok(vec![Frame { duration: half, ..f }, between])
                }
                fn finish(&mut self) -> Result<Vec<Frame>, String> {
                    Ok(vec![])
                }
                fn output_dimensions(&self) -> [usize; 2] {
                    self.0
                }
                fn cancellation(&self) -> Cancellation {
                    self.1.clone()
                }
                fn progress(&self) -> Progress {
                    Progress { phase: String::new(), completed: 0, total: None }
                }
            }
            Ok(Box::new(S([w, h], Cancellation::default())))
        }
    }
    #[test]
    fn retimed_chain_pairs_new_frames_with_the_previous_original() {
        let dir = std::env::temp_dir().join(format!("abner-retime-{}", std::process::id()));
        let _ = std::fs::create_dir(&dir);
        let path = dir.join("retime.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=64x64:rate=24", "-frames:v", "6",
                "-c:v", "libx264", "-crf", "18", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let info = crate::probe::probe(&path).unwrap();
        let mut reg = Registry::default();
        reg.register(Box::new(Doubler)).unwrap();
        let chain = vec![("example.double".to_string(), json!({}))];
        assert_eq!(chain_rate(&reg, &chain, info.fps).unwrap(), [48, 1]);
        let request = |generation, target| Request {
            inputs:Default::default(),
            generation,
            chain: chain.clone(),
            target,
            info: info.clone(),
            start: 0.0,
            end: None,
            cancel: Cancellation::default(),
            budget: DEFAULT_PREVIEW_BYTES,
        };
        let (tx, rx) = mpsc::sync_channel(64);
        render(&reg, &request(1, None), &tx, None, None).unwrap();
        let frames: Vec<Preview> = rx.try_iter().filter_map(|e| match e { Event::Frame(_, f) => Some(f), _ => None }).collect();
        assert_eq!(frames.len(), 12, "two output frames per source frame");
        let period = 1.0 / 24.0;
        for (k, pair) in frames.chunks(2).enumerate() {
            let source = k as f64 * period;
            assert!((pair[0].pts - source).abs() < 1e-3 && (pair[0].source_pts - source).abs() < 1e-3);
            assert!((pair[1].pts - (source + period / 2.0)).abs() < 1e-3, "new frame lands mid-period");
            // The new frame's A side is the original it follows, not a gap.
            assert!((pair[1].source_pts - source).abs() < 1e-3);
            assert!(Arc::ptr_eq(&pair[0].before, &pair[1].before));
            assert_ne!(pair[1].after, pair[1].before);
        }
        let mut bounded = request(3, None);
        bounded.budget = 5 * 64 * 64 * 8;
        render(&reg, &bounded, &tx, None, None).unwrap();
        let bounded: Vec<_> = rx.try_iter().filter_map(|e| match e { Event::Frame(_, f) => Some(f), _ => None }).collect();
        assert_eq!(bounded.len(), 5, "retiming must not exceed the output-pair budget");
        // A frame render returns every output frame its source frame owns.
        render(&reg, &request(2, Some(2.0 * period)), &tx, None, None).unwrap();
        let owned: Vec<Preview> = rx.try_iter().filter_map(|e| match e { Event::Frame(_, f) => Some(f), _ => None }).collect();
        assert_eq!(owned.len(), 2);
        assert!(owned.iter().all(|f| (f.source_pts - 2.0 * period).abs() < 1e-3));

        // The page steps and shows frames at the OUTPUT rate, and seeks the
        // players to the original at or before the playhead.
        let mut p = Panel::new(reg);
        p.open = true;
        let mut uploads = vec![];
        p.tick(&info, 0.0, None, &mut uploads, 1);
        assert!(!p.retimed, "a bypassed retimer leaves the source rate");
        p.enabled[0] = true;
        p.frames = frames;
        p.tick(&info, 2.5 * period, None, &mut uploads, 1);
        assert!(p.retimed && (p.rate - 48.0).abs() < 1e-9);
        let shown = p.shown().unwrap();
        assert!((shown.pts - 2.5 * period).abs() < 1e-3, "the in-between frame is on screen");
        assert_eq!(uploads.len(), 1);
        let seek = p.source_seek(2.5 * period);
        assert!((seek - 1.5 * period).abs() < 1e-9, "half a frame before source frame 2, so the exact seek lands on 2, not 3: {seek}");
        p.step(1);
        assert!((p.take_seek().unwrap() - 3.0 * period).abs() < 1e-9, "one output frame on");
        p.step(-1);
        assert!((p.take_seek().unwrap() - 2.0 * period).abs() < 1e-9, "one output frame back");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn segment_preview_exceeds_old_frame_limit_and_stops_at_budget_or_boundary() {
        let dir = std::env::temp_dir().join(format!("abner-segment-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.mp4");
        assert!(std::process::Command::new("ffmpeg").args([
            "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=64x64:rate=30",
            "-frames:v", "40", "-c:v", "libx264", "-pix_fmt", "yuv420p",
        ]).arg(&path).status().unwrap().success());
        let info = crate::probe::probe(&path).unwrap();
        let mut request = Request {
            inputs: Default::default(), generation: 1, chain: vec![], target: None,
            info, start: 0., end: None, cancel: Cancellation::default(), budget: 32 * 64 * 64 * 8,
        };
        let (tx, rx) = mpsc::sync_channel(64);
        render(&Registry::default(), &request, &tx, None, None).unwrap();
        let frames: Vec<_> = rx.try_iter().filter_map(|e| match e { Event::Frame(_, f) => Some(f), _ => None }).collect();
        assert_eq!(frames.len(), 32, "segment uses the budget, not the old 24-frame ceiling");
        assert!(frames.windows(2).all(|p| (p[0].pts + p[0].duration - p[1].pts).abs() < 0.00001));
        assert!(frames.iter().map(|f| f.before.len() + f.after.len()).sum::<usize>() <= request.budget);
        request.start = 0.5;
        request.end = Some(1.0);
        render(&Registry::default(), &request, &tx, None, None).unwrap();
        let frames: Vec<_> = rx.try_iter().filter_map(|e| match e { Event::Frame(_, f) => Some(f), _ => None }).collect();
        assert_eq!(frames.len(), 15);
        assert!(frames.iter().all(|f| f.pts >= 0.5 && f.pts < 1.0));
        let mut panel = Panel::default();
        panel.open = true;
        panel.segment_start = 0.5;
        assert!(panel.key(Key::Char('R')));
        assert!(panel.render_segment);
        assert_eq!(panel.take_seek(), Some(0.5));
        assert!(panel.key(Key::Char('r')));
        assert!(!panel.render_segment);
        let button = Panel::segment_button((720., 480.));
        assert!(panel.click(button.x + 1., button.y + 1., (720., 480.)));
        assert!(panel.render_segment);
        std::fs::remove_dir_all(dir).unwrap();
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
        assert!(!p.modifies_video());
        p.enabled[1] = true;
        assert!(p.modifies_video(), "export includes enabled stages after the preview selection");
        p.enabled = vec![true, true];
        assert!(p.modifies_video());
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
        p.enabled[1] = true; // a different chain: its pixels can't be the old ones
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
    #[test]
    fn process_canvas_meets_columns_and_header() {
        for vp in [(720.0, 480.0), (1280.0, 900.0), (1920.0, 1080.0)] {
            let canvas = Panel::viewer(vp);
            assert_eq!(canvas.x, Panel::left(vp));
            assert_eq!(canvas.x + canvas.w, vp.0 - Panel::right(vp));
            assert_eq!(canvas.y, HEADER_H + CANVAS_HEADER_H);
            assert!(canvas.y + canvas.h < Panel::transport_y(vp));
            let first = Panel::transport(vp, 0);
            let last = Panel::transport(vp, 4);
            let track = Panel::timeline(vp);
            let info = Panel::progress_panel(vp);
            assert!(first.x >= canvas.x);
            assert!(last.x + last.w < track.x);
            assert!((track.y - first.y).abs() <= 1.0);
            assert!(track.w >= 100.0);
            assert!(track.x + track.w <= canvas.x + canvas.w);
            assert!(track.y + 37.0 < info.y);
            assert!(info.y + info.h < Panel::bottom(vp));
        }
    }
    #[test]
    fn custom_presets_save_reload_update_delete_and_keep_builtins() {
        let root=std::env::temp_dir().join(format!("abner-preset-panel-{}",std::process::id()));
        let _=std::fs::remove_dir_all(&root);
        let vp=(1280.,900.);
        let click=|panel:&mut Panel,index:usize|{let r=Panel::preset_buttons(vp)[index];assert!(panel.click(r.x+r.w/2.,r.y+r.h/2.,vp));};
        let mut panel=Panel::with_presets(registry(),Some(root.clone()));panel.open=true;
        panel.settings["strength"]=json!(0.75);
        click(&mut panel,3);assert_eq!(panel.preset_name.as_deref(),Some("Custom 1"));
        panel.text_input(" extended");assert_eq!(panel.preset_name.as_deref(),Some("Custom 1 extended"));
        panel.shortcut('a');click(&mut panel,0);panel.text_input(" again");
        assert_eq!(panel.preset_name.as_deref(),Some("Custom 1 extended again"));
        panel.key(Key::Backspace);assert_eq!(panel.preset_name.as_deref(),Some("Custom 1 extended agai"));
        panel.shortcut('a');panel.text_input("Soft restoration");panel.key(Key::Enter);
        assert!(panel.preset_name.is_none());assert_eq!(panel.user_presets.len(),1);
        let file=panel.user_presets.values().next().unwrap().clone();assert!(file.is_file());
        let mut reloaded=Panel::with_presets(registry(),Some(root.clone()));reloaded.open=true;
        assert_eq!(reloaded.settings["strength"],json!(0.5));
        click(&mut reloaded,2);assert_eq!(reloaded.settings["strength"],json!(0.75));assert!(!reloaded.enabled[0]);
        reloaded.settings["strength"]=json!(0.9);click(&mut reloaded,3);reloaded.key(Key::Enter);
        assert_eq!(reloaded.user_presets.len(),1);assert_eq!(crate::processor_presets::load(&root,"example.buffered").0[0].1.settings["strength"],json!(0.9));
        click(&mut reloaded,4);assert!(!file.exists());assert_eq!(reloaded.settings["strength"],json!(0.5));
        reloaded.descriptors[0].presets.push(abner_processor_api::Preset{id:"builtin".into(),name:"Built-in".into(),settings:json!({"strength":0.25})});
        click(&mut reloaded,2);click(&mut reloaded,4);assert_eq!(reloaded.descriptors[0].presets.len(),1);assert!(reloaded.status.contains("Built-in presets cannot"));
        for vp in [(720.,480.),(1280.,800.),(1440.,900.)]{let rects=Panel::preset_buttons(vp);for pair in rects.windows(2){assert!(pair[0].x+pair[0].w<=pair[1].x);}assert!(rects[4].x+rects[4].w<=vp.0-15.);}
        std::fs::remove_dir_all(root).unwrap();
    }

}

/// Attach to the label so placement uses the renderer's font metrics.
fn set_caret(items: &mut [Item], visible: bool) {
    if let Some(Item::Text(text)) = items.last_mut() { text.caret = visible; }
}
