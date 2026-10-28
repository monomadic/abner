//! Selective H.264 GOP rendering. Originals are never overwritten. A verified
//! working file is published only after all requested regions succeed.
use std::{path::{Path, PathBuf}, sync::atomic::AtomicBool};
#[cfg(test)]
use std::{process::{Command, Stdio}, sync::atomic::Ordering, time::{Duration, Instant}};
#[cfg(test)]
use anyhow::ensure;

pub struct Region { pub start: f64, pub encode_start: f64, pub end: f64, pub boundaries: Vec<f64> }
#[cfg(test)]
struct Work(PathBuf);
#[cfg(test)]
impl Drop for Work { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

#[cfg(test)]
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
#[cfg(test)]
fn ffmpeg() -> Command {
    let mut c = Command::new("ffmpeg"); c.args(["-nostdin", "-y", "-v", "error", "-xerror"]); c.stdout(Stdio::null()); c
}
#[cfg(test)]
fn probe(path: &Path, dir: &Path, cancel: &AtomicBool) -> anyhow::Result<serde_json::Value> {
    let json = dir.join("probe.json");
    run(Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_streams", "-show_packets", "-show_entries", "stream=codec_name,pix_fmt,has_b_frames,width,height:packet=pts_time,duration_time,flags", "-of", "json"])
        .arg(path).stdout(std::fs::File::create(&json)?), dir, cancel)?;
    Ok(serde_json::from_slice(&std::fs::read(json)?)?)
}

#[cfg(test)]
fn frames(v: &serde_json::Value) -> Vec<f64> {
    let mut pts: Vec<_> = v["packets"].as_array().into_iter().flatten().filter_map(|p| p["pts_time"].as_str()?.parse::<f64>().ok()).collect();
    pts.sort_by(f64::total_cmp); pts
}

pub fn encode(source: &Path, regions: &[Region], cancel: &AtomicBool) -> anyhow::Result<PathBuf> {
    crate::media_io::smart(source, regions, cancel)
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
