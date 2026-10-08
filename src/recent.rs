//! The launch window's recent files: the clips abner last opened, most
//! recent first, and a thumbnail of each.
//!
//! The list is one plain-text file, an absolute path per line
//! (`~/.config/abner/recent`, next to the config overlay). It is written
//! by the runner when clips load — never by `App`, so the test suite
//! can't touch the user's list — and read back at startup and whenever
//! the launch window comes up again.
//!
//! Thumbnails are not cached (abner keeps no cache): each launch window
//! probes the files again and pulls one frame from each with an ffmpeg
//! child, on worker threads, under the probe's deadline. A file that has
//! moved, or sits on a volume that has gone away, simply drops out of
//! the row.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use crate::probe::{self, VideoInfo};

/// How many paths the file keeps. More than the row shows, so a file
/// that has gone missing still leaves the row full.
const KEEP: usize = 12;
/// Tiles on the launch window (and ⌘1–4).
pub const SHOWN: usize = 4;
/// The thumbnail atlas: `ATLAS_COLS` cells a row, `ATLAS` in all. The
/// launch row uses the first `SHOWN`; cut mode's clip and chapter posters
/// use the lot.
pub const ATLAS_COLS: usize = 4;
pub const ATLAS: usize = 16;
/// One thumbnail's pixel size: a 16:9 cover crop, about 2.4× the tile's
/// logical size so it stays sharp on a retina display.
pub const THUMB_W: u32 = 320;
pub const THUMB_H: u32 = 180;
/// Where in the clip the thumbnail frame is taken: far enough in to
/// clear a black leader or a fade-up.
const THUMB_AT: f64 = 0.10;
const THUMB_DEADLINE: Duration = Duration::from_secs(20);

fn list_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/abner/recent"))
}

/// The recent paths, most recent first. A missing or unreadable file is
/// an empty list, never an error: it is a convenience.
pub fn load() -> Vec<PathBuf> {
    let Some(file) = list_path() else { return Vec::new() };
    parse(&std::fs::read_to_string(file).unwrap_or_default())
}

fn parse(text: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let p = PathBuf::from(line);
        if p.is_absolute() && !out.contains(&p) {
            out.push(p);
        }
    }
    out.truncate(KEEP);
    out
}

/// `paths` were just opened: move them to the front (the first of them
/// first), drop duplicates, keep `KEEP`. Written atomically so a crash
/// mid-write can't leave half a list.
pub fn record<'a>(paths: impl IntoIterator<Item = &'a Path>) {
    let Some(file) = list_path() else { return };
    let opened: Vec<PathBuf> = paths
        .into_iter()
        // Absolute but NOT canonical: resolving symlinks would touch the
        // disk (this runs on the event loop) and turn /tmp into
        // /private/tmp, so the same file would be listed twice.
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
        .collect();
    if opened.is_empty() {
        return;
    }
    let merged = merge(&opened, &load());
    let text: String = merged.iter().map(|p| format!("{}\n", p.display())).collect();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &file)
    };
    if let Err(e) = write() {
        log::warn!("recent files {}: {e}", file.display());
    }
}

fn merge(opened: &[PathBuf], old: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in opened.iter().chain(old) {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out.truncate(KEEP);
    out
}

/// One finished thumbnail job. `rgba` is `THUMB_W × THUMB_H`, straight
/// alpha (all opaque).
pub struct Thumb {
    pub info: VideoInfo,
    pub rgba: Vec<u8>,
}

/// What a thumbnail worker sends back: the path it was given, and the
/// probe plus frame — or why there is none, so the row can drop that
/// tile instead of waiting on it forever.
pub type ThumbResult = (PathBuf, Result<Thumb, String>);

/// Probe each path and pull its thumbnail, one worker per file. Results
/// arrive in whatever order they finish.
pub fn spawn_thumbs(paths: &[PathBuf]) -> Receiver<ThumbResult> {
    let (tx, rx) = channel();
    for path in paths.iter().cloned() {
        let tx = tx.clone();
        let fallback = (tx.clone(), path.clone());
        let spawned = std::thread::Builder::new().name("recent-thumb".into()).spawn(move || {
            let result = thumb(&path).map_err(|e| format!("{e:#}"));
            let _ = tx.send((path, result));
        });
        if spawned.is_err() {
            let _ = fallback.0.send((fallback.1, Err("could not start a worker".into())));
        }
    }
    rx
}

fn thumb(path: &Path) -> anyhow::Result<Thumb> {
    if !path.is_file() {
        anyhow::bail!("missing");
    }
    let info = probe::probe(path)?;
    let at = if info.duration > 0.0 { info.duration * THUMB_AT } else { 0.0 };
    let rgba = frame(path, at).or_else(|e| {
        // A clip too short (or too oddly timed) to seek into still has a
        // first frame.
        if at > 0.0 { frame(path, 0.0) } else { Err(e) }
    })?;
    Ok(Thumb { info, rgba })
}

/// One frame at `at` seconds, scaled to cover `THUMB_W × THUMB_H` and
/// centre-cropped, as raw RGBA on stdout. ffmpeg applies the display
/// rotation itself, so a phone clip comes out upright.
pub fn frame(path: &Path, at: f64) -> anyhow::Result<Vec<u8>> {
    let vf = format!(
        "scale={THUMB_W}:{THUMB_H}:force_original_aspect_ratio=increase:flags=bicubic,crop={THUMB_W}:{THUMB_H}"
    );
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-ss", &format!("{at:.3}"), "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v", "1", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
    let out = probe::run_deadlined(&mut cmd, THUMB_DEADLINE)?
        .ok_or_else(|| anyhow::anyhow!("no frame within {}s", THUMB_DEADLINE.as_secs()))?;
    let want = (THUMB_W * THUMB_H * 4) as usize;
    if !out.success || out.stdout.len() != want {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffmpeg: {}", err.lines().last().unwrap_or("no frame").trim());
    }
    Ok(out.stdout)
}

/// The tile's corner chip: the container, from the file's extension.
pub fn container(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_ascii_uppercase()).unwrap_or_default()
}

/// `23.976`, `29.97`, `30`: three decimals at most, trailing zeros gone.
pub fn fmt_fps(fps: f64) -> String {
    let s = format!("{fps:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `HH:MM:SS`, the tile's duration chip.
pub fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_absolute_unique_paths_in_order() {
        let got = parse("/a.mov\n\nrelative.mp4\n/b.mkv\n/a.mov\n");
        assert_eq!(got, vec![PathBuf::from("/a.mov"), PathBuf::from("/b.mkv")]);
    }

    #[test]
    fn merge_puts_the_opened_first_and_caps() {
        let old: Vec<PathBuf> = (0..KEEP).map(|i| PathBuf::from(format!("/{i}.mov"))).collect();
        let opened = vec![PathBuf::from("/new.mov"), PathBuf::from("/3.mov")];
        let got = merge(&opened, &old);
        assert_eq!(got.len(), KEEP);
        assert_eq!(got[0], PathBuf::from("/new.mov"));
        assert_eq!(got[1], PathBuf::from("/3.mov"));
        assert_eq!(got[2], PathBuf::from("/0.mov"));
        assert_eq!(got.iter().filter(|p| **p == PathBuf::from("/3.mov")).count(), 1);
    }

    #[test]
    fn labels() {
        assert_eq!(fmt_fps(24000.0 / 1001.0), "23.976");
        assert_eq!(fmt_fps(29.97), "29.97");
        assert_eq!(fmt_fps(30.0), "30");
        assert_eq!(fmt_duration(134.4), "00:02:14");
        assert_eq!(fmt_duration(3725.0), "01:02:05");
        assert_eq!(container(Path::new("/x/clip.mov")), "MOV");
    }
}
