//! Whole-chain format selection and execution, including buffered tails.
use crate::color;
use abner_processor_api::{Cancellation, FormatPair, PixelFormat, Result, Session, VideoFrame};

/// Conversion cost prioritizes precision, then chroma detail, then conversion count.
/// Plugin-native input→output changes are also included, so a low-depth pair is
/// not selected just to save a conversion elsewhere in the chain.
type Cost = (u64, u64, u64);
fn add(a: Cost, b: Cost) -> Cost {
    (a.0 + b.0, a.1 + b.1, a.2 + b.2)
}
fn cost(a: PixelFormat, b: PixelFormat, source: PixelFormat) -> Cost {
    if a == b {
        return (0, 0, 0);
    }
    let subs = |f: PixelFormat| f.subsampling().map_or(1, |(x, y)| x * y) as u64;
    // Expanding 10-bit source samples to RGB16 does not create six additional
    // source bits to lose on the way back to 10-bit. Rank losses against source
    // precision; never prefer an early 8-bit bottleneck to satisfy an 8-bit sink.
    (
        u64::from(a.depth().min(source.depth()).saturating_sub(b.depth())),
        subs(b).saturating_sub(subs(a).max(subs(source))),
        1,
    )
}
#[derive(Clone, Debug)]
pub struct FormatPlan {
    pub source: PixelFormat,
    pub sink: PixelFormat,
    pub stages: Vec<FormatPair>,
}
impl FormatPlan {
    pub fn select(
        source: PixelFormat,
        sink: PixelFormat,
        options: &[Vec<FormatPair>],
    ) -> Result<Self> {
        let mut paths = vec![((0, 0, 0), source, Vec::new())];
        for choices in options {
            if choices.is_empty() {
                return Err("processor declared no format pairs".into());
            }
            let mut next = Vec::new();
            for &pair in choices {
                let (score, previous, path) = paths
                    .iter()
                    .min_by_key(|(n, f, _)| add(*n, cost(*f, pair.input, source)))
                    .ok_or("no format path")?;
                let mut path = path.clone();
                path.push(pair);
                next.push((
                    add(
                        add(*score, cost(*previous, pair.input, source)),
                        cost(pair.input, pair.output, source),
                    ),
                    pair.output,
                    path,
                ));
            }
            paths = next;
        }
        let (_, _, stages) = paths
            .into_iter()
            .min_by_key(|(n, f, _)| add(*n, (0, 0, u64::from(*f != sink))))
            .ok_or("no format path")?;
        Ok(Self {
            source,
            sink,
            stages,
        })
    }
    pub fn new(
        sessions: &mut [Box<dyn Session>],
        source: PixelFormat,
        sink: PixelFormat,
    ) -> Result<Self> {
        let plan = Self::select(
            source,
            sink,
            &sessions
                .iter()
                .map(|s| s.format_pairs())
                .collect::<Vec<_>>(),
        )?;
        for (s, pair) in sessions.iter_mut().zip(&plan.stages) {
            s.configure_format(*pair)?;
        }
        Ok(plan)
    }
    fn validate(
        &self,
        frames: &[VideoFrame],
        format: PixelFormat,
        dims: Option<[usize; 2]>,
        budget: usize,
    ) -> Result<()> {
        let mut bytes = 0usize;
        for f in frames {
            f.validate()?;
            if f.format != format || dims.is_some_and(|d| d != [f.width, f.height]) {
                return Err("processor violated negotiated format or dimensions".into());
            }
            bytes = bytes
                .checked_add(f.byte_len())
                .ok_or("batch size overflow")?;
            if bytes > budget {
                return Err("Processor batch exceeds the memory budget".into());
            }
        }
        Ok(())
    }
    /// Convert only after the last processor, for a single-sink job.
    pub fn process(
        &self,
        sessions: &mut [Box<dyn Session>],
        from: usize,
        frames: Vec<VideoFrame>,
        cancel: &Cancellation,
        budget: usize,
    ) -> Result<Vec<VideoFrame>> {
        let frames = self.process_native(sessions, from, frames, cancel, budget)?;
        self.to_sink(frames, budget)
    }
    fn to_sink(&self, frames: Vec<VideoFrame>, budget: usize) -> Result<Vec<VideoFrame>> {
        let out = frames
            .into_iter()
            .map(|f| color::convert(f, self.sink))
            .collect::<Result<Vec<_>>>()?;
        self.validate(&out, self.sink, None, budget)?;
        Ok(out)
    }
    /// Keep the final processor's format when a host branches to display and
    /// encode. Each sink converts from this frame independently, avoiding a
    /// display round trip through the encoder's subsampled format.
    pub fn process_native(
        &self,
        sessions: &mut [Box<dyn Session>],
        from: usize,
        mut frames: Vec<VideoFrame>,
        cancel: &Cancellation,
        budget: usize,
    ) -> Result<Vec<VideoFrame>> {
        if sessions.len() != self.stages.len() || from > sessions.len() {
            return Err("invalid negotiated chain position".into());
        }
        cancel.check()?;
        let expected = if from == 0 {
            self.source
        } else {
            self.stages[from - 1].output
        };
        self.validate(&frames, expected, None, budget)?;
        for (session, pair) in sessions[from..].iter_mut().zip(&self.stages[from..]) {
            let mut outputs = Vec::new();
            for f in frames {
                cancel.check()?;
                let f = color::convert(f, pair.input)?;
                self.validate(std::slice::from_ref(&f), pair.input, None, budget)?;
                let next = session.process_video(f)?;
                self.validate(
                    &next,
                    pair.output,
                    Some(session.output_dimensions()),
                    budget,
                )?;
                outputs.extend(next);
                self.validate(
                    &outputs,
                    pair.output,
                    Some(session.output_dimensions()),
                    budget,
                )?;
            }
            frames = outputs;
        }
        cancel.check()?;
        Ok(frames)
    }
    pub fn finish_stage(
        &self,
        sessions: &mut [Box<dyn Session>],
        stage: usize,
        cancel: &Cancellation,
        budget: usize,
    ) -> Result<Vec<VideoFrame>> {
        let frames = self.finish_stage_native(sessions, stage, cancel, budget)?;
        self.to_sink(frames, budget)
    }
    pub fn finish_stage_native(
        &self,
        sessions: &mut [Box<dyn Session>],
        stage: usize,
        cancel: &Cancellation,
        budget: usize,
    ) -> Result<Vec<VideoFrame>> {
        cancel.check()?;
        if sessions.len() != self.stages.len() || stage >= sessions.len() {
            return Err("invalid negotiated chain position".into());
        }
        let session = &mut sessions[stage];
        let tail = session.finish_video()?;
        self.validate(
            &tail,
            self.stages[stage].output,
            Some(session.output_dimensions()),
            budget,
        )?;
        self.process_native(sessions, stage + 1, tail, cancel, budget)
    }
}

/// Host-owned RGB16 boundary for operations such as crop/downsize. The inner
/// plugin still receives its chosen native format, including buffered tails.
pub struct Rgb16Boundary {
    sessions: Vec<Box<dyn Session>>,
    plan: FormatPlan,
    budget: usize,
}
impl Rgb16Boundary {
    pub fn new(session: Box<dyn Session>, budget: usize) -> Result<Self> {
        let mut sessions = vec![session];
        let plan = FormatPlan::new(&mut sessions, PixelFormat::Rgb16, PixelFormat::Rgb16)?;
        Ok(Self {
            sessions,
            plan,
            budget,
        })
    }
}
impl Session for Rgb16Boundary {
    fn process(
        &mut self,
        input: abner_processor_api::Frame,
    ) -> Result<Vec<abner_processor_api::Frame>> {
        let cancel = self.cancellation();
        self.plan
            .process(
                &mut self.sessions,
                0,
                vec![input.into()],
                &cancel,
                self.budget,
            )?
            .into_iter()
            .map(VideoFrame::into_rgb16)
            .collect()
    }
    fn finish(&mut self) -> Result<Vec<abner_processor_api::Frame>> {
        let cancel = self.cancellation();
        self.plan
            .finish_stage(&mut self.sessions, 0, &cancel, self.budget)?
            .into_iter()
            .map(VideoFrame::into_rgb16)
            .collect()
    }
    fn output_dimensions(&self) -> [usize; 2] {
        self.sessions[0].output_dimensions()
    }
    fn cancellation(&self) -> Cancellation {
        self.sessions[0].cancellation()
    }
    fn progress(&self) -> abner_processor_api::Progress {
        self.sessions[0].progress()
    }
    fn set_progress_reporter(&mut self, r: abner_processor_api::ProgressReporter) {
        self.sessions[0].set_progress_reporter(r)
    }
    fn set_preview_target(&mut self, p: i64, t: [u32; 2]) {
        self.sessions[0].set_preview_target(p, t)
    }
}

#[cfg(test)]
mod tests;
