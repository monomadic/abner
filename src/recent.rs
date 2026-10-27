//! The launch window's recent entries: the SETS of clips abner last had
//! open, most recent first, and a thumbnail of each clip.
//!
//! An entry is a group — the whole clip list as it stood after a load,
//! so a pair being compared comes back as one tile that reopens the
//! pair (2026-10-20; before that the list was single files, and a
//! comparison had to be rebuilt by hand). A lone clip is a group of one.
//!
//! The list is one plain-text file, an entry per line with its absolute
//! paths tab-separated (`~/.config/abner/recent`, next to the config
//! overlay; the old one-path-per-line file reads as groups of one). It
//! is written by the runner when clips load — never by `App`, so the
//! test suite can't touch the user's list — and read back at startup
//! and whenever the launch window comes up again.
//!
//! Thumbnails are not cached (abner keeps no cache): each launch window
//! probes the files again and decodes Filmcraft frames on worker threads
//! under a deadline. A file that has
//! moved, or sits on a volume that has gone away, drops out of its
//! group's stack; a group with no file left drops out of the row.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use crate::probe::{self, VideoInfo};

/// How many entries the file keeps. More than the row shows, so a group
/// whose files have all gone missing still leaves the row full.
const KEEP: usize = 12;
/// Tiles on the launch window (and ⌘1–4).
pub const SHOWN: usize = 4;
/// How many of a group's clips a tile stacks: the front one and the
/// cards peeking out behind it. The rest still open with the group.
pub const STACK: usize = 3;
/// A group's paths on one line of the list file. A tab can't start an
/// absolute path, and a filename with one in it is a thing nobody has.
const SEP: char = '\t';
/// The thumbnail atlas: `ATLAS_COLS` cells a row, `ATLAS` in all. The
/// launch row uses the first `SHOWN * STACK`; cut mode's clip and
/// chapter posters use the lot.
pub const ATLAS_COLS: usize = 4;
pub const ATLAS: usize = 64;
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

/// One recent entry: the clips that were open together, in slot order.
/// Never empty.
pub type Group = Vec<PathBuf>;

/// The recent groups, most recent first, with every path that is no
/// longer a file dropped (`prune`), so the launch window never shows a
/// tile for a clip that has gone. A missing or unreadable list is an
/// empty list, never an error: it is a convenience.
pub fn load() -> Vec<Group> {
    let Some(file) = list_path() else { return Vec::new() };
    prune(parse(&std::fs::read_to_string(file).unwrap_or_default()))
}

/// Keep only the paths that are files right now; a group with none left
/// goes. The list file itself is left alone (`KEEP` is deeper than the
/// row for exactly this), so a volume that comes back brings its
/// entries back with it. This stats every listed path: a dead network
/// mount can block here as it would in the thumbnail probe, but the
/// row would otherwise show its clips for the probe deadline and then
/// drop them, which is the hole this check closes.
fn prune(groups: Vec<Group>) -> Vec<Group> {
    groups
        .into_iter()
        .map(|g| {
            g.into_iter()
                .filter(|p| {
                    let ok = p.is_file();
                    if !ok {
                        log::info!("recent {}: no longer a file, skipped", p.display());
                    }
                    ok
                })
                .collect::<Group>()
        })
        .filter(|g| !g.is_empty())
        .collect()
}

fn parse(text: &str) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut group: Group = Vec::new();
        for p in line.split(SEP).map(str::trim).filter(|p| !p.is_empty()).map(PathBuf::from) {
            if p.is_absolute() && !group.contains(&p) {
                group.push(p);
            }
        }
        if !group.is_empty() && !out.contains(&group) {
            out.push(group);
        }
    }
    out.truncate(KEEP);
    out
}

/// `paths` are the clips open now, in slot order: put that group at the
/// front, drop an earlier copy of it, keep `KEEP`. Written atomically so
/// a crash mid-write can't leave half a list.
pub fn record<'a>(paths: impl IntoIterator<Item = &'a Path>) {
    let Some(file) = list_path() else { return };
    let mut opened: Group = Vec::new();
    for p in paths {
        // Absolute but NOT canonical: resolving symlinks would touch the
        // disk (this runs on the event loop) and turn /tmp into
        // /private/tmp, so the same file would be listed twice.
        let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        if !opened.contains(&p) {
            opened.push(p);
        }
    }
    if opened.is_empty() {
        return;
    }
    let merged = merge(&opened, &load());
    let text: String = merged
        .iter()
        .map(|g| g.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(&SEP.to_string()) + "\n")
        .collect();
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

/// The same clips in a different slot order are the same group: a
/// comparison swapped with Tab is not a new entry.
fn same_group(a: &[PathBuf], b: &[PathBuf]) -> bool {
    a.len() == b.len() && a.iter().all(|p| b.contains(p))
}

fn merge(opened: &Group, old: &[Group]) -> Vec<Group> {
    let mut out: Vec<Group> = vec![opened.clone()];
    for g in old {
        if !out.iter().any(|o| same_group(o, g)) {
            out.push(g.clone());
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

/// Filmcraft-decoded recent poster, display rotation applied and cover-cropped.
pub fn frame(path: &Path, at: f64) -> anyhow::Result<Vec<u8>> {
    decoded(path, at, true)
}
fn decoded(path: &Path, at: f64, cover: bool) -> anyhow::Result<Vec<u8>> {
    let path = path.to_owned();
    crate::fc::deadlined("thumbnail", THUMB_DEADLINE, move || {
        let source = crate::fc::open(&path)?;
        let time = filmcraft_time::Tick::from_seconds_f64(at.max(0.));
        let picture = filmcraft_media::cancel::with_background(true, || {
            source.video_frame(filmcraft_media::FrameRequest::full(time))
        })
        .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        if cover {
            crate::fc::cover_rgba_into(&picture, THUMB_W, THUMB_H, &mut out);
        } else {
            crate::fc::fit_rgba_into(
                &picture,
                crate::fc::Fit::Exact {
                    w: THUMB_W,
                    h: THUMB_H,
                },
                &mut out,
            );
        }
        Ok(out)
    })
    .map_err(anyhow::Error::msg)
}
/// A timeline frame on its actual presentation timestamp. Filmcraft handles B-frame order.
pub fn timeline_frame(path: &Path, at: f64, previous: Option<f64>) -> anyhow::Result<Vec<u8>> {
    let mut current = decoded(path, at, false)?;
    if let Some(t) = previous {
        let before = decoded(path, t, false)?;
        mask_unchanged(&mut current, &before);
    }
    Ok(current)
}

/// Changes is a decoded-image comparison, never a claim about codec residuals.
/// A small threshold suppresses quantization noise; unchanged pixels turn white.
fn mask_unchanged(current: &mut [u8], previous: &[u8]) {
    for (a, b) in current.as_chunks_mut::<4>().0.iter_mut().zip(previous.as_chunks::<4>().0.iter()) {
        if (0..3).all(|c| a[c].abs_diff(b[c]) <= 12) { a.copy_from_slice(&[255; 4]); }
    }
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
    #[test]
    fn changes_preserve_changed_pixels_and_white_out_small_differences() {
        let previous=[20,30,40,255,100,110,120,255];
        let mut current=[22,31,40,255,200,110,120,255];
        super::mask_unchanged(&mut current,&previous);
        assert_eq!(current,[255,255,255,255,200,110,120,255]);
    }

    #[test]
    fn timeline_preview_seeks_the_requested_b_frame() {
        use std::process::Command;
                if Command::new("ffmpeg").arg("-version").output().is_err() {return;}
        let path=std::env::temp_dir().join(format!("abner-frame-preview-{}.mp4",std::process::id()));
        assert!(Command::new("ffmpeg").args(["-y","-v","error","-f","lavfi","-i","testsrc2=size=96x64:rate=8:duration=1","-c:v","libx264","-threads","1","-g","4","-bf","2"]).arg(&path).status().unwrap().success());
        let all=Command::new("ffmpeg").args(["-v","error","-i"]).arg(&path).args(["-vf","scale=320:180:flags=area","-threads","1","-f","rawvideo","-pix_fmt","rgba","-"]).output().unwrap();
        assert!(all.status.success());
        let size=(super::THUMB_W*super::THUMB_H*4) as usize;
        let frame=super::timeline_frame(&path,0.375,None).unwrap();
        let error=|reference:&[u8]| frame.iter().zip(reference).map(|(a,b)|f64::from(*a)-f64::from(*b)).map(|d|d*d).sum::<f64>()/frame.len() as f64;
        let correct=error(&all.stdout[size*3..size*4]);
        assert!(correct<400.,"thumbnail conversion diverged grossly from reference: MSE {correct}");
        eprintln!("Filmcraft thumbnail reference MSE={correct}");
        if let Some(prefix)=std::env::var_os("ABNER_TEST_THUMB_OUTPUT"){
            for (suffix,pixels) in [("timeline.png",frame.clone()),("poster.png",super::frame(&path,0.375).unwrap())]{
                let path=std::path::PathBuf::from(&prefix).with_extension(suffix);let file=std::fs::File::options().write(true).create_new(true).open(path).unwrap();let mut encoder=png::Encoder::new(file,super::THUMB_W,super::THUMB_H);encoder.set_color(png::ColorType::Rgba);encoder.set_depth(png::BitDepth::Eight);encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
            }
        }
        assert!(correct<error(&all.stdout[size*2..size*3]),"requested B-frame must be closer than previous picture");
        assert!(correct<error(&all.stdout[size*4..size*5]),"requested B-frame must be closer than next picture");
        let changes=super::timeline_frame(&path,0.375,Some(0.25)).unwrap();
        let mut expected=frame;
        super::mask_unchanged(&mut expected,&super::timeline_frame(&path,0.25,None).unwrap());
        assert_eq!(changes,expected);
        assert!(changes.chunks_exact(4).any(|p|p==[255;4]));
        std::fs::remove_file(path).unwrap();
    }

    use super::*;

    fn g(paths: &[&str]) -> Group {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn parse_keeps_absolute_unique_groups_in_order() {
        // The old file (a path per line) reads as groups of one; a line
        // with tabs is one group.
        let got = parse("/a.mov\n\nrelative.mp4\n/b.mkv\t/c.mkv\t/b.mkv\n/a.mov\n");
        assert_eq!(got, vec![g(&["/a.mov"]), g(&["/b.mkv", "/c.mkv"])]);
    }

    #[test]
    fn prune_drops_missing_paths_and_empty_groups() {
        let dir = std::env::temp_dir().join(format!("abner-recent-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.mp4");
        let b = dir.join("b.mp4");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let gone = dir.join("gone.mp4");
        // A directory is not a file either.
        let got = prune(vec![vec![a.clone(), gone.clone()], vec![gone.clone()], vec![dir.clone()], vec![b.clone()]]);
        assert_eq!(got, vec![vec![a.clone()], vec![b.clone()]]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn merge_puts_the_opened_group_first_and_caps() {
        let old: Vec<Group> = (0..KEEP).map(|i| g(&[&format!("/{i}.mov")])).collect();
        let opened = g(&["/new.mov", "/3.mov"]);
        let got = merge(&opened, &old);
        assert_eq!(got.len(), KEEP);
        assert_eq!(got[0], opened);
        // A pair is its own entry: the lone /3.mov stays listed too.
        assert_eq!(got[1], g(&["/0.mov"]));
        assert!(got.contains(&g(&["/3.mov"])));
    }

    #[test]
    fn merge_drops_the_same_group_in_another_order() {
        let old = vec![g(&["/a.mov", "/b.mov"]), g(&["/c.mov"])];
        let got = merge(&g(&["/b.mov", "/a.mov"]), &old);
        assert_eq!(got, vec![g(&["/b.mov", "/a.mov"]), g(&["/c.mov"])]);
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
