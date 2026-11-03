//! Generic host integration example: original image plus independently cropped and masked inputs.
//! cargo run --example spatial-inputs -- --processor example.spatial clip.mp4
use abner_processor_api::*;
struct Example;
impl Factory for Example {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: "example.spatial".into(),
            name: "Spatial inputs example".into(),
            settings_schema: serde_json::json!({"type":"object","properties":{}}),
            presets: vec![],
        }
    }
    fn spatial_inputs(&self) -> Vec<SpatialInputDescriptor> {
        [
            ("reference", "Reference image", SpatialInputKind::Image),
            ("repair", "Affected image", SpatialInputKind::MaskedImage),
            ("region", "Processing region", SpatialInputKind::Region),
        ]
        .into_iter()
        .map(|(key, title, kind)| SpatialInputDescriptor {
            key: key.into(),
            title: title.into(),
            kind,
            required: false,
        })
        .collect()
    }
    fn create(&self, w: usize, h: usize, _: &serde_json::Value) -> Result<Box<dyn Session>> {
        Ok(Box::new(ExampleSession {
            dims: [w, h],
            cancel: Cancellation::default(),
        }))
    }
}
struct ExampleSession {
    dims: [usize; 2],
    cancel: Cancellation,
}
impl Session for ExampleSession {
    fn process(&mut self, f: Frame) -> Result<Vec<Frame>> {
        Ok(vec![f])
    }
    fn process_with_inputs(
        &mut self,
        mut f: Frame,
        inputs: Vec<SpatialInput>,
    ) -> Result<Vec<Frame>> {
        // Tint selected samples without changing dimensions. The original frame
        // and every named image remain independently available to the processor.
        for input in inputs {
            if input.key != "repair" {
                continue;
            }
            if let Some(mask) = input.mask {
                let [x, y, w, h] = input.region;
                for row in 0..h {
                    for col in 0..w {
                        let alpha = mask[row * w + col] as u32;
                        let offset = ((y + row) * f.width + x + col) * 3;
                        for (channel, target) in [65535, 18000, 18000].into_iter().enumerate() {
                            f.pixels[offset + channel] =
                                ((f.pixels[offset + channel] as u32 * (255 - alpha)
                                    + target * alpha)
                                    / 255) as u16;
                        }
                    }
                }
            }
        }
        Ok(vec![f])
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
        Ok(vec![])
    }
    fn output_dimensions(&self) -> [usize; 2] {
        self.dims
    }
    fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    fn progress(&self) -> Progress {
        Progress {
            phase: "Ready".into(),
            completed: 0,
            total: None,
        }
    }
}
fn main() -> anyhow::Result<()> {
    let mut registry = Registry::default();
    registry
        .register(Box::new(Example))
        .map_err(anyhow::Error::msg)?;
    abner::run(registry)
}
