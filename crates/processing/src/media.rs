//! FilmCraft transport. Explicit SDR RGB16 processing, ProRes HQ MOV output,
//! and packet-preserving audio; no external media programs or display buffers.
use crate::{Chain, Stage};
use abner_processor_api::{Cancellation, Frame, Registry, Result};
use filmcraft_color::{Matrix, Transfer, normalize_c, normalize_y, rgb_to_ycbcr, ycbcr_to_rgb};
use filmcraft_frame::{PixelData, VideoFrame};
use filmcraft_isobmff::{
    self as mp4, Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, TrackKind, WriteSample,
    WriterOptions,
};
use filmcraft_media::{ByteReader, FrameRequest};
use filmcraft_time::Tick;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct Job {
    pub input: PathBuf,
    pub output: PathBuf,
    pub stages: Vec<Stage>,
    pub copy_audio: bool,
    /// Bounds frame batches, excluding model/runtime allocations and decoder caches.
    pub memory_bytes: usize,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct Report {
    pub frames: usize,
    pub width: usize,
    pub height: usize,
    pub audio_tracks: usize,
    pub output: PathBuf,
}

struct Reader {
    file: File,
    len: u64,
}
impl Reader {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(err)?;
        let len = file.metadata().map_err(err)?.len();
        Ok(Self { file, len })
    }
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
impl ByteReader for Reader {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, off)
        }
        #[cfg(not(unix))]
        {
            let _ = (off, buf);
            Err(std::io::Error::other("positional reads require Unix"))
        }
    }
}
impl mp4::ByteSource for Reader {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        ByteReader::read_at(self, off, buf)
    }
}
struct Work(PathBuf);
impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Convert encoded SDR planes to encoded RGB16, without a display transfer/linear-light round trip.
pub fn rgb16(frame: &VideoFrame) -> Result<Vec<u16>> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0
        || h == 0
        || w > 8192
        || h > 8192
        || frame.color.transfer != Transfer::Bt709
        || frame.color.primaries != filmcraft_color::Primaries::Bt709
    {
        return Err("requires SDR Rec.709 dimensions 1..8192".into());
    }
    let mut out = vec![0; w * h * 3];
    let mut pixel = |p: usize, y: u32, u: u32, v: u32, bits: u32| {
        let rgb = ycbcr_to_rgb(
            normalize_y(y, bits, frame.color.range),
            normalize_c(u, bits, frame.color.range),
            normalize_c(v, bits, frame.color.range),
            frame.color.matrix,
        );
        for c in 0..3 {
            out[p * 3 + c] = (rgb[c].clamp(0., 1.) * 65535.).round_ties_even() as u16;
        }
    };
    match &frame.data {
        PixelData::Rgba8(data) => {
            if data.len() != w * h * 4 {
                return Err("invalid RGBA frame".into());
            }
            for (p, rgba) in data.as_chunks::<4>().0.iter().enumerate() {
                if rgba[3] != 255 {
                    return Err("alpha video is unsupported".into());
                }
                for c in 0..3 {
                    out[p * 3 + c] = u16::from(rgba[c]) * 257;
                }
            }
        }
        PixelData::Yuv8 {
            planes,
            chroma,
            alpha,
        } => {
            if alpha.is_some() {
                return Err("alpha video is unsupported".into());
            }
            let (sx, sy) = chroma.shifts();
            let cw = w.div_ceil(1 << sx);
            let ch = h.div_ceil(1 << sy);
            if planes[0].len() != w * h || planes[1].len() != cw * ch || planes[2].len() != cw * ch
            {
                return Err("invalid YUV planes".into());
            }
            for y in 0..h {
                for x in 0..w {
                    let p = y * w + x;
                    let q = (y >> sy) * cw + (x >> sx);
                    pixel(
                        p,
                        u32::from(planes[0][p]),
                        u32::from(planes[1][q]),
                        u32::from(planes[2][q]),
                        8,
                    );
                }
            }
        }
        PixelData::Yuv16 {
            planes,
            chroma,
            bits,
            alpha,
        } => {
            if alpha.is_some() || !(9..=16).contains(bits) {
                return Err("unsupported YUV depth/alpha".into());
            }
            let (sx, sy) = chroma.shifts();
            let cw = w.div_ceil(1 << sx);
            let ch = h.div_ceil(1 << sy);
            if planes[0].len() != w * h || planes[1].len() != cw * ch || planes[2].len() != cw * ch
            {
                return Err("invalid YUV planes".into());
            }
            for y in 0..h {
                for x in 0..w {
                    let p = y * w + x;
                    let q = (y >> sy) * cw + (x >> sx);
                    pixel(
                        p,
                        u32::from(planes[0][p]),
                        u32::from(planes[1][q]),
                        u32::from(planes[2][q]),
                        *bits,
                    );
                }
            }
        }
        PixelData::RgbaF32(_) => {
            return Err("linear RGBA sources require an explicit SDR transform".into());
        }
    }
    Ok(out)
}
/// Filmcraft ProRes HQ uses 10-bit limited-range BT.709 4:2:2; average each horizontal chroma pair.
pub fn prores_frame(rgb: &[u16], w: usize, h: usize) -> Result<filmcraft_prores::Frame> {
    if rgb.len() != w * h * 3 || w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err("invalid ProRes RGB buffer".into());
    }
    let mut frame = filmcraft_prores::Frame::new(
        w as u32,
        h as u32,
        filmcraft_prores::ChromaFormat::Yuv422,
        10,
        false,
    );
    for y in 0..h {
        for x in (0..w).step_by(2) {
            let mut uv = [0f32; 2];
            let n = (w - x).min(2);
            for dx in 0..n {
                let p = (y * w + x + dx) * 3;
                let c = rgb_to_ycbcr(
                    f32::from(rgb[p]) / 65535.,
                    f32::from(rgb[p + 1]) / 65535.,
                    f32::from(rgb[p + 2]) / 65535.,
                    Matrix::Bt709,
                );
                frame.y[y * w + x + dx] =
                    (64. + 876. * c[0]).round_ties_even().clamp(64., 940.) as u16;
                uv[0] += c[1];
                uv[1] += c[2];
            }
            let q = y * w.div_ceil(2) + x / 2;
            frame.cb[q] = (512. + 896. * uv[0] / n as f32)
                .round_ties_even()
                .clamp(64., 960.) as u16;
            frame.cr[q] = (512. + 896. * uv[1] / n as f32)
                .round_ties_even()
                .clamp(64., 960.) as u16;
        }
    }
    Ok(frame)
}
fn box_children(file: &mut File, start: u64, end: u64) -> Result<Vec<([u8; 4], u64, u64)>> {
    let mut pos = start;
    let mut boxes = Vec::new();
    while pos < end {
        if end - pos < 8 {
            return Err("truncated output box".into());
        }
        file.seek(SeekFrom::Start(pos)).map_err(err)?;
        let mut head = [0; 8];
        file.read_exact(&mut head).map_err(err)?;
        let size = u32::from_be_bytes(head[..4].try_into().map_err(err)?);
        let kind = head[4..].try_into().map_err(err)?;
        let (size, header) = if size == 1 {
            let mut b = [0; 8];
            file.read_exact(&mut b).map_err(err)?;
            (u64::from_be_bytes(b), 16)
        } else if size == 0 {
            (end - pos, 8)
        } else {
            (u64::from(size), 8)
        };
        if size < header || size > end - pos {
            return Err("invalid output box extent".into());
        }
        boxes.push((kind, pos + header, pos + size));
        pos += size;
    }
    Ok(boxes)
}
fn preserve_audio_flags(file: &mut File, audio: &[&mp4::Track]) -> Result<()> {
    let len = file.metadata().map_err(err)?.len();
    let moov = box_children(file, 0, len)?
        .into_iter()
        .find(|b| b.0 == *b"moov")
        .ok_or("missing output moov")?;
    let traks: Vec<_> = box_children(file, moov.1, moov.2)?
        .into_iter()
        .filter(|b| b.0 == *b"trak")
        .collect();
    if traks.len() != audio.len() + 1 {
        return Err("unexpected output track count".into());
    }
    for (i, input) in audio.iter().enumerate() {
        let t = traks[i + 1];
        let h = box_children(file, t.1, t.2)?
            .into_iter()
            .find(|b| b.0 == *b"tkhd")
            .ok_or("missing output tkhd")?;
        file.seek(SeekFrom::Start(h.1)).map_err(err)?;
        let mut vf = [0; 4];
        file.read_exact(&mut vf).map_err(err)?;
        let group = h.1
            + match vf[0] {
                0 => 34,
                1 => 46,
                _ => return Err("unsupported tkhd version".into()),
            };
        if group + 4 > h.2 {
            return Err("short output tkhd".into());
        }
        vf[3] = (vf[3] & !1) | u8::from(input.enabled);
        file.seek(SeekFrom::Start(h.1)).map_err(err)?;
        file.write_all(&vf).map_err(err)?;
        file.seek(SeekFrom::Start(group)).map_err(err)?;
        file.write_all(&input.alternate_group.to_be_bytes())
            .map_err(err)?;
        file.write_all(&input.volume.to_be_bytes()).map_err(err)?;
    }
    file.flush().map_err(err)
}
fn packet_hash(
    movie: &mp4::Mp4File,
    reader: &Reader,
    index: usize,
    cancel: &Cancellation,
) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    for k in 0..movie.tracks[index].samples.len() {
        cancel.check()?;
        hash.update(movie.read_sample(reader, index, k).map_err(err)?);
    }
    Ok(hash.finalize().into())
}

/// Run a complete, ordered job. Currently accepts zero-start CFR MP4/MOV SDR
/// sources and writes ProRes HQ MOV. Rejects unsupported formats before inference.
/// Output timestamps may be retimed by processors, but total duration must remain.
pub fn run(
    job: &Job,
    registry: &Registry,
    cancel: &Cancellation,
    progress: abner_processor_api::ProgressReporter,
) -> Result<Report> {
    cancel.check()?;
    if job.output.symlink_metadata().is_ok() {
        return Err("output already exists".into());
    }
    if job.output.extension().and_then(|s| s.to_str()) != Some("mov") {
        return Err(
            "native output requires .mov (ProRes HQ); no implicit codec substitution".into(),
        );
    }
    if job.memory_bytes == 0 {
        return Err("memory budget must be positive".into());
    }
    let reader = Reader::open(&job.input)?;
    let movie = mp4::open(Reader::open(&job.input)?)
        .map_err(|e| format!("native packet-copy jobs currently require MP4/MOV input: {e}"))?;
    let videos: Vec<_> = movie
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .collect();
    let audio: Vec<_> = if job.copy_audio {
        movie
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .collect()
    } else {
        vec![]
    };
    if videos.len() != 1
        || movie
            .tracks
            .iter()
            .any(|t| !matches!(t.kind, TrackKind::Video | TrackKind::Audio))
    {
        return Err(
            "requires one video and optional audio; other tracks are not silently dropped".into(),
        );
    }
    let vt = videos[0];
    let vp = vt.video().ok_or("missing video sample entry")?;
    let (w, h) = (usize::from(vp.width), usize::from(vp.height));
    if w == 0
        || h == 0
        || w > 8192
        || h > 8192
        || w * h * 6 > job.memory_bytes
        || vt.display_rotation() != Some(0)
        || vp.pixel_aspect.is_some_and(|(a, b)| a != b)
        || vp.clean_aperture.is_some()
        || vp.mastering_display.is_some()
        || vp.content_light.is_some()
    {
        return Err(
            "requires square-pixel SDR video without rotation/aperture, within frame budget".into(),
        );
    }
    if vt.entries.len() != 1
        || vt.samples.is_empty()
        || vt.timescale == 0
        || movie.timescale == 0
        || vt.samples.iter().any(|s| s.description_index != 1)
    {
        return Err("unsupported video sample table".into());
    }
    let mut pts: Vec<_> = (0..vt.samples.len())
        .map(|i| vt.presentation_pts(i).ok_or("invalid video timestamp"))
        .collect::<std::result::Result<_, _>>()?;
    pts.sort_unstable();
    let delta = vt.samples[0].duration;
    if delta == 0
        || pts[0] != 0
        || pts
            .iter()
            .enumerate()
            .any(|(i, &p)| p != i as i64 * i64::from(delta))
        || vt.samples.iter().any(|s| s.duration != delta)
    {
        return Err(
            "native job currently requires zero-start CFR input; VFR is not silently resampled"
                .into(),
        );
    }
    for t in &audio {
        if t.timescale == 0
            || t.entries.len() != 1
            || t.samples.is_empty()
            || t.samples[0].dts != 0
            || t.samples.iter().any(|s| s.description_index != 1)
            || t.samples
                .windows(2)
                .any(|s| s[1].dts != s[0].dts + i64::from(s[0].duration))
        {
            return Err("unsupported audio sample table".into());
        }
    }
    let mut rate = [vt.timescale, delta];
    let mut timescale = vt.timescale;
    for stage in &job.stages {
        rate = registry
            .get(&stage.processor)?
            .output_rate(&stage.settings, rate)?;
        if rate.contains(&0) || u64::from(rate[0]) > 1000 * u64::from(rate[1]) {
            return Err("invalid processor output rate".into());
        }
        timescale = lcm(timescale, rate[0])?;
    }
    let end = i128::try_from(pts.len()).map_err(err)? * i128::from(delta) * i128::from(timescale)
        / i128::from(vt.timescale);
    let expected_end = i64::try_from(end).map_err(err)?;
    #[cfg(target_os = "macos")]
    {
        static REGISTER: std::sync::Once = std::sync::Once::new();
        REGISTER.call_once(|| {
            filmcraft_platform::register();
        });
    }
    let name = job
        .input
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid filename")?;
    let source = filmcraft_media::reader::open_reader(
        name,
        Arc::new(Reader::open(&job.input)?),
        &filmcraft_codecs::reader_openers(),
        &[],
    )
    .map_err(err)?;
    let info = source
        .info()
        .video
        .as_ref()
        .ok_or("missing decoded video")?;
    if info.color.primaries != filmcraft_color::Primaries::Bt709
        || info.color.transfer != Transfer::Bt709
        || info.has_alpha
        || info.hdr.is_some()
    {
        return Err("native processor input requires SDR Rec.709 without alpha".into());
    }
    let parent = job
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(err)?
        .as_nanos();
    let dir = parent.join(format!(".processing-{}-{stamp}", std::process::id()));
    fs::create_dir(&dir).map_err(err)?;
    let work = Work(dir);
    let path = work.0.join("output.mov");
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(err)?;
    let mut chain = Chain::new(
        registry,
        &job.stages,
        w,
        h,
        job.memory_bytes,
        cancel.clone(),
    )?;
    chain.set_progress_reporter(progress.clone());
    let [ow, oh] = chain.dimensions();
    if !ow.is_multiple_of(2) || !oh.is_multiple_of(2) {
        return Err("ProRes output requires even dimensions".into());
    }
    let mut options = WriterOptions::new(Brand::Mov);
    options.movie_timescale = movie.timescale;
    options.metadata = movie.metadata.entries.clone();
    let mut mux = Mp4Writer::new(file, options).map_err(err)?;
    let mut entry = SampleEntry::prores(FourCc(*b"apch"), ow as u16, oh as u16);
    if let Some(v) = &mut entry.video {
        v.color = Some(mp4::ColorInfo::Nclc {
            primaries: 1,
            transfer: 1,
            matrix: 1,
        });
    }
    let video = mux
        .add_track(TrackConfig::new(entry, timescale))
        .map_err(err)?;
    let mut audio_tracks = Vec::new();
    for t in &audio {
        let mut cfg = TrackConfig::new(t.entries[0].clone(), t.timescale);
        cfg.language = t.language.clone();
        cfg.handler_name = Some(t.handler_name.clone());
        cfg.edits = t.edits.clone();
        audio_tracks.push(mux.add_track(cfg).map_err(err)?);
    }
    let mut encoder =
        filmcraft_prores::Encoder::new(filmcraft_prores::Profile::Hq, ow as u32, oh as u32);
    let mut frames = 0usize;
    let mut next_pts = 0i64;
    {
        let mut accept = |outputs: Vec<Frame>| -> Result<()> {
            for f in outputs {
                cancel.check()?;
                f.validate()?;
                if [f.width, f.height] != [ow, oh] {
                    return Err("output dimensions changed".into());
                }
                let at = exact_ticks(f.pts, f.time_base, timescale)?;
                let duration = exact_ticks(f.duration, f.time_base, timescale)?;
                if at != next_pts || duration <= 0 {
                    return Err(
                        "processor output timestamps are missing, reordered or discontinuous"
                            .into(),
                    );
                }
                next_pts = next_pts.checked_add(duration).ok_or("timestamp overflow")?;
                if next_pts > expected_end {
                    return Err("processor output extends beyond source duration".into());
                }
                let bytes = encoder
                    .encode(&prores_frame(&f.pixels, ow, oh)?)
                    .map_err(err)?;
                mux.write_sample(
                    video,
                    WriteSample {
                        data: &bytes,
                        duration: u32::try_from(duration).map_err(err)?,
                        composition_offset: 0,
                        is_sync: true,
                    },
                )
                .map_err(err)?;
                frames += 1;
                progress(abner_processor_api::Progress {
                    phase: "Encoded frames".into(),
                    completed: frames as u64,
                    total: None,
                });
            }
            Ok(())
        };
        for p in pts {
            cancel.check()?;
            let request = Tick::from_units(p, i64::from(vt.timescale));
            let request = Tick(
                request
                    .0
                    .checked_add(Tick::from_units(i64::from(delta), i64::from(vt.timescale)).0 / 4)
                    .ok_or("timestamp overflow")?,
            );
            let decoded = source
                .video_frame(FrameRequest::full(request))
                .map_err(err)?;
            if [decoded.width as usize, decoded.height as usize] != [w, h] {
                return Err("decoded dimensions changed".into());
            }
            let frame = Frame {
                width: w,
                height: h,
                pixels: rgb16(&decoded)?,
                pts: p,
                duration: i64::from(delta),
                time_base: [1, vt.timescale],
            };
            accept(chain.process(frame)?)?;
        }
        chain.finish(&mut accept)?;
    }
    if frames == 0 || next_pts != expected_end {
        return Err("processor output does not cover the source duration".into());
    }
    for (out, t) in audio_tracks.iter().zip(&audio) {
        let input = movie
            .tracks
            .iter()
            .position(|x| std::ptr::eq(x, *t))
            .ok_or("missing audio")?;
        for (k, s) in t.samples.iter().enumerate() {
            cancel.check()?;
            let bytes = movie.read_sample(&reader, input, k).map_err(err)?;
            mux.write_sample(
                *out,
                WriteSample {
                    data: &bytes,
                    duration: s.duration,
                    composition_offset: i32::try_from(s.pts - s.dts).map_err(err)?,
                    is_sync: s.is_sync,
                },
            )
            .map_err(err)?;
        }
    }
    let mut file = mux.finish().map_err(err)?;
    preserve_audio_flags(&mut file, &audio)?;
    file.sync_all().map_err(err)?;
    drop(file);
    let check = mp4::open(Reader::open(&path)?).map_err(err)?;
    let out_reader = Reader::open(&path)?;
    let ov = check.tracks.first().ok_or("missing output video")?;
    if check.tracks.len() != audio.len() + 1
        || ov.samples.len() != frames
        || ov.timescale != timescale
        || ov.width != ow as u32
        || ov.height != oh as u32
    {
        return Err("output validation failed".into());
    }
    for (i, t) in audio.iter().enumerate() {
        let before = movie
            .tracks
            .iter()
            .position(|x| std::ptr::eq(x, *t))
            .ok_or("missing audio")?;
        let after = &check.tracks[i + 1];
        if after.entries != t.entries
            || after.timescale != t.timescale
            || after.edits != t.edits
            || after.duration != t.duration
            || after.tkhd_duration != t.tkhd_duration
            || after.language != t.language
            || after.enabled != t.enabled
            || after.alternate_group != t.alternate_group
            || after.volume != t.volume
            || packet_hash(&movie, &reader, before, cancel)?
                != packet_hash(&check, &out_reader, i + 1, cancel)?
        {
            return Err("audio preservation verification failed".into());
        }
    }
    cancel.check()?;
    fs::hard_link(&path, &job.output).map_err(err)?;
    Ok(Report {
        frames,
        width: ow,
        height: oh,
        audio_tracks: audio.len(),
        output: job.output.clone(),
    })
}
fn lcm(a: u32, b: u32) -> Result<u32> {
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    (a / x)
        .checked_mul(b)
        .ok_or_else(|| "incompatible time bases exceed output timescale".into())
}
fn exact_ticks(value: i64, time_base: [u32; 2], scale: u32) -> Result<i64> {
    if time_base.contains(&0) {
        return Err("invalid time base".into());
    }
    let n = i128::from(value) * i128::from(time_base[0]) * i128::from(scale);
    let d = i128::from(time_base[1]);
    if n % d != 0 {
        return Err("output timestamp cannot be represented exactly".into());
    }
    i64::try_from(n / d).map_err(err)
}

#[cfg(test)]
mod tests;
