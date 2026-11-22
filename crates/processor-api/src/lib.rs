//! Streaming CPU frame contracts, including optional host-negotiated layouts.
//! No media implementation dependencies.
use serde::{Deserialize, Serialize};
mod formats;
mod spatial;
pub use formats::*;
pub use spatial::*;
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
/// Clone this handle before processing to request cancellation from another thread.
/// In-flight device work need not be interruptible; implementations check between jobs.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
            Err("processor cancelled".into())
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Serialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub settings: serde_json::Value,
}
pub type Result<T> = std::result::Result<T, String>;
/// Frames per second as `[numerator, denominator]`, e.g. `[30000, 1001]`.
pub type Rate = [u32; 2];
#[derive(Clone, Serialize)]
pub struct Descriptor {
    pub id: String,
    pub name: String,
    /// A JSON Schema object. The host builds its editor from `properties`:
    /// `enum` or `oneOf` choices, `number`/`integer` with `minimum`/`maximum`,
    /// `boolean`, `string`. Each property's `title` labels it and its
    /// `description` is shown as a tooltip. To explain each choice of a
    /// selector, use `oneOf` with one `{"const", "title", "description"}`
    /// per choice; the hovered setting shows the current choice's text.
    pub settings_schema: serde_json::Value,
    pub presets: Vec<Preset>,
}
/// Legacy packed full-range RGB16, SDR Rec.709; no alpha. Use `VideoFrame`
/// through negotiated session methods for other layouts. The host converts.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u16>,
    pub pts: i64,
    pub duration: i64,
    pub time_base: [u32; 2],
}
impl Frame {
    pub fn validate(&self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > 8192
            || self.height > 8192
            || self
                .width
                .checked_mul(self.height)
                .and_then(|n| n.checked_mul(3))
                != Some(self.pixels.len())
            || self.duration <= 0
            || self.time_base.contains(&0)
        {
            return Err("invalid RGB16 frame geometry or timing".into());
        }
        Ok(())
    }
}
/// A phase-local report: what a stage is doing and how far into THAT phase it
/// is. `completed / total` describes this phase only; a stage may run several
/// phases inside one call, each restarting at 0, so the host never reads it as
/// frame or job progress. Hosts show it as detail ("Tiles 3/12"). Unchanged
/// since API 0.2 (fields and JSON shape) so existing plugins keep compiling.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
}
impl Progress {
    /// `completed / total` in 0..=1, or `None` when the total is unknown or 0.
    pub fn fraction(&self) -> Option<f64> {
        self.total
            .filter(|t| *t > 0)
            .map(|t| self.completed.min(t) as f64 / t as f64)
    }
}
/// Optional in-flight phase reports, including work inside a single process()
/// call. The host callback must return promptly. Percentages describe this
/// phase only; report [`FrameProgress`] for the whole call's share.
pub type ProgressReporter = Arc<dyn Fn(Progress) + Send + Sync>;

/// How far the CURRENT call (`process`, `process_video`, `process_with_inputs`,
/// `finish`, `finish_video`) is through ALL of its work: every step that call
/// performs before it returns, counted once (e.g. VAE encode tiles + transformer
/// blocks + decode tiles + colour pass = 63 steps). This is the frame bar.
///
/// Contract:
/// - Report only while one of those calls is running, from any thread.
/// - `total` is the whole call's step count, > 0, and stays the same within
///   the call; `completed` never decreases within the call and is ≤ `total`.
/// - Every call starts over: the first report of a call may be `completed: 0`.
/// - `phase` names the step in progress; it may change freely within the call
///   without restarting the count.
/// - Reaching `total` does not mean output was delivered; the host finishes the
///   frame when the call returns.
/// - `frames` is how many output frames this call delivers when it returns: a
///   stage that buffers and renders a batch in one call says so, so the host's
///   total bar covers the whole batch instead of one frame. `0` (the default,
///   and what an older report deserializes to) means one.
///
/// The host owns job totals (frames done of frames to do) and folds this
/// fraction into them, so the total bar moves smoothly during slow frames.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameProgress {
    pub phase: String,
    pub completed: u64,
    pub total: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub frames: u64,
}
fn is_zero(n: &u64) -> bool {
    *n == 0
}
impl FrameProgress {
    /// Output frames this call delivers, at least one.
    pub fn frame_count(&self) -> u64 {
        self.frames.max(1)
    }
    /// `completed / total` in 0..=1, or `None` for a zero total.
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| self.completed.min(self.total) as f64 / self.total as f64)
    }
}
/// Receives [`FrameProgress`]. Must return promptly.
pub type FrameProgressReporter = Arc<dyn Fn(FrameProgress) + Send + Sync>;

#[derive(Clone, Copy, Default, Serialize)]
pub enum Role {
    Generator,
    #[default]
    Processor,
    PostProcessor,
}
#[derive(Clone, Copy, Default, Serialize)]
pub enum InputRequirement {
    /// Legacy processors have not declared their temporal requirements.
    #[default]
    Unspecified,
    IndependentFrame,
    Window {
        before: u32,
        after: u32,
    },
    /// History must be replayed from the segment boundary; startup may look ahead.
    Recurrent {
        startup_frames: u32,
    },
    /// Bounded batches, with short tails padded by the implementation.
    Batch {
        min_frames: u32,
        max_frames: u32,
    },
    WholeClip,
}
/// What a plugin is for. A plugin declares one or more; the host groups and
/// labels with them and never infers behaviour from them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Category {
    Interpolation,
    Enhancement,
    Upscaling,
    Denoising,
    Sharpening,
    Deinterlacing,
    Restoration,
    Stabilization,
    Color,
    Analysis,
    Generation,
    /// Anything the list above does not cover; shown as written.
    Other(String),
}
impl Category {
    pub fn label(&self) -> &str {
        match self {
            Self::Interpolation => "Interpolation",
            Self::Enhancement => "Enhancement",
            Self::Upscaling => "Upscaling",
            Self::Denoising => "Denoising",
            Self::Sharpening => "Sharpening",
            Self::Deinterlacing => "Deinterlacing",
            Self::Restoration => "Restoration",
            Self::Stabilization => "Stabilization",
            Self::Color => "Color",
            Self::Analysis => "Analysis",
            Self::Generation => "Generation",
            Self::Other(s) => s,
        }
    }
}
#[derive(Clone, Copy, Default, Serialize)]
pub struct Capabilities {
    pub role: Role,
    pub input: InputRequirement,
}
/// Ordered streaming frames. Calls may emit zero or several outputs; outputs keep
/// their inputs' timestamps unless `Factory::output_rate` changes the rate. After EOF,
/// call finish once and consume all returned frames. Drop/recreate a session at
/// cuts, seeks or configuration changes: recurrent history must not cross them.
/// Construct and use on one thread; only the cancellation handle is thread-safe.
/// Errors are terminal: discard this session and any unpublished job output.
/// Legacy RGB16 methods remain source-compatible. Opt in to other CPU layouts
/// through format_pairs/configure_format/process_video/finish_video. Conversion
/// belongs to the host; plugins only implement their declared native layouts.
pub trait Session {
    fn format_pairs(&self) -> Vec<FormatPair> {
        vec![FormatPair::RGB16]
    }
    fn configure_format(&mut self, pair: FormatPair) -> Result<()> {
        if pair == FormatPair::RGB16 {
            Ok(())
        } else {
            Err("session only supports RGB16".into())
        }
    }
    fn process_video(&mut self, input: VideoFrame) -> Result<Vec<VideoFrame>> {
        self.process(input.into_rgb16()?)
            .map(|frames| frames.into_iter().map(Into::into).collect())
    }
    fn finish_video(&mut self) -> Result<Vec<VideoFrame>> {
        self.finish()
            .map(|frames| frames.into_iter().map(Into::into).collect())
    }

    /// Optional requested output timestamp for a timestamp-preserving preview.
    /// Lets implementations aggregate required warmup/context work for progress;
    /// it must not change the model's output or truncate required input history.
    fn set_preview_target(&mut self, _pts: i64, _time_base: [u32; 2]) {}
    /// Implementations may retain this callback to report phase-local
    /// progress ([`Progress`]) during work.
    fn set_progress_reporter(&mut self, _reporter: ProgressReporter) {}
    /// Implementations whose calls are slow (seconds or more per frame) should
    /// retain this callback and report [`FrameProgress`] for the whole call.
    /// Wrapping sessions must forward it. Both reporters may be used together.
    fn set_frame_progress_reporter(&mut self, _reporter: FrameProgressReporter) {}
    fn process(&mut self, _input: Frame) -> Result<Vec<Frame>> {
        Err("session requires negotiated frames".into())
    }
    /// Named spatial inputs accompany the unmodified primary frame. Opt in explicitly.
    fn process_with_inputs(
        &mut self,
        input: Frame,
        inputs: Vec<SpatialInput>,
    ) -> Result<Vec<Frame>> {
        if !inputs.is_empty() {
            return Err("Session does not accept spatial inputs".into());
        }
        self.process(input)
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        Err("session requires negotiated frames".into())
    }
    /// Start an unrelated sequence: discard every buffered input and all
    /// temporal state, as if the session had just been created with the same
    /// settings and dimensions, but keep what is expensive to make (a loaded
    /// model, device buffers). Return a fresh token from
    /// [`cancellation`](Session::cancellation) afterwards: a raised one is
    /// never lowered. The host calls this instead of creating a new session
    /// between an export's kept ranges and between preview renders, then
    /// negotiates the format again and sets the reporters and preview target
    /// again. It may be called with frames still buffered (a preview stops at
    /// its frame without `finish`). Return `false`, the default, when the
    /// session cannot: the host drops it and creates a new one. Wrapping
    /// sessions must forward it.
    fn reset(&mut self) -> bool {
        false
    }
    fn output_dimensions(&self) -> [usize; 2];
    fn cancellation(&self) -> Cancellation;
    /// Completed output frames. The host owns timeline totals and job-wide
    /// progress; per-call work goes through [`FrameProgress`].
    fn progress(&self) -> Progress;
}
/// An integer upscale a processor offers, and the settings overlay that
/// selects it. The host uses these for "Downsize input": it shrinks the frame
/// by `factor` with a spline, applies `settings` over the user's, and the
/// model's own upscale brings the frame back to its size — a generic form of
/// the "downscale, then upscale" focus trick, available to any processor
/// whose output is an integer multiple of its input.
#[derive(Clone, Debug, Serialize)]
pub struct ScaleOption {
    pub factor: usize,
    pub settings: serde_json::Value,
}
/// Factories can move to a preview worker; sessions stay on their creating thread.
/// Optional exact source decoder supplied by a processor. Thread-confined;
/// frames retain native timestamps, finite unclipped samples and source dimensions.
pub trait FrameSource {
    fn seek(&mut self, seconds: f64) -> Result<()>;
    fn read(&mut self) -> Result<Option<VideoFrame>>;
}
pub trait Factory: Send {
    /// None means use the host decoder. Errors must not silently fall back.
    fn open_source(
        &self,
        _path: &std::path::Path,
        _width: usize,
        _height: usize,
    ) -> Result<Option<Box<dyn FrameSource>>> {
        Ok(None)
    }

    /// The integer upscales this processor can produce, each with the
    /// settings that select it (`{"scale": "2x"}`, say). Empty by default,
    /// which hides the host's Downsize input row. Cheap and side-effect free:
    /// the host reads it on the UI thread.
    fn scales(&self) -> Vec<ScaleOption> {
        Vec::new()
    }

    /// Optional controls shown by the host, independently of numeric settings.
    fn spatial_inputs(&self) -> Vec<SpatialInputDescriptor> {
        Vec::new()
    }
    /// Declarations describe the plugin, not a guarantee of host execution support.
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    /// Opt out if timestamp-preserving frame previews are unsupported.
    fn supports_preview(&self) -> bool {
        true
    }

    /// One or more categories this plugin belongs to. The default (none)
    /// exists so older plugins still build; the host lists them as
    /// uncategorized until they declare.
    fn categories(&self) -> Vec<Category> {
        Vec::new()
    }

    /// The output frame rate for an `input` rate under `settings`. Identity
    /// by default. A retiming processor (frame-rate conversion, 2x
    /// interpolation, ...) returns its target and emits frames on that grid,
    /// each with its own `pts` and `duration`, strictly increasing. The host
    /// pairs every output frame with the latest input frame at or before it,
    /// so an A/B shows each new frame against the original it follows.
    /// Called on the UI thread: keep it cheap and free of side effects.
    fn output_rate(&self, _settings: &serde_json::Value, input: Rate) -> Result<Rate> {
        Ok(input)
    }

    /// Human-readable description of this processor. Empty when unspecified.
    fn description(&self) -> &str {
        ""
    }

    /// Optional display-only build status, for example "Research build" or "Beta".
    /// None (the default) leaves the host's status label hidden. This is not a
    /// capability, execution restriction, or quality certification. Keep it cheap
    /// and side-effect free; hosts may read it while registering the catalog.
    fn build_status(&self) -> Option<&str> {
        None
    }

    fn descriptor(&self) -> Descriptor;
    fn create(
        &self,
        width: usize,
        height: usize,
        settings: &serde_json::Value,
    ) -> Result<Box<dyn Session>>;
}
#[derive(Default)]
pub struct Registry(BTreeMap<String, Box<dyn Factory>>);
impl Registry {
    pub fn register(&mut self, factory: Box<dyn Factory>) -> Result<()> {
        let id = factory.descriptor().id;
        if id.is_empty() || self.0.contains_key(&id) {
            return Err(format!("duplicate or empty processor ID: {id}"));
        }
        self.0.insert(id, factory);
        Ok(())
    }
    pub fn descriptors(&self) -> Vec<Descriptor> {
        self.0.values().map(|p| p.descriptor()).collect()
    }
    pub fn get(&self, id: &str) -> Result<&dyn Factory> {
        self.0
            .get(id)
            .map(|p| p.as_ref())
            .ok_or_else(|| format!("processor not registered: {id}"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Example;
    impl Factory for Example {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "example.identity".into(),
                name: "Example".into(),
                settings_schema: serde_json::json!({}),
                presets: vec![],
            }
        }
        fn create(&self, _: usize, _: usize, _: &serde_json::Value) -> Result<Box<dyn Session>> {
            Err("fixture only".into())
        }
    }
    #[test]
    fn duplicate_registration_is_rejected_without_replacing() {
        assert_eq!(Example.build_status(), None);
        let mut r = Registry::default();
        r.register(Box::new(Example)).unwrap();
        assert!(r.register(Box::new(Example)).is_err());
        assert_eq!(r.descriptors().len(), 1);
        assert!(r.get("missing").is_err());
    }
}
