//! One ffprobe per input at startup — abner has no cache and no library,
//! so a plain synchronous probe is the right weight.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};

/// Hard deadline on the ffprobe child. The probe runs synchronously on
/// the main thread before any window exists, so a child wedged on a dead
/// network mount would otherwise look exactly like a crash: no window,
/// no message, forever. Local files probe in milliseconds; this only ever
/// fires on storage that has gone away.
const PROBE_DEADLINE: Duration = Duration::from_secs(30);

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
pub(crate) fn run_deadlined(cmd: &mut Command, deadline: Duration) -> std::io::Result<Option<Output>> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
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
            return Ok(Some(Output { success: status.success(), stdout, stderr }));
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
fn display_size(w: u32, h: u32, rotation: Option<f64>) -> (u32, u32) {
    match rotation.map(|r| ((r / 90.0).round() as i64).rem_euclid(4)) {
        Some(1) | Some(3) => (h, w),
        _ => (w, h),
    }
}

/// Probe `path`: from the file's own headers when it is Matroska/WebM and
/// they say enough, and by running ffprobe otherwise.
pub fn probe(path: &Path) -> anyhow::Result<VideoInfo> {
    match probe_mkv_deadlined(path, PROBE_DEADLINE) {
        Some(Some(info)) => Ok(info),
        Some(None) => probe_ffprobe(path),
        // ffprobe would wedge on this source the same way, for another
        // whole deadline.
        None => Err(anyhow!(
            "no answer within {}s for {} — is the file on a storage volume that has gone away?",
            PROBE_DEADLINE.as_secs(),
            path.display()
        )),
    }
}

/// `probe_mkv` under the deadline ffprobe gets. Reading in-process leaves
/// no child to kill, and this runs on the main thread before any window
/// exists — a read stuck on a dead mount would look like a crash. So the
/// read runs on its own thread, abandoned on expiry.
///
/// `None` is a timeout; `Some(None)` is `probe_mkv` declining.
fn probe_mkv_deadlined(path: &Path, deadline: Duration) -> Option<Option<VideoInfo>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let owned = path.to_path_buf();
    let spawned = std::thread::Builder::new().name("probe-mkv".into()).spawn(move || {
        let _ = tx.send(probe_mkv(&owned));
    });
    if spawned.is_err() {
        return Some(None);
    }
    match rx.recv_timeout(deadline) {
        Ok(info) => Some(info),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Some(None),
    }
}

/// `probe` for a Matroska/WebM file from its headers, or `None` to send
/// the caller to ffprobe. Declines rather than guesses:
///
/// - not Matroska (mp4, mov, …);
/// - no duration, no declared frame duration (variable frame rate: an
///   average needs the packets), or a codec this cannot name the way
///   ffprobe does — `vt_accel` matches on that name.
///
/// A pixel format the headers do not give (anything but H.264/HEVC) is
/// `"?"`, as it already is when ffprobe omits one; nothing here decides
/// on it, it only appears in the info panel.
fn probe_mkv(path: &Path) -> Option<VideoInfo> {
    let m = fastmkv::read(path).ok()?;
    let track = m.video()?;
    let video = track.video?;
    let codec = track.codec()?;
    let duration = m.duration?;
    let fps = track.frame_rate()?;
    let (w, h) = (u32::try_from(video.width).ok()?, u32::try_from(video.height).ok()?);
    if w == 0 || h == 0 {
        return None;
    }
    let file_size = std::fs::metadata(path).ok()?.len();
    // ffprobe's container bit rate is exactly this: the file's size over
    // its duration. Matroska tracks carry no bit rate of their own.
    let bit_rate = (duration > 0.0).then(|| (file_size as f64 * 8.0 / duration) as u64);
    let (width, height) = display_size(w, h, video.rotation);
    Some(VideoInfo {
        path: path.to_path_buf(),
        width,
        height,
        fps,
        duration,
        codec: codec.to_string(),
        pix_fmt: video.pix_fmt().unwrap_or_else(|| "?".to_string()),
        bit_rate,
        file_size,
        rotation: video.rotation,
    })
}

fn probe_ffprobe(path: &Path) -> anyhow::Result<VideoInfo> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"]).arg(path);
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
        .or_else(|| v["format"]["duration"].as_str().and_then(|d| d.parse().ok()))
        .unwrap_or(0.0);
    let bit_rate = s["bit_rate"]
        .as_str()
        .and_then(|b| b.parse::<u64>().ok())
        .or_else(|| v["format"]["bit_rate"].as_str().and_then(|b| b.parse().ok()));
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

/// Hardware decode gate carried over from switchblade (benchmarked there):
/// VideoToolbox only for the codecs it actually accelerates — VP9/AV1
/// measured *slower* routed through VT than straight software decode.
pub fn vt_accel(codec: &str) -> bool {
    cfg!(target_os = "macos") && matches!(codec, "h264" | "hevc" | "h265" | "prores")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have(bin: &str) -> bool {
        Command::new(bin).arg("-version").output().is_ok_and(|o| o.status.success())
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
        ffmpeg(&["-display_rotation", "90", "-i", h264.to_str().unwrap(), "-c", "copy",
                 turned.to_str().unwrap()]);

        for p in [&h264, &hevc, &turned] {
            let ours = probe_mkv(p).unwrap_or_else(|| panic!("probe_mkv declined {p:?}"));
            let theirs = probe_ffprobe(p).unwrap();
            assert_eq!((ours.width, ours.height), (theirs.width, theirs.height), "{p:?}");
            assert_eq!(ours.codec, theirs.codec, "{p:?}");
            assert_eq!(ours.pix_fmt, theirs.pix_fmt, "{p:?}");
            assert_eq!(ours.rotation, theirs.rotation, "{p:?}");
            assert_eq!(ours.file_size, theirs.file_size, "{p:?}");
            assert!((ours.fps - theirs.fps).abs() < 1e-6, "{p:?}");
            assert!((ours.duration - theirs.duration).abs() < 0.01, "{p:?}");
            let (a, b) = (ours.bit_rate.unwrap() as f64, theirs.bit_rate.unwrap() as f64);
            assert!((a - b).abs() / b < 0.01, "{p:?}: bit rate {a} vs {b}");
        }
        // Display size has the rotation applied: 160x96 turned is 96x160.
        assert_eq!(
            probe_mkv(&turned).map(|i| (i.width, i.height)),
            Some((96, 160))
        );

        let mp4 = dir.join("plain.mp4");
        ffmpeg(&["-f", "lavfi", "-i", src, "-c:v", "libx264", mp4.to_str().unwrap()]);
        assert!(probe_mkv(&mp4).is_none(), "mp4 is ffprobe's");
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
        if !Command::new("mkfifo").arg(&fifo).status().is_ok_and(|s| s.success()) {
            eprintln!("skipping: no mkfifo");
            return;
        }
        let t0 = Instant::now();
        assert!(probe_mkv_deadlined(&fifo, Duration::from_millis(200)).is_none());
        assert!(t0.elapsed() < Duration::from_secs(5));
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        let _ = std::fs::remove_file(&fifo);
    }
}
