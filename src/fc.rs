//! The FilmCraft bridge (switchblade's `fc.rs`, carried over): opening a clip as a
//! [`SharedSource`], the hardware decoder registration, the container's sync-sample table, and
//! turning a decoded frame into the RGBA the renderer uploads.
//!
//! FilmCraft decodes to planar Y'CbCr at native resolution, upright (it applies the container's
//! display rotation and clean aperture itself), with a GOP cache per source: sequential requests
//! decode forward without re-seeking, a jump decodes forward from the preceding sync sample. On
//! Apple silicon H.264, HEVC, VP9 and AV1 go through VideoToolbox (bit-exact with the software
//! decoders, which take over anything the hardware declines). abner shows pixels at native
//! size, so the usual path is the one-pass fixed-point conversion with no decimation; a player
//! asked for another size (thumbnails) decimates and resamples as switchblade's tiles do.

use std::path::Path;
use std::sync::Arc;

use filmcraft_frame::VideoFrame;
use filmcraft_media::{FrameRequest, MediaError, SharedSource};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use rayon::prelude::*;

/// Register the OS hardware decoder once (VideoToolbox on macOS). Cheap after the first call.
pub(crate) fn register_hardware() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(target_os = "macos")]
        {
            let a = filmcraft_platform::register();
            log::info!("hardware decoding: {a:?}");
        }
        filmcraft_codecs::hw::set_hardware_decoding(true);
    });
}

/// Hardware pictures decoded so far, process-wide: a lane compares before and after its first
/// frame to learn whether it really is on the hardware.
pub(crate) fn hw_frames() -> u64 {
    filmcraft_codecs::hw::hw_stats().frames
}

/// Positional reads of a file for FilmCraft's in-place container readers, so a 4K clip is not
/// read whole to show one frame.
pub(crate) struct FileReader {
    file: std::fs::File,
    len: u64,
}

impl filmcraft_media::ByteReader for FileReader {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if filmcraft_media::cancel::cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "media read cancelled",
            ));
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, offset)
        }
        #[cfg(not(unix))]
        {
            let _ = (offset, buf);
            Err(std::io::Error::other("positional reads are unix-only here"))
        }
    }
}

/// Content-relative times (seconds, ascending) of a clip's sync samples: where a decoder can
/// start without decoding forward. From the MP4 / MOV sample tables or the Matroska cues, read
/// from the container headers only (no cluster scan). `None` for other containers or a file
/// without an index: every seek is then exact.
pub(crate) struct SyncTable {
    times: Vec<f64>,
}

impl SyncTable {
    pub(crate) fn open(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let src = FileReader { file, len };
        let mut times = match ext.as_str() {
            "mp4" | "m4v" | "mov" | "mp4v" | "m4a" => {
                let f = filmcraft_isobmff::open(src).ok()?;
                let i = f.track_of_kind(filmcraft_isobmff::TrackKind::Video)?;
                let t = &f.tracks[i];
                if t.timescale == 0 {
                    return None;
                }
                let start = (0..t.samples.len())
                    .filter_map(|k| t.presentation_pts(k))
                    .min()
                    .unwrap_or(0);
                t.sync_samples()
                    .iter()
                    .filter_map(|&k| t.presentation_pts(k as usize))
                    .map(|p| (p - start).max(0) as f64 / t.timescale as f64)
                    .collect::<Vec<f64>>()
            }
            "mkv" | "webm" | "mka" => {
                let opts = filmcraft_matroska::OpenOptions {
                    index: false,
                    ..Default::default()
                };
                let f = filmcraft_matroska::open_with(src, &opts).ok()?;
                if f.cues.is_empty() {
                    return None;
                }
                let scale = f.info.timestamp_scale.max(1) as f64 / 1e9;
                f.cues
                    .iter()
                    .map(|c| c.time as f64 * scale)
                    .collect::<Vec<f64>>()
            }
            _ => return None,
        };
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        times.dedup();
        (!times.is_empty()).then_some(Self { times })
    }

    /// The greatest sync time ≤ `t` (the first one if `t` precedes all).
    pub(crate) fn before(&self, t: f64) -> f64 {
        let k = self.times.partition_point(|&x| x <= t + 1e-6);
        self.times[k.saturating_sub(1)]
    }

    /// The smallest sync time ≥ `t`, if any.
    pub(crate) fn at_or_after(&self, t: f64) -> Option<f64> {
        let k = self.times.partition_point(|&x| x < t - 1e-6);
        self.times.get(k).copied()
    }
}

impl filmcraft_isobmff::ByteSource for FileReader {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        filmcraft_media::ByteReader::read_at(self, offset, buf)
    }
}

impl filmcraft_matroska::ByteSource for FileReader {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        filmcraft_media::ByteReader::read_at(self, offset, buf)
    }
}

/// Regular-file reader shared by metadata and playback; never opens a FIFO.
pub(crate) fn reader(path: &Path) -> Result<FileReader, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("stat: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    Ok(FileReader {
        file,
        len: meta.len(),
    })
}
/// Bound caller latency and propagate cooperative cancellation into Filmcraft.
/// A kernel read on unavailable storage cannot be interrupted; the worker is abandoned.
pub(crate) fn deadlined<T: Send + 'static>(
    label: &'static str,
    deadline: std::time::Duration,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancel.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(label.into())
        .spawn(move || {
            let result = filmcraft_media::cancel::with_cancel(&flag, work);
            let _ = tx.send(result);
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(deadline) {
        Ok(r) => r,
        Err(e) => {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            Err(format!("{label}: {e}; storage may be unavailable"))
        }
    }
}

/// Open `path` with every container and codec FilmCraft knows, reading in place; a container
/// without an in-place reader is read whole.
pub(crate) fn open(path: &Path) -> Result<SharedSource, String> {
    register_hardware();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Stat before open: opening a FIFO for reading blocks until a writer appears, and a
    // directory or a socket is never a clip. (A dead network mount can still block in the
    // kernel here; nothing in user space interrupts that.)
    let meta = std::fs::metadata(path).map_err(|e| format!("stat: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let len = meta.len();
    let reader: filmcraft_media::SharedReader = Arc::new(FileReader { file, len });
    match filmcraft_media::reader::open_reader(
        &name,
        reader,
        &filmcraft_codecs::reader_openers(),
        &filmcraft_codecs::openers(),
    ) {
        Ok(s) => Ok(s),
        Err(MediaError::Unsupported(_)) => {
            let bytes: Arc<[u8]> = std::fs::read(path)
                .map_err(|e| format!("read: {e}"))?
                .into();
            filmcraft_codecs::open_bytes(&name, bytes).map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The video stream's frame interval in seconds and its frame count, or `None` for a source
/// without video or without a usable rate.
pub(crate) fn timing(src: &SharedSource) -> Option<(f64, u64)> {
    let info = src.info();
    let v = info.video.as_ref()?;
    let (num, den) = (v.frame_rate.num, v.frame_rate.den);
    if num <= 0 || den <= 0 {
        return None;
    }
    let frame_s = den as f64 / num as f64;
    let duration_s = info.duration.0 as f64 / TICKS_PER_SECOND as f64;
    let frames = (duration_s / frame_s).round().max(1.0) as u64;
    Some((frame_s, frames))
}

/// Media time to ask FilmCraft for frame `index`: a quarter of a frame past its start, which
/// tolerates timestamps rounded either way by a coarse container timescale (Matroska's 1 ms).
pub(crate) fn request(index: u64, frame_s: f64) -> FrameRequest {
    let t = (index as f64 + 0.25) * frame_s;
    FrameRequest::full(Tick((t * TICKS_PER_SECOND as f64).round() as i64))
}

/// Frame `index` of the source, or `Err(true)` when the request was cancelled through `cancel`
/// (a seek superseded it), `Err(false)` for any other failure.
pub(crate) fn frame_at(
    src: &SharedSource,
    index: u64,
    frame_s: f64,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<Arc<VideoFrame>, bool> {
    filmcraft_media::cancel::with_cancel(cancel, || src.video_frame(request(index, frame_s)))
        .map_err(|e| {
            let cancelled = matches!(e, MediaError::Cancelled);
            if !cancelled {
                log::debug!("frame {index}: {e}");
            }
            cancelled
        })
}

/// How a decoded picture is fitted to the pixels asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // switchblade's fits; abner uses `Exact` (native size, or a thumbnail's)
pub enum Fit {
    /// Exactly `w`×`h`, stretching as the renderer's tile already has the clip's aspect.
    Exact { w: u32, h: u32 },
    /// The clip's own aspect inside `w`×`h`, each side even (chapter chips).
    Inside { w: u32, h: u32 },
    /// Exactly `w`×`h` after turning a portrait picture clockwise to landscape (the sprite
    /// sheet's cells; `transpose=clock:passthrough=landscape` in the old filter chain).
    Landscape { w: u32, h: u32 },
}

impl Fit {
    /// The output size for a picture of `fw`×`fh`, and whether it is turned first.
    fn resolve(self, fw: u32, fh: u32) -> ((u32, u32), bool) {
        match self {
            Fit::Exact { w, h } => ((w.max(1), h.max(1)), false),
            Fit::Inside { w, h } => {
                let (w, h) = (w.max(2) as f64, h.max(2) as f64);
                let s = (w / fw.max(1) as f64).min(h / fh.max(1) as f64);
                let ow = ((fw as f64 * s).round() as u32).max(2) & !1;
                let oh = ((fh as f64 * s).round() as u32).max(2) & !1;
                ((ow, oh), false)
            }
            Fit::Landscape { w, h } => ((w.max(1), h.max(1)), fh > fw),
        }
    }
}

/// `frame` fitted per `fit`, as tightly packed RGBA8 written into `out` (resized), with its size.
pub(crate) fn fit_rgba_into(frame: &VideoFrame, fit: Fit, out: &mut Vec<u8>) -> (u32, u32) {
    let ((tw, th), turn) = fit.resolve(frame.width, frame.height);
    let turned;
    let frame = if turn {
        turned = frame.rotated(1);
        &turned
    } else {
        frame
    };
    // Most of a big reduction as an integer box filter in Y'CbCr, before the colour conversion.
    let n = ((frame.width / tw.max(1)).min(frame.height / th.max(1))) as usize;
    if n >= 1
        && let Some((ow, oh)) = fused_decimate_rgba(frame, n, out)
    {
        // 8-bit 4:2:0 with an even factor: one pass over the planes, straight into `out`.
        if (ow as u32, oh as u32) == (tw, th) {
            return (tw, th);
        }
        let src = std::mem::take(out);
        resize_bilinear(&src, ow, oh, tw as usize, th as usize, out);
        return (tw, th);
    }
    let (rgba, sw, sh) = {
        {
            let decimated;
            let frame = if n >= 2 {
                match frame.box_decimated(n) {
                    Some(d) => {
                        decimated = d;
                        &decimated
                    }
                    None => frame,
                }
            } else {
                frame
            };
            (
                frame.to_rgba8(),
                frame.width as usize,
                frame.height as usize,
            )
        }
    };
    if (sw as u32, sh as u32) == (tw, th) {
        out.clear();
        out.extend_from_slice(&rgba);
    } else {
        resize_bilinear(&rgba, sw, sh, tw as usize, th as usize, out);
    }
    (tw, th)
}

/// Box-decimate an 8-bit 4:2:0 frame by an even factor `n` and convert to RGBA8 in one pass:
/// each output pixel averages `n`×`n` luma samples and the `n/2`×`n/2` chroma samples under
/// them, then goes through the frame's own matrix and range (the direct display conversion
/// `VideoFrame::to_rgba8` uses for BT.709 / sRGB transfers). `None` for anything else, which
/// takes the two-pass path.
fn fused_decimate_rgba(frame: &VideoFrame, n: usize, out: &mut Vec<u8>) -> Option<(usize, usize)> {
    use filmcraft_frame::{Chroma, PixelData};
    let PixelData::Yuv8 {
        planes,
        chroma: Chroma::C420,
        alpha: None,
    } = &frame.data
    else {
        return None;
    };
    if n < 1
        || !(n == 1 || n.is_multiple_of(2))
        || !matches!(
            frame.color.transfer,
            filmcraft_color::Transfer::Bt709 | filmcraft_color::Transfer::Srgb
        )
    {
        return None;
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (ow, oh) = ((w / n).max(1), (h / n).max(1));
    // n == 1: no decimation; each 4:2:0 chroma sample serves a 2×2 block of output pixels.
    let m = (n / 2).max(1);
    if planes[0].len() < w * h
        || planes[1].len() < cw * ch
        || planes[2].len() < cw * ch
        || ow * n > w
        || oh * n > h
    {
        return None;
    }
    let (kr, kb) = frame.color.matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    let (ys, yo, cs) = match frame.color.range {
        filmcraft_color::Range::Limited => (255.0 / 219.0, 16.0, 255.0 / 224.0),
        filmcraft_color::Range::Full => (1.0, 0.0, 1.0),
    };
    let (crr, cbb) = (2.0 * (1.0 - kr) * cs, 2.0 * (1.0 - kb) * cs);
    let (cgr, cgb) = (crr * kr / kg, cbb * kb / kg);
    // 16.16 fixed point: the luma sum of n×n samples and the chroma sum of m×m samples are
    // scaled by their counts inside the coefficients, so a pixel is a few multiply-adds.
    const F: f32 = 65536.0;
    let ny = (n * n) as f32;
    let nc = (m * m) as f32;
    let ky = (ys / ny * F) as i64;
    let k_rv = (crr / nc * F) as i64;
    let k_gu = (cgb / nc * F) as i64;
    let k_gv = (cgr / nc * F) as i64;
    let k_bu = (cbb / nc * F) as i64;
    // yo in luma units, 128 in chroma units, both pre-scaled by the counts they offset.
    let off_y = (yo * ny) as i64;
    let off_c = (128.0 * nc) as i64;
    let round = (F / 2.0) as i64;
    let clamp8 = |v: i64| -> u8 { ((v + round) >> 16).clamp(0, 255) as u8 };
    let (yp, up, vp) = (&planes[0], &planes[1], &planes[2]);
    out.resize(ow * oh * 4, 0);
    // Rows fan out over rayon for big pictures; a small one (thumbnails, test clips) is cheaper
    // on the calling thread than as a few hundred pool tasks contending with other players.
    let parallel = ow * oh >= 512 * 1024;
    let convert_row = |(oy, row): (usize, &mut [u8])| {
        if n == 1 {
            // Native size: one luma sample per pixel, chroma shared by 2×2 (nearest, as the
            // direct display conversion in `VideoFrame::to_rgba8`).
            let yr = &yp[oy * w..oy * w + ow];
            let (ur, vr) = (
                &up[(oy / 2) * cw..(oy / 2) * cw + cw],
                &vp[(oy / 2) * cw..(oy / 2) * cw + cw],
            );
            for (ox, o) in row.chunks_exact_mut(4).enumerate() {
                let yy = (yr[ox] as i64 - off_y) * ky;
                let u = ur[ox / 2] as i64 - off_c;
                let v = vr[ox / 2] as i64 - off_c;
                o[0] = clamp8(yy + k_rv * v);
                o[1] = clamp8(yy - k_gv * v - k_gu * u);
                o[2] = clamp8(yy + k_bu * u);
                o[3] = 255;
            }
            return;
        }
        if n == 2 {
            // The common case (4K to a 1080p view): 2×2 luma, one chroma sample per pixel.
            let (y0, y1) = (
                &yp[oy * 2 * w..oy * 2 * w + ow * 2],
                &yp[(oy * 2 + 1) * w..(oy * 2 + 1) * w + ow * 2],
            );
            let (ur, vr) = (&up[oy * cw..oy * cw + ow], &vp[oy * cw..oy * cw + ow]);
            for (ox, o) in row.chunks_exact_mut(4).enumerate() {
                let ysum = y0[ox * 2] as i64
                    + y0[ox * 2 + 1] as i64
                    + y1[ox * 2] as i64
                    + y1[ox * 2 + 1] as i64;
                let yy = (ysum - off_y) * ky;
                let u = ur[ox] as i64 - off_c;
                let v = vr[ox] as i64 - off_c;
                o[0] = clamp8(yy + k_rv * v);
                o[1] = clamp8(yy - k_gv * v - k_gu * u);
                o[2] = clamp8(yy + k_bu * u);
                o[3] = 255;
            }
            return;
        }
        // Luma rows of this output row and chroma rows under them.
        let yrows: Vec<&[u8]> = (oy * n..oy * n + n)
            .map(|y| &yp[y * w..y * w + ow * n])
            .collect();
        let urows: Vec<&[u8]> = (oy * m..oy * m + m)
            .map(|y| &up[y * cw..y * cw + ow * m])
            .collect();
        let vrows: Vec<&[u8]> = (oy * m..oy * m + m)
            .map(|y| &vp[y * cw..y * cw + ow * m])
            .collect();
        for (ox, o) in row.chunks_exact_mut(4).enumerate() {
            let mut ysum = 0i64;
            for r in &yrows {
                ysum += r[ox * n..ox * n + n].iter().map(|&v| v as i64).sum::<i64>();
            }
            let (mut usum, mut vsum) = (0i64, 0i64);
            for (ur, vr) in urows.iter().zip(&vrows) {
                usum += ur[ox * m..ox * m + m]
                    .iter()
                    .map(|&v| v as i64)
                    .sum::<i64>();
                vsum += vr[ox * m..ox * m + m]
                    .iter()
                    .map(|&v| v as i64)
                    .sum::<i64>();
            }
            let yy = (ysum - off_y) * ky;
            let u = usum - off_c;
            let v = vsum - off_c;
            o[0] = clamp8(yy + k_rv * v);
            o[1] = clamp8(yy - k_gv * v - k_gu * u);
            o[2] = clamp8(yy + k_bu * u);
            o[3] = 255;
        }
    };
    if parallel {
        out.par_chunks_mut(ow * 4).enumerate().for_each(convert_row);
    } else {
        out.chunks_mut(ow * 4).enumerate().for_each(convert_row);
    }
    Some((ow, oh))
}

/// Bilinear resample of tightly packed RGBA8 into `out`.
fn resize_bilinear(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, out: &mut Vec<u8>) {
    out.resize(dw * dh * 4, 0);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh * 4 {
        return;
    }
    let sx = sw as f32 / dw as f32;
    let sy = sh as f32 / dh as f32;
    out.par_chunks_mut(dw * 4).enumerate().for_each(|(y, row)| {
        let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
        let y0 = (fy as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = fy - y0 as f32;
        let r0 = &src[y0 * sw * 4..][..sw * 4];
        let r1 = &src[y1 * sw * 4..][..sw * 4];
        for x in 0..dw {
            let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
            let x0 = (fx as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = fx - x0 as f32;
            let o = &mut row[x * 4..x * 4 + 4];
            for c in 0..4 {
                let top = r0[x0 * 4 + c] as f32 * (1.0 - wx) + r0[x1 * 4 + c] as f32 * wx;
                let bot = r1[x0 * 4 + c] as f32 * (1.0 - wx) + r1[x1 * 4 + c] as f32 * wx;
                o[c] = (top * (1.0 - wy) + bot * wy + 0.5) as u8;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_keeps_aspect_and_evenness() {
        assert_eq!(
            Fit::Inside { w: 320, h: 180 }.resolve(1080, 1920),
            ((100, 180), false)
        );
        assert_eq!(
            Fit::Inside { w: 320, h: 180 }.resolve(1920, 1080),
            ((320, 180), false)
        );
        assert_eq!(
            Fit::Landscape { w: 320, h: 180 }.resolve(1080, 1920),
            ((320, 180), true)
        );
    }

    #[test]
    fn fused_decimation_matches_the_two_pass_path() {
        use filmcraft_frame::{Chroma, PixelData};
        let (w, h) = (64u32, 36u32);
        let mut y = vec![0u8; (w * h) as usize];
        for (i, v) in y.iter_mut().enumerate() {
            *v = (i * 7 % 200 + 20) as u8;
        }
        let (cw, ch) = ((w / 2) as usize, (h / 2) as usize);
        // Chroma constant over 2×2 blocks: the two-pass path averages chroma 2×2 before the
        // conversion where the fused one uses the one chroma sample under each output pixel
        // (finer, since 4:2:0 chroma at half the decimation is already at the output size).
        let u: Vec<u8> = (0..cw * ch)
            .map(|i| ((i % cw / 2) * 3 % 120 + 60) as u8)
            .collect();
        let v: Vec<u8> = (0..cw * ch)
            .map(|i| ((i / cw / 2) * 5 % 140 + 50) as u8)
            .collect();
        let frame = VideoFrame {
            width: w,
            height: h,
            data: PixelData::Yuv8 {
                planes: [Arc::new(y), Arc::new(u), Arc::new(v)],
                chroma: Chroma::C420,
                alpha: None,
            },
            color: filmcraft_color::ColorInfo::REC709,
            par: (1, 1),
            pts: Tick::ZERO,
        };
        let mut fused = Vec::new();
        let (ow, oh) = fused_decimate_rgba(&frame, 2, &mut fused).unwrap();
        let two_pass = frame.box_decimated(2).unwrap().to_rgba8();
        assert_eq!((ow, oh), (32, 18));
        let worst = fused
            .iter()
            .zip(&two_pass)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 2, "fused and two-pass differ by {worst}");
    }

    #[test]
    fn bilinear_resize_averages() {
        // 2×1 → 1×1: the midpoint of both pixels
        let src = [0u8, 0, 0, 255, 100, 200, 50, 255];
        let mut out = Vec::new();
        resize_bilinear(&src, 2, 1, 1, 1, &mut out);
        assert_eq!(out, [50, 100, 25, 255]);
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    /// `cargo test -p sb-media --release -- --ignored fit_timing --nocapture`: the cost of
    /// fitting one 4K frame to a tile, which must fit a 60 fps budget with decode.
    #[test]
    #[ignore]
    fn fit_timing() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks/hevc_2160p60.mp4");
        let Ok(src) = open(&path) else {
            eprintln!("no fixture");
            return;
        };
        let (frame_s, _) = timing(&src).unwrap();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = frame_at(&src, 10, frame_s, &cancel).unwrap();
        let mut buf = Vec::new();
        for fit in [
            Fit::Exact { w: 400, h: 225 },
            Fit::Exact { w: 1920, h: 1080 },
        ] {
            let t = std::time::Instant::now();
            for _ in 0..20 {
                fit_rgba_into(&f, fit, &mut buf);
            }
            eprintln!(
                "{fit:?}: {:.2} ms/frame",
                t.elapsed().as_secs_f64() * 1000.0 / 20.0
            );
        }
        let t = std::time::Instant::now();
        for i in 11..31 {
            let _ = frame_at(&src, i, frame_s, &cancel).unwrap();
        }
        eprintln!(
            "decode: {:.2} ms/frame",
            t.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
        // The end of the clip, then a wrap to the start: what the player does on a loop.
        let (_, frames) = timing(&src).unwrap();
        let stats = |label: &str| {
            let h = filmcraft_codecs::hw::hw_stats();
            eprintln!(
                "{label}: hw frames {} sessions {} declined {} fallbacks {}",
                h.frames, h.sessions, h.declined, h.fallbacks
            );
        };
        stats("before tail");
        for i in frames - 12..frames {
            let _ = frame_at(&src, i, frame_s, &cancel);
        }
        stats("after tail");
        let t = std::time::Instant::now();
        for i in 0..30 {
            let t1 = std::time::Instant::now();
            let r = frame_at(&src, i, frame_s, &cancel);
            if i < 6 || i % 10 == 0 {
                eprintln!(
                    "  wrap frame {i}: {:.1} ms ok={}",
                    t1.elapsed().as_secs_f64() * 1000.0,
                    r.is_ok()
                );
            }
        }
        eprintln!(
            "after wrap: {:.2} ms/frame",
            t.elapsed().as_secs_f64() * 1000.0 / 30.0
        );
        stats("after wrap");
    }
}

#[cfg(test)]
mod register_timing {
    /// `cargo test --release -- --ignored register_cost --nocapture`: the one-time cost of the
    /// hardware decoder registration, paid by the first open in a process.
    #[test]
    #[ignore]
    fn register_cost() {
        let t = std::time::Instant::now();
        super::register_hardware();
        eprintln!(
            "register_hardware: {:.1} ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
        let t = std::time::Instant::now();
        super::register_hardware();
        eprintln!("second call: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Display-aspect cover crop for recent posters, before reduction to avoid giant intermediates.
pub(crate) fn cover_rgba_into(frame: &VideoFrame, w: u32, h: u32, out: &mut Vec<u8>) {
    let par = frame.par.0.max(1) as f64 / frame.par.1.max(1) as f64;
    let source = frame.width as f64 * par / frame.height.max(1) as f64;
    let target = w.max(1) as f64 / h.max(1) as f64;
    let (cw, ch) = if source > target {
        (
            ((frame.height as f64 * target / par).round() as usize).clamp(1, frame.width as usize),
            frame.height as usize,
        )
    } else {
        (
            frame.width as usize,
            ((frame.width as f64 * par / target).round() as usize).clamp(1, frame.height as usize),
        )
    };
    let cropped = frame.cropped(filmcraft_frame::Region {
        x: (frame.width as usize - cw) / 2,
        y: (frame.height as usize - ch) / 2,
        w: cw,
        h: ch,
    });
    fit_rgba_into(&cropped, Fit::Exact { w, h }, out);
}
#[cfg(test)]
mod migration_tests {
    use super::*;
    #[test]
    fn deadline_bounds_caller_and_signals_cooperative_work() {
        let (tx, rx) = std::sync::mpsc::channel();
        let start = std::time::Instant::now();
        let r: Result<(), _> = deadlined(
            "test-read",
            std::time::Duration::from_millis(5),
            move || {
                std::thread::sleep(std::time::Duration::from_millis(40));
                tx.send(filmcraft_media::cancel::cancelled()).unwrap();
                Ok(())
            },
        );
        assert!(r.is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert!(rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap());
    }
    #[test]
    fn poster_cover_crops_center_without_stretching_or_losing_alpha() {
        let mut rgba = vec![0; 160 * 64 * 4];
        for y in 0..64 {
            for x in 0..160 {
                let p = (y * 160 + x) * 4;
                rgba[p..p + 4].copy_from_slice(if (48..112).contains(&x) {
                    &[0, 255, 0, 255]
                } else {
                    &[255, 0, 0, 255]
                });
            }
        }
        let frame = VideoFrame::rgba8(160, 64, rgba);
        let mut out = Vec::new();
        cover_rgba_into(&frame, 64, 64, &mut out);
        assert_eq!(out.len(), 64 * 64 * 4);
        assert!(
            out.as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 255, 0, 255])
        );
    }
}
