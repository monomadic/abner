//! Bounded Filmcraft metadata for startup and preview validation.

#[cfg(test)]
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

#[cfg(test)]
use anyhow::Context;
use anyhow::anyhow;

/// Bound synchronous startup metadata reads, including unavailable storage.
/// Filmcraft runs on a cooperatively cancelled worker; the UI caller never joins
/// an uninterruptible kernel read.
const PROBE_DEADLINE: Duration = Duration::from_secs(30);

#[cfg(test)]
pub(crate) struct Output {
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// `Command::output()` with a deadline: on expiry the child is killed and
/// `None` comes back. Both pipes drain on their own threads while we
/// poll, because a child that fills a pipe buffer blocks forever and the
/// deadline would then fire on work that was making progress. A process
/// stuck in an uninterruptible read does not die on SIGKILL either — so
/// the kill is fire-and-forget and the drain threads are left unjoined,
/// or the hang would just move here.
#[cfg(test)]
pub(crate) fn run_deadlined(
    cmd: &mut Command,
    deadline: Duration,
) -> std::io::Result<Option<Output>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    }
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let started = Instant::now();
    let mut nap = Duration::from_micros(200);
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = out.join().unwrap_or_default();
            let stderr = err.join().unwrap_or_default();
            return Ok(Some(Output {
                success: status.success(),
                stdout,
                stderr,
            }));
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(None);
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(2));
    }
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub path: PathBuf,
    /// Display dimensions (rotation already applied — a ±90° phone clip
    /// reports its portrait size here; the decoder output matches).
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Seconds; 0.0 when the container doesn't say.
    pub duration: f64,
    pub codec: String,
    pub pix_fmt: String,
    /// Bits per second, stream-level preferred, container fallback.
    pub bit_rate: Option<u64>,
    pub file_size: u64,
    /// Display rotation in degrees (phone footage).
    pub rotation: Option<f64>,
}

#[cfg(test)]
fn parse_rate(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let (num, den) = (num.parse::<f64>().ok()?, den.parse::<f64>().ok()?);
    if den > 0.0 && num > 0.0 {
        Some(num / den)
    } else {
        None
    }
}

/// Displayed size: ±90° (odd quarter-turns) swap the coded dimensions.
#[cfg(test)]
fn display_size(w: u32, h: u32, rotation: Option<f64>) -> (u32, u32) {
    match rotation.map(|r| ((r / 90.0).round() as i64).rem_euclid(4)) {
        Some(1) | Some(3) => (h, w),
        _ => (w, h),
    }
}

/// Probe through Filmcraft, under a caller deadline for unavailable storage.
pub fn probe(path: &Path) -> anyhow::Result<VideoInfo> {
    metadata(path, PROBE_DEADLINE).map(|m| m.info)
}
pub(crate) fn metadata(
    path: &Path,
    deadline: Duration,
) -> anyhow::Result<crate::media_metadata::Metadata> {
    let path = path.to_owned();
    crate::fc::deadlined("metadata", deadline, move || {
        crate::media_metadata::read(&path)
    })
    .map_err(|e| anyhow!(e))
}

#[cfg(test)]
fn probe_ffprobe(path: &Path) -> anyhow::Result<VideoInfo> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .arg(path);
    let out = run_deadlined(&mut cmd, PROBE_DEADLINE)
        .context("running ffprobe (is ffmpeg installed?)")?
        .ok_or_else(|| {
            anyhow!(
                "ffprobe did not finish within {}s for {} — is the file on a storage volume that has gone away?",
                PROBE_DEADLINE.as_secs(),
                path.display()
            )
        })?;
    if !out.success {
        return Err(anyhow!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("ffprobe json")?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let s = streams
        .iter()
        .find(|s| s["codec_type"].as_str() == Some("video"))
        .ok_or_else(|| anyhow!("no video stream in {}", path.display()))?;

    let w = s["width"].as_u64().unwrap_or(0) as u32;
    let h = s["height"].as_u64().unwrap_or(0) as u32;
    if w == 0 || h == 0 {
        return Err(anyhow!("no dimensions for {}", path.display()));
    }
    let fps = s["avg_frame_rate"]
        .as_str()
        .and_then(parse_rate)
        .or_else(|| s["r_frame_rate"].as_str().and_then(parse_rate))
        .unwrap_or(30.0);
    let dur = s["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            v["format"]["duration"]
                .as_str()
                .and_then(|d| d.parse().ok())
        })
        .unwrap_or(0.0);
    let bit_rate = s["bit_rate"]
        .as_str()
        .and_then(|b| b.parse::<u64>().ok())
        .or_else(|| {
            v["format"]["bit_rate"]
                .as_str()
                .and_then(|b| b.parse().ok())
        });
    let file_size = v["format"]["size"]
        .as_str()
        .and_then(|b| b.parse::<u64>().ok())
        .or_else(|| std::fs::metadata(path).ok().map(|m| m.len()))
        .unwrap_or(0);

    // Display rotation: new-style side_data_list, legacy tags.rotate fallback.
    let rotation = s["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|sd| sd["rotation"].as_f64()))
        .or_else(|| s["tags"]["rotate"].as_str().and_then(|r| r.parse().ok()));

    let (width, height) = display_size(w, h, rotation);

    Ok(VideoInfo {
        path: path.to_path_buf(),
        width,
        height,
        fps,
        duration: dur,
        codec: s["codec_name"].as_str().unwrap_or("?").to_string(),
        pix_fmt: s["pix_fmt"].as_str().unwrap_or("?").to_string(),
        bit_rate,
        file_size,
        rotation,
    })
}

/// Hardware decode gate. h264/hevc/prores always go through VideoToolbox
/// on macOS; VP9 and AV1 only when this chip has the block (M1+ for VP9,
/// M3+ for AV1), asked once per codec via `vt_shim.c`. Without the block
/// they stay in software: the native AV1 decoder the hwaccel needs has no
/// software path, and VT's software VP9 was measured slower than libav's
/// own in switchblade.
pub fn vt_accel(codec: &str) -> bool {
    match codec {
        "h264" | "hevc" | "h265" | "prores" => cfg!(target_os = "macos"),
        "vp9" => vt_hw_decodes(b"vp09"),
        "av1" => vt_hw_decodes(b"av01"),
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn vt_hw_decodes(fourcc: &[u8; 4]) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    unsafe extern "C" {
        fn ab_vt_hw_decodes(codec_type: u32) -> bool;
    }
    static CACHE: OnceLock<Mutex<HashMap<u32, bool>>> = OnceLock::new();
    let key = u32::from_be_bytes(*fourcc);
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    *cache.entry(key).or_insert_with(|| {
        let yes = unsafe { ab_vt_hw_decodes(key) };
        log::info!(
            "VideoToolbox hardware {}: {}",
            String::from_utf8_lossy(fourcc),
            if yes { "yes" } else { "no" }
        );
        yes
    })
}

#[cfg(not(target_os = "macos"))]
fn vt_hw_decodes(_fourcc: &[u8; 4]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have(bin: &str) -> bool {
        Command::new(bin)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn ffmpeg(args: &[&str]) {
        let ok = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg {args:?} failed");
    }

    /// The header read must agree with ffprobe on everything abner uses,
    /// for the files it agrees to answer.
    #[test]
    fn probe_mkv_matches_ffprobe() {
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("abner-probe-mkv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "testsrc=size=160x96:rate=25:duration=2";
        let mk = |name: &str, enc: &[&str]| -> PathBuf {
            let out = dir.join(name);
            let mut a = vec!["-f", "lavfi", "-i", src];
            a.extend_from_slice(enc);
            a.push(out.to_str().unwrap());
            ffmpeg(&a);
            out
        };
        let h264 = mk("h264.mkv", &["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
        let hevc = mk("hevc.mkv", &["-c:v", "libx265", "-pix_fmt", "yuv420p10le"]);
        let turned = dir.join("turned.mkv");
        ffmpeg(&[
            "-display_rotation",
            "90",
            "-i",
            h264.to_str().unwrap(),
            "-c",
            "copy",
            turned.to_str().unwrap(),
        ]);

        for p in [&h264, &hevc, &turned] {
            let ours = probe(p).unwrap_or_else(|e| panic!("Filmcraft declined {p:?}: {e}"));
            let theirs = probe_ffprobe(p).unwrap();
            assert_eq!(
                (ours.width, ours.height),
                (theirs.width, theirs.height),
                "{p:?}"
            );
            assert_eq!(ours.codec, theirs.codec, "{p:?}");
            assert_eq!(ours.pix_fmt, theirs.pix_fmt, "{p:?}");
            assert_eq!(ours.rotation, theirs.rotation, "{p:?}");
            assert_eq!(ours.file_size, theirs.file_size, "{p:?}");
            assert!((ours.fps - theirs.fps).abs() < 1e-6, "{p:?}");
            assert!((ours.duration - theirs.duration).abs() < 0.01, "{p:?}");
            let (a, b) = (
                ours.bit_rate.unwrap() as f64,
                theirs.bit_rate.unwrap() as f64,
            );
            assert!((a - b).abs() / b < 0.01, "{p:?}: bit rate {a} vs {b}");
        }
        // Display size has the rotation applied: 160x96 turned is 96x160.
        assert_eq!(
            probe(&turned).ok().map(|i| (i.width, i.height)),
            Some((96, 160))
        );

        let mp4 = dir.join("plain.mp4");
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            src,
            "-c:v",
            "libx264",
            mp4.to_str().unwrap(),
        ]);
        assert!(probe(&mp4).is_ok(), "and still probes");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A source that never answers is an error, not a hang. A FIFO with no
    /// writer blocks any `open`, as a dead mount does.
    #[test]
    #[cfg(unix)]
    fn a_source_that_never_answers_times_out() {
        let fifo = std::env::temp_dir().join(format!("abner-stuck-{}.mkv", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        if !Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success())
        {
            eprintln!("skipping: no mkfifo");
            return;
        }
        let t0 = Instant::now();
        assert!(metadata(&fifo, Duration::from_millis(200)).is_err());
        assert!(t0.elapsed() < Duration::from_secs(5));

        let _ = std::fs::remove_file(&fifo);
    }
}
