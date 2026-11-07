use super::*;
use abner_processor_api::{Descriptor, Factory, Progress};
struct Buffered {
    pending: Option<Frame>,
    cancel: Cancellation,
    delta: u16,
}
impl Session for Buffered {
    fn process(&mut self, mut f: Frame) -> Result<Vec<Frame>> {
        self.cancel.check()?;
        for p in &mut f.pixels {
            *p += self.delta;
        }
        Ok(self.pending.replace(f).into_iter().collect())
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        Ok(self.pending.take().into_iter().collect())
    }
    fn output_dimensions(&self) -> [usize; 2] {
        [2, 2]
    }
    fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    fn progress(&self) -> Progress {
        Progress {
            phase: "fixture".into(),
            completed: 0,
            total: None,
        }
    }
}
fn frame(pts: i64) -> Frame {
    Frame {
        width: 2,
        height: 2,
        pixels: vec![100; 12],
        pts,
        duration: 1,
        time_base: [1, 24],
    }
}
#[test]
fn buffered_chain_drains_in_order_and_preserves_low_bits() {
    let cancel = Cancellation::default();
    let mut sessions: Vec<Box<dyn Session>> = (1..=2)
        .map(|delta| {
            Box::new(Buffered {
                pending: None,
                cancel: cancel.clone(),
                delta,
            }) as Box<dyn Session>
        })
        .collect();
    let mut out = vec![];
    for i in 0..3 {
        out.extend(process_chain(&mut sessions, 0, vec![frame(i)], &cancel, 1024).unwrap());
    }
    for i in 0..sessions.len() {
        out.extend(finish_stage(&mut sessions, i, &cancel, 1024).unwrap());
    }
    assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), [0, 1, 2]);
    assert!(out.iter().all(|f| f.pixels == vec![103; 12]));
}
#[test]
fn cancelled_and_oversized_batches_are_rejected_even_for_empty_chains() {
    let c = Cancellation::default();
    assert!(process_chain(&mut [], 0, vec![frame(0)], &c, 23).is_err());
    assert!(process_chain(&mut [], 1, vec![], &c, 1024).is_err());
    c.cancel();
    assert!(process_chain(&mut [], 0, vec![frame(0)], &c, 1024).is_err());
}
struct Fixture;
impl Factory for Fixture {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: "fixture".into(),
            name: "fixture".into(),
            settings_schema: serde_json::json!({}),
            presets: vec![],
        }
    }
    fn create(&self, _: usize, _: usize, _: &serde_json::Value) -> Result<Box<dyn Session>> {
        Ok(Box::new(Buffered {
            pending: None,
            cancel: Cancellation::default(),
            delta: 1,
        }))
    }
}
#[test]
fn chain_is_terminal_after_finish_or_failure() {
    let mut registry = Registry::default();
    registry.register(Box::new(Fixture)).unwrap();
    let stages = [Stage {
        processor: "fixture".into(),
        settings: serde_json::json!({}),
    }];
    let mut c = Chain::new(&registry, &stages, 2, 2, 1024, Cancellation::default()).unwrap();
    c.process(frame(0)).unwrap();
    let mut out = vec![];
    c.finish(|f| {
        out.extend(f);
        Ok(())
    })
    .unwrap();
    assert_eq!(out.len(), 1);
    assert!(c.finish(|_| Ok(())).is_err());
    assert!(c.process(frame(1)).is_err());
    let mut c = Chain::new(&registry, &stages, 2, 2, 1024, Cancellation::default()).unwrap();
    let mut bad = frame(0);
    bad.pixels.pop();
    assert!(c.process(bad).is_err());
    assert!(c.process(frame(1)).is_err());
}
