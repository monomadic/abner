use abner_processor_api::*;
pub struct Identity;
struct S { w: usize, h: usize, n: u64, c: Cancellation, reporter: Option<ProgressReporter> }
impl Session for S {
    fn set_progress_reporter(&mut self, reporter: ProgressReporter) { self.reporter = Some(reporter); }
    fn process(&mut self, input: Frame) -> Result<Vec<Frame>> { self.c.check()?; self.n += 1; if let Some(report) = &self.reporter { report(Progress { phase: "Passing through frame".into(), completed: 1, total: Some(1) }); } Ok(vec![input]) }
    fn finish(&mut self) -> Result<Vec<Frame>> { Ok(vec![]) }
    fn output_dimensions(&self) -> [usize; 2] { [self.w, self.h] }
    fn cancellation(&self) -> Cancellation { self.c.clone() }
    fn progress(&self) -> Progress { Progress { phase: "run".into(), completed: self.n, total: None } }
}
impl Factory for Identity {
    fn capabilities(&self) -> Capabilities {
        Capabilities { role: Role::Processor, input: InputRequirement::IndependentFrame }
    }
    fn categories(&self) -> Vec<Category> { vec![] }

    fn descriptor(&self) -> Descriptor {
        Descriptor { id: "example.identity".into(), name: "Identity".into(), settings_schema: serde_json::json!({}), presets: vec![] }
    }
    fn create(&self, w: usize, h: usize, _: &serde_json::Value) -> Result<Box<dyn Session>> {
        Ok(Box::new(S { w, h, n: 0, reporter: None, c: Cancellation::default() }))
    }
}
pub fn factories() -> Vec<Box<dyn Factory>> { vec![Box::new(Identity)] }
