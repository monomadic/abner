//! Headless processor execution shared by GUI hosts and command-line launchers.
//! Plugins own their algorithms; this crate owns chain execution and media jobs.
pub mod cli;
pub mod color;
pub mod media;
pub mod negotiation;
use abner_processor_api::{Cancellation, Frame, InputRequirement, Registry, Result, Role, Session};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stage {
    pub processor: String,
    #[serde(default = "empty_settings")]
    pub settings: serde_json::Value,
}
fn empty_settings() -> serde_json::Value {
    serde_json::json!({})
}

/// Enforce the shared frame contract and bound every returned batch.
fn validate_batch(frames: &[Frame], budget: usize) -> Result<()> {
    let mut bytes = 0usize;
    for frame in frames {
        frame.validate()?;
        bytes = bytes
            .checked_add(
                frame
                    .pixels
                    .len()
                    .checked_mul(2)
                    .ok_or("frame size overflow")?,
            )
            .ok_or("batch size overflow")?;
        if bytes > budget {
            return Err("Processor batch exceeds the memory budget".into());
        }
    }
    Ok(())
}

/// Run a batch through the remaining stages. Sessions stay on their owning thread.
/// Errors are terminal: the caller must discard all sessions and unpublished output.
pub fn process_chain(
    sessions: &mut [Box<dyn Session>],
    from: usize,
    mut frames: Vec<Frame>,
    cancel: &Cancellation,
    budget: usize,
) -> Result<Vec<Frame>> {
    if from > sessions.len() {
        return Err("invalid chain position".into());
    }
    cancel.check()?;
    validate_batch(&frames, budget)?;
    for session in &mut sessions[from..] {
        let dims = session.output_dimensions();
        let mut outputs = Vec::new();
        for frame in frames {
            cancel.check()?;
            let next = session.process(frame)?;
            validate_batch(&next, budget)?;
            if next.iter().any(|f| [f.width, f.height] != dims) {
                return Err("processor changed declared output dimensions".into());
            }
            outputs.extend(next);
            validate_batch(&outputs, budget)?;
        }
        frames = outputs;
    }
    cancel.check()?;
    Ok(frames)
}

/// Flush one stage and pass its tail through downstream stages. Hosts may stop a
/// targeted preview between stages; complete jobs drain every stage in order.
pub fn finish_stage(
    sessions: &mut [Box<dyn Session>],
    stage: usize,
    cancel: &Cancellation,
    budget: usize,
) -> Result<Vec<Frame>> {
    cancel.check()?;
    let session = sessions.get_mut(stage).ok_or("invalid chain position")?;
    let dims = session.output_dimensions();
    let tail = session.finish()?;
    validate_batch(&tail, budget)?;
    if tail.iter().any(|f| [f.width, f.height] != dims) {
        return Err("processor changed declared output dimensions".into());
    }
    process_chain(sessions, stage + 1, tail, cancel, budget)
}

/// An owned chain for complete streaming jobs. Drop/recreate at cuts or seeks.
/// Keeps terminal state so failed/finished sessions cannot accidentally be reused.
pub struct Chain {
    sessions: Vec<Box<dyn Session>>,
    cancellation: Cancellation,
    budget: usize,
    dimensions: [usize; 2],
    closed: bool,
    started: bool,
    plan: Option<negotiation::FormatPlan>,
}
impl Chain {
    pub fn new(
        registry: &Registry,
        stages: &[Stage],
        width: usize,
        height: usize,
        budget: usize,
        cancellation: Cancellation,
    ) -> Result<Self> {
        cancellation.check()?;
        if width == 0 || height == 0 || width > 8192 || height > 8192 || width * height * 6 > budget
        {
            return Err("input dimensions exceed frame budget".into());
        }
        // Check all declarations before loading any plugin/model.
        for stage in stages {
            let factory = registry.get(&stage.processor)?;
            let caps = factory.capabilities();
            if !matches!(caps.role, Role::Processor)
                || matches!(caps.input, InputRequirement::WholeClip)
            {
                return Err(format!(
                    "{} requires a different execution contract",
                    stage.processor
                ));
            }
        }
        let mut dimensions = [width, height];
        let mut sessions = Vec::new();
        for stage in stages {
            cancellation.check()?;
            let session = registry.get(&stage.processor)?.create(
                dimensions[0],
                dimensions[1],
                &stage.settings,
            )?;
            dimensions = session.output_dimensions();
            if dimensions.contains(&0)
                || dimensions.iter().any(|&v| v > 8192)
                || dimensions[0] * dimensions[1] * 6 > budget
            {
                return Err("processor dimensions exceed frame budget".into());
            }
            sessions.push(session);
        }
        Ok(Self {
            sessions,
            cancellation,
            budget,
            dimensions,
            closed: false,
            started: false,
            plan: None,
        })
    }
    /// Negotiate once before feeding a segment. Recreate the chain at format changes.
    pub fn negotiate(
        &mut self,
        source: abner_processor_api::PixelFormat,
        sink: abner_processor_api::PixelFormat,
    ) -> Result<()> {
        if self.closed || self.started || self.plan.is_some() {
            return Err("chain already negotiated or closed".into());
        }
        match negotiation::FormatPlan::new(&mut self.sessions, source, sink) {
            Ok(plan) => {
                self.plan = Some(plan);
                Ok(())
            }
            Err(e) => {
                self.closed = true;
                Err(e)
            }
        }
    }
    pub fn format_plan(&self) -> Option<&negotiation::FormatPlan> {
        self.plan.as_ref()
    }
    pub fn process_video(
        &mut self,
        frame: abner_processor_api::VideoFrame,
    ) -> Result<Vec<abner_processor_api::VideoFrame>> {
        if self.closed {
            return Err("processing chain is closed".into());
        }
        let plan = self
            .plan
            .as_ref()
            .ok_or("negotiate formats before processing")?;
        self.started = true;
        let result = plan.process(
            &mut self.sessions,
            0,
            vec![frame],
            &self.cancellation,
            self.budget,
        );
        if result.is_err() {
            self.closed = true;
        }
        result
    }
    pub fn finish_video(
        &mut self,
        mut accept: impl FnMut(Vec<abner_processor_api::VideoFrame>) -> Result<()>,
    ) -> Result<()> {
        if self.closed {
            return Err("processing chain is closed".into());
        }
        let plan = self
            .plan
            .as_ref()
            .ok_or("negotiate formats before processing")?;
        self.closed = true;
        for i in 0..self.sessions.len() {
            accept(plan.finish_stage(&mut self.sessions, i, &self.cancellation, self.budget)?)?;
        }
        self.cancellation.check()
    }
    pub fn dimensions(&self) -> [usize; 2] {
        self.dimensions
    }
    pub fn set_progress_reporter(&mut self, reporter: abner_processor_api::ProgressReporter) {
        for session in &mut self.sessions {
            session.set_progress_reporter(reporter.clone());
        }
    }
    pub fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        if self.closed || self.plan.is_some() {
            return Err("processing chain is closed or uses negotiated frames".into());
        }
        self.started = true;
        let result = process_chain(
            &mut self.sessions,
            0,
            vec![frame],
            &self.cancellation,
            self.budget,
        );
        if result.is_err() {
            self.closed = true;
        }
        result
    }
    pub fn finish(&mut self, mut accept: impl FnMut(Vec<Frame>) -> Result<()>) -> Result<()> {
        if self.closed || self.plan.is_some() {
            return Err("processing chain is closed or uses negotiated frames".into());
        }
        self.closed = true;
        for i in 0..self.sessions.len() {
            accept(finish_stage(
                &mut self.sessions,
                i,
                &self.cancellation,
                self.budget,
            )?)?;
        }
        self.cancellation.check()
    }
}
impl Drop for Chain {
    fn drop(&mut self) {
        for s in &self.sessions {
            s.cancellation().cancel();
        }
    }
}

#[cfg(test)]
mod tests;
