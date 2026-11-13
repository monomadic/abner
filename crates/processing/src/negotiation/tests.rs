use super::*;
use PixelFormat::*;
use abner_processor_api::{FramePixels, Progress};
use std::sync::Arc;
fn rgba(pts: i64) -> VideoFrame {
    VideoFrame {
        width: 2,
        height: 2,
        format: Rgba8,
        pixels: FramePixels::Rgba8(Arc::new([10, 20, 30, 255].repeat(4))),
        pts,
        duration: 1,
        time_base: [1, 24],
    }
}
struct Buffer {
    pair: FormatPair,
    pending: Option<VideoFrame>,
    cancel: Cancellation,
    bad: bool,
}
impl Session for Buffer {
    fn format_pairs(&self) -> Vec<FormatPair> {
        vec![self.pair]
    }
    fn configure_format(&mut self, p: FormatPair) -> Result<()> {
        if p == self.pair {
            Ok(())
        } else {
            Err("wrong pair".into())
        }
    }
    fn process_video(&mut self, f: VideoFrame) -> Result<Vec<VideoFrame>> {
        assert_eq!(f.format, self.pair.input);
        // Fixture only: identity plugins need no conversion code.
        assert_eq!(self.pair.input, self.pair.output);
        Ok(self.pending.replace(f).into_iter().collect())
    }
    fn finish_video(&mut self) -> Result<Vec<VideoFrame>> {
        let mut out: Vec<_> = self.pending.take().into_iter().collect();
        if self.bad {
            out[0].format = Rgb16;
        }
        Ok(out)
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
fn session(format: PixelFormat) -> Box<dyn Session> {
    Box::new(Buffer {
        pair: FormatPair::same(format),
        pending: None,
        cancel: Cancellation::default(),
        bad: false,
    })
}
#[test]
fn planner_looks_ahead_and_avoids_early_precision_loss() {
    let pairs = vec![FormatPair::same(Rgba8), FormatPair::RGB16];
    let p = FormatPlan::select(Rgba8, Rgb16, &[pairs.clone(), vec![FormatPair::RGB16]]).unwrap();
    assert_eq!(p.stages[0], FormatPair::same(Rgba8)); // equal one-conversion paths, stable declared preference
    let p = FormatPlan::select(Yuv422p10, Yuv422p10, std::slice::from_ref(&pairs)).unwrap();
    assert_eq!(p.stages[0], FormatPair::RGB16); // RGB promotion must not look worse than 8-bit clipping
    let p = FormatPlan::select(Rgb16, Rgba8, &[pairs]).unwrap();
    assert_eq!(p.stages[0], FormatPair::RGB16); // quantize at display, not before processing
    let choices = vec![
        FormatPair {
            input: Rgba8,
            output: Rgb16,
        },
        FormatPair::same(Rgba8),
    ];
    let p = FormatPlan::select(Rgba8, Rgba8, &[choices, vec![FormatPair::same(Rgba8)]]).unwrap();
    assert_eq!(p.stages[0], FormatPair::same(Rgba8));
    assert!(FormatPlan::select(Rgb16, Rgb16, &[vec![]]).is_err());
}
#[test]
fn compatible_buffers_survive_two_buffered_stages_without_copy_or_conversion() {
    let mut sessions = vec![session(Rgba8), session(Rgba8)];
    let plan = FormatPlan::new(&mut sessions, Rgba8, Rgba8).unwrap();
    let c = Cancellation::default();
    let mut out = vec![];
    let mut pointers = vec![];
    for i in 0..3 {
        let f = rgba(i);
        let FramePixels::Rgba8(p) = &f.pixels else {
            unreachable!()
        };
        pointers.push(p.as_ptr());
        out.extend(plan.process(&mut sessions, 0, vec![f], &c, 1000).unwrap());
    }
    for i in 0..2 {
        out.extend(plan.finish_stage(&mut sessions, i, &c, 1000).unwrap());
    }
    assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), [0, 1, 2]);
    for (f, pointer) in out.iter().zip(pointers) {
        let FramePixels::Rgba8(p) = &f.pixels else {
            unreachable!()
        };
        assert_eq!(p.as_ptr(), pointer);
    }
}
#[test]
fn mixed_native_and_rgb16_stages_convert_at_boundaries_and_flush() {
    let mut sessions = vec![session(Rgba8), session(Rgb16), session(Rgba8)];
    let plan = FormatPlan::new(&mut sessions, Rgba8, Rgba8).unwrap();
    let c = Cancellation::default();
    let mut out = vec![];
    for i in 0..4 {
        out.extend(
            plan.process(&mut sessions, 0, vec![rgba(i)], &c, 1000)
                .unwrap(),
        );
    }
    for i in 0..3 {
        out.extend(plan.finish_stage(&mut sessions, i, &c, 1000).unwrap());
    }
    assert_eq!(out.len(), 4);
    for (i, f) in out.into_iter().enumerate() {
        assert_eq!(f.pts, i as i64);
        let FramePixels::Rgba8(p) = f.pixels else {
            unreachable!()
        };
        assert_eq!(*p, [10, 20, 30, 255].repeat(4));
    }
}
#[test]
fn layout_mismatch_budget_cancellation_and_bad_tail_fail() {
    let mut sessions = vec![session(Rgb16)];
    let plan = FormatPlan::new(&mut sessions, Rgba8, Rgba8).unwrap();
    let c = Cancellation::default();
    assert!(
        plan.process(&mut sessions, 0, vec![rgba(0)], &c, 16)
            .is_err()
    ); // RGB16 allocation needs 24
    let wrong = color::convert(rgba(0), Rgb16).unwrap();
    assert!(
        plan.process(&mut sessions, 0, vec![wrong], &c, 100)
            .is_err()
    );
    c.cancel();
    assert!(
        plan.process(&mut sessions, 0, vec![rgba(0)], &c, 100)
            .is_err()
    );
    let mut sessions: Vec<Box<dyn Session>> = vec![Box::new(Buffer {
        pair: FormatPair::same(Rgba8),
        pending: Some(rgba(0)),
        cancel: Cancellation::default(),
        bad: true,
    })];
    let plan = FormatPlan::new(&mut sessions, Rgba8, Rgba8).unwrap();
    assert!(
        plan.finish_stage(&mut sessions, 0, &Cancellation::default(), 100)
            .is_err()
    );
}
#[test]
fn rgb16_host_boundary_supports_native_only_sessions() {
    let mut boundary = Rgb16Boundary::new(session(Rgba8), 1000).unwrap();
    assert!(
        boundary
            .process(
                color::convert(rgba(0), Rgb16)
                    .unwrap()
                    .into_rgb16()
                    .unwrap()
            )
            .unwrap()
            .is_empty()
    );
    let out = boundary.finish().unwrap();
    assert_eq!(out[0].pixels, [2570, 5140, 7710].repeat(4));
}
#[test]
fn planar_resampling_preserves_headroom_without_rgb_round_trip() {
    let mut f = rgba(0);
    f.format = Yuv420p8;
    f.pixels = FramePixels::Yuv8([
        Arc::new(vec![0, 16, 235, 255]),
        Arc::new(vec![255]),
        Arc::new(vec![0]),
    ]);
    let g = color::convert(f, Yuv422p10).unwrap();
    let FramePixels::Yuv16(p) = &g.pixels else {
        unreachable!()
    };
    assert_eq!(**p[0], [0, 64, 940, 1020]);
    assert_eq!(**p[1], [1020, 1020]);
    assert_eq!(**p[2], [0, 0]);
    let pointers = p.each_ref().map(|p| p.as_ptr());
    let prores = color::into_prores(g).unwrap();
    assert_eq!(prores.y.as_ptr(), pointers[0]);
    assert_eq!(prores.cb.as_ptr(), pointers[1]);
    assert_eq!(prores.cr.as_ptr(), pointers[2]);
}
#[test]
fn odd_plane_geometry_alpha_and_invalid_samples_are_checked() {
    let mut f = rgba(0);
    f.width = 3;
    f.height = 3;
    f.format = Yuv420p10;
    f.pixels = FramePixels::Yuv16([
        Arc::new(vec![500; 9]),
        Arc::new(vec![512; 4]),
        Arc::new(vec![512; 4]),
    ]);
    assert!(f.validate().is_ok());
    let g = color::convert(f.clone(), Yuv444p10).unwrap();
    assert_eq!(g.byte_len(), 54);
    let FramePixels::Yuv16(p) = &mut f.pixels else {
        unreachable!()
    };
    p[2] = Arc::new(vec![1024; 4]);
    assert!(f.validate().is_err());
    let mut f = rgba(0);
    f.pixels = FramePixels::Rgba8(Arc::new(vec![0; 16]));
    assert!(f.validate().is_err());
}
#[test]
fn decoder_plane_mapping_preserves_shared_storage_and_rejects_hdr() {
    let planes = [
        Arc::new(vec![400; 4]),
        Arc::new(vec![512; 2]),
        Arc::new(vec![512; 2]),
    ];
    let mut decoded = filmcraft_frame::VideoFrame {
        width: 2,
        height: 2,
        data: filmcraft_frame::PixelData::Yuv16 {
            planes: planes.clone(),
            chroma: filmcraft_frame::Chroma::C422,
            bits: 10,
            alpha: None,
        },
        color: filmcraft_color::ColorInfo::REC709,
        par: (1, 1),
        pts: filmcraft_time::Tick::ZERO,
    };
    let f = color::from_decoded(&decoded, 7, 1, [1, 24]).unwrap();
    assert_eq!(f.format, Yuv422p10);
    assert_eq!(f.pts, 7);
    let FramePixels::Yuv16(p) = f.pixels else {
        unreachable!()
    };
    assert!(Arc::ptr_eq(&p[0], &planes[0]));
    decoded.color.transfer = filmcraft_color::Transfer::Linear;
    assert!(color::from_decoded(&decoded, 0, 1, [1, 24]).is_err());
}

#[test]
fn legacy_rgb16_conversion_is_numerically_unchanged() {
    let pixels: Vec<u16> = (0..8 * 8 * 3)
        .map(|i| ((i * 7919) % 65536) as u16)
        .collect();
    let reference = crate::media::prores_frame(&pixels, 8, 8).unwrap();
    let f = abner_processor_api::Frame {
        width: 8,
        height: 8,
        pixels,
        pts: 2,
        duration: 1,
        time_base: [1, 24],
    };
    let actual = color::into_prores(f.into()).unwrap();
    assert_eq!(actual.y, reference.y);
    assert_eq!(actual.cb, reference.cb);
    assert_eq!(actual.cr, reference.cr);
    let decoded = filmcraft_frame::VideoFrame {
        width: 8,
        height: 8,
        data: filmcraft_frame::PixelData::Yuv16 {
            planes: [
                Arc::new(reference.y),
                Arc::new(reference.cb),
                Arc::new(reference.cr),
            ],
            chroma: filmcraft_frame::Chroma::C422,
            bits: 10,
            alpha: None,
        },
        color: filmcraft_color::ColorInfo::REC709,
        par: (1, 1),
        pts: filmcraft_time::Tick::ZERO,
    };
    let expected = crate::media::rgb16(&decoded).unwrap();
    let actual = color::convert(color::from_decoded(&decoded, 2, 1, [1, 24]).unwrap(), Rgb16)
        .unwrap()
        .into_rgb16()
        .unwrap();
    assert_eq!(actual.pixels, expected);
}

#[test]
fn display_branch_bypasses_encoder_subsampling() {
    let f = abner_processor_api::Frame {
        width: 2,
        height: 2,
        pixels: vec![65535, 0, 0, 0, 0, 65535, 0, 65535, 0, 65535, 65535, 65535],
        pts: 0,
        duration: 1,
        time_base: [1, 24],
    };
    let pointer = f.pixels.as_ptr();
    let plan = FormatPlan::new(&mut [], Rgb16, Yuv422p10).unwrap();
    let mut out = plan
        .process_native(&mut [], 0, vec![f.into()], &Cancellation::default(), 1024)
        .unwrap();
    let f = out.pop().unwrap();
    let FramePixels::Rgb16(p) = &f.pixels else {
        unreachable!()
    };
    assert_eq!(p.as_ptr(), pointer);
    assert_eq!(
        *color::rgba8(&f).unwrap(),
        [
            255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 255, 255, 255, 255, 255
        ]
    );
    let encoded = color::into_prores(f).unwrap();
    assert_eq!(encoded.cb.len(), 2);
}

#[test]
fn float_chain_preserves_headroom_and_original_allocation() {
    let p=Arc::new([-0.125,0.12345679,1.25].repeat(4));
    let f=VideoFrame{width:2,height:2,format:RgbF32,pixels:FramePixels::RgbF32(p.clone()),pts:0,duration:1,time_base:[1,24]};
    let out=color::convert(f.clone(),RgbF32).unwrap();
    let FramePixels::RgbF32(q)=out.pixels else {panic!()};assert!(Arc::ptr_eq(&p,&q));
    let plan=FormatPlan::select(RgbF32,Rgba8,&[vec![FormatPair::RGB16,FormatPair::same(RgbF32)]]).unwrap();
    assert_eq!(plan.stages[0],FormatPair::same(RgbF32));
    let display=color::rgba8(&f).unwrap();assert_eq!(display[0],0);assert_eq!(display[2],255);
    assert_eq!(p[0],-0.125);assert_eq!(p[2],1.25);
    let mut bad=f;bad.pixels=FramePixels::RgbF32(Arc::new(vec![f32::NAN;12]));assert!(bad.validate().is_err());
}

#[test]
fn float_encoder_boundary_keeps_yuv_headroom() {
    let f=VideoFrame{width:2,height:2,format:RgbF32,pixels:FramePixels::RgbF32(Arc::new(vec![-0.05;12])),pts:0,duration:1,time_base:[1,24]};
    let yuv=color::convert(f.clone(),Yuv422p10).unwrap();
    let FramePixels::Yuv16(p)=yuv.pixels else{panic!()};assert!(p[0].iter().all(|v|*v<64));assert_eq!(p[1][0],512);
    let f=VideoFrame{pixels:FramePixels::RgbF32(Arc::new(vec![1.05;12])),..f};
    let yuv=color::convert(f,Yuv422p10).unwrap();let FramePixels::Yuv16(p)=yuv.pixels else{panic!()};assert!(p[0].iter().all(|v|*v>940 && *v<=1023));
}
