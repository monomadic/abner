//! Native indexed media I/O. Payloads stay on disk until needed; no media helper processes.
use crate::cut::{Cue, FrameInfo};
use anyhow::{Context, Result, ensure};
use filmcraft_isobmff::{self as mp4, CodecConfig, SampleEntry};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Video,
    Audio,
    Subtitle,
    Other,
}
#[derive(Clone)]
pub(crate) struct Sample {
    pub pts: f64,
    pub dts: f64,
    pub duration: f64,
    pub key: bool,
    pub size: u32,
}
pub(crate) struct Track {
    pub kind: Kind,
    pub entry: Option<SampleEntry>,
    pub samples: Vec<Sample>,
    pub language: String,
}
enum Container {
    Mp4(Box<mp4::Mp4File>),
    Mkv(Box<filmcraft_matroska::MkvFile>),
}
pub(crate) struct Input {
    source: crate::fc::FileReader,
    container: Container,
    pub tracks: Vec<Track>,
}
impl Input {
    pub fn open(path: &Path) -> Result<Self> {
        let source = crate::fc::reader(path).map_err(anyhow::Error::msg)?;
        let mut signature = [0; 8];
        filmcraft_media::ByteReader::read_at(&source, 0, &mut signature)?;

        let (container, tracks) = if signature[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
            let f = filmcraft_matroska::open(&source)?;
            let mut tracks = Vec::new();
            for (ti, t) in f.tracks.iter().enumerate() {
                use filmcraft_matroska::TrackKind as K;
                let scale = t.timebase.0 as f64 / t.timebase.1.max(1) as f64;
                let fallback = t.default_duration_ns.unwrap_or(0) as f64 / 1e9;
                let mut decode_time = 0.;
                let samples = t
                    .samples
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let dts = decode_time;
                        let duration = if s.duration > 0 {
                            s.duration as f64 * scale
                        } else if fallback > 0. {
                            fallback
                        } else {
                            t.samples
                                .get(i + 1)
                                .map(|n| (n.pts - s.pts).max(0) as f64 * scale)
                                .unwrap_or(0.)
                        };
                        let duration = match &t.codec {
                            filmcraft_matroska::Codec::Aac { .. } => {
                                1024. / t.audio.as_ref().map_or(48000., |a| a.sampling_frequency)
                            }
                            filmcraft_matroska::Codec::Opus { .. } => {
                                let data = f.read_sample(&source, ti, i).unwrap_or_default();
                                opus_duration(&data).unwrap_or(duration)
                            }
                            _ => duration,
                        };
                        decode_time += duration;
                        Sample {
                            pts: s.pts as f64 * scale,
                            dts,
                            duration,
                            key: s.keyframe,
                            size: s.size,
                        }
                    })
                    .collect();
                tracks.push(Track {
                    kind: match t.kind {
                        K::Video => Kind::Video,
                        K::Audio => Kind::Audio,
                        K::Subtitle => Kind::Subtitle,
                        _ => Kind::Other,
                    },
                    entry: mkv_entry(t)?,
                    samples,
                    language: t.language.clone(),
                });
            }
            (Container::Mkv(Box::new(f)), tracks)
        } else if matches!(&signature[4..8], b"ftyp" | b"moov" | b"wide" | b"mdat") {
            let f = mp4::open(&source)?;
            let mut tracks = Vec::new();
            let chapter_ids: Vec<_> = f
                .tracks
                .iter()
                .flat_map(|t| &t.references)
                .filter(|(kind, _)| kind.0 == *b"chap")
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect();
            for t in &f.tracks {
                ensure!(t.timescale > 0, "zero track timescale");
                ensure!(
                    t.entries.len() <= 1,
                    "multiple sample descriptions require native remux support"
                );
                let scale = t.timescale as f64;
                let samples = t
                    .samples
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let pts = t.presentation_pts(i).unwrap_or(s.pts) as f64 / scale;
                        Sample {
                            pts,
                            dts: pts - (s.pts - s.dts) as f64 / scale,
                            duration: s.duration as f64 / scale,
                            key: s.is_sync,
                            size: s.size,
                        }
                    })
                    .collect();
                tracks.push(Track {
                    kind: match t.kind {
                        mp4::TrackKind::Video => Kind::Video,
                        mp4::TrackKind::Audio => Kind::Audio,
                        mp4::TrackKind::Subtitle if !chapter_ids.contains(&t.id) => Kind::Subtitle,
                        _ => Kind::Other,
                    },
                    entry: t.entries.first().cloned(),
                    samples,
                    language: t.language.clone(),
                });
            }
            (Container::Mp4(Box::new(f)), tracks)
        } else {
            anyhow::bail!(
                "Native packet inspection/export currently supports MP4, MOV, MKV and WebM containers"
            )
        };
        Ok(Self {
            source,
            container,
            tracks,
        })
    }
    pub fn data(&self, t: usize, i: usize) -> Result<Vec<u8>> {
        ensure!(
            self.tracks[t].samples[i].size <= 256 * 1024 * 1024,
            "oversized media packet"
        );
        Ok(match &self.container {
            Container::Mp4(f) => f.read_sample(&self.source, t, i)?,
            Container::Mkv(f) => f.read_sample(&self.source, t, i)?,
        })
    }
    pub fn video(&self) -> Result<usize> {
        self.tracks
            .iter()
            .position(|t| t.kind == Kind::Video)
            .context("no video track")
    }
}
fn opus_duration(b: &[u8]) -> Option<f64> {
    let toc = *b.first()?;
    let config = toc >> 3;
    let frame = if config < 12 {
        [0.01, 0.02, 0.04, 0.06][(config & 3) as usize]
    } else if config < 16 {
        [0.01, 0.02][(config & 1) as usize]
    } else {
        [0.0025, 0.005, 0.01, 0.02][(config & 3) as usize]
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => b.get(1)? & 63,
    };
    Some(frame * count as f64)
}
fn opus_head(head: &[u8]) -> Result<mp4::OpusConfig> {
    ensure!(
        head.len() >= 19 && head.starts_with(b"OpusHead"),
        "invalid OpusHead"
    );
    Ok(mp4::OpusConfig {
        output_channels: head[9],
        pre_skip: u16::from_le_bytes(head[10..12].try_into()?),
        input_sample_rate: u32::from_le_bytes(head[12..16].try_into()?),
        output_gain: i16::from_le_bytes(head[16..18].try_into()?),
        channel_mapping_family: head[18],
        channel_mapping: head[19..].to_vec(),
    })
}
fn mkv_entry(t: &filmcraft_matroska::Track) -> Result<Option<SampleEntry>> {
    use filmcraft_matroska::Codec as C;
    let (w, h) = t
        .video
        .as_ref()
        .map(|v| (v.pixel_width as u16, v.pixel_height as u16))
        .unwrap_or((0, 0));
    let audio = t.audio.as_ref();
    let e = match &t.codec {
        C::Avc { avcc } => SampleEntry::avc(mp4::AvcConfig::parse(avcc)?, w, h),
        C::Hevc { hvcc } => SampleEntry::hevc(mp4::HevcConfig::parse(hvcc)?, w, h),
        C::Vp9 { .. } => SampleEntry::video(
            mp4::FourCc(*b"vp09"),
            CodecConfig::Vp9(mp4::VpcConfig {
                profile: 0,
                level: 0,
                bit_depth: 8,
                chroma_subsampling: 1,
                full_range: false,
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                codec_init: Vec::new(),
            }),
            w,
            h,
        ),
        C::Av1 { av1c } => SampleEntry::av1(mp4::Av1Config::parse(av1c)?, w, h),
        C::ProRes { fourcc } => SampleEntry::prores(mp4::FourCc(fourcc.unwrap_or(*b"apcn")), w, h),
        C::Mjpeg => SampleEntry::jpeg(w, h),
        C::Aac { asc } => {
            let a = audio.context("missing AAC parameters")?;
            SampleEntry::aac(asc.clone(), a.channels as u32, a.sampling_frequency as u32)
        }
        C::Opus { head } => {
            let a = audio.context("missing Opus parameters")?;
            SampleEntry::audio(
                mp4::FourCc(*b"Opus"),
                CodecConfig::Opus(opus_head(head)?),
                a.channels as u32,
                48000.,
                16,
            )
        }
        C::Pcm {
            float,
            big_endian,
            bits,
        } => {
            let a = audio.context("missing PCM parameters")?;
            SampleEntry::pcm(mp4::PcmConfig {
                channels: a.channels as u32,
                sample_rate: a.sampling_frequency,
                signed: true,
                bits: *bits,
                float: *float,
                big_endian: *big_endian,
            })
        }
        _ => return Ok(None),
    };
    Ok(Some(e))
}

/// Plain lane text, retaining commas and removing only the two supported markup syntaxes.
fn plain(text: &str, ass: bool) -> String {
    let mut out = String::new();
    let mut tag = false;
    let mut override_tag = false;
    for c in text.chars() {
        match c {
            '<' if !ass => tag = true,
            '>' if tag => tag = false,
            '{' if ass => override_tag = true,
            '}' if override_tag => override_tag = false,
            c if !tag && !override_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("\\N", " ")
        .replace("\\n", " ")
        .replace("\\h", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
pub(crate) fn cues(path: &Path) -> Result<Vec<Cue>> {
    let input = Input::open(path)?;
    let Some(ti) = input.tracks.iter().position(|t| t.kind == Kind::Subtitle) else {
        return Ok(Vec::new());
    };
    let t = &input.tracks[ti];
    let mut out = Vec::new();
    for (i, s) in t.samples.iter().enumerate() {
        let bytes = input.data(ti, i)?;
        let (text, ass) = match &input.container {
            Container::Mkv(f) => {
                use filmcraft_matroska::Codec;
                match &f.tracks[ti].codec {
                    Codec::SubRip => (String::from_utf8_lossy(&bytes).into_owned(), false),
                    Codec::WebVtt => {
                        let text = String::from_utf8_lossy(&bytes);
                        (
                            text.splitn(3, '\n').nth(2).unwrap_or(&text).to_owned(),
                            false,
                        )
                    }
                    Codec::Ass { .. } => (
                        String::from_utf8_lossy(&bytes)
                            .splitn(9, ',')
                            .nth(8)
                            .unwrap_or("")
                            .to_owned(),
                        true,
                    ),
                    _ => continue,
                }
            }
            Container::Mp4(_) => {
                let format = t.entry.as_ref().map(|e| e.format.0).unwrap_or([0; 4]);
                match &format {
                    b"tx3g" | b"text" => {
                        let Some(n) = bytes.get(..2) else { continue };
                        let n = u16::from_be_bytes([n[0], n[1]]) as usize;
                        let text = bytes.get(2..2 + n).context("truncated subtitle sample")?;
                        (decode_text(text), false)
                    }
                    b"wvtt" => (vtt_payload(&bytes)?, false),
                    _ => continue,
                }
            }
        };
        let text = plain(&text, ass);
        if !text.is_empty() && s.duration > 0. {
            out.push(Cue {
                start: s.pts.max(0.),
                end: (s.pts + s.duration).max(0.),
                text,
            });
        }
    }
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(out)
}
fn decode_text(b: &[u8]) -> String {
    if b.starts_with(&[0xfe, 0xff]) || b.starts_with(&[0xff, 0xfe]) {
        let le = b[0] == 0xff;
        String::from_utf16_lossy(
            &b[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| {
                    if le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                })
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}
fn vtt_payload(b: &[u8]) -> Result<String> {
    vtt_boxes(b, 0)
}
fn vtt_boxes(b: &[u8], depth: usize) -> Result<String> {
    ensure!(depth <= 4, "WebVTT subtitle boxes nested too deeply");
    let mut out = String::new();
    let mut p = 0;
    while p + 8 <= b.len() {
        let n = u32::from_be_bytes(b[p..p + 4].try_into()?) as usize;
        ensure!(n >= 8 && p + n <= b.len(), "invalid WebVTT subtitle box");
        match &b[p + 4..p + 8] {
            b"vttc" => out.push_str(&vtt_boxes(&b[p + 8..p + n], depth + 1)?),
            b"payl" => out.push_str(&String::from_utf8_lossy(&b[p + 8..p + n])),
            _ => {}
        }
        p += n;
    }
    Ok(out)
}

/// Parse every slice, refusing to label mixed/unknown access units as safe I/P pictures.
fn picture_type(codec: &CodecConfig, data: &[u8]) -> Result<char> {
    if let CodecConfig::Avc(c) = codec {
        let mut kind = None;
        for nal in filmcraft_bitstream::length_prefixed_nals(data, c.length_size as usize)? {
            let Some(&h) = nal.first() else { continue };
            if !(1..=5).contains(&(h & 31)) {
                continue;
            }
            let rbsp = filmcraft_bitstream::unescape_rbsp(&nal[1..]);
            let mut bits = filmcraft_bitstream::BitReader::new(&rbsp);
            bits.read_ue()?;
            let k = match bits.read_ue()? % 5 {
                0 | 3 => 'P',
                1 => 'B',
                2 | 4 => 'I',
                _ => '?',
            };
            if kind.is_some_and(|old| old != k) {
                return Ok('?');
            }
            kind = Some(k);
        }
        return Ok(kind.unwrap_or('?'));
    }
    if let CodecConfig::Hevc(c) = codec {
        use filmcraft_hevc::{
            params::{Pps, Sps},
            slice::{NalHeader, SliceHeader, SliceType},
        };
        let nals = filmcraft_bitstream::length_prefixed_nals(data, c.length_size as usize)?;
        let mut spss = std::collections::HashMap::new();
        let mut ppss = std::collections::HashMap::new();
        for n in c.sps().into_iter().chain(
            nals.iter()
                .copied()
                .filter(|n| n.len() >= 2 && (n[0] >> 1) & 63 == 33),
        ) {
            let p = Sps::parse(&filmcraft_bitstream::unescape_rbsp(&n[2..]))?;
            spss.insert(p.id, Arc::new(p));
        }
        for n in c.pps().into_iter().chain(
            nals.iter()
                .copied()
                .filter(|n| n.len() >= 2 && (n[0] >> 1) & 63 == 34),
        ) {
            let p = Pps::parse(&filmcraft_bitstream::unescape_rbsp(&n[2..]))?;
            ppss.insert(p.id, Arc::new(p));
        }
        let mut previous = None;
        let mut kind = None;
        for n in nals {
            let h = NalHeader::parse(n)?;
            if !h.is_vcl() {
                continue;
            }
            let (header, _, _) = SliceHeader::parse(
                &filmcraft_bitstream::unescape_rbsp(&n[2..]),
                h,
                |id| {
                    let p = ppss.get(&id).ok_or_else(|| {
                        filmcraft_hevc::Error::MissingParameterSet(format!("PPS {id}"))
                    })?;
                    let s = spss.get(&p.sps_id).ok_or_else(|| {
                        filmcraft_hevc::Error::MissingParameterSet(format!("SPS {}", p.sps_id))
                    })?;
                    Ok((p.clone(), s.clone()))
                },
                previous.as_ref(),
            )?;
            let k = match header.slice_type {
                SliceType::I => 'I',
                SliceType::P => 'P',
                SliceType::B => 'B',
            };
            if kind.is_some_and(|old| old != k) {
                return Ok('?');
            }
            kind = Some(k);
            previous = Some(header);
        }
        return Ok(kind.unwrap_or('?'));
    }
    if let CodecConfig::Vp9(_) = codec {
        let mut bits = filmcraft_bitstream::BitReader::new(data);
        ensure!(bits.read_bits(2)? == 2, "invalid VP9 frame marker");
        let low = bits.read_bits(1)?;
        let high = bits.read_bits(1)?;
        if low + 2 * high == 3 {
            bits.read_bits(1)?;
        }
        if bits.read_flag()? {
            return Ok('?');
        }
        let inter = bits.read_flag()?;
        let show = bits.read_flag()?;
        bits.read_flag()?;
        return Ok(if !inter || (!show && bits.read_flag()?) {
            'I'
        } else {
            'P'
        });
    }
    Ok(match codec {
        CodecConfig::ProRes { .. } | CodecConfig::Jpeg { .. } | CodecConfig::Dnx { .. } => 'I',
        _ => '?',
    })
}
pub(crate) fn inspect(
    path: &Path,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<FrameInfo>) -> bool,
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    crate::fc::register_hardware();
    let input = Input::open(path)?;
    let ti = input.video()?;
    let t = &input.tracks[ti];
    let entry = t.entry.as_ref().context("codec inspection unavailable")?;
    let mut decoder = filmcraft_codecs::make_video_decoder(entry)?;
    let scale = 1_000_000.;
    let mut metadata = std::collections::BTreeMap::new();
    let mut batch = Vec::new();
    {
        let mut accept = |frames: Vec<filmcraft_codecs::video::DecodedFrame>,
                          metadata: &mut std::collections::BTreeMap<i64, (char, bool)>|
         -> bool {
            for f in frames {
                let (kind, key) = metadata.remove(&f.pts).unwrap_or(('?', false));
                batch.push(FrameInfo {
                    pts: f.pts as f64 / scale,
                    kind,
                    key,
                });
                if batch.len() >= 64 && !emit(std::mem::take(&mut batch)) {
                    return false;
                }
            }
            true
        };
        for (i, s) in t.samples.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let data = input.data(ti, i)?;
            let pts = (s.pts * scale).round() as i64;
            metadata.insert(pts, (picture_type(&entry.codec, &data)?, s.key));
            if !accept(decoder.decode(&data, pts)?, &mut metadata) {
                return Ok(());
            }
        }
        if !accept(decoder.flush(), &mut metadata) {
            return Ok(());
        }
    }
    ensure!(
        metadata.is_empty(),
        "decoder did not deliver every indexed picture"
    );
    if !batch.is_empty() {
        emit(batch);
    }
    Ok(())
}

/// Removes unpublished output on every failure, including encoder errors and cancellation.
struct Pending(std::path::PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn pending(dest: &Path) -> Result<Pending> {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let p = dest.with_extension(format!("{}.{}.tmp", std::process::id(), n));
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)?;
    Ok(Pending(p))
}
const SCALE: u32 = 1_000_000;
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "Encoding cancelled");
    Ok(())
}
fn keep_time(keeps: &[(f64, f64)], time: f64) -> Option<f64> {
    let mut base = 0.;
    for &(a, b) in keeps {
        if time >= a - 0.000001 && time < b - 0.000001 {
            return Some(base + (time - a).max(0.));
        }
        base += b - a;
    }
    None
}
fn validate_ranges(keeps: &[(f64, f64)]) -> Result<()> {
    ensure!(!keeps.is_empty(), "no kept ranges");
    let mut last = 0.;
    for &(a, b) in keeps {
        ensure!(
            a.is_finite() && b.is_finite() && a >= last && b > a,
            "invalid export ranges"
        );
        last = b;
    }
    Ok(())
}
fn box_bytes(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = ((data.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(data);
    out
}
/// Top-level boxes of an in-memory box payload: (kind, header + body).
fn child_boxes(data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        ensure!(data.len() - at >= 8, "truncated MP4 box");
        let mut size = u32::from_be_bytes(data[at..at + 4].try_into()?) as usize;
        if size == 1 {
            ensure!(data.len() - at >= 16, "truncated MP4 box");
            size = u64::from_be_bytes(data[at + 8..at + 16].try_into()?) as usize;
        } else if size == 0 {
            size = data.len() - at;
        }
        ensure!(size >= 8 && at + size <= data.len(), "malformed MP4 box");
        out.push((data[at + 4..at + 8].try_into()?, &data[at..at + size]));
        at += size;
    }
    Ok(out)
}
fn full_box(kind: &[u8; 4], version: u8, flags: u32, data: &[u8]) -> Vec<u8> {
    let mut body = ((version as u32) << 24 | flags).to_be_bytes().to_vec();
    body.extend(data);
    box_bytes(kind, &body)
}
/// The QuickTime chapter track Apple's players read (a disabled `text`
/// track the video and audio tracks name through `tref/chap`), one sample a
/// chapter, at movie timescale `movie_ts`. Samples live in their own `mdat`
/// at `offset`, which the caller writes. Returns (trak, mdat).
fn chapter_trak(chapters: &[(f64, f64, String)], id: u32, movie_ts: u32, offset: u64) -> Result<(Vec<u8>, Vec<u8>)> {
    let ms = |t: f64| (t.max(0.) * 1000.).round() as u64;
    let mut samples = Vec::new();
    let mut sizes = Vec::new();
    let mut durations: Vec<u32> = Vec::new();
    for (a, b, title) in chapters {
        ensure!(title.len() <= 65535, "chapter title too long");
        let d = ms(*b).saturating_sub(ms(*a));
        ensure!(d > 0, "zero-length chapter");
        let mut sample = (title.len() as u16).to_be_bytes().to_vec();
        sample.extend(title.as_bytes());
        // QuickTime text defaults to Mac Roman; `encd` 0x100 says UTF-8.
        sample.extend(box_bytes(b"encd", &0x100u32.to_be_bytes()));
        sizes.push(sample.len() as u32);
        durations.push(u32::try_from(d)?);
        samples.extend(sample);
    }
    let lead = ms(chapters[0].0);
    let media: u64 = durations.iter().map(|&d| d as u64).sum();
    let movie = |ms: u64| ms * movie_ts as u64 / 1000;
    let duration = movie(lead + media);
    let mut tkhd = vec![0; 8];
    tkhd.extend(id.to_be_bytes());
    tkhd.extend([0; 4]);
    tkhd.extend(u32::try_from(duration)?.to_be_bytes());
    tkhd.extend([0; 16]); // reserved, layer, group, volume, reserved
    for v in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000] {
        tkhd.extend(v.to_be_bytes());
    }
    tkhd.extend([0; 8]); // width, height
    let mut trak = full_box(b"tkhd", 0, 2, &tkhd); // in movie, not enabled
    if lead > 0 {
        // An empty edit holds the first chapter at its own start.
        let mut elst = 2u32.to_be_bytes().to_vec();
        elst.extend(u32::try_from(movie(lead))?.to_be_bytes());
        elst.extend((-1i32).to_be_bytes());
        elst.extend(0x10000u32.to_be_bytes());
        elst.extend(u32::try_from(movie(media))?.to_be_bytes());
        elst.extend(0u32.to_be_bytes());
        elst.extend(0x10000u32.to_be_bytes());
        trak.extend(box_bytes(b"edts", &full_box(b"elst", 0, 0, &elst)));
    }
    let mut mdhd = vec![0; 8];
    mdhd.extend(1000u32.to_be_bytes());
    mdhd.extend(u32::try_from(media)?.to_be_bytes());
    mdhd.extend([0x55, 0xc4, 0, 0]); // und
    let mut hdlr = b"mhlrtext".to_vec();
    hdlr.extend([0; 12]);
    hdlr.extend(b"\x08Chapters");
    let mut gmin = 0x40u16.to_be_bytes().to_vec();
    gmin.extend([0x80, 0, 0x80, 0, 0x80, 0, 0, 0, 0, 0]);
    let mut text = Vec::new();
    for v in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000] {
        text.extend(v.to_be_bytes());
    }
    let gmhd = box_bytes(b"gmhd", &[full_box(b"gmin", 0, 0, &gmin), box_bytes(b"text", &text)].concat());
    let dref = full_box(b"dref", 0, 0, &[1u32.to_be_bytes().to_vec(), full_box(b"url ", 0, 1, &[])].concat());
    // QuickTime TextSampleEntry: reserved, data ref 1, then display flags,
    // justification, colours, box, font: all zero/default.
    let mut entry = vec![0, 0, 0, 0, 0, 0, 0, 1];
    entry.extend([0; 43]);
    entry.push(0); // empty font name
    let stsd = full_box(b"stsd", 0, 0, &[1u32.to_be_bytes().to_vec(), box_bytes(b"text", &entry)].concat());
    let mut stts = (durations.len() as u32).to_be_bytes().to_vec();
    for d in &durations {
        stts.extend(1u32.to_be_bytes());
        stts.extend(d.to_be_bytes());
    }
    let stsc = [1u32, 1, durations.len() as u32, 1].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>();
    let mut stsz = [0u32, sizes.len() as u32].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>();
    stsz.extend(sizes.iter().flat_map(|v| v.to_be_bytes()));
    let data = offset + 8;
    let co = match u32::try_from(data) {
        Ok(o) => full_box(b"stco", 0, 0, &[1u32.to_be_bytes(), o.to_be_bytes()].concat()),
        Err(_) => full_box(b"co64", 0, 0, &[1u32.to_be_bytes().to_vec(), data.to_be_bytes().to_vec()].concat()),
    };
    let stbl = box_bytes(b"stbl", &[stsd, full_box(b"stts", 0, 0, &stts), full_box(b"stsc", 0, 0, &stsc),
        full_box(b"stsz", 0, 0, &stsz), co].concat());
    let minf = box_bytes(b"minf", &[gmhd, box_bytes(b"dinf", &dref), stbl].concat());
    let mdia = box_bytes(b"mdia", &[full_box(b"mdhd", 0, 0, &mdhd), full_box(b"hdlr", 0, 0, &hdlr), minf].concat());
    trak.extend(mdia);
    ensure!(samples.len() + 8 <= u32::MAX as usize, "chapter samples too large");
    Ok((box_bytes(b"trak", &trak), box_bytes(b"mdat", &samples)))
}
/// Chapters for an MP4/MOV the muxer just finished (`moov` last in the
/// file): a Nero `chpl` list, which VLC/mpv read, and, with `qt_track`, the
/// QuickTime chapter track Apple's players read. `moov` is rebuilt in place
/// with the chapter samples in an `mdat` ahead of it; nothing before it moves.
fn add_mp4_chapters(path: &Path, chapters: &[(f64, f64, String)], qt_track: bool) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    if chapters.is_empty() {
        return Ok(());
    }
    ensure!(
        chapters.len() <= 255,
        "MP4 supports at most 255 chapter titles"
    );
    let src = crate::fc::reader(path).map_err(anyhow::Error::msg)?;
    let f = mp4::open(&src)?;
    let mut payload = vec![1, 0, 0, 0, 0, 0, 0, 0, chapters.len() as u8];
    for (a, _, title) in chapters {
        ensure!(title.len() <= 255, "chapter title exceeds 255 UTF-8 bytes");
        payload.extend(((*a * 1e7).round() as u64).to_be_bytes());
        payload.push(title.len() as u8);
        payload.extend(title.as_bytes());
    }
    let chpl = box_bytes(b"chpl", &payload);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let end = file.seek(SeekFrom::End(0))?;
    let mut moov = vec![0; usize::try_from(end - f.moov_offset)?];
    file.seek(SeekFrom::Start(f.moov_offset))?;
    file.read_exact(&mut moov)?;
    let boxes = child_boxes(&moov)?;
    ensure!(boxes.len() == 1 && &boxes[0].0 == b"moov", "moov must end the file");
    let children = child_boxes(&boxes[0].1[8..])?;
    let mvhd = children.iter().find(|(k, _)| k == b"mvhd").context("missing mvhd")?.1;
    let movie_ts = u32::from_be_bytes(mvhd[if mvhd[8] == 1 { 28..32 } else { 20..24 }].try_into()?);
    let id = u32::from_be_bytes(mvhd[mvhd.len() - 4..].try_into()?).max(f.tracks.iter().map(|t| t.id + 1).max().unwrap_or(1));
    let referring: Vec<u32> = f.tracks.iter()
        .filter(|t| matches!(t.kind, mp4::TrackKind::Video | mp4::TrackKind::Audio))
        .map(|t| t.id).collect();
    let (trak, mdat) = if qt_track {
        let (trak, mdat) = chapter_trak(chapters, id, movie_ts, f.moov_offset)?;
        (Some(trak), mdat)
    } else { (None, Vec::new()) };
    let mut body = Vec::new();
    let mut udta = false;
    for (kind, b) in &children {
        match kind {
            b"mvhd" if trak.is_some() => {
                let mut b = b.to_vec();
                let n = b.len();
                b[n - 4..].copy_from_slice(&(id + 1).to_be_bytes());
                body.extend(b);
            }
            b"trak" if trak.is_some() => {
                let parts = child_boxes(&b[8..])?;
                let tkhd = parts.iter().find(|(k, _)| k == b"tkhd").context("missing tkhd")?.1;
                let p = if tkhd[8] == 1 { 28 } else { 20 };
                let tid = u32::from_be_bytes(tkhd[p..p + 4].try_into()?);
                if !referring.contains(&tid) {
                    body.extend(*b);
                    continue;
                }
                let chap = box_bytes(b"chap", &id.to_be_bytes());
                let mut inner = Vec::new();
                let mut has_tref = false;
                for (k, part) in &parts {
                    if k == b"tref" {
                        has_tref = true;
                        inner.extend(box_bytes(b"tref", &[&part[8..], &chap[..]].concat()));
                    } else {
                        inner.extend(*part);
                    }
                    if k == b"tkhd" && !parts.iter().any(|(k, _)| k == b"tref") {
                        inner.extend(box_bytes(b"tref", &chap));
                        has_tref = true;
                    }
                }
                ensure!(has_tref, "chapter reference not placed");
                body.extend(box_bytes(b"trak", &inner));
            }
            b"udta" if !udta => {
                udta = true;
                body.extend(box_bytes(b"udta", &[&b[8..], &chpl[..]].concat()));
            }
            _ => body.extend(*b),
        }
    }
    if let Some(trak) = trak { body.extend(trak); }
    if !udta { body.extend(box_bytes(b"udta", &chpl)); }
    ensure!(body.len() + 8 < u32::MAX as usize, "chapter metadata exceeds MP4 box size");
    file.seek(SeekFrom::Start(f.moov_offset))?;
    file.write_all(&mdat)?;
    file.write_all(&box_bytes(b"moov", &body))?;
    let len = file.stream_position()?;
    file.set_len(len)?;
    Ok(())
}
fn write_packet<W: std::io::Write + std::io::Seek>(
    mux: &mut mp4::Mp4Writer<W>,
    track: usize,
    data: &[u8],
    duration: f64,
    offset: f64,
    key: bool,
) -> Result<()> {
    write_packet_scaled(mux, track, SCALE, data, duration, offset, key)
}
fn write_packet_scaled<W: std::io::Write + std::io::Seek>(
    mux: &mut mp4::Mp4Writer<W>,
    track: usize,
    scale: u32,
    data: &[u8],
    duration: f64,
    offset: f64,
    key: bool,
) -> Result<()> {
    ensure!(
        duration > 0. && duration * scale as f64 <= u32::MAX as f64,
        "invalid packet duration"
    );
    mux.write_sample(
        track,
        mp4::WriteSample {
            data,
            duration: (duration * scale as f64).round().max(1.) as u32,
            composition_offset: i32::try_from((offset * scale as f64).round() as i64)?,
            is_sync: key,
        },
    )?;
    Ok(())
}
/// Filmcraft's muxer writes identity track matrices. Preserve the original display transform
/// for copied pictures; rendered pictures are already upright and keep the identity matrix.
fn carry_transform(input: &Input, path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let Container::Mp4(original) = &input.container else {
        return Ok(());
    };
    let original = &original.tracks[input.video()?];
    let src = crate::fc::reader(path).map_err(anyhow::Error::msg)?;
    let output = mp4::open(&src)?;
    let target = &output.tracks[output
        .track_of_kind(mp4::TrackKind::Video)
        .context("missing output video")?];
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    for (_, a, b) in crate::media_metadata::atoms(
        &src,
        output.moov_offset + 8,
        filmcraft_media::ByteReader::len(&src),
    )
    .map_err(anyhow::Error::msg)?
    .into_iter()
    .filter(|(k, _, _)| k == b"trak")
    {
        for (_, start, end) in crate::media_metadata::atoms(&src, a, b)
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .filter(|(k, _, _)| k == b"tkhd")
        {
            let mut head = [0; 24];
            filmcraft_media::ByteReader::read_at(&src, start, &mut head)?;
            let version = head[0];
            let id_offset = if version == 1 { 20 } else { 12 };
            let id = u32::from_be_bytes(head[id_offset..id_offset + 4].try_into()?);
            if id != target.id {
                continue;
            }
            let matrix = start + if version == 1 { 52 } else { 40 };
            ensure!(matrix + 44 <= end, "truncated track header");
            file.seek(SeekFrom::Start(matrix))?;
            for v in original.matrix {
                file.write_all(&v.to_be_bytes())?;
            }
            file.write_all(&(original.width.saturating_mul(65536)).to_be_bytes())?;
            file.write_all(&(original.height.saturating_mul(65536)).to_be_bytes())?;
        }
    }
    Ok(())
}
fn text_handlers(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let src = crate::fc::reader(path).map_err(anyhow::Error::msg)?;
    let file = mp4::open(&src)?;
    let mut output = std::fs::OpenOptions::new().write(true).open(path)?;
    for (_, a, b) in crate::media_metadata::atoms(
        &src,
        file.moov_offset + 8,
        filmcraft_media::ByteReader::len(&src),
    )
    .map_err(anyhow::Error::msg)?
    .into_iter()
    .filter(|(kind, _, _)| kind == b"trak")
    {
        let atoms = crate::media_metadata::atoms(&src, a, b).map_err(anyhow::Error::msg)?;
        let Some((_, start, _)) = atoms.iter().find(|(kind, _, _)| kind == b"tkhd") else {
            continue;
        };
        let mut head = [0; 24];
        filmcraft_media::ByteReader::read_at(&src, *start, &mut head)?;
        let p = if head[0] == 1 { 20 } else { 12 };
        let id = u32::from_be_bytes(head[p..p + 4].try_into()?);
        if !file.tracks.iter().any(|t| {
            t.id == id
                && t.entries
                    .first()
                    .is_some_and(|e| matches!(&e.format.0, b"tx3g" | b"text" | b"wvtt"))
        }) {
            continue;
        }
        for (_, a, b) in atoms.into_iter().filter(|(kind, _, _)| kind == b"mdia") {
            for (_, a, b) in crate::media_metadata::atoms(&src, a, b)
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .filter(|(kind, _, _)| kind == b"hdlr")
            {
                ensure!(b >= a + 12, "truncated subtitle handler");
                output.seek(SeekFrom::Start(a + 8))?;
                output.write_all(b"sbtl")?;
            }
        }
    }
    Ok(())
}
fn new_mux(input: &Input, path: &Path, dest: &Path) -> Result<mp4::Mp4Writer<std::fs::File>> {
    let brand = if dest
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mov"))
    {
        mp4::Brand::Mov
    } else {
        mp4::Brand::Mp4
    };
    let mut options = mp4::WriterOptions::new(brand);
    if let Container::Mp4(f) = &input.container {
        options.metadata = f.metadata.entries.clone();
    }
    Ok(mp4::Mp4Writer::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(true)
            .open(path)?,
        options,
    )?)
}
fn cfg(entry: SampleEntry, language: &str) -> mp4::TrackConfig {
    let mut c = mp4::TrackConfig::new(entry, SCALE);
    c.language = language.into();
    c
}

/// Lossless packet selection: key boundaries are checked again at the I/O boundary.
fn copy_ranges(
    input: &Input,
    mux: &mut mp4::Mp4Writer<std::fs::File>,
    keeps: &[(f64, f64)],
    cancel: &AtomicBool,
    subtitles: bool,
) -> Result<()> {
    let vi = input.video()?;
    let video = &input.tracks[vi];
    for &(a, b) in keeps {
        ensure!(
            video
                .samples
                .iter()
                .any(|s| s.key && (s.pts - a).abs() < 0.0001),
            "Stream-copy start is not a keyframe"
        );
        let end = video
            .samples
            .iter()
            .map(|s| s.pts + s.duration)
            .fold(0., f64::max);
        ensure!(
            (b - end).abs() < 0.05
                || video
                    .samples
                    .iter()
                    .any(|s| s.key && (s.pts - b).abs() < 0.0001),
            "Stream-copy end is not a keyframe"
        );
    }
    for (ti, t) in input.tracks.iter().enumerate().filter(|(ti, t)| {
        t.kind == Kind::Audio
            || (t.kind == Kind::Video && *ti == vi)
            || (subtitles && t.kind == Kind::Subtitle)
    }) {
        let entry = t
            .entry
            .clone()
            .context("This track has no lossless native MP4 mapping")?;
        if let CodecConfig::Pcm(pcm) = &entry.codec {
            let rate = pcm.sample_rate.round() as u32;
            ensure!(rate > 0, "invalid PCM sample rate");
            let width = pcm.bytes_per_frame() as usize;
            let target = mux.add_track(mp4::TrackConfig::new(entry.clone(), rate))?;
            for &(a, b) in keeps {
                for (i, s) in t.samples.iter().enumerate() {
                    let end = s.pts + s.duration;
                    if end <= a || s.pts >= b {
                        continue;
                    }
                    cancelled(cancel)?;
                    let data = input.data(ti, i)?;
                    let frames = data.len() / width;
                    let first = (((a - s.pts).max(0.) * rate as f64).round() as usize).min(frames);
                    let last = (((b - s.pts).max(0.) * rate as f64).round() as usize).min(frames);
                    if last <= first {
                        continue;
                    }
                    mux.write_sample(
                        target,
                        mp4::WriteSample {
                            data: &data[first * width..last * width],
                            duration: (last - first) as u32,
                            composition_offset: 0,
                            is_sync: true,
                        },
                    )?;
                }
            }
            continue;
        }
        let first_dts = video.samples.first().map(|s| s.dts).unwrap_or(0.);
        let first_pts = video.samples.first().map(|s| s.pts).unwrap_or(0.);
        let delay = if t.kind == Kind::Video {
            (first_pts - first_dts).max(0.)
        } else {
            (-t.samples.first().map(|s| s.pts).unwrap_or(0.)).max(0.)
        };
        let scale = match &input.container {
            Container::Mp4(f) => f.tracks[ti].timescale,
            _ => SCALE,
        };
        let mut config = cfg(entry, &t.language);
        config.timescale = scale;
        config.media_start = Some((delay * scale as f64).round() as i64);
        if t.kind == Kind::Audio {
            config.edits = vec![mp4::Edit {
                segment_duration: (keeps.iter().map(|(a, b)| b - a).sum::<f64>() * 1000.).round()
                    as u64,
                media_time: (delay * scale as f64).round() as i64,
                media_rate: 65536,
            }];
        }
        let target = mux.add_track(config)?;
        for &(a, b) in keeps {
            let selected: Vec<_> = t
                .samples
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    (s.pts >= a - 0.000001 || (t.kind == Kind::Audio && a == 0. && s.pts < 0.))
                        && s.pts < b - 0.000001
                })
                .collect();
            if selected.is_empty() {
                continue;
            }
            for (i, s) in selected {
                cancelled(cancel)?;
                let data = input.data(ti, i)?;
                let offset = if t.kind == Kind::Video {
                    s.pts - s.dts
                } else {
                    0.
                };
                // Audio packets remain whole. The output may differ by one compressed audio packet at a cut.
                let duration = if t.kind == Kind::Video {
                    s.duration
                } else {
                    s.duration.max(0.000001)
                };
                write_packet_scaled(mux, target, scale, &data, duration, offset, s.key)?;
            }
        }
    }
    Ok(())
}
struct H264 {
    encoder: filmcraft_h264enc::Encoder,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    w: usize,
    h: usize,
}
impl H264 {
    fn new(w: u32, h: u32, fps: f64) -> Result<Self> {
        ensure!(
            w > 0
                && h > 0
                && w.is_multiple_of(2)
                && h.is_multiple_of(2)
                && w <= 65535
                && h <= 65535,
            "H.264 export requires even dimensions within 65535 pixels"
        );
        let mut cfg =
            filmcraft_h264enc::EncoderConfig::new(w, h, (fps * 1000.).round() as u32, 1000);
        cfg.rate = filmcraft_h264enc::RateControl::Crf(16.);
        cfg.bframes = 0;
        cfg.keyint = (fps * 2.).round().max(1.) as u32;
        cfg.format = filmcraft_h264enc::PacketFormat::LengthPrefixed;
        cfg.threads = 2;
        Ok(Self {
            encoder: filmcraft_h264enc::Encoder::new(cfg)?,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            w: w as usize,
            h: h as usize,
        })
    }
    fn entry(&self) -> Result<SampleEntry> {
        Ok(SampleEntry::avc(
            mp4::AvcConfig::parse(&self.encoder.avcc())?,
            self.w as u16,
            self.h as u16,
        ))
    }
    fn encode(
        &mut self,
        frame: &filmcraft_frame::VideoFrame,
        index: i64,
    ) -> Result<Vec<filmcraft_h264enc::Packet>> {
        let rgba = frame.to_rgba8();
        filmcraft_export::rgba_to_yuv420_8(
            &rgba,
            self.w,
            self.h,
            &mut self.y,
            &mut self.u,
            &mut self.v,
        );
        Ok(self.encoder.encode(
            &filmcraft_h264enc::YuvFrame {
                y: &self.y,
                u: &self.u,
                v: &self.v,
                y_stride: self.w,
                uv_stride: self.w / 2,
            },
            index,
        )?)
    }
}
fn audio_encode(
    source: &filmcraft_media::SharedSource,
    mux: &mut mp4::Mp4Writer<std::fs::File>,
    keeps: &[(f64, f64)], pcm: bool, cancel: &AtomicBool,
) -> Result<()> { audio_encode_checked(source,mux,keeps,pcm,||cancelled(cancel)) }
fn audio_encode_checked(
    source: &filmcraft_media::SharedSource,
    mux: &mut mp4::Mp4Writer<std::fs::File>,
    keeps: &[(f64, f64)], pcm: bool, mut check: impl FnMut()->Result<()>,
) -> Result<()> {
    for (stream, a) in source.info().audio_streams.iter().enumerate() {
        let rate = 48000u32;
        let channels = a.channels as usize;
        ensure!(
            (1..=8).contains(&channels),
            "unsupported audio channel count"
        );
        // Preserve Filmcraft's channel order; no implicit downmix.
        let mut encoder =
            filmcraft_aac::Encoder::new(filmcraft_aac::EncoderConfig::cbr(rate, channels, 192000))?;
        let entry = if pcm {
            SampleEntry::pcm(mp4::PcmConfig {
                bits: 16,
                float: false,
                big_endian: false,
                signed: true,
                channels: channels as u32,
                sample_rate: rate as f64,
            })
        } else {
            SampleEntry::aac(encoder.audio_specific_config(), channels as u32, rate)
        };
        let mut config = mp4::TrackConfig::new(entry, rate);
        if !pcm {
            config.media_start = Some(encoder.priming_samples() as i64);
            config.edits = vec![mp4::Edit {
                segment_duration: (keeps.iter().map(|(a, b)| b - a).sum::<f64>() * 1000.).round()
                    as u64,
                media_time: encoder.priming_samples() as i64,
                media_rate: 65536,
            }];
        }
        let track = mux.add_track(config)?;
        for &(a, b) in keeps {
            let start = (a * rate as f64).round() as i64;
            let end = (b * rate as f64).round() as i64;
            for pos in (start..end).step_by(rate as usize) {
                check()?;
                let count = (end - pos).min(rate as i64) as usize;
                let audio = source.audio_stream(stream, pos, count, rate)?;
                if pcm {
                    let mut bytes = Vec::with_capacity(count * channels * 2);
                    for i in 0..count {
                        for c in 0..channels {
                            bytes.extend(
                                (((audio.channels[c][i] * 32768.)
                                    .round()
                                    .clamp(-32768., 32767.))
                                    as i16)
                                    .to_le_bytes(),
                            );
                        }
                    }
                    mux.write_sample(
                        track,
                        mp4::WriteSample {
                            data: &bytes,
                            duration: count as u32,
                            composition_offset: 0,
                            is_sync: true,
                        },
                    )?;
                } else {
                    for data in encoder
                        .encode(&audio.channels.iter().map(Vec::as_slice).collect::<Vec<_>>())
                    {
                        mux.write_sample(
                            track,
                            mp4::WriteSample {
                                data: &data,
                                duration: 1024,
                                composition_offset: 0,
                                is_sync: true,
                            },
                        )?;
                    }
                }
            }
        }
        if !pcm {
            for data in encoder.flush() {
                mux.write_sample(
                    track,
                    mp4::WriteSample {
                        data: &data,
                        duration: 1024,
                        composition_offset: 0,
                        is_sync: true,
                    },
                )?;
            }
        }
    }
    Ok(())
}
fn encode_ranges(
    input: &Input,
    path: &Path,
    mux: &mut mp4::Mp4Writer<std::fs::File>,
    keeps: &[(f64, f64)],
    crop: Option<(u32, u32, u32, u32)>,
    cancel: &AtomicBool,
) -> Result<()> {
    crate::media_metadata::read(path)
        .map_err(anyhow::Error::msg)?
        .check_sdr()
        .map_err(anyhow::Error::msg)?;
    let source = crate::fc::open(path).map_err(anyhow::Error::msg)?;
    let info = source.info();
    let vi = input.video()?;
    let (w, h) = crop.map(|(_, _, w, h)| (w, h)).unwrap_or((
        info.video.as_ref().context("no video")?.width,
        info.video.as_ref().context("no video")?.height,
    ));
    let fps = info.frame_rate().as_f64();
    ensure!(fps.is_finite() && fps > 0., "invalid frame rate");
    let mut h264 = if crop.is_none() {
        Some(H264::new(w, h, fps)?)
    } else {
        None
    };
    let mut prores =
        crop.map(|_| filmcraft_prores::Encoder::new(filmcraft_prores::Profile::Proxy, w, h));
    let entry = match &h264 {
        Some(e) => e.entry()?,
        None => SampleEntry::prores(mp4::FourCc(*b"apco"), w as u16, h as u16),
    };
    let rate = info.frame_rate();
    let scale = if crop.is_none() {
        u32::try_from(rate.num)?
    } else {
        match &input.container {
            Container::Mp4(f) => f.tracks[vi].timescale,
            _ => SCALE,
        }
    };
    let mut config = cfg(entry, "und");
    config.timescale = scale;
    let track = mux.add_track(config)?;
    let mut index = 0i64;
    for s in {
        let mut samples = input.tracks[vi].samples.clone();
        samples.sort_by(|a, b| a.pts.total_cmp(&b.pts));
        samples
    } {
        if keep_time(keeps, s.pts).is_none() {
            continue;
        }
        cancelled(cancel)?;
        let frame = source.video_frame(filmcraft_media::FrameRequest::full(
            filmcraft_time::Tick::from_seconds_f64(s.pts.max(0.)),
        ))?;
        let frame = if let Some((x, y, w, h)) = crop {
            ensure!(
                x.checked_add(w).is_some_and(|n| n <= frame.width)
                    && y.checked_add(h).is_some_and(|n| n <= frame.height),
                "crop outside decoded picture"
            );
            Arc::new(frame.cropped(filmcraft_frame::Region {
                x: x as usize,
                y: y as usize,
                w: w as usize,
                h: h as usize,
            }))
        } else {
            frame
        };
        if let Some(enc) = h264.as_mut() {
            for p in enc.encode(&frame, index)? {
                write_packet_scaled(
                    mux,
                    track,
                    scale,
                    &p.data,
                    rate.den as f64 / rate.num as f64,
                    0.,
                    p.keyframe,
                )?;
            }
        }
        if let Some(enc) = prores.as_mut() {
            let mut p = filmcraft_prores::Frame::new(
                w,
                h,
                filmcraft_prores::ChromaFormat::Yuv422,
                10,
                false,
            );
            filmcraft_export::rgba_to_yuv422_10(
                &frame.to_rgba8(),
                w as usize,
                h as usize,
                &mut p.y,
                &mut p.cb,
                &mut p.cr,
            );
            write_packet_scaled(mux, track, scale, &enc.encode(&p)?, s.duration, 0., true)?;
        }
        index += 1;
    }
    if let Some(enc) = h264.as_mut() {
        for p in enc.encoder.flush() {
            write_packet_scaled(
                mux,
                track,
                scale,
                &p.data,
                rate.den as f64 / rate.num as f64,
                0.,
                p.keyframe,
            )?;
        }
    }
    audio_encode(&source, mux, keeps, crop.is_some(), cancel)?;
    Ok(())
}

/// Locate only the GOPs intersected by an exact trim boundary. Whole GOPs
/// remain coded packets, including when frame mode was selected unnecessarily.
fn trim_regions(input: &Input, keeps: &[(f64, f64)]) -> Result<Vec<crate::smart::Region>> {
    let video = &input.tracks[input.video()?];
    let duration = video.samples.iter().map(|s| s.pts + s.duration).fold(0., f64::max);
    let mut keys: Vec<_> = video.samples.iter().filter(|s| s.key).map(|s| s.pts).collect();
    keys.sort_by(f64::total_cmp);
    let mut regions = Vec::new();
    for (i, &start) in keys.iter().enumerate() {
        let end = keys.get(i + 1).copied().unwrap_or(duration);
        let mut boundaries: Vec<_> = keeps.iter().flat_map(|&(a, b)| [a, b])
            .filter(|t| *t > start + 0.0001 && *t < end - 0.0001).collect();
        boundaries.sort_by(f64::total_cmp);
        boundaries.dedup_by(|a, b| (*a - *b).abs() < 0.0001);
        if !boundaries.is_empty() {
            regions.push(crate::smart::Region { start, encode_start: start, end, boundaries });
        }
    }
    Ok(regions)
}

pub(crate) fn export(
    path: &Path,
    dest: &Path,
    keeps: &[(f64, f64)],
    copy: bool,
    chapters: &[(f64, f64, String)],
    crop: Option<(u32, u32, u32, u32)>,
) -> Result<()> {
    export_with_cancel(path, dest, keeps, copy, chapters, crop, &AtomicBool::new(false))
}

pub(crate) fn export_with_cancel(
    path: &Path,
    dest: &Path,
    keeps: &[(f64, f64)],
    copy: bool,
    chapters: &[(f64, f64, String)],
    crop: Option<(u32, u32, u32, u32)>,
    cancel: &AtomicBool,
) -> Result<()> {
    validate_ranges(keeps)?;
    ensure!(
        path != dest && (!dest.exists() || path.canonicalize()? != dest.canonicalize()?),
        "source must never be overwritten"
    );
    ensure!(
        dest.extension()
            .is_some_and(|e| ["mp4", "m4v", "mov", "mkv", "webm"]
                .iter()
                .any(|v| e.eq_ignore_ascii_case(v))),
        "unsupported output container"
    );
    let input = Input::open(path)?;
    // Pixel edits always encode every kept frame. Exact trims without pixel
    // edits repair only intersected GOPs, then use the existing lossless muxer.
    // Unsupported smart-encoding formats fail explicitly; never silently fall
    // back to a full transcode.
    let repaired = if !copy && crop.is_none() {
        let regions = trim_regions(&input, keeps)?;
        if regions.is_empty() { None } else { Some(Pending(smart(path, &regions, cancel)?)) }
    } else { None };
    let input = if let Some(work) = &repaired { Input::open(&work.0)? } else { input };
    let copy = crop.is_none();
    let temp = pending(dest)?;
    let mut mux = new_mux(&input, &temp.0, dest)?;
    if copy {
        copy_ranges(&input, &mut mux, keeps, cancel, false)?;
    } else {
        encode_ranges(&input, path, &mut mux, keeps, crop, cancel)?;
    }
    ensure!(!cancel.load(std::sync::atomic::Ordering::Relaxed), "Export cancelled");
    mux.finish()?;
    if copy {
        carry_transform(&input, &temp.0)?;
    }
    let mkv = dest
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mkv") || e.eq_ignore_ascii_case("webm"));
    // The Matroska remux writes its own Chapters and copies every track.
    add_mp4_chapters(&temp.0, chapters, !mkv)?;
    if mkv
    {
        remux_mkv(&temp.0, dest, chapters)?;
    } else {
        std::fs::rename(&temp.0, dest)?;
    }
    Ok(())
}
fn ebml(id: u32, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    filmcraft_matroska::ebml::write_id(&mut b, id);
    filmcraft_matroska::ebml::write_size(&mut b, data.len() as u64, 1);
    b.extend(data);
    b
}
fn uint(id: u32, v: u64) -> Vec<u8> {
    let n = (8 - v.leading_zeros() as usize / 8).max(1);
    ebml(id, &v.to_be_bytes()[8 - n..])
}
fn remux_mkv(path: &Path, dest: &Path, chapters: &[(f64, f64, String)]) -> Result<()> {
    let input = Input::open(path)?;
    if let Container::Mp4(f) = &input.container {
        ensure!(
            f.tracks
                .iter()
                .filter(|t| t.kind == mp4::TrackKind::Video)
                .all(|t| t.display_rotation() == Some(0)),
            "Native Matroska muxing cannot yet preserve a display transform; choose MOV or MP4"
        );
    }
    let mut tracks = Vec::new();
    let mut delays = Vec::new();
    for t in &input.tracks {
        let e = t.entry.as_ref().context("missing output sample entry")?;
        let (id, private) = match &e.codec {
            CodecConfig::Avc(c) => ("V_MPEG4/ISO/AVC", c.to_bytes()),
            CodecConfig::Hevc(c) => ("V_MPEGH/ISO/HEVC", c.to_bytes()),
            CodecConfig::Av1(c) => ("V_AV1", c.to_bytes()),
            CodecConfig::ProRes { fourcc } => ("V_PRORES", fourcc.0.to_vec()),
            CodecConfig::Vp9(_) => ("V_VP9", Vec::new()),
            CodecConfig::Opus(c) => {
                let mut head = b"OpusHead".to_vec();
                head.extend([1, c.output_channels]);
                head.extend(c.pre_skip.to_le_bytes());
                head.extend(c.input_sample_rate.to_le_bytes());
                head.extend(c.output_gain.to_le_bytes());
                head.push(c.channel_mapping_family);
                head.extend(&c.channel_mapping);
                ("A_OPUS", head)
            }
            CodecConfig::Aac(c) => ("A_AAC", c.asc.clone()),
            CodecConfig::Pcm(_) => ("A_PCM/INT/LIT", Vec::new()),
            _ => anyhow::bail!("codec has no native Matroska export mapping"),
        };
        let mut spec = filmcraft_matroska::TrackSpec::new(
            if t.kind == Kind::Video {
                filmcraft_matroska::TrackKind::Video
            } else {
                filmcraft_matroska::TrackKind::Audio
            },
            id,
        );
        spec.codec_private = private;
        spec.language = Some(t.language.clone());
        let delay = (-t.samples.iter().map(|s| s.pts).fold(0., f64::min)).max(0.);
        spec.codec_delay_ns = (delay * 1e9).round() as u64;
        if matches!(e.codec, CodecConfig::Opus(_)) {
            spec.seek_pre_roll_ns = 80_000_000;
        }
        delays.push(delay);
        spec.video_size = e.video.as_ref().map(|v| (v.width as u32, v.height as u32));
        spec.audio = e
            .audio
            .as_ref()
            .map(|a| (a.sample_rate, a.channels as u64, Some(a.sample_size as u64)));
        tracks.push(spec);
    }
    // WebM cannot carry H.264/AAC or ProRes; never publish a falsely labelled file.
    ensure!(
        !dest
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("webm"))
            || input
                .tracks
                .iter()
                .all(|t| t.entry.as_ref().is_some_and(|e| matches!(
                    e.codec,
                    CodecConfig::Vp9(_) | CodecConfig::Av1(_) | CodecConfig::Opus(_)
                ))),
        "Choose MKV or MP4 for H.264/AAC output; WebM requires VP8/VP9/AV1 and Opus/Vorbis"
    );
    let temp = pending(dest)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(&temp.0)?;
    let mut mux = filmcraft_matroska::MkvWriter::new(
        file,
        tracks,
        filmcraft_matroska::MuxOptions {
            timestamp_scale: 1000,
            ..Default::default()
        },
    )?;
    let mut order = Vec::new();
    for (ti, t) in input.tracks.iter().enumerate() {
        for (i, s) in t.samples.iter().enumerate() {
            order.push((s.dts, ti, i));
        }
    }
    order.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (_, ti, i) in order {
        let s = &input.tracks[ti].samples[i];
        mux.write_frame(
            ti,
            ((s.pts + delays[ti]) * 1e9).round() as i64,
            s.key,
            &input.data(ti, i)?,
            Some((s.duration * 1e9).round() as u64),
        )?;
    }
    let mut file = mux.finish()?;
    if !chapters.is_empty() {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut atoms = Vec::new();
        for (i, (a, b, title)) in chapters.iter().enumerate() {
            let mut atom = uint(0x73c4, i as u64 + 1);
            atom.extend(uint(0x91, (a * 1e9).round() as u64));
            atom.extend(uint(0x92, (b * 1e9).round() as u64));
            atom.extend(ebml(0x80, &ebml(0x85, title.as_bytes())));
            atoms.extend(ebml(0xb6, &atom));
        }
        let chapter = ebml(0x1043a770, &ebml(0x45b9, &atoms));
        file.seek(SeekFrom::End(0))?;
        file.write_all(&chapter)?;
        // The muxer's Segment size must include the newly appended Chapters element.
        file.seek(SeekFrom::Start(0))?;
        let mut head = [0; 128];
        file.read_exact(&mut head)?;
        let p = head
            .windows(4)
            .position(|b| b == [0x18, 0x53, 0x80, 0x67])
            .context("missing Segment")?
            + 4;
        let end = file.seek(SeekFrom::End(0))?;
        let mut size = Vec::new();
        filmcraft_matroska::ebml::write_size(&mut size, end - (p + 8) as u64, 8);
        file.seek(SeekFrom::Start(p as u64))?;
        file.write_all(&size)?;
    }
    drop(file);
    std::fs::rename(&temp.0, dest)?;
    Ok(())
}

/// Copy untouched coded pictures; validate before publishing.
pub(crate) fn smart(
    path: &Path,
    regions: &[crate::smart::Region],
    cancel: &AtomicBool,
) -> Result<std::path::PathBuf> {
    cancelled(cancel)?;
    ensure!(!regions.is_empty(), "No GOPs need encoding");
    let input = Input::open(path)?;
    let vi = input.video()?;
    let t = &input.tracks[vi];
    let entry = t.entry.as_ref().context("missing video configuration")?;
    let CodecConfig::Avc(original) = &entry.codec else {
        anyhow::bail!("GOP encoding currently supports 8-bit H.264 4:2:0 video")
    };
    let info = crate::media_metadata::read(path).map_err(anyhow::Error::msg)?;
    ensure!(
        info.info.pix_fmt == "yuv420p",
        "GOP encoding currently supports 8-bit H.264 4:2:0 video"
    );
    info.check_sdr().map_err(anyhow::Error::msg)?;
    let mut pts: Vec<_> = t.samples.iter().map(|s| s.pts).collect();
    pts.sort_by(f64::total_cmp);
    ensure!(
        pts.len() > 1 && pts[0].abs() < 0.0001,
        "GOP encoding requires a zero-based video timeline"
    );
    let duration = t
        .samples
        .iter()
        .map(|s| s.pts + s.duration)
        .fold(0., f64::max);
    let boundary =
        |at: f64| pts.iter().any(|p| (p - at).abs() < 0.0001) || (duration - at).abs() < 0.0001;
    let mut previous = 0.;
    for r in regions {
        ensure!(
            r.start >= previous
                && r.encode_start >= r.start
                && r.end > r.encode_start
                && r.end <= duration + 0.0001,
            "invalid/overlapping GOP regions"
        );
        ensure!(
            std::iter::once(&r.start)
                .chain(std::iter::once(&r.encode_start))
                .chain(std::iter::once(&r.end))
                .chain(&r.boundaries)
                .all(|p| p.is_finite() && boundary(*p)),
            "An edit is between frames; move it to a frame boundary before encoding"
        );
        ensure!(
            t.samples
                .iter()
                .any(|s| s.key && (s.pts - r.start).abs() < 0.0001),
            "GOP must start on a keyframe"
        );
        previous = r.end;
    }
    if regions.iter().any(|r| r.encode_start > r.start + 0.0001) {
        let mut types = Vec::new();
        inspect(path, cancel, |batch| {
            types.extend(batch);
            true
        })?;
        for r in regions.iter().filter(|r| r.encode_start > r.start + 0.0001) {
            let frames: Vec<_> = types
                .iter()
                .filter(|f| f.pts >= r.start - 0.0001 && f.pts < r.end - 0.0001)
                .collect();
            let count = pts
                .iter()
                .filter(|p| **p >= r.start - 0.0001 && **p < r.end - 0.0001)
                .count();
            ensure!(
                frames.len() == count && frames.iter().all(|f| matches!(f.kind, 'I' | 'P')),
                "Partial GOP copy requires complete I/P frame analysis; source unchanged"
            );
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let stem = stem
        .rsplit_once(".encoded-")
        .filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit()))
        .map_or(stem.as_ref(), |(s, _)| s);
    let dest = path.with_file_name(format!("{stem}.encoded-{stamp}.mp4"));
    let temp = pending(&dest)?;
    let mut mux = new_mux(&input, &temp.0, &dest)?;
    let scale = match &input.container {
        Container::Mp4(f) => f.tracks[vi].timescale,
        _ => SCALE,
    };
    let delay = (-t.samples[0].dts).max(0.);
    let mut output_entry = entry.clone();
    output_entry.format = mp4::FourCc(*b"avc3");
    if let CodecConfig::Avc(c) = &mut output_entry.codec {
        c.length_size = 4;
    }
    let mut config = cfg(output_entry, &t.language);
    config.timescale = scale;
    config.media_start = Some((delay * scale as f64).round() as i64);
    let vt = mux.add_track(config)?;
    let source = crate::fc::open(path).map_err(anyhow::Error::msg)?;
    let v = entry.video.as_ref().context("no coded dimensions")?;
    let fps = source.info().frame_rate().as_f64();
    let mut at = 0usize;
    let mut dts = 0.;
    while at < t.samples.len() {
        cancelled(cancel)?;
        let sample = &t.samples[at];
        let region = regions
            .iter()
            .find(|r| sample.pts >= r.encode_start - 0.0001 && sample.pts < r.end - 0.0001);
        if let Some(r) = region {
            let end = (at + 1..t.samples.len())
                .find(|&i| t.samples[i].key && t.samples[i].pts >= r.end - 0.0001)
                .unwrap_or(t.samples.len());
            let mut times: Vec<_> = t.samples[at..end]
                .iter()
                .map(|s| s.pts)
                .filter(|p| *p >= r.encode_start - 0.0001 && *p < r.end - 0.0001)
                .collect();
            times.sort_by(f64::total_cmp);
            ensure!(
                times.len() == end - at,
                "GOP packet span crosses unresolved dependencies"
            );
            let mut enc = H264::new(v.width as u32, v.height as u32, fps)?;
            let mut local = 0;
            let mut headers = enc.encoder.sps_pps();
            for (j, pts) in times.iter().enumerate() {
                cancelled(cancel)?;
                if j > 0 && r.boundaries.iter().any(|p| (p - pts).abs() < 0.0001) {
                    ensure!(enc.encoder.flush().is_empty(), "unexpected encoder delay");
                    enc = H264::new(v.width as u32, v.height as u32, fps)?;
                    headers = enc.encoder.sps_pps();
                    local = 0;
                }
                let decoded = source.video_frame(filmcraft_media::FrameRequest::full(
                    filmcraft_time::Tick::from_seconds_f64(*pts),
                ))?;
                let inverse = info
                    .info
                    .rotation
                    .map(|r| ((r / 90.).rem_euclid(4.)) as u8)
                    .unwrap_or(0);
                let decoded = if inverse > 0 {
                    Arc::new(decoded.rotated(inverse))
                } else {
                    decoded
                };
                let packets = enc.encode(&decoded, local)?;
                ensure!(packets.len() == 1, "unexpected H.264 encoder buffering");
                let p = &packets[0];
                let mut data = Vec::new();
                if p.keyframe {
                    for n in [&headers.0, &headers.1] {
                        data.extend((n.len() as u32).to_be_bytes());
                        data.extend(n);
                    }
                }
                data.extend(&p.data);
                let slot = &t.samples[at + j];
                write_packet_scaled(
                    &mut mux,
                    vt,
                    scale,
                    &data,
                    slot.duration,
                    *pts + delay - dts,
                    p.keyframe,
                )?;
                dts += slot.duration;
                local += 1;
            }
            ensure!(enc.encoder.flush().is_empty(), "unexpected encoder delay");
            at = end;
        } else {
            let mut data = input.data(vi, at)?;
            if sample.key {
                let mut prefix = Vec::new();
                for n in original.sps.iter().chain(&original.pps) {
                    prefix.extend((n.len() as u32).to_be_bytes());
                    prefix.extend(n);
                }
                if original.length_size != 4 {
                    let mut normalized = Vec::new();
                    for n in filmcraft_bitstream::length_prefixed_nals(
                        &data,
                        original.length_size as usize,
                    )? {
                        normalized.extend((n.len() as u32).to_be_bytes());
                        normalized.extend(n);
                    }
                    data = normalized;
                }
                prefix.extend(data);
                data = prefix;
            } else if original.length_size != 4 {
                let mut normalized = Vec::new();
                for n in
                    filmcraft_bitstream::length_prefixed_nals(&data, original.length_size as usize)?
                {
                    normalized.extend((n.len() as u32).to_be_bytes());
                    normalized.extend(n);
                }
                data = normalized;
            }
            write_packet_scaled(
                &mut mux,
                vt,
                scale,
                &data,
                sample.duration,
                sample.pts + delay - dts,
                sample.key,
            )?;
            dts += sample.duration;
            at += 1;
        }
    }
    for (ti, t) in input
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| matches!(t.kind, Kind::Audio | Kind::Subtitle))
    {
        if let Container::Mp4(f) = &input.container {
            let original = &f.tracks[ti];
            let mut config = mp4::TrackConfig::new(
                t.entry
                    .clone()
                    .context("auxiliary track has no native MP4 mapping")?,
                original.timescale,
            );
            config.language = t.language.clone();
            config.edits = original
                .edits
                .iter()
                .map(|edit| mp4::Edit {
                    segment_duration: (edit.segment_duration as f64 * 1000. / f.timescale as f64)
                        .round() as u64,
                    ..*edit
                })
                .collect();
            let target = mux.add_track(config)?;
            for (i, s) in original.samples.iter().enumerate() {
                cancelled(cancel)?;
                mux.write_sample(
                    target,
                    mp4::WriteSample {
                        data: &input.data(ti, i)?,
                        duration: s.duration,
                        composition_offset: i32::try_from(s.pts - s.dts)?,
                        is_sync: s.is_sync,
                    },
                )?;
            }
        } else {
            let target = mux.add_track(cfg(
                t.entry
                    .clone()
                    .context("auxiliary track has no native MP4 mapping")?,
                &t.language,
            ))?;
            for (i, s) in t.samples.iter().enumerate() {
                cancelled(cancel)?;
                write_packet(
                    &mut mux,
                    target,
                    &input.data(ti, i)?,
                    s.duration,
                    s.pts - s.dts,
                    s.key,
                )?;
            }
        }
    }
    mux.finish()?;
    text_handlers(&temp.0)?;
    carry_transform(&input, &temp.0)?;
    let chapters = crate::media_metadata::timeline(path)
        .map_err(anyhow::Error::msg)?
        .map(|t| t.chapters)
        .unwrap_or_default();
    let chapters: Vec<_> = chapters
        .iter()
        .enumerate()
        .map(|(i, (a, title))| {
            (
                *a,
                chapters.get(i + 1).map_or(duration, |c| c.0),
                title.clone(),
            )
        })
        .collect();
    add_mp4_chapters(&temp.0, &chapters, false)?;
    let result = Input::open(&temp.0)?;
    let out = &result.tracks[result.video()?];
    let mut rendered: Vec<_> = out.samples.iter().map(|s| s.pts).collect();
    rendered.sort_by(f64::total_cmp);
    ensure!(
        rendered.len() == pts.len()
            && pts
                .iter()
                .zip(&rendered)
                .all(|(a, b)| (a - b).abs() < 0.0001),
        "Rendered video failed frame timing verification; source unchanged"
    );
    ensure!(
        regions.iter().flat_map(|r| &r.boundaries).all(|b| out
            .samples
            .iter()
            .any(|s| s.key && (s.pts - b).abs() < 0.0001)),
        "Rendered boundaries are not keyframes; source unchanged"
    );
    inspect(&temp.0, cancel, |_| true)?;
    cancelled(cancel)?;
    std::fs::rename(&temp.0, &dest)?;
    Ok(dest)
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Dir(std::path::PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "abner-native-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
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
    fn fixture(path: &Path) {
        let mut mux = mp4::Mp4Writer::new(
            std::fs::File::create(path).unwrap(),
            mp4::WriterOptions::new(mp4::Brand::Mov),
        )
        .unwrap();
        let mut enc = H264::new(96, 64, 12.).unwrap();
        let video = mux.add_track(cfg(enc.entry().unwrap(), "und")).unwrap();
        for i in 0..24 {
            let mut pixels = vec![0; 96 * 64 * 4];
            for (j, p) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *p = [(j % 96 * 2) as u8, (i * 8) as u8, (j / 96 * 3) as u8, 255];
            }
            let f = filmcraft_frame::VideoFrame::rgba8(96, 64, pixels);
            for p in enc.encode(&f, i).unwrap() {
                write_packet(&mut mux, video, &p.data, 1. / 12., 0., p.keyframe).unwrap();
            }
        }
        for p in enc.encoder.flush() {
            write_packet(&mut mux, video, &p.data, 1. / 12., 0., p.keyframe).unwrap();
        }
        for channel in 0..2 {
            let audio = mux
                .add_track(mp4::TrackConfig::new(
                    SampleEntry::pcm(mp4::PcmConfig {
                        bits: 16,
                        float: false,
                        big_endian: false,
                        signed: true,
                        channels: 1,
                        sample_rate: 48000.,
                    }),
                    48000,
                ))
                .unwrap();
            let data: Vec<u8> = (0..96000)
                .flat_map(|i| {
                    let value = if i < 48000 {
                        if channel == 0 { 4096i16 } else { -8192i16 }
                    } else {
                        0
                    };
                    value.to_le_bytes()
                })
                .collect();
            mux.write_sample(
                audio,
                mp4::WriteSample {
                    data: &data,
                    duration: 96000,
                    composition_offset: 0,
                    is_sync: true,
                },
            )
            .unwrap();
        }
        mux.finish().unwrap();
    }
    #[test]
    fn native_export_crop_inspection_and_failure_cleanup_need_no_helpers() {
        let dir = Dir::new("pipeline");
        let source = dir.0.join("source.mov");
        fixture(&source);
        let original = std::fs::read(&source).unwrap();
        let rendered = smart(
            &source,
            &[crate::smart::Region {
                start: 0.,
                encode_start: 0.,
                end: 2.,
                boundaries: vec![1.],
            }],
            &AtomicBool::new(false),
        )
        .unwrap();
        let rendered = Input::open(&rendered).unwrap();
        assert_eq!(rendered.tracks[rendered.video().unwrap()].samples.len(), 24);
        assert_eq!(
            rendered
                .tracks
                .iter()
                .filter(|t| t.kind == Kind::Audio)
                .count(),
            2
        );
        let pcm_copy = dir.0.join("pcm-copy.mov");
        export(&source, &pcm_copy, &[(0., 2.)], true, &[], None).unwrap();
        let pcm = Input::open(&pcm_copy).unwrap();
        assert_eq!(
            pcm.tracks.iter().filter(|t| t.kind == Kind::Audio).count(),
            2
        );

        let encoded = dir.0.join("encoded.mp4");
        export(
            &source,
            &encoded,
            &[(0., 0.5), (1., 2.)],
            false,
            &[(0., 1.5, "Kept ✓".into())],
            None,
        )
        .unwrap();
        let input = Input::open(&encoded).unwrap();
        assert_eq!(input.tracks[input.video().unwrap()].samples.len(), 18);
        assert_eq!(
            input
                .tracks
                .iter()
                .filter(|t| t.kind == Kind::Audio)
                .count(),
            2
        );
        let meta = crate::media_metadata::timeline(&encoded).unwrap().unwrap();
        assert_eq!(meta.chapters, vec![(0., "Kept ✓".into())]);
        // Apple reads a `tref/chap` text track, not the Nero list.
        let src = crate::fc::reader(&encoded).unwrap();
        let f = mp4::open(&src).unwrap();
        let chap = f.tracks.iter().flat_map(|t| &t.references)
            .find(|(k, _)| k.0 == *b"chap").map(|(_, ids)| ids[0]).expect("chapter reference");
        let track = f.tracks.iter().find(|t| t.id == chap).unwrap();
        assert_eq!(track.entries[0].format.0, *b"text");
        assert_eq!(track.samples.len(), 1);
        let crop = dir.0.join("crop.mov");
        export(
            &source,
            &crop,
            &[(0., 2.)],
            true, // Pixel edits override a stream-copy request.
            &[],
            Some((16, 8, 64, 48)),
        )
        .unwrap();
        let input = Input::open(&crop).unwrap();
        let video = &input.tracks[input.video().unwrap()];
        assert_eq!(video.samples.len(), 24);
        let v = video.entry.as_ref().unwrap().video.as_ref().unwrap();
        assert_eq!((v.width, v.height), (64, 48));
        assert_eq!(
            input
                .tracks
                .iter()
                .filter(|t| t.kind == Kind::Audio)
                .count(),
            2
        );
        let mut frames = Vec::new();
        inspect(&crop, &AtomicBool::new(false), |batch| {
            frames.extend(batch);
            true
        })
        .unwrap();
        assert_eq!(frames.len(), 24);
        assert!(frames.iter().all(|f| f.kind == 'I' && f.key));
        let decoded = crate::fc::open(&crop)
            .unwrap()
            .video_frame(filmcraft_media::FrameRequest::full(
                filmcraft_time::Tick::from_seconds_f64(0.5),
            ))
            .unwrap();
        let reference = crate::fc::open(&source)
            .unwrap()
            .video_frame(filmcraft_media::FrameRequest::full(
                filmcraft_time::Tick::from_seconds_f64(0.5),
            ))
            .unwrap()
            .cropped(filmcraft_frame::Region {
                x: 16,
                y: 8,
                w: 64,
                h: 48,
            })
            .to_rgba8();
        let pixels = decoded.to_rgba8();
        let mse = reference
            .iter()
            .zip(&pixels)
            .map(|(a, b)| (*a as f64 - *b as f64).powi(2))
            .sum::<f64>()
            / pixels.len() as f64;
        assert!(mse < 100., "crop pixel MSE {mse}");
        let audio = crate::fc::open(&encoded).unwrap();
        let first = audio.audio_stream(0, 4800, 4800, 48000).unwrap();
        let second = audio.audio_stream(1, 4800, 4800, 48000).unwrap();
        assert!(first.channels[0].iter().copied().sum::<f32>() / 4800. > 0.05);
        assert!(second.channels[0].iter().copied().sum::<f32>() / 4800. < -0.1);
        let silence = audio.audio_stream(0, 36000, 4800, 48000).unwrap();
        assert!(silence.channels[0].iter().all(|s| s.abs() < 0.01));
        let copy = dir.0.join("copy.mp4");
        export(&encoded, &copy, &[(0., 1.5)], true, &[], None).unwrap();
        let copied = Input::open(&copy).unwrap();
        assert_eq!(copied.tracks[copied.video().unwrap()].samples.len(), 18);
        let mkv = dir.0.join("copy.mkv");
        export(
            &encoded,
            &mkv,
            &[(0., 1.5)],
            true,
            &[(0., 1.5, "Chapter".into())],
            None,
        )
        .unwrap();
        let meta = crate::media_metadata::timeline(&mkv).unwrap().unwrap();
        assert_eq!(meta.chapters, vec![(0., "Chapter".into())]);
        assert_eq!(meta.packets.len(), 18);
        let failed = dir.0.join("failed.mp4");
        assert!(export(&encoded, &failed, &[(0.1, 1.5)], true, &[], None).is_err());
        assert!(!failed.exists());
        assert!(
            !std::fs::read_dir(&dir.0).unwrap().any(|e| e
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "tmp"))
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
    #[test]
    fn native_text_subtitle_variants_and_bounded_vtt_need_no_helpers() {
        let dir = Dir::new("subtitles");
        for (name, id, body) in [
            ("srt", "S_TEXT/UTF8", "<i>Hello</i>, 世界\nsecond line"),
            (
                "ass",
                "S_TEXT/ASS",
                "0,0,Default,,0,0,0,,{\\i1}Hello, 世界\\Nsecond line",
            ),
            (
                "vtt",
                "S_TEXT/WEBVTT",
                "cue-id\nalign:start\n<i>Hello</i>, 世界\nsecond line",
            ),
        ] {
            let path = dir.0.join(format!("{name}.mkv"));
            let spec =
                filmcraft_matroska::TrackSpec::new(filmcraft_matroska::TrackKind::Subtitle, id);
            let mut mux = filmcraft_matroska::MkvWriter::new(
                std::fs::File::create(&path).unwrap(),
                vec![spec],
                Default::default(),
            )
            .unwrap();
            mux.write_frame(0, 1_500_000_000, true, body.as_bytes(), Some(2_000_000_000))
                .unwrap();
            mux.finish().unwrap();
            let out = cues(&path).unwrap();
            assert_eq!(out.len(), 1);
            assert_eq!((out[0].start, out[0].end), (1.5, 3.5));
            assert_eq!(out[0].text, "Hello, 世界 second line");
        }
        let vtt = box_bytes(b"vttc", &box_bytes(b"payl", b"a,b"));
        assert_eq!(vtt_payload(&vtt).unwrap(), "a,b");
        assert!(vtt_payload(&[0, 0, 0, 100, b'v', b't', b't', b'c']).is_err());
        assert_eq!(decode_text(&[0xfe, 0xff, 0, 65]), "A");
        let mut nested = box_bytes(b"payl", b"text");
        for _ in 0..6 {
            nested = box_bytes(b"vttc", &nested);
        }
        assert!(vtt_payload(&nested).is_err());
    }
    #[test]
    fn reordered_frame_types_and_packet_copy_match_reference() {
        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = Dir::new("reference");
        for codec in ["libx264", "libx265"] {
            let source = dir.0.join(format!("{codec}.mp4"));
            let mut cmd = Command::new("ffmpeg");
            cmd.args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=96x64:rate=30000/1001:duration=1.001",
                "-c:v",
                codec,
                "-threads",
                "1",
                "-g",
                "15",
                "-bf",
                "2",
            ]);
            if codec == "libx265" {
                cmd.args(["-x265-params", "pools=none:frame-threads=1:log-level=error"]);
            }
            assert!(cmd.arg(&source).status().unwrap().success());
            let reference = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_frames",
                    "-show_entries",
                    "frame=best_effort_timestamp_time,pict_type,key_frame",
                    "-of",
                    "json",
                ])
                .arg(&source)
                .output()
                .unwrap();
            let reference: serde_json::Value = serde_json::from_slice(&reference.stdout).unwrap();
            let reference = reference["frames"].as_array().unwrap();
            let mut frames = Vec::new();
            inspect(&source, &AtomicBool::new(false), |b| {
                frames.extend(b);
                true
            })
            .unwrap();
            assert_eq!(frames.len(), reference.len());
            for (actual, expected) in frames.iter().zip(reference) {
                assert!(
                    (actual.pts
                        - expected["best_effort_timestamp_time"]
                            .as_str()
                            .unwrap()
                            .parse::<f64>()
                            .unwrap())
                    .abs()
                        < 0.000002
                );
                assert_eq!(
                    actual.kind.to_string(),
                    expected["pict_type"].as_str().unwrap()
                );
                assert_eq!(actual.key, expected["key_frame"].as_i64() == Some(1));
            }
            let input = Input::open(&source).unwrap();
            let t = &input.tracks[input.video().unwrap()];
            let end = t
                .samples
                .iter()
                .map(|s| s.pts + s.duration)
                .fold(0., f64::max);
            let dest = dir.0.join(format!("{codec}-copy.mp4"));
            export(&source, &dest, &[(0., end)], true, &[], None).unwrap();
            let copied = Input::open(&dest).unwrap();
            let ct = &copied.tracks[copied.video().unwrap()];
            assert_eq!(ct.samples.len(), t.samples.len());
            for (i, (a, b)) in t.samples.iter().zip(&ct.samples).enumerate() {
                assert!((a.pts - b.pts).abs() < 0.00003);
                assert_eq!(
                    input.data(input.video().unwrap(), i).unwrap(),
                    copied.data(copied.video().unwrap(), i).unwrap()
                );
            }
        }
    }
    #[test]
    fn rotated_fractional_rate_copy_and_subtitles_survive_native_paths() {
        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = Dir::new("rotation");
        let source = dir.0.join("base.mp4");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=96x64:rate=30000/1001:duration=12.012",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-threads",
                    "1",
                    "-g",
                    "30",
                    "-bf",
                    "2",
                    "-metadata",
                    "title=Synthetic timing"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let rotated = dir.0.join("rotated.mov");
        assert!(
            Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-display_rotation", "90", "-i"])
                .arg(&source)
                .args(["-c", "copy"])
                .arg(&rotated)
                .status()
                .unwrap()
                .success()
        );
        let input = Input::open(&rotated).unwrap();
        let t = &input.tracks[input.video().unwrap()];
        let end = t
            .samples
            .iter()
            .map(|s| s.pts + s.duration)
            .fold(0., f64::max);
        let copied = dir.0.join("copy.mov");
        export(&rotated, &copied, &[(0., end)], true, &[], None).unwrap();
        let output = Input::open(&copied).unwrap();
        if let (Container::Mp4(a), Container::Mp4(b)) = (&input.container, &output.container) {
            assert_eq!(a.tracks[0].matrix, b.tracks[0].matrix);
            assert_eq!(a.metadata.title(), b.metadata.title());
        }
        let copied_samples = &output.tracks[output.video().unwrap()].samples;
        assert_eq!(t.samples.len(), copied_samples.len());
        for (a, b) in t.samples.iter().zip(copied_samples) {
            assert!((a.pts - b.pts).abs() < 0.000001);
        }
        let mut pts: Vec<_> = t.samples.iter().map(|s| s.pts).collect();
        pts.sort_by(f64::total_cmp);
        let rendered = smart(
            &rotated,
            &[crate::smart::Region {
                start: pts[30],
                encode_start: pts[30],
                end: pts[60],
                boundaries: vec![pts[42]],
            }],
            &AtomicBool::new(false),
        )
        .unwrap();
        let metadata = crate::media_metadata::read(&rendered).unwrap();
        assert_eq!((metadata.info.width, metadata.info.height), (64, 96));
        let srt = dir.0.join("sub.srt");
        std::fs::write(
            &srt,
            "1\n00:00:00,100 --> 00:00:00,900\n<i>Native</i>, 世界\n",
        )
        .unwrap();
        let subtitled = dir.0.join("subtitled.mp4");
        assert!(
            Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-i"])
                .arg(&source)
                .arg("-i")
                .arg(&srt)
                .args([
                    "-map", "0:v", "-map", "1:0", "-c:v", "copy", "-c:s", "mov_text"
                ])
                .arg(&subtitled)
                .status()
                .unwrap()
                .success()
        );
        let out = cues(&subtitled).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Native, 世界");
        assert!((out[0].start - 0.1).abs() < 0.001);
        assert!((out[0].end - 0.9).abs() < 0.001);
        let rendered = smart(
            &subtitled,
            &[crate::smart::Region {
                start: pts[30],
                encode_start: pts[30],
                end: pts[60],
                boundaries: vec![pts[42]],
            }],
            &AtomicBool::new(false),
        )
        .unwrap();
        let out = cues(&rendered).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "Native, 世界");
    }
    #[test]
    fn webm_packets_and_opus_remux_through_native_mkv_writer() {
        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = Dir::new("webm");
        let source = dir.0.join("source.webm");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=96x64:rate=12:duration=1",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                    "-c:v",
                    "libvpx-vp9",
                    "-deadline",
                    "realtime",
                    "-threads",
                    "1",
                    "-c:a",
                    "libopus"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let input = Input::open(&source).unwrap();
        let t = &input.tracks[input.video().unwrap()];
        let start = t.samples[0].pts;
        let end = t
            .samples
            .iter()
            .map(|s| s.pts + s.duration)
            .fold(0., f64::max);
        let dest = dir.0.join("copy.mkv");
        export(&source, &dest, &[(start, end)], true, &[], None).unwrap();
        let output = Input::open(&dest).unwrap();
        assert_eq!(
            output.tracks[output.video().unwrap()].samples.len(),
            t.samples.len()
        );
        for i in 0..t.samples.len() {
            assert_eq!(
                input.data(input.video().unwrap(), i).unwrap(),
                output.data(output.video().unwrap(), i).unwrap()
            );
        }
        let decoded = crate::fc::open(&dest).unwrap();
        assert_eq!(decoded.info().audio_streams.len(), 1);
        let audio = decoded.audio(4800, 4800, 48000).unwrap();
        assert!(audio.channels[0].iter().any(|s| s.abs() > 0.02));
        let mut frames = Vec::new();
        inspect(&dest, &AtomicBool::new(false), |b| {
            frames.extend(b);
            true
        })
        .unwrap();
        assert_eq!(frames.len(), 12);
    }
}

/// Export the enabled processor chain. Output is published only after mux finalization.
#[cfg(test)]
pub(crate) fn pipeline_export(
    info:&crate::probe::VideoInfo,dest:&Path,keeps:&[(f64,f64)],registry:&abner_processor_api::Registry,
    chain:&[(String,serde_json::Value)],prores:bool,crop:Option<(u32,u32,u32,u32)>,
    cancel:&abner_processor_api::Cancellation,active:&Mutex<Vec<abner_processor_api::Cancellation>>,
    emit:impl FnMut(crate::export_page::Rendered)->Result<(),String>,
)->Result<()> {pipeline_export_with_inputs(&Default::default(),info,dest,keeps,&[],registry,chain,prores,crop,cancel,active,emit)}

pub(crate) fn pipeline_export_with_inputs(
    inputs:&crate::spatial::JobInputs,
    info: &crate::probe::VideoInfo,
    dest: &Path,
    keeps: &[(f64, f64)],
    chapters: &[(f64, f64, String)],
    registry: &abner_processor_api::Registry,
    chain: &[(String, serde_json::Value)],
    prores: bool,
    crop: Option<(u32, u32, u32, u32)>,
    cancel: &abner_processor_api::Cancellation,
    active: &Mutex<Vec<abner_processor_api::Cancellation>>,
    mut emit: impl FnMut(crate::export_page::Rendered) -> Result<(), String>,
) -> Result<()> {
    use abner_processor_api::{Frame, InputRequirement, Role};
    inputs.validate_geometry(info.width,info.height).map_err(anyhow::Error::msg)?;
    ensure!(crop.is_none() || inputs.crops.is_empty(), "Use either the timeline crop or an export crop");
    validate_ranges(keeps)?;
    ensure!(
        !dest.exists(),
        "Output already exists; choose another filename"
    );
    let mkv = dest
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mkv"));
    ensure!(
        mkv || dest
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4")),
        "Choose MP4 or MKV for pipeline export"
    );
    ensure!(!prores || mkv, "ProRes requires MKV");
    cancel.check().map_err(anyhow::Error::msg)?;
    if chain.is_empty() && crop.is_none() && inputs.crops.is_empty() && !prores {
        // Keep the packet path available through the Export workspace. Repair
        // only boundary GOPs, and never pass untouched HDR through the SDR host.
        // Publish after checking cancellation; RAII removes unpublished work.
        let reservation = pending(dest)?;
        let work_path = reservation.0.with_extension(if mkv { "mkv" } else { "mp4" });
        ensure!(!work_path.exists(), "Export working file already exists");
        let work = Pending(work_path);
        export(&info.path, &work.0, keeps, false, &[], None)?;
        cancel.check().map_err(anyhow::Error::msg)?;
        std::fs::rename(&work.0, dest)?;
        return Ok(());
    }
    crate::media_metadata::read(&info.path)
        .map_err(anyhow::Error::msg)?
        .check_sdr()
        .map_err(anyhow::Error::msg)?;
    let temp = pending(dest)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(&temp.0)?;
    let mut mux = mp4::Mp4Writer::new(
        file,
        mp4::WriterOptions::new(if prores {
            mp4::Brand::Mov
        } else {
            mp4::Brand::Mp4
        }),
    )?;
    ensure!(
        info.fps.is_finite() && info.fps > 0. && info.fps <= 1000.,
        "Export requires a valid constant frame rate"
    );
    let (cx, cy, cw, ch) = crop.unwrap_or((0, 0, info.width, info.height));
    ensure!(
        cw >= 2
            && ch >= 2
            && cx.checked_add(cw).is_some_and(|x| x <= info.width)
            && cy.checked_add(ch).is_some_and(|y| y <= info.height),
        "Crop lies outside the source"
    );
    let mut encoder: Option<PipelineEncoder> = None;
    let mut track = None;
    let mut index = 0u64;
    let mut output_time = 0.;
    let mut expected_duration: Option<f64> = None;
    for &(start, end) in keeps {
        cancel.check().map_err(anyhow::Error::msg)?;
        active.lock().unwrap().clear();
        let mut dims = [cw as usize, ch as usize];
        let mut sessions = Vec::new();
        for (stage,(id, settings)) in chain.iter().enumerate() {
            ensure!(stage==0 || inputs.bindings.get(id).is_none_or(|b|b.is_empty()), "Spatial inputs currently require the first enabled processor");
            let f = registry.get(id).map_err(anyhow::Error::msg)?;
            ensure!(matches!(f.capabilities().role, Role::Processor),
                "{}: generator/post-processor execution is not supported by export", f.descriptor().name);
            ensure!(!matches!(f.capabilities().input, InputRequirement::WholeClip),
                "{}: whole-clip execution is not supported by export", f.descriptor().name);
            let session = abner_processor_api::create_bound(f,dims[0],dims[1],settings,inputs.bindings.get(id).cloned().unwrap_or_default())
                .map_err(anyhow::Error::msg)?;
            active.lock().unwrap().push(session.cancellation());
            dims = session.output_dimensions();
            ensure!(
                dims.iter().all(|&d| d >= 2 && d <= 8192),
                "Unsupported processor output size"
            );
            sessions.push(session);
        }
        let player = crate::player::Player::spawn(
            &info.path,
            info.width,
            info.height,
            crate::probe::vt_accel(&info.codec),
            info.rotation,
            16 * 1024 * 1024,
        )
        .context("Could not open export decoder")?;
        player.seek(start, true);
        let mut previous_end: Option<f64> = None;
        let mut accept = |frames: Vec<Frame>| -> Result<()> {
            for f in frames {
                cancel.check().map_err(anyhow::Error::msg)?;
                f.validate().map_err(anyhow::Error::msg)?;
                ensure!(
                    f.width == dims[0] && f.height == dims[1],
                    "Processor changed declared dimensions"
                );
                let unit = f.time_base[0] as f64 / f.time_base[1] as f64;
                let source_pts = f.pts as f64 * unit;
                if source_pts < start - 1e-6 || source_pts >= end - 1e-6 {
                    continue;
                }
                let duration = f.duration as f64 * unit;
                ensure!(
                    duration > 0. && duration.is_finite(),
                    "Invalid processor output duration"
                );
                if let Some(previous) = previous_end {
                    ensure!(
                        (source_pts - previous).abs() < 0.002,
                        "Processor returned missing, reordered, or discontinuous output timestamps"
                    );
                }
                previous_end = Some(source_pts + duration);
                if let Some(expected) = expected_duration {
                    ensure!(
                        (expected - duration).abs() < 0.00001,
                        "Export currently requires constant-rate output"
                    );
                } else {
                    expected_duration = Some(duration);
                }
                let rgba: Vec<u8> = f
                    .pixels
                    .chunks_exact(3)
                    .flat_map(|rgb| {
                        [
                            ((rgb[0] as u32 + 128) / 257) as u8,
                            ((rgb[1] as u32 + 128) / 257) as u8,
                            ((rgb[2] as u32 + 128) / 257) as u8,
                            255,
                        ]
                    })
                    .collect();
                if encoder.is_none() {
                    encoder = Some(PipelineEncoder::new(
                        f.width as u32,
                        f.height as u32,
                        duration,
                        prores,
                    )?);
                }
                let e = encoder.as_mut().unwrap();
                ensure!(
                    e.dims() == [f.width as u32, f.height as u32],
                    "Output dimensions changed between segments"
                );
                let packets = e.encode(&rgba, index)?;
                let ti = match track {
                    Some(t) => t,
                    None => {
                        let mut config = cfg(e.entry(), "und");
                        config.timescale = e.scale();
                        let t = mux.add_track(config)?;
                        track = Some(t);
                        t
                    }
                };
                for packet in packets {
                    write_packet_scaled(
                        &mut mux,
                        ti,
                        e.scale(),
                        &packet.data,
                        packet.duration as f64 / e.scale() as f64,
                        packet.composition_offset as f64 / e.scale() as f64,
                        packet.key,
                    )?;
                }
                index += 1;
                output_time += duration;
                emit(crate::export_page::Rendered {
                    pixels: Arc::new(rgba),
                    dims: [f.width as u32, f.height as u32],
                    source_pts,
                    output_time,
                    count: index,
                    frame_duration: duration,
                })
                .map_err(anyhow::Error::msg)?;
            }
            Ok(())
        };
        // A rounded nominal duration cannot join independently rounded PTS values:
        // 24 fps alternates between 41,666 and 41,667 microseconds. Look ahead
        // once, as the Process preview does, so every input ends at the next PTS.
        let read_next = || -> Result<Option<(f64, crate::player::Rgba)>> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                cancel.check().map_err(anyhow::Error::msg)?;
                if let Some(frame) = player.take_next() {
                    return Ok(Some(frame));
                }
                ensure!(!player.failed(), "Export decoder failed");
                if player.exhausted() {
                    return Ok(None);
                }
                ensure!(
                    std::time::Instant::now() < deadline,
                    "Export decoder timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        let nominal = (1e6 / info.fps).round().max(1.) as i64;
        let mut next = read_next()?;
        while let Some((pts, rgba)) = next.take() {
            cancel.check().map_err(anyhow::Error::msg)?;
            if pts >= end - 1e-6 {
                break;
            }
            next = read_next()?;
            let timestamp = (pts * 1e6).round() as i64;
            let duration = next.as_ref().map_or(nominal, |(next_pts, _)| {
                (next_pts * 1e6).round() as i64 - timestamp
            });
            ensure!(
                duration > 0 && (duration - nominal).abs() < 2000,
                "Export currently requires constant-rate input"
            );
            let pixels = (cy..cy + ch)
                .flat_map(|y| {
                    let start = ((y * info.width + cx) * 4) as usize;
                    rgba[start..start + (cw * 4) as usize]
                        .chunks_exact(4)
                        .flat_map(|p| {
                            [
                                u16::from(p[0]) * 257,
                                u16::from(p[1]) * 257,
                                u16::from(p[2]) * 257,
                            ]
                        })
                })
                .collect();
            let mut f = Frame {
                width: cw as usize,
                height: ch as usize,
                pixels,
                pts: timestamp,
                duration,
                time_base: [1, 1_000_000],
            };
            crate::spatial::apply_crop(&mut f,&inputs.crops);
            accept(
                crate::processor_gui::process_chain(
                    &mut sessions,
                    0,
                    vec![f],
                    cancel,
                    512 * 1024 * 1024,
                )
                .map_err(anyhow::Error::msg)?,
            )?;
        }
        for i in 0..sessions.len() {
            let tail = sessions[i].finish().map_err(anyhow::Error::msg)?;
            accept(
                crate::processor_gui::process_chain(
                    &mut sessions,
                    i + 1,
                    tail,
                    cancel,
                    512 * 1024 * 1024,
                )
                .map_err(anyhow::Error::msg)?,
            )?;
        }
    }
    cancel.check().map_err(anyhow::Error::msg)?;
    let e = encoder
        .as_mut()
        .context("Pipeline returned no output frames")?;
    for packet in e.flush()? {
        write_packet_scaled(
            &mut mux,
            track.unwrap(),
            e.scale(),
            &packet.data,
            packet.duration as f64 / e.scale() as f64,
            packet.composition_offset as f64 / e.scale() as f64,
            packet.key,
        )?;
    }
    let source = crate::fc::open(&info.path).map_err(anyhow::Error::msg)?;
    // The existing audio path preserves kept-range timing and encodes AAC for MP4/MKV.
    audio_encode_checked(&source, &mut mux, keeps, false, || {
        cancel.check().map_err(anyhow::Error::msg)
    })?;
    cancel.check().map_err(anyhow::Error::msg)?;
    mux.finish()?;
    let mkv = dest
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mkv"));
    add_mp4_chapters(&temp.0, chapters, !mkv)?;
    if mkv {
        let remuxed = pending(dest)?;
        remux_mkv(&temp.0, &remuxed.0, chapters)?;
        cancel.check().map_err(anyhow::Error::msg)?;
        std::fs::hard_link(&remuxed.0, dest)?;
    } else {
        std::fs::hard_link(&temp.0, dest)?;
    }
    Ok(())
}

enum PipelineEncoder {
    ProRes {
        encoder: filmcraft_prores::Encoder,
        dims: [u32; 2],
        duration: u32,
    },
    #[cfg(target_os = "macos")]
    Hevc {
        encoder: Box<dyn filmcraft_export::VideoEncoder>,
        dims: [u32; 2],
    },
}
impl PipelineEncoder {
    fn new(w: u32, h: u32, duration: f64, prores: bool) -> Result<Self> {
        ensure!(
            w.is_multiple_of(2) && h.is_multiple_of(2),
            "Export requires even output dimensions"
        );
        if prores {
            return Ok(Self::ProRes {
                encoder: filmcraft_prores::Encoder::new(filmcraft_prores::Profile::Standard, w, h),
                dims: [w, h],
                duration: (duration * SCALE as f64).round() as u32,
            });
        }
        #[cfg(target_os = "macos")]
        {
            let settings = filmcraft_export::ExportSettings {
                format: filmcraft_export::Format::Hevc,
                bitrate_kbps: 35000,
                ..Default::default()
            };
            let encoder = filmcraft_platform::hardware_encode::videotoolbox_encoder_factory(
                filmcraft_export::Format::Hevc,
                w,
                h,
                filmcraft_time::FrameRate::new(1_000_000, (duration * 1e6).round() as i64),
                &settings,
            )
            .context("HEVC encoder unavailable on this machine")??;
            Ok(Self::Hevc {
                encoder,
                dims: [w, h],
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            anyhow::bail!("HEVC export requires macOS VideoToolbox")
        }
    }
    fn dims(&self) -> [u32; 2] {
        match self {
            Self::ProRes { dims, .. } => *dims,
            #[cfg(target_os = "macos")]
            Self::Hevc { dims, .. } => *dims,
        }
    }
    fn scale(&self) -> u32 {
        match self {
            Self::ProRes { .. } => SCALE,
            #[cfg(target_os = "macos")]
            Self::Hevc { encoder, .. } => encoder.timescale(),
        }
    }
    fn entry(&self) -> SampleEntry {
        match self {
            Self::ProRes { dims, .. } => {
                SampleEntry::prores(mp4::FourCc(*b"apcn"), dims[0] as u16, dims[1] as u16)
            }
            #[cfg(target_os = "macos")]
            Self::Hevc { encoder, .. } => encoder.sample_entry(),
        }
    }
    fn encode(&mut self, rgba: &[u8], index: u64) -> Result<Vec<filmcraft_export::EncodedPacket>> {
        match self {
            Self::ProRes {
                encoder,
                dims,
                duration,
            } => {
                let [w, h] = *dims;
                let mut p = filmcraft_prores::Frame::new(
                    w,
                    h,
                    filmcraft_prores::ChromaFormat::Yuv422,
                    10,
                    false,
                );
                filmcraft_export::rgba_to_yuv422_10(
                    rgba, w as usize, h as usize, &mut p.y, &mut p.cb, &mut p.cr,
                );
                Ok(vec![filmcraft_export::EncodedPacket {
                    data: encoder.encode(&p)?,
                    duration: *duration,
                    composition_offset: 0,
                    key: true,
                }])
            }
            #[cfg(target_os = "macos")]
            Self::Hevc { encoder, dims } => {
                Ok(encoder.encode(&filmcraft_export::EncoderFrame {
                    width: dims[0],
                    height: dims[1],
                    rgba,
                    hdr: None,
                    index,
                })?)
            }
        }
    }
    fn flush(&mut self) -> Result<Vec<filmcraft_export::EncodedPacket>> {
        match self {
            Self::ProRes { .. } => Ok(vec![]),
            #[cfg(target_os = "macos")]
            Self::Hevc { encoder, .. } => Ok(encoder.flush()?),
        }
    }
}
