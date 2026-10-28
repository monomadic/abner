//! Native packet metadata for the containers outside the indexed MP4/Matroska path.
//! Timestamp origins follow Filmcraft playback; unknown key flags stay unknown/false.
use crate::media_metadata::{Packet, Timeline};
use anyhow::{Context, Result, ensure};
use filmcraft_media::{ByteReader, MediaInfo, MediaSource};
use std::{path::Path, sync::Arc};

struct Bytes<'a>(&'a crate::fc::FileReader);
macro_rules! source {
    ($module:ident) => {
        impl $module::ByteSource for Bytes<'_> {
            fn len(&self) -> u64 {
                ByteReader::len(self.0)
            }
            fn read_at(&self, at: u64, b: &mut [u8]) -> std::io::Result<()> {
                self.0.read_at(at, b)
            }
        }
    };
}
source!(filmcraft_avi);
source!(filmcraft_mxf);
source!(filmcraft_mpegts);

fn facts(info: &MediaInfo) -> Result<serde_json::Value> {
    let v = info.video.as_ref().context("no video stream")?;
    ensure!(
        v.width > 0 && v.height > 0 && v.frame_rate.num > 0 && v.frame_rate.den > 0,
        "invalid video metadata"
    );
    let mut streams = vec![
        serde_json::json!({"codec_type":"video","codec_name":crate::media_metadata::canonical_codec(&v.codec),"width":v.width,"height":v.height,"pix_fmt":crate::media_metadata::pix_label(&v.pixel_format),"avg_frame_rate":format!("{}/{}",v.frame_rate.num,v.frame_rate.den),"bit_rate":v.bitrate,"color_space":"unknown"}),
    ];
    for a in &info.audio_streams {
        streams.push(serde_json::json!({"codec_type":"audio","codec_name":audio_codec(&a.codec),"sample_rate":a.sample_rate,"channels":a.channels}));
    }
    Ok(
        serde_json::json!({"streams":streams,"format":{"bit_rate":info.file_size.filter(|_|info.duration.seconds()>0.).map(|n|(n as f64*8./info.duration.seconds()) as u64)}}),
    )
}
fn audio_codec(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for (needle, label) in [
        ("aac", "aac"),
        ("pcm", "pcm"),
        ("mp3", "mp3"),
        ("mpeg audio", "mp2"),
        ("e-ac-3", "eac3"),
        ("ac-3", "ac3"),
        ("opus", "opus"),
        ("flac", "flac"),
    ] {
        if lower.contains(needle) {
            return label.into();
        }
    }
    lower
}
fn result(info: &MediaInfo, packets: Vec<Packet>) -> Result<Timeline> {
    ensure!(!packets.is_empty(), "no presented video packets");
    Ok(Timeline {
        packets,
        chapters: Vec::new(),
        facts: facts(info)?,
    })
}
fn header(src: &dyn ByteReader, at: u64, size: u64, limit: usize) -> Result<Vec<u8>> {
    let mut b = vec![0; (size.min(limit as u64)) as usize];
    src.read_at(at, &mut b)?;
    Ok(b)
}
fn random_access(data: &[u8], hevc: bool) -> bool {
    filmcraft_bitstream::annexb_nals(data).iter().any(|n| {
        n.first().is_some_and(|h| {
            if hevc {
                (16..=21).contains(&((h >> 1) & 63))
            } else {
                h & 31 == 5
            }
        })
    })
}
fn video_profile(data: &[u8], facts: &mut serde_json::Value) {
    let nals = filmcraft_bitstream::annexb_nals(data);
    let sps: Vec<_> = nals
        .iter()
        .filter(|n| n.first().is_some_and(|h| h & 31 == 7))
        .map(|n| n.to_vec())
        .collect();
    let pps: Vec<_> = nals
        .iter()
        .filter(|n| n.first().is_some_and(|h| h & 31 == 8))
        .map(|n| n.to_vec())
        .collect();
    if !sps.is_empty() {
        crate::media_metadata::profile(
            &filmcraft_isobmff::CodecConfig::Avc(filmcraft_isobmff::AvcConfig::new(sps, pps, 4)),
            &mut facts["streams"][0],
        );
    }
}

pub(crate) fn timeline(path: &Path) -> Result<Option<Timeline>, String> {
    native(path).map(Some).map_err(|e| e.to_string())
}
fn native(path: &Path) -> Result<Timeline> {
    crate::fc::register_hardware();
    let src = crate::fc::reader(path).map_err(anyhow::Error::msg)?;
    let head = header(&src, 0, ByteReader::len(&src), 65536)?;
    let name = path.to_string_lossy();
    if filmcraft_codecs::mpeg::sniff(&head) {
        let shared: filmcraft_media::SharedReader =
            Arc::new(crate::fc::reader(path).map_err(anyhow::Error::msg)?);
        let source = filmcraft_codecs::mpeg::MpegSource::open_reader(&name, shared)?;
        return mpeg(&src, &source);
    }
    let info = {
        let source = crate::fc::open(path).map_err(anyhow::Error::msg)?;
        source.info().clone()
    };
    if filmcraft_avi::sniff(&head) {
        return avi(&src, &info);
    }
    if filmcraft_mxf::sniff(&head) {
        return mxf(&src, &info);
    }
    if filmcraft_codecs::y4m::sniff(&head) {
        return y4m(&src, &info);
    }
    if filmcraft_codecs::apv::sniff(&head) {
        return apv(&src, &info);
    }
    if info.kind == filmcraft_media::MediaKind::Still {
        return result(
            &info,
            vec![Packet {
                pts: 0.,
                duration: 1. / info.frame_rate().as_f64(),
                size: ByteReader::len(&src),
                key: true,
            }],
        );
    }
    anyhow::bail!("Filmcraft has no packet-metadata adapter for this container")
}
fn avi(src: &crate::fc::FileReader, info: &MediaInfo) -> Result<Timeline> {
    use filmcraft_avi::StreamKind;
    let file = filmcraft_avi::open(&Bytes(src))?;
    let t = file
        .streams
        .iter()
        .find(|t| t.kind == StreamKind::Video)
        .context("AVI has no video track")?;
    ensure!(t.rate > 0 && t.scale > 0, "invalid AVI timebase");
    let period = t.scale as f64 / t.rate as f64;
    let chunks: Vec<_> = t.chunks.iter().filter(|c| c.size > 0).collect();
    let slots: Vec<i64> = t
        .chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| c.size > 0)
        .map(|(i, _)| t.start as i64 + i as i64)
        .collect();
    let mut pts = slots.clone();
    let mut codec = t.video.as_ref().map(|v| v.compression).unwrap_or(t.handler);
    codec.make_ascii_uppercase();
    let avc = matches!(&codec, b"H264" | b"X264" | b"AVC1" | b"DAVC" | b"VSSH");
    let hevc = matches!(&codec, b"HEVC" | b"H265" | b"HEV1" | b"HVC1" | b"X265");
    let heads = |n: usize| -> Result<Vec<Vec<u8>>> {
        chunks
            .iter()
            .take(n)
            .map(|c| header(src, c.offset, c.size.into(), 8192))
            .collect()
    };
    if avc
        && filmcraft_codecs::mxf::avc_presentation_order(&heads(300)?)
            .is_some_and(|p| p.windows(2).any(|p| p[1] < p[0]))
    {
        let ranks = filmcraft_codecs::mxf::avc_presentation_order(&heads(chunks.len())?)
            .context("AVI H.264 presentation order could not be resolved")?;
        ensure!(
            ranks.len() == pts.len(),
            "incomplete AVI presentation order"
        );
        for (p, rank) in pts.iter_mut().zip(ranks) {
            *p = *slots
                .get(usize::try_from(rank)?)
                .context("invalid AVI display rank")?;
        }
    }
    let intra = matches!(
        &codec,
        b"MJPG" | b"JPEG" | b"MJPA" | b"AVRN" | b"DMB1" | b"YUY2" | b"UYVY" | b"YV12" | b"I420"
    ) || t.video.as_ref().is_some_and(|v| v.is_rgb());
    let packets = chunks
        .iter()
        .zip(pts)
        .map(|(c, p)| {
            Ok(Packet {
                pts: p as f64 * period,
                duration: period,
                size: c.size.into(),
                key: if intra {
                    true
                } else if t.keyframes_known {
                    c.key
                } else if avc || hevc {
                    random_access(&header(src, c.offset, c.size.into(), 8192)?, hevc)
                } else {
                    false
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut out = result(info, packets)?;
    if avc && let Some(c) = chunks.first() {
        video_profile(&header(src, c.offset, c.size.into(), 8192)?, &mut out.facts);
    }
    Ok(out)
}
fn mxf(src: &crate::fc::FileReader, info: &MediaInfo) -> Result<Timeline> {
    let file = filmcraft_mxf::open(&Bytes(src))?;
    let t = file
        .tracks
        .iter()
        .find(|t| t.kind == filmcraft_mxf::TrackKind::Picture)
        .context("MXF has no picture track")?;
    ensure!(t.edit_rate.is_valid(), "invalid MXF edit rate");
    let period = t.edit_rate.den as f64 / t.edit_rate.num as f64;
    let mut pts: Vec<_> = t.samples.iter().map(|s| s.pts).collect();
    if t.needs_reorder && matches!(t.codec, filmcraft_mxf::Codec::Avc { .. }) {
        let heads = t
            .samples
            .iter()
            .map(|s| header(src, s.offset, s.size.into(), 16384))
            .collect::<Result<Vec<_>>>()?;
        pts = filmcraft_codecs::mxf::avc_presentation_order(&heads)
            .context("MXF AVC presentation order could not be resolved")?;
    }
    let first = t.first_edit_unit().min(t.samples.len() as i64);
    let count = t
        .duration
        .unwrap_or(t.samples.len() as i64 - first)
        .min(t.samples.len() as i64 - first)
        .max(0);
    let mut packets = Vec::new();
    for (s, p) in t
        .samples
        .iter()
        .zip(pts)
        .filter(|(_, p)| *p >= first && *p < first + count)
    {
        let key = if t.codec == filmcraft_mxf::Codec::Mpeg2 {
            let picture =
                filmcraft_mpeg2v::scan_access_unit(&header(src, s.offset, s.size.into(), 16384)?);
            if picture.pictures.is_empty() {
                s.key
            } else {
                picture.is_intra()
            }
        } else {
            s.key
        };
        packets.push(Packet {
            pts: (p - first) as f64 * period,
            duration: period,
            size: s.size.into(),
            key,
        });
    }
    let mut out = result(info, packets)?;
    if matches!(t.codec, filmcraft_mxf::Codec::Avc { .. })
        && let Some(s) = t.samples.first()
    {
        video_profile(
            &header(src, s.offset, s.size.into(), 16384)?,
            &mut out.facts,
        );
    }
    Ok(out)
}

// The same ordering rules as Filmcraft's MPEG playback source: PTS when complete,
// otherwise MPEG GOP/temporal-reference order; leading open-GOP pictures are not presented.
fn order(units: &[filmcraft_mpegts::Unit], mpeg: bool) -> Vec<usize> {
    let keys: Vec<i64> = if units.iter().all(|u| u.pts.is_some()) {
        units.iter().map(|u| u.pts.unwrap_or(0)).collect()
    } else if mpeg {
        let mut base = 0;
        units
            .iter()
            .enumerate()
            .map(|(i, u)| {
                if u.picture.is_some_and(|p| p.gop) || i == 0 {
                    base = i as i64
                }
                (base + u.picture.map_or(0, |p| p.temporal_reference as i64)) * 4
            })
            .collect()
    } else {
        let mut last = i64::MIN / 4;
        units
            .iter()
            .map(|u| {
                last = u.pts.unwrap_or(last + 1);
                last
            })
            .collect()
    };
    let mut order: Vec<_> = (0..keys.len()).filter(|&i| keys[i] >= keys[0]).collect();
    order.sort_by_key(|&i| (keys[i], i));
    order
}
fn mpeg(
    src: &crate::fc::FileReader,
    source: &filmcraft_codecs::mpeg::MpegSource,
) -> Result<Timeline> {
    let info = source.info();
    let period = 1. / info.frame_rate().as_f64();
    let Some(file) = source.file() else {
        return elementary(src, info);
    };
    let (video, _) = source.stream_indices();
    let ti = video.context("MPEG has no supported video stream")?;
    let stream = &file.streams[ti];
    let start = stream
        .units
        .iter()
        .position(|u| u.key)
        .context("MPEG has no random-access picture")?;
    let units = &stream.units[start..];
    let is_mpeg = matches!(
        stream.codec,
        filmcraft_mpegts::Codec::Mpeg1Video | filmcraft_mpegts::Codec::Mpeg2Video
    );
    let display = order(units, is_mpeg);
    let times = source.video_pts();
    ensure!(
        times.len() == display.len(),
        "incomplete MPEG presentation table"
    );
    let origin = times.iter().flatten().next().copied().unwrap_or(0);
    let mut stamps = vec![None; units.len()];
    for (rank, (&stored, time)) in display.iter().zip(times).enumerate() {
        stamps[stored] = Some(time.map_or(rank as f64 * period, |t| (t - origin) as f64 / 90000.));
    }
    let mut packets = Vec::with_capacity(display.len());
    for (i, u) in units.iter().enumerate() {
        if let Some(pts) = stamps[i] {
            packets.push(Packet {
                pts,
                duration: period,
                size: u.size.into(),
                key: u.key,
            });
        }
    }
    // Real VFR timestamps define each displayed interval; the final packet uses the declared rate.
    let mut sorted: Vec<_> = (0..packets.len()).collect();
    sorted.sort_by(|&a, &b| packets[a].pts.total_cmp(&packets[b].pts));
    for p in sorted.windows(2) {
        let duration = packets[p[1]].pts - packets[p[0]].pts;
        if duration > 0. {
            packets[p[0]].duration = duration;
        }
    }
    let mut out = result(info, packets)?;
    if stream.codec == filmcraft_mpegts::Codec::H264 {
        video_profile(&file.read_unit(&Bytes(src), ti, start)?, &mut out.facts);
    }
    for stream in &file.streams {
        if stream.kind() == filmcraft_mpegts::Kind::Subtitle {
            out.facts["streams"]
                .as_array_mut()
                .context("invalid stream facts")?
                .push(
                    serde_json::json!({"codec_type":"subtitle","codec_name":stream.codec.name()}),
                );
        }
    }
    Ok(out)
}
// Access-unit boundaries and picture headers are Filmcraft's parser, with one unfinished
// unit retained between reads. No second whole-file copy is needed for this index.
fn elementary(src: &crate::fc::FileReader, info: &MediaInfo) -> Result<Timeline> {
    let len = ByteReader::len(src);
    let mut at = 0;
    let mut pending = Vec::new();
    let mut units: Vec<(u64, u16, bool, bool)> = Vec::new();
    loop {
        let n = (len - at).min(65536) as usize;
        let old = pending.len();
        pending.resize(old + n, 0);
        src.read_at(at, &mut pending[old..])?;
        at += n as u64;
        ensure!(
            pending.len() <= 128 * 1024 * 1024,
            "MPEG access unit exceeds 128 MiB"
        );
        let ranges = filmcraft_mpeg2v::access_units(&pending);
        let take = if at == len {
            ranges.len()
        } else {
            ranges.len().saturating_sub(1)
        };
        for range in ranges.iter().take(take) {
            let p = filmcraft_mpeg2v::scan_access_unit(&pending[range.clone()]);
            let picture = p
                .pictures
                .first()
                .context("MPEG access unit has no picture")?;
            units.push((
                (range.end - range.start) as u64,
                picture.0.temporal_reference,
                p.gop.is_some(),
                p.is_intra(),
            ));
        }
        if at == len {
            break;
        }
        if take > 0 {
            let keep = ranges[take].start;
            pending.drain(..keep);
        }
    }
    let first = units
        .iter()
        .position(|u| u.3)
        .context("elementary MPEG has no I picture")?;
    let units = &units[first..];
    let mut base = 0i64;
    let keys: Vec<_> = units
        .iter()
        .enumerate()
        .map(|(i, u)| {
            if i == 0 || u.2 {
                base = i as i64
            }
            (base + u.1 as i64) * 4
        })
        .collect();
    let mut order: Vec<_> = (0..units.len()).filter(|&i| keys[i] >= keys[0]).collect();
    order.sort_by_key(|&i| (keys[i], i));
    let mut stamps = vec![None; units.len()];
    let period = 1. / info.frame_rate().as_f64();
    for (rank, &i) in order.iter().enumerate() {
        stamps[i] = Some(rank as f64 * period);
    }
    result(
        info,
        units
            .iter()
            .zip(stamps)
            .filter_map(|(u, pts)| {
                pts.map(|pts| Packet {
                    pts,
                    duration: period,
                    size: u.0,
                    key: u.3,
                })
            })
            .collect(),
    )
}
fn line(src: &dyn ByteReader, at: u64) -> Result<(Vec<u8>, u64)> {
    let n = (src.len().saturating_sub(at)).min(4096) as usize;
    let b = header(src, at, n as u64, 4096)?;
    let end = b
        .iter()
        .position(|b| *b == b'\n')
        .context("unterminated or oversized frame header")?;
    Ok((b[..end].to_vec(), at + end as u64 + 1))
}
fn y4m(src: &crate::fc::FileReader, info: &MediaInfo) -> Result<Timeline> {
    let (h, mut at) = line(src, 0)?;
    let h = std::str::from_utf8(&h)?;
    let v = info.video.as_ref().context("no Y4M video")?;
    let color = h
        .split_whitespace()
        .find_map(|t| t.strip_prefix('C'))
        .unwrap_or("420jpeg");
    let bits = color
        .split_once('p')
        .and_then(|(_, n)| n.parse::<u32>().ok())
        .or_else(|| {
            color
                .strip_prefix("mono")
                .and_then(|n| n.parse::<u32>().ok())
        })
        .unwrap_or(8);
    let w = v.width as u64;
    let h = v.height as u64;
    let luma = w * h;
    let chroma = if color.starts_with("420") {
        w.div_ceil(2) * h.div_ceil(2)
    } else if color.starts_with("422") {
        w.div_ceil(2) * h
    } else if color.starts_with("444") {
        luma
    } else if color.starts_with("mono") {
        0
    } else {
        anyhow::bail!("unsupported Y4M chroma layout")
    };
    let bytes = (luma + 2 * chroma + if color == "444alpha" { luma } else { 0 })
        * if bits > 8 { 2 } else { 1 };
    let period = 1. / v.frame_rate.as_f64();
    let mut packets = Vec::new();
    while at < src.len() {
        let (h, data) = line(src, at)?;
        ensure!(
            h == b"FRAME" || h.starts_with(b"FRAME "),
            "invalid Y4M frame header"
        );
        let end = data
            .checked_add(bytes)
            .filter(|&end| end <= src.len())
            .context("truncated Y4M frame")?;
        packets.push(Packet {
            pts: packets.len() as f64 * period,
            duration: period,
            size: bytes,
            key: true,
        });
        at = end;
    }
    result(info, packets)
}
fn apv(src: &crate::fc::FileReader, info: &MediaInfo) -> Result<Timeline> {
    let mut at = 0;
    let mut packets = Vec::new();
    let period = 1. / info.frame_rate().as_f64();
    while at < src.len() {
        let h = header(src, at, src.len() - at, 8)?;
        let (size, prefix) = if h.len() >= 8 && &h[4..8] == b"aPv1" {
            (u32::from_be_bytes(h[..4].try_into()?) as u64, 4)
        } else if at == 0 && h.starts_with(b"aPv1") {
            (src.len(), 0)
        } else {
            anyhow::bail!("invalid APV access unit")
        };
        ensure!(
            size >= 8
                && at
                    .checked_add(size + prefix)
                    .is_some_and(|end| end <= src.len()),
            "truncated APV access unit"
        );
        packets.push(Packet {
            pts: packets.len() as f64 * period,
            duration: period,
            size,
            key: true,
        });
        at += size + prefix;
    }
    result(info, packets)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "abner-packet-meta-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture(path: &Path, rate: &str, args: &[&str]) {
        let out = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc2=size=128x96:rate={rate}:duration=1.2"),
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1.2",
                "-threads",
                "1",
            ])
            .args(args)
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "fixture {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[test]
    fn other_container_packets_match_reference_sizes_flags_and_timestamps() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = Dir::new();
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "mjpeg.avi",
                "30000/1001",
                &["-c:v", "mjpeg", "-q:v", "3", "-c:a", "pcm_s16le"],
            ),
            (
                "h264.avi",
                "30000/1001",
                &[
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "12",
                    "-bf",
                    "2",
                    "-c:a",
                    "pcm_s16le",
                ],
            ),
            (
                "mpeg.ts",
                "30000/1001",
                &["-c:v", "mpeg2video", "-g", "12", "-bf", "2", "-c:a", "mp2"],
            ),
            (
                "h264.ts",
                "30000/1001",
                &[
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "12",
                    "-bf",
                    "2",
                    "-c:a",
                    "aac",
                ],
            ),
            (
                "hevc.m2ts",
                "25",
                &[
                    "-c:v",
                    "libx265",
                    "-x265-params",
                    "pools=none:frame-threads=1:log-level=error",
                    "-g",
                    "12",
                    "-bf",
                    "2",
                    "-c:a",
                    "ac3",
                ],
            ),
            (
                "program.mpg",
                "25",
                &["-c:v", "mpeg2video", "-g", "12", "-bf", "2", "-c:a", "mp2"],
            ),
            (
                "mpeg.mxf",
                "25",
                &[
                    "-c:v",
                    "mpeg2video",
                    "-pix_fmt",
                    "yuv422p",
                    "-g",
                    "12",
                    "-bf",
                    "2",
                    "-c:a",
                    "pcm_s16le",
                    "-ar",
                    "48000",
                    "-ac",
                    "2",
                ],
            ),
            (
                "avc.mxf",
                "25",
                &[
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "12",
                    "-bf",
                    "2",
                    "-c:a",
                    "pcm_s16le",
                    "-ar",
                    "48000",
                    "-ac",
                    "2",
                ],
            ),
            (
                "elementary.m2v",
                "30000/1001",
                &["-c:v", "mpeg2video", "-g", "12", "-bf", "2", "-an"],
            ),
            (
                "planes.y4m",
                "30000/1001",
                &["-pix_fmt", "yuv420p10le", "-strict", "-1", "-an"],
            ),
        ];
        for &(name, rate, args) in cases {
            let path = dir.0.join(name);
            fixture(&path, rate, args);
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "media_packets::tests::no_tools_packet_fixture_child",
                ])
                .env("PATH", "/nonexistent")
                .env("ABNER_PACKET_TEST_PATH", &path)
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "{name}: native metadata without helpers: {} {}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            let raw = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_packets",
                    "-show_streams",
                    "-of",
                    "json",
                ])
                .arg(&path)
                .output()
                .unwrap();
            assert!(raw.status.success());
            let reference: serde_json::Value = serde_json::from_slice(&raw.stdout).unwrap();
            let packets = reference["packets"].as_array().unwrap();
            let native = crate::media_metadata::timeline(&path)
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .unwrap();
            assert_eq!(native.packets.len(), packets.len(), "{name}: packet count");
            let missing_composition_offsets = if name == "avc.mxf" {
                let reader = crate::fc::reader(&path).unwrap();
                let file = filmcraft_mxf::open(&Bytes(&reader)).unwrap();
                let track = file
                    .tracks
                    .iter()
                    .find(|t| t.kind == filmcraft_mxf::TrackKind::Picture)
                    .unwrap();
                assert!(
                    track.needs_reorder && !track.temporal_offsets,
                    "fixture must exercise bitstream POC recovery"
                );
                // The fixture's two B pictures decode after the P picture but display before it.
                for (packet, rank) in native.packets.iter().take(4).zip([0., 3., 1., 2.]) {
                    assert!((packet.pts - rank / 25.).abs() < 1e-12);
                }
                true
            } else {
                false
            };
            let mut origin = None;
            for (i, (a, b)) in native.packets.iter().zip(packets).enumerate() {
                let size = b["size"].as_str().unwrap().parse::<u64>().unwrap();
                assert_eq!(a.size, size, "{name} packet {i}: payload size");
                assert_eq!(
                    a.key,
                    b["flags"].as_str().unwrap().contains('K'),
                    "{name} packet {i}: key flag"
                );
                if !missing_composition_offsets
                    && let Some(t) = b["pts_time"].as_str().and_then(|v| v.parse::<f64>().ok())
                {
                    let offset = *origin.get_or_insert(t - a.pts);
                    assert!(
                        (a.pts - (t - offset)).abs() < 0.000003,
                        "{name} packet {i}: native {} vs reference {t} with offset {offset}",
                        a.pts
                    );
                }
            }
            if missing_composition_offsets {
                let reference = Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(&path)
                    .args([
                        "-vf",
                        "select=eq(n\\,3)",
                        "-frames:v",
                        "1",
                        "-threads",
                        "1",
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                        "rgba",
                        "-",
                    ])
                    .output()
                    .unwrap();
                assert!(reference.status.success());
                let frame = crate::fc::open(&path)
                    .unwrap()
                    .video_frame(filmcraft_media::FrameRequest::full(
                        filmcraft_time::Tick::from_seconds_f64(3. / 25.),
                    ))
                    .unwrap();
                let actual = frame.to_rgba8();
                assert_eq!(reference.stdout.len(), actual.len());
                let mse = actual
                    .iter()
                    .zip(reference.stdout)
                    .map(|(a, b)| (*a as f64 - b as f64).powi(2))
                    .sum::<f64>()
                    / actual.len() as f64;
                assert!(mse < 100., "MXF decoded display-order MSE {mse}");
            }
            assert_eq!(native.facts["streams"][0]["width"], 128);
            assert_eq!(native.facts["streams"][0]["height"], 96);
            if !name.ends_with("m2v") && !name.ends_with("y4m") {
                assert!(
                    native.facts["streams"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s["codec_type"] == "audio"),
                    "{name}: audio stream facts"
                );
            }
        }
    }
    #[test]
    fn y4m_packet_tables_and_rejection_work_without_external_tools() {
        use std::io::Write;
        let dir = Dir::new();
        for (color, bytes) in [
            ("420jpeg", 16 * 16 + 8 * 8 * 2),
            ("420p10", (16 * 16 + 8 * 8 * 2) * 2),
            ("mono10", 16 * 16 * 2),
            ("444alpha", 16 * 16 * 4),
        ] {
            let path = dir.0.join(format!("{color}.y4m"));
            let mut file = std::fs::File::create(&path).unwrap();
            writeln!(file, "YUV4MPEG2 W16 H16 F30000:1001 Ip C{color}").unwrap();
            for _ in 0..7 {
                file.write_all(b"FRAME Xtag=synthetic\n").unwrap();
                file.write_all(&vec![0; bytes]).unwrap();
            }
            drop(file);
            let meta = crate::media_metadata::timeline(&path).unwrap().unwrap();
            assert_eq!(meta.packets.len(), 7);
            for (i, p) in meta.packets.iter().enumerate() {
                assert_eq!(p.size, bytes as u64);
                assert!(p.key);
                assert!((p.pts - i as f64 * 1001. / 30000.).abs() < 1e-12);
            }
            assert!(meta.chapters.is_empty());
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(std::fs::metadata(&path).unwrap().len() - 1)
                .unwrap();
            assert!(
                crate::media_metadata::timeline(&path).is_err(),
                "truncated {color}"
            );
        }
        let invalid = dir.0.join("invalid.bin");
        std::fs::write(&invalid, b"this is not a media source").unwrap();
        assert!(crate::media_metadata::timeline(&invalid).is_err());
    }
    #[test]
    fn native_mxf_packet_metadata_works_without_helpers() {
        use filmcraft_mxf::{
            FrameInfo, MxfWriter, PackageIds, Pattern, PictureCoding, PictureDesc, Rational,
            WriterConfig,
        };
        let dir = Dir::new();
        let path = dir.0.join("synthetic.mxf");
        let mut config = WriterConfig::new(
            Pattern::Op1a,
            Rational::new(30000, 1001),
            PackageIds::from_seed("abner-native-packet-test", "Synthetic"),
        );
        config.picture = Some(PictureDesc::new(
            PictureCoding::ProRes { profile: 0 },
            32,
            32,
        ));
        let mut writer = MxfWriter::new(std::fs::File::create(&path).unwrap(), config).unwrap();
        let mut encoder = filmcraft_prores::Encoder::new(filmcraft_prores::Profile::Proxy, 32, 32);
        let frame =
            filmcraft_prores::Frame::new(32, 32, filmcraft_prores::ChromaFormat::Yuv422, 10, false);
        let mut sizes = Vec::new();
        for _ in 0..7 {
            let data = encoder.encode(&frame).unwrap();
            sizes.push(data.len() as u64);
            writer.push_picture(&data, FrameInfo::intra()).unwrap();
        }
        writer.finish().unwrap();
        let timeline = crate::media_metadata::timeline(&path).unwrap().unwrap();
        assert_eq!(timeline.packets.len(), 7);
        for (i, (packet, size)) in timeline.packets.iter().zip(sizes).enumerate() {
            assert_eq!(packet.size, size);
            assert!(packet.key);
            assert!((packet.pts - i as f64 * 1001. / 30000.).abs() < 1e-12);
        }
        assert_eq!(timeline.facts["streams"][0]["codec_name"], "prores");
    }
    #[test]
    fn no_tools_packet_fixture_child() {
        let Some(path) = std::env::var_os("ABNER_PACKET_TEST_PATH") else {
            return;
        };
        let timeline = crate::media_metadata::timeline(Path::new(&path))
            .unwrap()
            .unwrap();
        assert!(!timeline.packets.is_empty());
        assert!(
            timeline
                .packets
                .iter()
                .all(|p| p.pts.is_finite() && p.duration.is_finite() && p.duration > 0.)
        );
    }
}
