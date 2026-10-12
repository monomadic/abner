//! Selective H.264 GOP rendering. Originals are never overwritten. A verified
//! working file is published only after all requested regions succeed.
use std::{path::{Path, PathBuf}, process::{Command, Stdio}, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};
use anyhow::{Context, ensure};

pub struct Region { pub start: f64, pub encode_start: f64, pub end: f64, pub boundaries: Vec<f64> }
struct Work(PathBuf);
impl Drop for Work { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

fn run(cmd: &mut Command, dir: &Path, cancel: &AtomicBool) -> anyhow::Result<()> {
    let log = dir.join("stderr");
    let stderr = std::fs::File::create(&log)?;
    let mut child = cmd.stdin(Stdio::null()).stderr(stderr).spawn()?;
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) || start.elapsed() > Duration::from_secs(3600) {
            let _ = child.kill(); let _ = child.wait();
            anyhow::bail!("Encoding cancelled or exceeded the one-hour limit");
        }
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "{}", std::fs::read_to_string(log)?.trim());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn ffmpeg() -> Command {
    let mut c = Command::new("ffmpeg"); c.args(["-nostdin", "-y", "-v", "error", "-xerror"]); c.stdout(Stdio::null()); c
}
fn probe(path: &Path, dir: &Path, cancel: &AtomicBool) -> anyhow::Result<serde_json::Value> {
    let json = dir.join("probe.json");
    run(Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_streams", "-show_packets", "-show_entries", "stream=codec_name,pix_fmt,has_b_frames,width,height:packet=pts_time,duration_time,flags", "-of", "json"])
        .arg(path).stdout(std::fs::File::create(&json)?), dir, cancel)?;
    Ok(serde_json::from_slice(&std::fs::read(json)?)?)
}

fn frames(v: &serde_json::Value) -> Vec<f64> {
    let mut pts: Vec<_> = v["packets"].as_array().into_iter().flatten().filter_map(|p| p["pts_time"].as_str()?.parse::<f64>().ok()).collect();
    pts.sort_by(f64::total_cmp); pts
}

pub fn encode(source: &Path, regions: &[Region], cancel: &AtomicBool) -> anyhow::Result<PathBuf> {
    ensure!(!regions.is_empty(), "No GOPs need encoding");
    let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let dir = source.parent().unwrap_or(Path::new(".")).join(format!(".abner-encode-{}-{id}", std::process::id()));
    std::fs::create_dir(&dir).context("Creating encoding workspace beside the source")?;
    let work = Work(dir);
    let input = probe(source, &work.0, cancel)?;
    let stream = &input["streams"][0];
    ensure!(stream["codec_name"] == "h264" && stream["pix_fmt"] == "yuv420p", "GOP encoding currently supports 8-bit H.264 4:2:0 video");
    let b_frames = stream["has_b_frames"].as_u64().unwrap_or(0).to_string();
    let pts = frames(&input);
    ensure!(pts.len() >= 2 && pts[0].abs() < 0.0001, "GOP encoding requires a zero-based video timeline");
    let period = pts[1] - pts[0];
    let last_duration = input["packets"].as_array().unwrap().iter().filter_map(|p| {
        let t: f64 = p["pts_time"].as_str()?.parse().ok()?;
        let d: f64 = p["duration_time"].as_str()?.parse().ok()?;
        ((t-pts.last().unwrap()).abs() < 0.00001).then_some(d)
    }).next().unwrap_or(period);
    let duration = pts.last().unwrap() + last_duration;
    for r in regions {
        for t in std::iter::once(&r.start).chain(std::iter::once(&r.encode_start)).chain(std::iter::once(&r.end)).chain(&r.boundaries) {
            ensure!((*t-duration).abs() < 0.0001 || pts.binary_search_by(|p| if (*p-*t).abs() < 0.0001 { std::cmp::Ordering::Equal } else { p.total_cmp(t) }).is_ok(), "An edit is between frames; move it to a frame boundary before encoding");
        }
    }
    // Recheck the source before copying a partial GOP: the UI's asynchronous
    // metadata must never authorize copying unknown or B-frame dependencies.
    if regions.iter().any(|r| r.encode_start > r.start + 0.0001) {
        let json = work.0.join("types.json");
        run(Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_frames",
            "-show_entries", "frame=best_effort_timestamp_time,pict_type", "-of", "json"])
            .arg(source).stdout(std::fs::File::create(&json)?), &work.0, cancel)?;
        let types: serde_json::Value = serde_json::from_slice(&std::fs::read(json)?)?;
        for r in regions.iter().filter(|r| r.encode_start > r.start + 0.0001) {
            let decoded: Vec<_> = types["frames"].as_array().into_iter().flatten().filter_map(|f| {
                let t: f64 = f["best_effort_timestamp_time"].as_str()?.parse().ok()?;
                (t >= r.start-0.0001 && t < r.end-0.0001).then_some((t, f["pict_type"].as_str().unwrap_or("")))
            }).collect();
            let expected: Vec<_> = pts.iter().filter(|t| **t >= r.start-0.0001 && **t < r.end-0.0001).collect();
            ensure!(!expected.is_empty() && decoded.len() == expected.len() &&
                decoded.iter().zip(expected).all(|((t,k), p)| (t-p).abs() < 0.0001 && matches!(*k, "I" | "P")),
                "Partial GOP copy requires complete I/P frame analysis; source unchanged");
        }
    }
    let mut spans = Vec::new(); let mut at = 0.0;
    for r in regions {
        if r.start > at + 0.0001 { spans.push((at, r.start, None)); }
        spans.push((r.start, r.end.min(duration), Some(r))); at = r.end;
    }
    if at < duration - 0.0001 { spans.push((at, duration, None)); }
    // Split on key packets in decode order. Trimming copied B-frame streams
    // with -t would instead drop trailing pictures or include the next GOP.
    let base = work.0.join("copy-%d.mp4");
    let times = spans.iter().skip(1).map(|(a,_,_)| format!("{a:.9}")).collect::<Vec<_>>().join(",");
    let mut split = ffmpeg();
    split.arg("-i").arg(source).args(["-map", "0:v:0", "-c", "copy", "-f", "segment", "-reset_timestamps", "1", "-segment_format_options", "video_track_timescale=90000"]);
    if !times.is_empty() { split.args(["-segment_time_delta", "0.0001", "-segment_times", &times]); }
    else { split.args(["-segment_time", &format!("{}", duration + 1.0)]); }
    run(split.arg(&base), &work.0, cancel)?;
    let mut list = String::from("ffconcat version 1.0\n");
    for (i, (a, b, region)) in spans.iter().enumerate() {
        let copied = work.0.join(format!("copy-{i}.mp4"));
        let part = work.0.join(format!("part-{i}.mp4"));
        if let Some(r) = region {
            let prefix = r.encode_start - a;
            if prefix > 0.0001 {
                let head = work.0.join(format!("head-{i}.mp4"));
                run(ffmpeg().arg("-i").arg(&copied).args(["-map", "0:v:0", "-c", "copy", "-t",
                    &format!("{prefix:.9}"), "-video_track_timescale", "90000"]).arg(&head), &work.0, cancel)?;
                list.push_str(&format!("file 'head-{i}.mp4'\nduration {prefix:.9}\n"));
            }
            let first = pts.partition_point(|t| *t < r.encode_start - 0.0001);
            let forced = r.boundaries.iter().filter(|t| **t > r.encode_start && **t < *b)
                .map(|t| format!("eq(n,{})", pts.partition_point(|p| *p < *t - 0.0001) - first)).collect::<Vec<_>>().join("+");
            let mut cmd = ffmpeg();
            cmd.arg("-noautorotate").arg("-i").arg(&copied).args(["-map", "0:v:0", "-fps_mode", "passthrough", "-enc_time_base", "1:90000", "-c:v", "libx264", "-preset", "medium", "-crf", "16", "-bf", &b_frames, "-forced-idr", "1", "-x264-params", "open-gop=0:repeat-headers=1", "-sc_threshold", "0", "-pix_fmt", "yuv420p"]);
            if prefix > 0.0001 {
                cmd.args(["-vf", &format!("trim=start={prefix:.9},setpts=PTS-STARTPTS"), "-bf", "0"]);
            }
            if !forced.is_empty() { cmd.args(["-force_key_frames", &format!("expr:{forced}")]); }
            cmd.args(["-video_track_timescale", "90000"]).arg(&part);
            run(&mut cmd, &work.0, cancel)?;
        } else { std::fs::rename(copied, &part)?; }
        list.push_str(&format!("file 'part-{i}.mp4'\nduration {:.9}\n", b-region.map_or(*a, |r| r.encode_start)));
    }
    let playlist = work.0.join("concat.txt"); std::fs::write(&playlist, list)?;
    let output = work.0.join("output.mp4");
    run(ffmpeg().args(["-f", "concat", "-safe", "0", "-i"]).arg(&playlist).arg("-i").arg(source)
        .args(["-map", "0:v:0", "-map", "1:a?", "-map", "1:s?", "-map_metadata", "1", "-map_chapters", "1", "-c", "copy", "-video_track_timescale", "90000"]).arg(&output), &work.0, cancel)?;
    let result = probe(&output, &work.0, cancel)?;
    ensure!(result["streams"][0]["width"] == stream["width"] && result["streams"][0]["height"] == stream["height"], "Rendered dimensions changed; source unchanged");
    let rendered = frames(&result);
    ensure!(pts.len() == rendered.len() && pts.iter().zip(&rendered).all(|(a,b)| (a-b).abs() < 0.0001), "Rendered video failed frame timing verification; source unchanged");
    let keys: Vec<f64> = result["packets"].as_array().unwrap().iter().filter(|p| p["flags"].as_str().unwrap_or("").contains('K')).filter_map(|p| p["pts_time"].as_str()?.parse().ok()).collect();
    ensure!(regions.iter().flat_map(|r| &r.boundaries).all(|t| keys.iter().any(|k| (t-k).abs() < 0.0001)), "Rendered boundaries are not keyframes; source unchanged");
    run(ffmpeg().arg("-i").arg(&output).args(["-map", "0:v:0", "-f", "null", "-"]), &work.0, cancel)?;
    ensure!(!cancel.load(Ordering::Relaxed), "Encoding cancelled");
    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    let stem = stem.rsplit_once(".encoded-").filter(|(_, suffix)| suffix.chars().all(|c| c.is_ascii_digit())).map_or(stem.as_ref(), |(base, _)| base);
    let dest = source.with_file_name(format!("{stem}.encoded-{id}.mp4"));
    std::fs::rename(output, &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hashes(path: &Path, audio: bool) -> Vec<String> {
        let out = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-map", if audio { "0:a:0" } else { "0:v:0" }, "-f", "framemd5", "-"]).output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#')).map(|l| l.split(',').last().unwrap().trim().to_string()).collect()
    }
    fn packet_hashes(path: &Path) -> Vec<String> {
        // Concat injects parameter sets at keyframes. Compare coded picture
        // slices, excluding those container/header differences.
        let out = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path)
            .args(["-map", "0:v:0", "-c", "copy", "-bsf:v", "filter_units=pass_types=1-5",
                "-f", "framehash", "-hash", "sha256", "-"]).output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#'))
            .map(|l| l.split(',').last().unwrap().trim().to_owned()).collect()
    }
    #[test]
    fn only_requested_gops_change_and_audio_and_frame_timing_survive() {
        let dir = std::env::temp_dir().join(format!("abner-smart-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap(); let work = Work(dir);
        for (bf, vfr) in [("0", false), ("0", true), ("2", false), ("2", true)] {
        let source = work.0.join(format!("source-{bf}-{vfr}.mp4")); let cancel = AtomicBool::new(false);
        run(ffmpeg().args(["-f", "lavfi", "-i", "testsrc2=size=160x90:rate=30", "-f", "lavfi", "-i", "sine=frequency=440", "-t", "4", "-vf", if vfr { "select=not(eq(mod(n\\,7)\\,6))" } else { "null" }, "-fps_mode", "vfr", "-c:v", "libx264", "-preset", "ultrafast", "-g", "30", "-bf", bf, "-sc_threshold", "0", "-c:a", "aac"]).arg(&source), &work.0, &cancel).unwrap();
        let original = std::fs::read(&source).unwrap();
        let info = probe(&source, &work.0, &cancel).unwrap();
        let pts = frames(&info);
        let end = pts.last().unwrap() + 1.0/30.0;
        let regions = [Region { start: pts[30], encode_start: if bf == "0" { pts[42] } else { pts[30] }, end: pts[60], boundaries: vec![pts[42], pts[51]] }, Region { start: pts[90], encode_start: if bf == "0" { pts[98] } else { pts[90] }, end, boundaries: vec![pts[98]] }];
        let dest = encode(&source, &regions, &cancel).unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), original);
        let a = hashes(&source, false); let b = hashes(&dest, false);
        assert_eq!(a.len(), pts.len()); assert_eq!(a.len(), b.len());
        assert_eq!(&a[..30], &b[..30]); assert_eq!(&a[60..90], &b[60..90]);
        if bf == "0" {
            assert_eq!(&a[30..42], &b[30..42]); assert_eq!(&a[90..98], &b[90..98]);
            let original_packets = packet_hashes(&source); let copied_packets = packet_hashes(&dest);
            assert_eq!(&original_packets[30..42], &copied_packets[30..42], "I/P prefix picture slices are copied verbatim");
            assert_eq!(&original_packets[90..98], &copied_packets[90..98]);
        } else {
            let invalid = [Region { start: pts[30], encode_start: pts[42], end: pts[60], boundaries: vec![pts[42]] }];
            assert!(encode(&source, &invalid, &cancel).unwrap_err().to_string().contains("complete I/P"));
        }
        assert_ne!(&a[42..60], &b[42..60]);
        assert_eq!(hashes(&source, true), hashes(&dest, true));
        cancel.store(true, Ordering::Relaxed);
        assert!(encode(&source, &regions, &cancel).is_err());
        assert!(!std::fs::read_dir(&work.0).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".abner-encode")));
        }
    }
}
