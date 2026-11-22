//! Actual native decode → plugin → ProRes mux regression, including fractional tails.
use abner_processing::{
    Stage,
    media::{Job, run},
};
use abner_processor_api::{Cancellation, Registry};
use std::{path::PathBuf, process::Command, sync::Arc};
#[test]
fn fractional_and_integer_exports_keep_source_end_and_nominal_rate() {
    let dir = std::env::temp_dir().join(format!("rife-export-test-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(dir.clone());
    let mut registry = Registry::default();
    for f in abner_builtin_rife_metal::factories() {
        registry.register(f).unwrap();
    }
    for (i, (source_rate, target)) in [
        ("24", "60"),
        ("25", "60"),
        ("30", "60"),
        ("50", "60"),
        ("24000/1001", "60"),
        ("24000/1001", "59.94"),
        ("30000/1001", "2x"),
    ]
    .into_iter()
    .enumerate()
    {
        let input = dir.join(format!("in{i}.mp4"));
        let output = dir.join(format!("out{i}.mov"));
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc2=size=128x64:rate={source_rate}"),
                    "-frames:v",
                    "4",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-video_track_timescale",
                    "120000",
                    "-movie_timescale",
                    "120000",
                    "-color_primaries",
                    "bt709",
                    "-color_trc",
                    "bt709",
                    "-colorspace",
                    "bt709"
                ])
                .arg(&input)
                .status()
                .unwrap()
                .success()
        );
        let report = run(
            &Job {
                input: input.clone(),
                output: output.clone(),
                stages: vec![Stage {
                    processor: "rife.metal".into(),
                    settings: serde_json::json!({"mode":"retime","target":target,"tier":"fast"}),
                }],
                copy_audio: false,
                memory_bytes: 64 * 1024 * 1024,
            },
            &registry,
            &Cancellation::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        assert!(report.frames >= 4);
        let duration = |path: &PathBuf| -> f64 {
            let p = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=duration",
                    "-of",
                    "json",
                ])
                .arg(path)
                .output()
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&p.stdout).unwrap();
            v["streams"][0]["duration"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        };
        let (a, b) = (duration(&input), duration(&output));
        assert!(
            (a - b).abs() < 0.000002,
            "{source_rate} → {target}: input {a}, output {b}"
        );
    }
}
