//! Sustained in-memory plugin benchmark; FFmpeg supplies scaled real-video frames.
use abner_processor_api::Frame;
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let source = &a[1];
    let seconds: u64 = a.get(2).map(|s| s.parse().unwrap()).unwrap_or(60);
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        for tier in ["hq", "balanced", "fast"] {
            if std::env::var("RIFE_BENCH_CASE").is_ok_and(|c| c != format!("{w}:{tier}")) {
                continue;
            }
            let mut decoder = Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-stream_loop",
                    "-1",
                    "-i",
                    source,
                    "-an",
                    "-vf",
                    &format!("scale={w}:{h}:flags=bicubic"),
                    "-pix_fmt",
                    "rgb48le",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut pipe = decoder.stdout.take().unwrap();
            let factory = abner_builtin_rife_metal::factories().remove(0);
            let cold = Instant::now();
            let mut s = factory
                .create(
                    w,
                    h,
                    &serde_json::json!({"mode":"retime","target":"60","tier":tier}),
                )
                .unwrap();
            s.configure_input_rate([30, 1]).unwrap();
            let load = cold.elapsed().as_secs_f64();
            let mut raw = vec![0u8; w * h * 6];
            let mut index = 0;
            let mut outputs = 0u64;
            let mut calls = 0u64;
            let mut stage = Duration::ZERO;
            let mut wall: Option<Instant> = None;
            let mut digest = 0u64;
            loop {
                if index == 3 {
                    wall = Some(Instant::now());
                }
                pipe.read_exact(&mut raw).unwrap();
                let pixels = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| u16::from_le_bytes([p[0], p[1]]))
                    .collect();
                let f = Frame {
                    width: w,
                    height: h,
                    pixels,
                    pts: index,
                    duration: 1,
                    time_base: [1, 30],
                };
                let t = Instant::now();
                let out = s.process(f).unwrap();
                let elapsed = t.elapsed();
                index += 1;
                if index <= 3 {
                    continue;
                }
                let wall = wall.as_ref().unwrap();
                stage += elapsed;
                calls += 1;
                outputs += out.len() as u64;
                for f in out {
                    digest = digest.wrapping_add(f.pixels[f.pixels.len() / 2] as u64);
                }
                if wall.elapsed().as_secs() >= seconds {
                    break;
                }
            }
            let wall_s = wall.unwrap().elapsed().as_secs_f64();
            decoder.kill().ok();
            decoder.wait().ok();
            println!(
                "{}",
                serde_json::json!({"width":w,"height":h,"tier":tier,"load_s":load,"wall_s":wall_s,"process_s":stage.as_secs_f64(),"input_frames":calls,"output_frames":outputs,"process_output_fps":outputs as f64/stage.as_secs_f64(),"wall_output_fps":outputs as f64/wall_s,"digest":digest})
            );
        }
    }
}
