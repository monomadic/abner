//! Small synthetic checks shared by decoder, preview and export tests.
use abner_processor_api::{
    Cancellation, Descriptor, Factory, FormatPair, Frame, PixelFormat, Progress, Session,
    VideoFrame,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(crate) struct Fixture {
    pub dir: PathBuf,
    pub path: PathBuf,
}
impl Fixture {
    pub fn new() -> Self {
        use filmcraft_isobmff as mp4;
        let dir = std::env::temp_dir().join(format!(
            "abner-native-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("gradient.mov");
        let mut mux = mp4::Mp4Writer::new(
            std::io::Cursor::new(Vec::new()),
            mp4::WriterOptions::new(mp4::Brand::Mov),
        )
        .unwrap();
        let mut entry = mp4::SampleEntry::prores(mp4::FourCc(*b"apch"), 64, 64);
        entry.video.as_mut().unwrap().color = Some(mp4::ColorInfo::Nclc {
            primaries: 1,
            transfer: 1,
            matrix: 1,
        });
        let track = mux.add_track(mp4::TrackConfig::new(entry, 24000)).unwrap();
        let mut enc = filmcraft_prores::Encoder::new(filmcraft_prores::Profile::Hq, 64, 64);
        for f in 0..12 {
            let mut frame = filmcraft_prores::Frame::new(
                64,
                64,
                filmcraft_prores::ChromaFormat::Yuv422,
                10,
                false,
            );
            for y in 0..64 {
                for x in 0..64 {
                    frame.y[y * 64 + x] = 256 + ((x + 3 * y + f) % 256) as u16;
                }
            }
            let bytes = enc.encode(&frame).unwrap();
            mux.write_sample(
                track,
                mp4::WriteSample {
                    data: &bytes,
                    duration: 1001,
                    composition_offset: 0,
                    is_sync: true,
                },
            )
            .unwrap();
        }
        std::fs::write(&path, mux.finish().unwrap().into_inner()).unwrap();
        Self { dir, path }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) struct Observe {
    pub format: PixelFormat,
    pub frames: Arc<Mutex<Vec<VideoFrame>>>,
}
struct ObserveSession {
    format: PixelFormat,
    frames: Arc<Mutex<Vec<VideoFrame>>>,
    pending: Option<VideoFrame>,
    dims: [usize; 2],
    cancel: Cancellation,
}
impl Factory for Observe {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            id: "fixture.observe".into(),
            name: "Observe".into(),
            settings_schema: serde_json::json!({}),
            presets: vec![],
        }
    }
    fn create(
        &self,
        w: usize,
        h: usize,
        _: &serde_json::Value,
    ) -> Result<Box<dyn Session>, String> {
        Ok(Box::new(ObserveSession {
            format: self.format,
            frames: self.frames.clone(),
            pending: None,
            dims: [w, h],
            cancel: Cancellation::default(),
        }))
    }
}
impl Session for ObserveSession {
    fn format_pairs(&self) -> Vec<FormatPair> {
        vec![FormatPair::same(self.format)]
    }
    fn configure_format(&mut self, p: FormatPair) -> Result<(), String> {
        assert_eq!(p, FormatPair::same(self.format));
        Ok(())
    }
    fn process_video(&mut self, f: VideoFrame) -> Result<Vec<VideoFrame>, String> {
        assert_eq!(f.format, self.format);
        f.validate()?;
        self.frames.lock().unwrap().push(f.clone());
        Ok(self.pending.replace(f).into_iter().collect())
    }
    fn finish_video(&mut self) -> Result<Vec<VideoFrame>, String> {
        Ok(self.pending.take().into_iter().collect())
    }
    fn process(&mut self, f: Frame) -> Result<Vec<Frame>, String> {
        self.process_video(f.into())?
            .into_iter()
            .map(VideoFrame::into_rgb16)
            .collect()
    }
    fn finish(&mut self) -> Result<Vec<Frame>, String> {
        self.finish_video()?
            .into_iter()
            .map(VideoFrame::into_rgb16)
            .collect()
    }
    fn output_dimensions(&self) -> [usize; 2] {
        self.dims
    }
    fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    fn progress(&self) -> Progress {
        Progress {
            phase: "observe".into(),
            completed: self.frames.lock().unwrap().len() as u64,
            total: Some(12),
        }
    }
}

pub(crate) fn reference(path: &std::path::Path, index: u64) -> VideoFrame {
    let src = crate::fc::open(path).unwrap();
    let f = crate::fc::frame_at(
        &src,
        index,
        1001. / 24000.,
        &Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    abner_processing::color::from_decoded(&f, 0, 1, [1, 24]).unwrap()
}

#[test]
fn export_receives_native_planes_and_crops_without_reducing_depth() {
    use abner_processor_api::{FramePixels, Registry};
    let fixture = Fixture::new();
    let info = crate::probe::probe(&fixture.path).unwrap();
    for (format, crop) in [
        (PixelFormat::Yuv422p10, None),
        (PixelFormat::Rgb16, None),
        (PixelFormat::Rgb16, Some((1, 1, 32, 32))),
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::default();
        registry
            .register(Box::new(Observe {
                format,
                frames: seen.clone(),
            }))
            .unwrap();
        let dest = fixture
            .dir
            .join(format!("{format:?}-{}.mkv", crop.is_some()));
        let mut displayed = 0;
        crate::media_io::pipeline_export(
            &info,
            &dest,
            &[(0., 12. * 1001. / 24000.)],
            &registry,
            &[("fixture.observe".into(), serde_json::json!({}))],
            true,
            crop,
            &Cancellation::default(),
            &Mutex::default(),
            |_| {
                displayed += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(displayed, 12);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 12);
        for (i, f) in seen.iter().enumerate() {
            assert_eq!(f.pts, ((i as f64) * 1001. / 24000. * 1e6).round() as i64);
            let reference = reference(&fixture.path, i as u64);
            if let FramePixels::Yuv16(p) = &f.pixels {
                let FramePixels::Yuv16(expected) = reference.pixels else {
                    panic!("reference is not YUV10")
                };
                assert_eq!(p, &expected);
                assert!(p[0].iter().any(|v| v % 4 != 0));
            } else {
                let reference = abner_processing::color::convert(reference, PixelFormat::Rgb16)
                    .unwrap()
                    .into_rgb16()
                    .unwrap();
                let expected: Vec<_> = if crop.is_some() {
                    (1..33)
                        .flat_map(|y| {
                            reference.pixels[y * 64 * 3 + 3..y * 64 * 3 + 33 * 3]
                                .iter()
                                .copied()
                        })
                        .collect()
                } else {
                    reference.pixels
                };
                let FramePixels::Rgb16(p) = &f.pixels else {
                    panic!("expected RGB16")
                };
                assert_eq!(p, &expected);
                assert!(p.iter().any(|v| v % 257 != 0));
            }
        }
        let decoded = reference(&dest, 0);
        assert!(matches!(decoded.pixels, FramePixels::Yuv16(_)));
    }
}
