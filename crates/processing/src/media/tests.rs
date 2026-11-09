use super::*;
use abner_processor_api::Frame;
use std::io::Cursor;
#[test]
fn ten_bit_decode_keeps_precision_below_eight_bit() {
    let f = VideoFrame {
        width: 2,
        height: 2,
        data: PixelData::Yuv16 {
            planes: [
                Arc::new(vec![500, 501, 502, 503]),
                Arc::new(vec![512; 2]),
                Arc::new(vec![512; 2]),
            ],
            chroma: filmcraft_frame::Chroma::C422,
            bits: 10,
            alpha: None,
        },
        color: filmcraft_color::ColorInfo::REC709,
        par: (1, 1),
        pts: Tick::ZERO,
    };
    let rgb = rgb16(&f).unwrap();
    assert!(
        rgb.as_chunks::<3>()
            .0
            .iter()
            .map(|p| p[0])
            .collect::<Vec<_>>()
            .windows(2)
            .all(|p| p[0] != p[1])
    );
    assert!(rgb.iter().any(|v| v % 257 != 0));
}
#[test]
fn exact_timebase_conversion_rejects_rounding() {
    assert_eq!(exact_ticks(1001, [1, 30000], 60000).unwrap(), 2002);
    assert!(exact_ticks(1, [1, 3], 1000).is_err());
}
fn fixture(path: &Path) {
    let mut mux = Mp4Writer::new(Cursor::new(Vec::new()), WriterOptions::new(Brand::Mov)).unwrap();
    let mut entry = SampleEntry::prores(FourCc(*b"apch"), 64, 64);
    entry.video.as_mut().unwrap().color = Some(mp4::ColorInfo::Nclc {
        primaries: 1,
        transfer: 1,
        matrix: 1,
    });
    let video = mux.add_track(TrackConfig::new(entry, 30000)).unwrap();
    let audio = mux
        .add_track(TrackConfig::new(
            SampleEntry::pcm(mp4::PcmConfig {
                bits: 16,
                float: false,
                big_endian: false,
                signed: true,
                channels: 1,
                sample_rate: 48000.,
            }),
            48000,
        ))
        .unwrap();
    let mut encoder = filmcraft_prores::Encoder::new(filmcraft_prores::Profile::Hq, 64, 64);
    for value in [10000, 30000, 50000] {
        let data = encoder
            .encode(&prores_frame(&vec![value; 64 * 64 * 3], 64, 64).unwrap())
            .unwrap();
        mux.write_sample(
            video,
            WriteSample {
                data: &data,
                duration: 1001,
                composition_offset: 0,
                is_sync: true,
            },
        )
        .unwrap();
    }
    let audio_data: Vec<u8> = (0..4804i16)
        .flat_map(|n| n.wrapping_mul(17).to_le_bytes())
        .collect();
    mux.write_sample(
        audio,
        WriteSample {
            data: &audio_data,
            duration: 4804,
            composition_offset: 0,
            is_sync: true,
        },
    )
    .unwrap();
    fs::write(path, mux.finish().unwrap().into_inner()).unwrap();
}
#[test]
fn native_roundtrip_preserves_audio_timing_and_never_overwrites() {
    let path = std::env::temp_dir().join(format!(
        "processing-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&path).unwrap();
    let work = Work(path);
    let input = work.0.join("source.mov");
    fixture(&input);
    let job = Job {
        input,
        output: work.0.join("output.mov"),
        stages: vec![],
        copy_audio: true,
        memory_bytes: 1024 * 1024,
    };
    let report = run(
        &job,
        &Registry::default(),
        &Cancellation::default(),
        Arc::new(|_| {}),
    )
    .unwrap();
    assert_eq!(report.frames, 3);
    assert_eq!(report.audio_tracks, 1);
    let out = mp4::open(Reader::open(&job.output).unwrap()).unwrap();
    assert_eq!(out.tracks[0].timescale, 30000);
    assert!(out.tracks[0].samples.iter().all(|s| s.duration == 1001));
    let bytes = fs::read(&job.output).unwrap();
    assert!(
        run(
            &job,
            &Registry::default(),
            &Cancellation::default(),
            Arc::new(|_| {})
        )
        .is_err()
    );
    assert_eq!(bytes, fs::read(&job.output).unwrap());
    let mut cancelled = job.clone();
    cancelled.output = work.0.join("cancelled.mov");
    let c = Cancellation::default();
    c.cancel();
    assert!(run(&cancelled, &Registry::default(), &c, Arc::new(|_| {})).is_err());
    assert!(!cancelled.output.exists());
}

struct RateFixture;
impl abner_processor_api::Factory for RateFixture {
    fn descriptor(&self) -> abner_processor_api::Descriptor {
        abner_processor_api::Descriptor {
            id: "rate-fixture".into(),
            name: "Rate fixture".into(),
            settings_schema: serde_json::json!({}),
            presets: vec![],
        }
    }
    fn output_rate(&self, _: &serde_json::Value, input: [u32; 2]) -> Result<[u32; 2]> {
        Ok([input[0] * 2, input[1]])
    }
    fn create(
        &self,
        w: usize,
        h: usize,
        settings: &serde_json::Value,
    ) -> Result<Box<dyn abner_processor_api::Session>> {
        Ok(Box::new(RateSession {
            dims: [w, h],
            fail: settings["fail"] == true,
            cancel: Cancellation::default(),
        }))
    }
}
struct RateSession {
    dims: [usize; 2],
    fail: bool,
    cancel: Cancellation,
}
impl abner_processor_api::Session for RateSession {
    fn process(&mut self, mut frame: Frame) -> Result<Vec<Frame>> {
        // Deliberately produce a discontinuity after an encoded frame on failure.
        frame.time_base[1] *= 2;
        frame.pts *= 2;
        let mut second = Frame {
            width: frame.width,
            height: frame.height,
            pixels: frame.pixels.clone(),
            pts: frame.pts,
            duration: frame.duration,
            time_base: frame.time_base,
        };
        second.pts += frame.duration + i64::from(self.fail);
        Ok(vec![frame, second])
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
    fn progress(&self) -> abner_processor_api::Progress {
        abner_processor_api::Progress {
            phase: "fixture".into(),
            completed: 0,
            total: None,
        }
    }
}
#[test]
fn retimed_output_keeps_duration_and_failed_jobs_publish_nothing() {
    let dir = std::env::temp_dir().join(format!(
        "processing-rate-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    let work = Work(dir);
    let input = work.0.join("source.mov");
    fixture(&input);
    let mut registry = Registry::default();
    registry.register(Box::new(RateFixture)).unwrap();
    let mut job = Job {
        input,
        output: work.0.join("ok.mov"),
        stages: vec![Stage {
            processor: "rate-fixture".into(),
            settings: serde_json::json!({}),
        }],
        copy_audio: true,
        memory_bytes: 1024 * 1024,
    };
    let report = run(&job, &registry, &Cancellation::default(), Arc::new(|_| {})).unwrap();
    assert_eq!(report.frames, 6);
    let output = mp4::open(Reader::open(&job.output).unwrap()).unwrap();
    assert_eq!(output.tracks[0].timescale, 60000);
    assert!(output.tracks[0].samples.iter().all(|s| s.duration == 1001));
    job.output = work.0.join("failed.mov");
    job.stages[0].settings = serde_json::json!({"fail": true});
    assert!(
        run(&job, &registry, &Cancellation::default(), Arc::new(|_| {}))
            .unwrap_err()
            .contains("discontinuous")
    );
    assert!(!job.output.exists());
    assert!(!fs::read_dir(&work.0).unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".processing-")
    }));
}

struct NativeIdentity([usize; 2]);
impl abner_processor_api::Session for NativeIdentity {
    fn format_pairs(&self) -> Vec<abner_processor_api::FormatPair> {
        vec![abner_processor_api::FormatPair::same(
            abner_processor_api::PixelFormat::Yuv422p10,
        )]
    }
    fn configure_format(&mut self, p: abner_processor_api::FormatPair) -> Result<()> {
        assert_eq!(p, self.format_pairs()[0]);
        Ok(())
    }
    fn process_video(
        &mut self,
        f: abner_processor_api::VideoFrame,
    ) -> Result<Vec<abner_processor_api::VideoFrame>> {
        assert_eq!(f.format, abner_processor_api::PixelFormat::Yuv422p10);
        Ok(vec![f])
    }
    fn finish_video(&mut self) -> Result<Vec<abner_processor_api::VideoFrame>> {
        Ok(vec![])
    }
    fn output_dimensions(&self) -> [usize; 2] {
        self.0
    }
    fn cancellation(&self) -> Cancellation {
        Cancellation::default()
    }
    fn progress(&self) -> abner_processor_api::Progress {
        abner_processor_api::Progress {
            phase: "native".into(),
            completed: 0,
            total: None,
        }
    }
}
struct NativeFactory;
impl abner_processor_api::Factory for NativeFactory {
    fn descriptor(&self) -> abner_processor_api::Descriptor {
        abner_processor_api::Descriptor {
            id: "native".into(),
            name: "Native".into(),
            settings_schema: serde_json::json!({}),
            presets: vec![],
        }
    }
    fn create(
        &self,
        w: usize,
        h: usize,
        _: &serde_json::Value,
    ) -> Result<Box<dyn abner_processor_api::Session>> {
        Ok(Box::new(NativeIdentity([w, h])))
    }
}
#[test]
fn native_yuv_plugin_matches_empty_chain_encoded_packets() {
    let dir = std::env::temp_dir().join(format!(
        "native-yuv-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    let work = Work(dir);
    let input = work.0.join("in.mov");
    fixture(&input);
    let mut job = Job {
        input,
        output: work.0.join("identity.mov"),
        stages: vec![],
        copy_audio: true,
        memory_bytes: 1024 * 1024,
    };
    let mut registry = Registry::default();
    registry.register(Box::new(NativeFactory)).unwrap();
    let c = Cancellation::default();
    run(&job, &registry, &c, Arc::new(|_| {})).unwrap();
    let reference = job.output.clone();
    job.output = work.0.join("native.mov");
    job.stages.push(Stage {
        processor: "native".into(),
        settings: serde_json::json!({}),
    });
    run(&job, &registry, &c, Arc::new(|_| {})).unwrap();
    let a = mp4::open(Reader::open(&reference).unwrap()).unwrap();
    let b = mp4::open(Reader::open(&job.output).unwrap()).unwrap();
    assert_eq!(
        packet_hash(&a, &Reader::open(&reference).unwrap(), 0, &c).unwrap(),
        packet_hash(&b, &Reader::open(&job.output).unwrap(), 0, &c).unwrap()
    );
}
