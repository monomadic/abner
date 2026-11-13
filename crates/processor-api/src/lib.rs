//! Streaming CPU frame contracts, including optional host-negotiated layouts.
//! No media implementation dependencies.
use serde::Serialize;
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
#[derive(Clone, Serialize)]
pub struct Progress {
    pub phase: String,
    pub completed: u64,
    pub total: Option<u64>,
}
/// Optional in-flight updates, including work inside a single process() call.
/// The host callback must return promptly. Percentages describe this phase only.
pub type ProgressReporter = Arc<dyn Fn(Progress) + Send + Sync>;

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
    /// Implementations may retain this callback to report progress during work.
    fn set_progress_reporter(&mut self, _reporter: ProgressReporter) {}
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
    fn output_dimensions(&self) -> [usize; 2];
    fn cancellation(&self) -> Cancellation;
    /// Completed output frames; the host owns timeline totals and job-wide progress.
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
        let mut r = Registry::default();
        r.register(Box::new(Example)).unwrap();
        assert!(r.register(Box::new(Example)).is_err());
        assert_eq!(r.descriptors().len(), 1);
        assert!(r.get("missing").is_err());
    }
}
