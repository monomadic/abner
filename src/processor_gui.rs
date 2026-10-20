//! Runtime-schema controls and cancellable, bounded display previews.
//! No processor identity or implementation belongs in this module.
use crate::{
    app::Key,
    player::{Player, Rgba},
    probe::VideoInfo,
    render::{Item, RectItem, RectPx, TextItem, Upload, VideoMode},
};
use abner_processor_api::{Cancellation, Descriptor, Frame, Registry};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

const PREVIEW_BYTES: usize = 96 * 1024 * 1024;
const FRAMES: usize = 24;
const INK: [f32; 4] = [0.8, 0.8, 0.8, 1.0];
const DIM: [f32; 4] = [0.43, 0.43, 0.43, 1.0];
const BLUE: [f32; 4] = [0.04, 0.33, 0.7, 1.0];

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
    id: String,
    settings: Value,
    info: VideoInfo,
    start: f64,
    end: Option<f64>,
    cancel: Cancellation,
}
struct Mailbox {
    pending: Option<Request>,
    stop: bool,
}
struct Worker {
    active: Arc<Mutex<Option<Cancellation>>>,
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    rx: mpsc::Receiver<Event>,
}
enum Event {
    Frame(u64, Preview),
    Done(u64, Result<(), String>),
}
struct Preview {
    before: Rgba,
    after: Rgba,
    input: [u32; 2],
    output: [u32; 2],
    pts: f64,
    elapsed: f64,
    total: usize,
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
        let active = Arc::new(Mutex::new(None));
        let active_worker = active.clone();
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
                    render(&registry, &request, &tx, Some(&active_worker))
                }))
                .unwrap_or_else(|_| Err("Processor panicked; preview stopped".into()));
                *active_worker.lock().unwrap() = None;
                if tx.send(Event::Done(generation, result)).is_err() {
                    break;
                }
            }
        });
        Self {
            mailbox,
            rx,
            active,
        }
    }
    fn cancel(&self) {
        if let Some(c) = self.active.lock().unwrap().as_ref() {
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
fn proxy_dimensions(w: u32, h: u32) -> [u32; 2] {
    let scale = (640.0 / w.max(1) as f64)
        .min(360.0 / h.max(1) as f64)
        .min(1.0);
    [
        (w as f64 * scale).round().max(2.0) as u32,
        (h as f64 * scale).round().max(2.0) as u32,
    ]
}
fn render(
    registry: &Registry,
    r: &Request,
    tx: &mpsc::SyncSender<Event>,
    active: Option<&Mutex<Option<Cancellation>>>,
) -> Result<(), String> {
    r.cancel.check()?;
    // The display decoder emits RGBA8. This preview is explicitly a proxy, not
    // a full precision export or a color-managed HDR conversion.
    check_sdr(&r.info.path)?;
    let [w, h] = proxy_dimensions(r.info.width, r.info.height);
    let mut session = registry
        .get(&r.id)?
        .create(w as usize, h as usize, &r.settings)?;
    if let Some(active) = active {
        *active.lock().unwrap() = Some(session.cancellation());
    }
    r.cancel.check()?;
    let [ow, oh] = session.output_dimensions();
    if ow < 2 || oh < 2 || ow > 8192 || oh > 8192 {
        return Err("Unsupported preview output dimensions".into());
    }
    let per_frame = (w as usize * h as usize + ow * oh) * 4;
    if per_frame > PREVIEW_BYTES {
        return Err("Preview output exceeds the display memory budget".into());
    }
    let count = (PREVIEW_BYTES / per_frame).clamp(1, FRAMES);
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
     -> Result<(), String> {
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
            received += 1;
            tx.send(Event::Frame(
                r.generation,
                Preview {
                    before,
                    after: Arc::new(rgba),
                    input: [w, h],
                    output: [ow as u32, oh as u32],
                    pts: source_pts,
                    elapsed: started.elapsed().as_secs_f64(),
                    total: count,
                },
            ))
            .map_err(|_| "Preview closed")?;
        }
        Ok(())
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
    for _ in 0..count {
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
        originals.insert(pts, (rgba, source_pts));
        emit(
            session.process(Frame {
                width: w as usize,
                height: h as usize,
                pixels,
                pts,
                duration,
                time_base: [1, 1_000_000],
            })?,
            &mut originals,
        )?;
    }
    r.cancel.check()?;
    emit(session.finish()?, &mut originals)?;
    if received == 0 {
        return Err("No preview frames at this position".into());
    }
    if !originals.is_empty() {
        return Err("Processor did not return all preview frames".into());
    }
    Ok(())
}
fn check_sdr(path: &std::path::Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=color_transfer,color_primaries,color_space",
        "-of",
        "json",
    ])
    .arg(path);
    let out = crate::probe::run_deadlined(&mut cmd, Duration::from_secs(5))
        .map_err(|e| e.to_string())?
        .ok_or("Color probe timed out")?;
    if !out.success {
        return Err("Could not check preview color metadata".into());
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let stream = &v["streams"][0];
    for key in ["color_transfer", "color_primaries", "color_space"] {
        if let Some(s) = stream[key].as_str()
            && !matches!(s, "bt709" | "unknown" | "unspecified")
        {
            return Err(format!("Preview supports SDR Rec.709 only; {key} is {s}"));
        }
    }
    Ok(())
}

pub struct Panel {
    pub open: bool,
    descriptors: Vec<Descriptor>,
    selected: usize,
    fields: Vec<Field>,
    settings: Value,
    worker: Option<Worker>,
    generation: u64,
    cancel: Cancellation,
    dirty: Option<Instant>,
    context: Option<(std::path::PathBuf, u64, Option<u64>)>,
    frames: Vec<Preview>,
    status: String,
    busy: bool,
    selected_frame: usize,
    playback: bool,
    play_started: Instant,
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
            fields: vec![],
            settings: json!({}),
            worker: None,
            generation: 0,
            cancel: Cancellation::default(),
            dirty: None,
            context: None,
            frames: vec![],
            status: String::new(),
            busy: false,
            selected_frame: 0,
            playback: true,
            play_started: Instant::now(),
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
        let worker = (!descriptors.is_empty()).then(|| Worker::new(registry));
        let mut s = Self::default();
        s.descriptors = descriptors;
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
        self.settings = defaults(&self.fields);
        self.scroll = 0.0;
        self.editing = None;
        self.changed();
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
        self.dirty = Some(Instant::now());
        self.status = "Waiting for settings…".into();
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
            self.frames.clear();
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
        self.dragging.take().is_some()
    }
    pub fn close(&mut self) {
        if self.open {
            self.toggle();
        }
    }
    pub fn animating(&self) -> bool {
        self.open && self.playback && self.frames.len() > 1
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
        if matches!(key, Key::Char('p' | 'P')) {
            self.toggle();
            return true;
        }
        if self.open && key == Key::Escape {
            self.toggle();
            return true;
        }
        if self.open && key == Key::Space {
            self.playback = !self.playback;
            self.play_started = Instant::now();
            return true;
        }
        false
    }
    fn button(vp: (f32, f32)) -> RectPx {
        RectPx {
            x: vp.0 - 160.0,
            y: vp.1 - 25.0,
            w: 150.0,
            h: 23.0,
        }
    }
    fn bounds(vp: (f32, f32)) -> RectPx {
        RectPx {
            x: 12.0,
            y: 76.0,
            w: (vp.0 - 24.0).max(280.0),
            h: (vp.1 - 240.0).max(180.0),
        }
    }
    fn row(&self, i: usize, vp: (f32, f32)) -> RectPx {
        let r = Self::bounds(vp);
        RectPx {
            x: r.x + 12.0,
            y: r.y + 110.0 + i as f32 * 51.0 - self.scroll,
            w: 270.0,
            h: 45.0,
        }
    }
    pub fn scroll(&mut self, dy: f32, cursor: (f32, f32), vp: (f32, f32)) -> bool {
        let r = Self::bounds(vp);
        if self.open && hit(r, cursor.0, cursor.1) {
            self.scroll = (self.scroll - dy).clamp(
                0.0,
                (self.fields.len() as f32 * 51.0 - (r.h - 170.0)).max(0.0),
            );
            return true;
        }
        false
    }
    pub fn click(&mut self, x: f32, y: f32, vp: (f32, f32)) -> bool {
        if hit(Self::button(vp), x, y) {
            self.toggle();
            return true;
        }
        if !self.open {
            return false;
        }
        let r = Self::bounds(vp);
        if !hit(r, x, y) {
            return false;
        }
        if y < r.y + 40.0 {
            if x > r.x + r.w - 60.0 {
                self.toggle();
            } else if !self.descriptors.is_empty() {
                self.select((self.selected + 1) % self.descriptors.len());
            }
            return true;
        }
        if self.descriptors.is_empty() {
            return true;
        }
        if y < r.y + 78.0 {
            if x < r.x + 145.0 {
                self.changed();
            } else if x < r.x + 290.0 {
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
            } else {
                self.playback = !self.playback;
                self.play_started = Instant::now();
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
            if let Some(worker) = &self.worker {
                while worker.rx.try_recv().is_ok() {}
            }
            return;
        }
        let context = (info.path.clone(), start.to_bits(), end.map(f64::to_bits));
        if self.context.as_ref() != Some(&context) {
            self.context = Some(context);
            self.changed();
        }
        if self
            .dirty
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(180))
            && self.editing.is_none()
            && !self.descriptors.is_empty()
        {
            self.cancel = Cancellation::default();
            self.dirty = None;
            self.busy = true;
            self.status = "Loading processor and decoding preview…".into();
            self.worker.as_ref().unwrap().submit(Request {
                generation: self.generation,
                id: self.descriptors[self.selected].id.clone(),
                settings: self.settings.clone(),
                info: info.clone(),
                start,
                end,
                cancel: self.cancel.clone(),
            });
        }
        if let Some(worker) = &self.worker {
            while let Ok(event) = worker.rx.try_recv() {
                match event {
                    Event::Frame(g, f) if g == self.generation => {
                        if self.frames.is_empty() {
                            self.play_started = Instant::now();
                        }
                        self.status = format!(
                            "Preview {}/{} · {:.1} fps · ETA {:.1}s",
                            self.frames.len() + 1,
                            f.total,
                            (self.frames.len() + 1) as f64 / f.elapsed.max(0.001),
                            (f.total - self.frames.len() - 1) as f64 * f.elapsed
                                / (self.frames.len() + 1) as f64
                        );
                        self.frames.push(f);
                    }
                    Event::Done(g, result) if g == self.generation => {
                        self.busy = false;
                        match result {
                            Ok(()) => {
                                self.status = format!(
                                    "Ready · {} frames · {:.1} fps",
                                    self.frames.len(),
                                    self.frames.len() as f64
                                        / self.frames.last().map_or(1.0, |f| f.elapsed).max(0.001)
                                );
                            }
                            Err(e) => {
                                self.frames.clear();
                                self.status = e;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !self.frames.is_empty() {
            if self.playback {
                let seconds = self.play_started.elapsed().as_secs_f64();
                let first = self.frames[0].pts;
                let duration =
                    (self.frames.last().unwrap().pts - first + 1.0 / info.fps).max(1.0 / info.fps);
                let t = first + seconds % duration;
                self.selected_frame = self
                    .frames
                    .partition_point(|f| f.pts <= t)
                    .saturating_sub(1);
            }
            let f = &self.frames[self.selected_frame.min(self.frames.len() - 1)];
            uploads.push(Upload {
                idx: slot,
                w: f.input[0],
                h: f.input[1],
                buf: f.before.clone(),
            });
            uploads.push(Upload {
                idx: slot + 1,
                w: f.output[0],
                h: f.output[1],
                buf: f.after.clone(),
            });
        }
    }
    pub fn draw(&self, items: &mut Vec<Item>, vp: (f32, f32), slot: usize) {
        button(items, Self::button(vp), "Processors [P]", self.open);
        if !self.open {
            return;
        }
        let r = Self::bounds(vp);
        items.push(Item::Clip(Some(r)));
        items.push(Item::Rect(RectItem::new(r, [0.018, 0.018, 0.018, 1.0])));
        let title = self
            .descriptors
            .get(self.selected)
            .map(|d| format!("Processor preview: {}  ▸", d.name))
            .unwrap_or_else(|| "No processors installed".into());
        text(items, r.x + 14.0, r.y + 12.0, 14.0, &title, INK, r.w - 90.0);
        text(
            items,
            r.x + r.w - 50.0,
            r.y + 12.0,
            13.0,
            "Close",
            INK,
            45.0,
        );
        if self.descriptors.is_empty() {
            text(
                items,
                r.x + 14.0,
                r.y + 60.0,
                12.0,
                "Build a composed launcher from your local processor directory.",
                INK,
                r.w - 28.0,
            );
            items.push(Item::Clip(None));
            return;
        }
        button(
            items,
            RectPx {
                x: r.x + 12.0,
                y: r.y + 44.0,
                w: 128.0,
                h: 28.0,
            },
            "Refresh preview",
            false,
        );
        button(
            items,
            RectPx {
                x: r.x + 150.0,
                y: r.y + 44.0,
                w: 132.0,
                h: 28.0,
            },
            "Cycle presets",
            false,
        );
        button(
            items,
            RectPx {
                x: r.x + 304.0,
                y: r.y + 44.0,
                w: 154.0,
                h: 28.0,
            },
            if self.playback {
                "Pause preview"
            } else {
                "Play preview"
            },
            self.playback,
        );
        text(
            items,
            r.x + 12.0,
            r.y + 86.0,
            10.0,
            "SETTINGS · scroll for more",
            DIM,
            270.0,
        );
        let clip = RectPx {
            x: r.x + 10.0,
            y: r.y + 105.0,
            w: 276.0,
            h: (r.h - 169.0).max(0.0),
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
        items.push(Item::Clip(Some(r)));
        let x = r.x + 304.0;
        let width = (r.w - 320.0).max(0.0);
        let half = (width - 12.0) / 2.0;
        for (i, label) in ["SOURCE", "PROCESSED"].iter().enumerate() {
            text(
                items,
                x + i as f32 * (half + 12.0),
                r.y + 88.0,
                11.0,
                label,
                DIM,
                half,
            );
        }
        if let Some(f) = self
            .frames
            .get(self.selected_frame.min(self.frames.len().saturating_sub(1)))
        {
            for (i, dims) in [f.input, f.output].iter().enumerate() {
                let cell = RectPx {
                    x: x + i as f32 * (half + 12.0),
                    y: r.y + 112.0,
                    w: half.max(1.0),
                    h: (r.h - 190.0).max(1.0),
                };
                let scale = (cell.w / dims[0] as f32).min(cell.h / dims[1] as f32);
                let w = dims[0] as f32 * scale;
                let h = dims[1] as f32 * scale;
                items.push(Item::Video {
                    a: slot + i,
                    b: slot + i,
                    r: RectPx {
                        x: cell.x + (cell.w - w) / 2.0,
                        y: cell.y + (cell.h - h) / 2.0,
                        w,
                        h,
                    },
                    uv: [0.0, 0.0, 1.0, 1.0],
                    mode: VideoMode::Tex,
                    p0: 0.0,
                    p1: 0.0,
                });
            }
            text(
                items,
                x,
                r.y + r.h - 69.0,
                11.0,
                &format!(
                    "Frame {}/{} · {:.3}s · {}×{} → {}×{}",
                    self.selected_frame + 1,
                    self.frames.len(),
                    f.pts,
                    f.input[0],
                    f.input[1],
                    f.output[0],
                    f.output[1]
                ),
                INK,
                width,
            );
        } else {
            text(
                items,
                x,
                r.y + 130.0,
                12.0,
                if self.busy {
                    "Building preview…"
                } else {
                    "Adjust a setting or refresh to preview."
                },
                DIM,
                width,
            );
        }
        if self.busy {
            let fraction = self
                .frames
                .last()
                .map_or(0.0, |f| self.frames.len() as f32 / f.total as f32);
            items.push(Item::Rect(RectItem::new(
                RectPx {
                    x: r.x + 12.0,
                    y: r.y + r.h - 56.0,
                    w: (r.w - 24.0) * fraction,
                    h: 3.0,
                },
                BLUE,
            )));
        }
        text(
            items,
            r.x + 12.0,
            r.y + r.h - 48.0,
            11.0,
            &self.status,
            INK,
            r.w - 24.0,
        );
        text(
            items,
            r.x + 12.0,
            r.y + r.h - 25.0,
            10.0,
            "Preview only · SDR / 640×360 / 8-bit decode · processors are not included in export",
            DIM,
            r.w - 24.0,
        );
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
    fn proxy_preserves_aspect_without_upscaling() {
        assert_eq!(proxy_dimensions(1920, 1080), [640, 360]);
        assert_eq!(proxy_dimensions(1080, 1920), [203, 360]);
        assert_eq!(proxy_dimensions(64, 64), [64, 64]);
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
                "-c:v",
                "ffv1",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let info = crate::probe::probe(&path).unwrap();
        let r = Request {
            generation: 7,
            id: "example.buffered".into(),
            settings: json!({}),
            info: info.clone(),
            start: 0.0,
            end: None,
            cancel: Cancellation::default(),
        };
        let (tx, rx) = mpsc::sync_channel(FRAMES + 2);
        render(&registry(), &r, &tx, None).unwrap();
        let mut frames = vec![];
        while let Ok(Event::Frame(g, f)) = rx.try_recv() {
            assert_eq!(g, 7);
            assert_eq!(f.before, f.after);
            frames.push(f);
        }
        assert_eq!(frames.len(), 6);
        assert_eq!(frames[0].pts, 0.0);
        assert!(frames.windows(2).all(|f| f[1].pts > f[0].pts));
        let limited = Request {
            generation: 9,
            id: r.id.clone(),
            settings: json!({}),
            info: info.clone(),
            start: 0.0,
            end: Some(0.1),
            cancel: Cancellation::default(),
        };
        render(&registry(), &limited, &tx, None).unwrap();
        let mut limited_count = 0;
        while let Ok(Event::Frame(_, f)) = rx.try_recv() {
            assert!(f.pts < 0.1);
            limited_count += 1;
        }
        assert_eq!(
            limited_count, 3,
            "a preview session must stop and drain at a timeline boundary"
        );
        // Already-cancelled requests must never publish output.
        r.cancel.cancel();
        assert!(render(&registry(), &r, &tx, None).is_err());
        assert!(rx.try_recv().is_err());
        // Late events from an old settings generation must never become uploads.
        let mut panel = Panel::default();
        panel.open = true;
        panel.generation = 8;
        panel.context = Some((info.path.clone(), 0.0f64.to_bits(), None));
        panel.worker = Some(Worker {
            active: Arc::new(Mutex::new(None)),
            mailbox: Arc::new((
                Mutex::new(Mailbox {
                    pending: None,
                    stop: false,
                }),
                Condvar::new(),
            )),
            rx,
        });
        tx.send(Event::Frame(7, frames.remove(0))).unwrap();
        let mut uploads = vec![];
        panel.tick(&info, 0.0, None, &mut uploads, 1);
        assert!(uploads.is_empty());
        assert!(panel.frames.is_empty());
    }
}
