//! Experimental streaming CPU frame contract. No implementation dependencies.
use serde::Serialize;
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
#[derive(Clone, Serialize)]
pub struct Descriptor {
    pub id: String,
    pub name: String,
    pub settings_schema: serde_json::Value,
    pub presets: Vec<Preset>,
}
/// Packed full-range RGB16, SDR Rec.709; no alpha. Other formats must be rejected
/// or explicitly converted by the host before using this prototype interface.
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
/// Ordered streaming frames. Calls may emit zero or several outputs. After EOF,
/// call finish once and consume all returned frames. Drop/recreate a session at
/// cuts, seeks or configuration changes: recurrent history must not cross them.
/// Construct and use on one thread; only the cancellation handle is thread-safe.
/// Errors are terminal: discard this session and any unpublished job output.
/// CPU RGB16 only; this contract does not yet negotiate GPU/color/auxiliary inputs.
pub trait Session {
    /// Implementations may retain this callback to report progress during work.
    fn set_progress_reporter(&mut self, _reporter: ProgressReporter) {}
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>>;
    fn finish(&mut self) -> Result<Vec<Frame>>;
    fn output_dimensions(&self) -> [usize; 2];
    fn cancellation(&self) -> Cancellation;
    /// Completed output frames; the host owns timeline totals and job-wide progress.
    fn progress(&self) -> Progress;
}
/// Factories can move to a preview worker; sessions stay on their creating thread.
pub trait Factory: Send {
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
