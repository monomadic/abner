//! One ffprobe per input at startup — abner has no cache and no library,
//! so a plain synchronous probe is the right weight.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow};

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

pub fn probe(path: &Path) -> anyhow::Result<VideoInfo> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .context("running ffprobe (is ffmpeg installed?)")?;
    if !out.status.success() {
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

    // ±90° (odd quarter-turns) swap the displayed dimensions.
    let (width, height) = match rotation.map(|r| ((r / 90.0).round() as i64).rem_euclid(4)) {
        Some(1) | Some(3) => (h, w),
        _ => (w, h),
    };

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
