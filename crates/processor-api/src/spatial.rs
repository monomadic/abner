//! Named, immutable image-space inputs. White mask pixels select the affected region.
use crate::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialInputKind {
    Image,
    MaskedImage,
    Mask,
    Region,
}
#[derive(Clone, Debug)]
pub struct SpatialInputDescriptor {
    pub key: String,
    pub title: String,
    pub kind: SpatialInputKind,
    pub required: bool,
}
#[derive(Clone, Default)]
pub struct SpatialBinding {
    /// Display-oriented dimensions in which the editor authored these values.
    pub dimensions: [usize; 2],
    pub crop: Option<[usize; 4]>,
    /// Full image-space 8-bit coverage; underlying image samples are never erased.
    pub mask: Option<Arc<Vec<u8>>>,
    pub revision: u64,
}
pub type SpatialBindings = BTreeMap<String, SpatialBinding>;
pub type ChainBindings = BTreeMap<String, SpatialBindings>;
pub struct SpatialInput {
    pub key: String,
    pub kind: SpatialInputKind,
    pub image: Option<Frame>,
    pub mask: Option<Vec<u8>>,
    pub region: [usize; 4],
    pub revision: u64,
}
impl SpatialBinding {
    pub fn resolve(
        &self,
        descriptor: &SpatialInputDescriptor,
        frame: &Frame,
    ) -> Result<SpatialInput> {
        frame.validate()?;
        if self.dimensions != [frame.width, frame.height] {
            return Err(format!(
                "{}: input geometry changed; edit the input again",
                descriptor.title
            ));
        }
        let [x, y, w, h] = self.crop.unwrap_or([0, 0, frame.width, frame.height]);
        if w == 0
            || h == 0
            || x.checked_add(w).is_none_or(|v| v > frame.width)
            || y.checked_add(h).is_none_or(|v| v > frame.height)
        {
            return Err("Invalid spatial input crop".into());
        }
        let needs_mask = matches!(
            descriptor.kind,
            SpatialInputKind::Mask | SpatialInputKind::MaskedImage
        );
        let mask = if needs_mask {
            let m = self.mask.as_ref().ok_or("Paint the required mask first")?;
            if m.len() != frame.width * frame.height {
                return Err("Invalid mask dimensions".into());
            }
            Some(
                (y..y + h)
                    .flat_map(|row| {
                        m[row * frame.width + x..row * frame.width + x + w]
                            .iter()
                            .copied()
                    })
                    .collect(),
            )
        } else {
            None
        };
        let image = matches!(
            descriptor.kind,
            SpatialInputKind::Image | SpatialInputKind::MaskedImage
        )
        .then(|| Frame {
            width: w,
            height: h,
            pixels: (y..y + h)
                .flat_map(|row| {
                    frame.pixels[(row * frame.width + x) * 3..(row * frame.width + x + w) * 3]
                        .iter()
                        .copied()
                })
                .collect(),
            pts: frame.pts,
            duration: frame.duration,
            time_base: frame.time_base,
        });
        Ok(SpatialInput {
            key: descriptor.key.clone(),
            kind: descriptor.kind,
            image,
            mask,
            region: [x, y, w, h],
            revision: self.revision,
        })
    }
}
/// Wraps every process call, including temporal flushes downstream, with named inputs.
/// Bindings are snapshotted at job creation; edits cannot mutate an in-flight job.
pub fn create_bound(
    factory: &dyn Factory,
    width: usize,
    height: usize,
    settings: &serde_json::Value,
    bindings: SpatialBindings,
) -> Result<Box<dyn Session>> {
    let declarations = factory.spatial_inputs();
    for d in &declarations {
        if d.required && !bindings.contains_key(&d.key) {
            return Err(format!("Configure {} first", d.title));
        }
    }
    if bindings.is_empty() {
        return factory.create(width, height, settings);
    }
    Ok(Box::new(BoundSession {
        inner: factory.create(width, height, settings)?,
        declarations,
        bindings,
    }))
}
struct BoundSession {
    inner: Box<dyn Session>,
    declarations: Vec<SpatialInputDescriptor>,
    bindings: SpatialBindings,
}
impl Session for BoundSession {
    fn configure_format(&mut self, pair: crate::FormatPair) -> Result<()> {
        if pair != crate::FormatPair::RGB16 || !self.inner.format_pairs().contains(&pair) {
            return Err("spatial inputs require an RGB16-capable session".into());
        }
        self.inner.configure_format(pair)
    }

    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let inputs = self
            .declarations
            .iter()
            .filter_map(|d| self.bindings.get(&d.key).map(|b| b.resolve(d, &frame)))
            .collect::<Result<Vec<_>>>()?;
        self.inner.process_with_inputs(frame, inputs)
    }
    fn finish(&mut self) -> Result<Vec<Frame>> {
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
    fn set_preview_target(&mut self, p: i64, t: [u32; 2]) {
        self.inner.set_preview_target(p, t)
    }
    fn set_progress_reporter(&mut self, r: ProgressReporter) {
        self.inner.set_progress_reporter(r)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cropped_mask_preserves_image_and_timing() {
        let f = Frame {
            width: 3,
            height: 2,
            pixels: (0..18).collect(),
            pts: 7,
            duration: 1,
            time_base: [1, 24],
        };
        let b = SpatialBinding {
            dimensions: [3, 2],
            crop: Some([1, 0, 2, 2]),
            mask: Some(Arc::new(vec![0, 255, 0, 0, 0, 255])),
            revision: 2,
        };
        let d = SpatialInputDescriptor {
            key: "repair".into(),
            title: "Repair".into(),
            kind: SpatialInputKind::MaskedImage,
            required: true,
        };
        let v = b.resolve(&d, &f).unwrap();
        let image = v.image.unwrap();
        assert_eq!(image.pixels, vec![3, 4, 5, 6, 7, 8, 12, 13, 14, 15, 16, 17]);
        assert_eq!(v.mask.unwrap(), vec![255, 0, 0, 255]);
        assert_eq!(image.pts, 7);
        assert_eq!(f.pixels.len(), 18);
        let mut bad = b.clone();
        bad.dimensions = [2, 3];
        assert!(bad.resolve(&d, &f).is_err());
        bad = b;
        bad.crop = Some([usize::MAX, 0, 1, 1]);
        assert!(bad.resolve(&d, &f).is_err());
    }
}
