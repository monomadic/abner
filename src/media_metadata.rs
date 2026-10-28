//! Filmcraft metadata shared by startup and preview validation. No media subprocesses.
use crate::probe::VideoInfo;
use filmcraft_isobmff::{CodecConfig, ColorInfo};
use std::path::Path;
#[derive(Debug, Clone)]
struct Declared {
    codes: [u16; 3],
    hdr: bool,
    icc: bool,
}
impl Default for Declared {
    fn default() -> Self {
        Self {
            codes: [2; 3],
            hdr: false,
            icc: false,
        }
    }
}
impl Declared {
    fn overlay(&mut self, codes: [u16; 3]) {
        for (dst, src) in self.codes.iter_mut().zip(codes) {
            if src != 2 {
                *dst = src;
            }
        }
    }
}
pub(crate) struct Metadata {
    pub info: VideoInfo,
    declared: Declared,
}
impl Metadata {
    pub(crate) fn check_sdr(&self) -> Result<(), String> {
        if self.declared.hdr || self.declared.icc {
            return Err(
                "Preview supports SDR Rec.709 only; HDR/ICC metadata requires a color transform"
                    .into(),
            );
        }
        for (name, value) in ["color_primaries", "color_transfer", "color_space"]
            .into_iter()
            .zip(self.declared.codes)
        {
            if !matches!(value, 1 | 2) {
                let label = match (name, value) {
                    ("color_primaries", 9) => "BT.2020 primaries".into(),
                    ("color_primaries", 12) => "Display P3 primaries".into(),
                    ("color_transfer", 16) => "HDR PQ".into(),
                    ("color_transfer", 18) => "HDR HLG".into(),
                    ("color_space", 5 | 6) => "BT.601 matrix".into(),
                    ("color_space", 9) => "BT.2020 matrix".into(),
                    _ => format!("unsupported {name} tag {value}"),
                };
                return Err(format!(
                    "Preview supports SDR Rec.709 only; the file declares {label}"
                ));
            }
        }
        Ok(())
    }
}
fn yuv(chroma: u32, bits: u32) -> String {
    let p = match chroma {
        0 => "gray",
        1 => "yuv420p",
        2 => "yuv422p",
        _ => "yuv444p",
    };
    if bits == 8 {
        p.into()
    } else {
        format!("{p}{bits}le")
    }
}
fn config(c: &CodecConfig) -> (String, Declared) {
    let mut color = Declared::default();
    let mut pix = String::new();
    match c {
        CodecConfig::Avc(a) => {
            if let Some(n) = a.sps.first().and_then(|n| n.get(1..))
                && let Ok(s) =
                    filmcraft_h264::params::Sps::parse(&filmcraft_bitstream::unescape_rbsp(n))
            {
                pix = yuv(s.chroma_format_idc, s.bit_depth_luma);
                if let Some(v) = s.vui {
                    color.overlay([
                        v.colour_primaries.into(),
                        v.transfer_characteristics.into(),
                        v.matrix_coefficients.into(),
                    ]);
                }
            }
        }
        CodecConfig::Hevc(a) => {
            pix = yuv(a.chroma_format_idc.into(), a.bit_depth_luma.into());
            if let Some(n) = a
                .arrays
                .iter()
                .flat_map(|a| &a.nalus)
                .find(|n| n.first().is_some_and(|b| (b >> 1) & 63 == 33))
                .and_then(|n| n.get(2..))
                && let Ok(s) =
                    filmcraft_hevc::params::Sps::parse(&filmcraft_bitstream::unescape_rbsp(n))
                && let Some(v) = s.vui
            {
                color.overlay([
                    v.colour_primaries.into(),
                    v.transfer_characteristics.into(),
                    v.matrix_coefficients.into(),
                ]);
            }
        }
        CodecConfig::Vp9(v) => {
            pix = yuv(
                match v.chroma_subsampling {
                    2 => 2,
                    3 => 3,
                    _ => 1,
                },
                u32::from(v.bit_depth.max(8)),
            );
            color.overlay([
                v.colour_primaries.into(),
                v.transfer_characteristics.into(),
                v.matrix_coefficients.into(),
            ]);
        }
        CodecConfig::Av1(v) => {
            pix = yuv(
                if v.monochrome {
                    0
                } else if !v.chroma_subsampling_x {
                    3
                } else if !v.chroma_subsampling_y {
                    2
                } else {
                    1
                },
                if v.twelve_bit {
                    12
                } else if v.high_bitdepth {
                    10
                } else {
                    8
                },
            );
            let mut data = v.config_obus.as_slice();
            while let Some((&head, rest)) = data.split_first() {
                data = rest;
                if head & 4 != 0 {
                    let Some(rest) = data.get(1..) else { break };
                    data = rest;
                }
                let mut size = 0usize;
                let mut shift = 0;
                let sized = head & 2 != 0;
                if sized {
                    loop {
                        let Some((&b, rest)) = data.split_first() else {
                            return (pix, color);
                        };
                        data = rest;
                        if shift >= usize::BITS as usize - 7 {
                            return (pix, color);
                        }
                        size |= usize::from(b & 127) << shift;
                        if b & 128 == 0 {
                            break;
                        }
                        shift += 7;
                    }
                } else {
                    size = data.len();
                }
                let Some(payload) = data.get(..size) else {
                    break;
                };
                if (head >> 3) & 15 == 1 {
                    if let Ok(s) = filmcraft_av1::SequenceHeader::parse(payload) {
                        let v = s.color;
                        color.overlay([
                            v.color_primaries.into(),
                            v.transfer_characteristics.into(),
                            v.matrix_coefficients.into(),
                        ]);
                    }
                    break;
                }
                data = &data[size..];
                if !sized {
                    break;
                }
            }
        }
        _ => {}
    }
    (pix, color)
}
fn canonical_codec(label: &str) -> String {
    let s = label.to_ascii_lowercase();
    for (need, name) in [
        ("h.264", "h264"),
        ("h264", "h264"),
        ("avc", "h264"),
        ("hevc", "hevc"),
        ("h.265", "hevc"),
        ("prores", "prores"),
        ("dnx", "dnxhd"),
        ("vc-3", "dnxhd"),
        ("av1", "av1"),
        ("vp9", "vp9"),
        ("vp8", "vp8"),
        ("mpeg-2", "mpeg2video"),
        ("mjpeg", "mjpeg"),
        ("motion jpeg", "mjpeg"),
    ] {
        if s.contains(need) {
            return name.into();
        }
    }
    s
}
fn pix_label(label: &str) -> String {
    let chroma = if label.contains("4:2:0") {
        1
    } else if label.contains("4:2:2") {
        2
    } else if label.contains("4:4:4") {
        3
    } else {
        return "?".into();
    };
    let bits = label
        .split_whitespace()
        .find_map(|s| s.strip_suffix("-bit")?.parse::<u32>().ok())
        .unwrap_or(8);
    yuv(chroma, bits)
}
fn rotation(q: Option<u8>) -> Option<f64> {
    match q? {
        0 => None,
        1 => Some(-90.),
        2 => Some(-180.),
        _ => Some(90.),
    }
}
// Common containers are header-only: no decoder construction or frame read on the UI's probe path.
fn headers(path: &Path, size: u64) -> Result<Option<Metadata>, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let make = |w, h, fps, duration, codec, pix_fmt, bit_rate, rotation, declared| Metadata {
        info: VideoInfo {
            path: path.to_owned(),
            width: w,
            height: h,
            fps,
            duration,
            codec,
            pix_fmt,
            bit_rate,
            file_size: size,
            rotation,
        },
        declared,
    };
    match ext.as_str() {
        "mp4" | "m4v" | "mov" | "3gp" => {
            let f = filmcraft_isobmff::open(crate::fc::reader(path)?).map_err(|e| e.to_string())?;
            let t = f
                .tracks
                .iter()
                .find(|t| t.kind == filmcraft_isobmff::TrackKind::Video)
                .ok_or("no video track")?;
            let e = t.entries.first().ok_or("missing video sample entry")?;
            let v = e.video.as_ref().ok_or("missing video dimensions")?;
            if v.clean_aperture.is_some() {
                return Ok(None);
            }
            if t.duration == 0
                || t.timescale == 0
                || t.samples.is_empty()
                || v.width == 0
                || v.height == 0
            {
                return Err("invalid video metadata".into());
            }
            let (mut pix, mut color) = config(&e.codec);
            if let CodecConfig::ProRes { fourcc } = &e.codec {
                pix = if fourcc.0[2] == b'4' {
                    "yuv444p12le"
                } else {
                    "yuv422p10le"
                }
                .into();
            }
            color.hdr = v.mastering_display.is_some() || v.content_light.is_some();
            match &v.color {
                Some(ColorInfo::Nclc {
                    primaries,
                    transfer,
                    matrix,
                })
                | Some(ColorInfo::Nclx {
                    primaries,
                    transfer,
                    matrix,
                    ..
                }) => color.overlay([*primaries, *transfer, *matrix]),
                Some(ColorInfo::Icc { .. }) => color.icc = true,
                None => {}
            }
            let (w, h) = if t.display_rotation().is_some_and(|q| q % 2 == 1) {
                (v.height.into(), v.width.into())
            } else {
                (v.width.into(), v.height.into())
            };
            let duration = if f.timescale > 0 && t.tkhd_duration > 0 {
                t.tkhd_duration as f64 / f.timescale as f64
            } else {
                t.duration as f64 / t.timescale as f64
            };
            let bitrate = (t.samples.iter().map(|s| u64::from(s.size)).sum::<u64>() as f64
                * 8.
                * t.timescale as f64
                / t.duration as f64) as u64;
            Ok(Some(make(
                w,
                h,
                t.samples.len() as f64 * t.timescale as f64 / t.duration as f64,
                duration,
                canonical_codec(e.codec.name()),
                if pix.is_empty() { "?".into() } else { pix },
                Some(bitrate),
                rotation(t.display_rotation()),
                color,
            )))
        }
        "mkv" | "webm" => {
            use filmcraft_matroska::{Codec, OpenOptions, TrackKind};
            let f = filmcraft_matroska::open_with(
                crate::fc::reader(path)?,
                &OpenOptions {
                    index: false,
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())?;
            let t = f
                .tracks
                .iter()
                .find(|t| t.kind == TrackKind::Video)
                .ok_or("no video track")?;
            let v = t.video.as_ref().ok_or("missing video dimensions")?;
            let (Some(duration), Some(ns)) = (f.info.duration, t.default_duration_ns) else {
                return Ok(None);
            };
            if ns == 0
                || duration <= 0.
                || !duration.is_finite()
                || v.pixel_width == 0
                || v.pixel_height == 0
            {
                return Err("invalid Matroska video metadata".into());
            }
            let (c, label) = match &t.codec {
                Codec::Avc { avcc } => (
                    Some(CodecConfig::Avc(
                        filmcraft_isobmff::AvcConfig::parse(avcc).map_err(|e| e.to_string())?,
                    )),
                    "h264",
                ),
                Codec::Hevc { hvcc } => (
                    Some(CodecConfig::Hevc(
                        filmcraft_isobmff::HevcConfig::parse(hvcc).map_err(|e| e.to_string())?,
                    )),
                    "hevc",
                ),
                Codec::Av1 { av1c } => (
                    Some(CodecConfig::Av1(
                        filmcraft_isobmff::Av1Config::parse(av1c).map_err(|e| e.to_string())?,
                    )),
                    "av1",
                ),
                Codec::Vp9 { .. } => (None, "vp9"),
                Codec::Vp8 => (None, "vp8"),
                Codec::ProRes { .. } => (None, "prores"),
                Codec::Mjpeg => (None, "mjpeg"),
                _ => return Ok(None),
            };
            let (pix, mut color) = c.as_ref().map(config).unwrap_or_default();
            if let Some(c) = &v.colour {
                color.overlay([
                    c.primaries.unwrap_or(2) as u16,
                    c.transfer_characteristics.unwrap_or(2) as u16,
                    c.matrix_coefficients.unwrap_or(2) as u16,
                ]);
                color.hdr = c.mastering.is_some() || c.max_cll.is_some() || c.max_fall.is_some();
            }
            let (w, h) = if v.display_rotation().is_some_and(|q| q % 2 == 1) {
                (v.pixel_height, v.pixel_width)
            } else {
                (v.pixel_width, v.pixel_height)
            };
            let seconds = duration * f.info.timestamp_scale as f64 / 1e9;
            Ok(Some(make(
                w,
                h,
                1e9 / ns as f64,
                seconds,
                label.into(),
                if pix.is_empty() { "?".into() } else { pix },
                Some((size as f64 * 8. / seconds) as u64),
                rotation(v.display_rotation()),
                color,
            )))
        }
        _ => Ok(None),
    }
}
pub(crate) fn read(path: &Path) -> Result<Metadata, String> {
    let stat = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !stat.is_file() {
        return Err("source is not a regular file".into());
    }
    if let Some(m) = headers(path, stat.len())? {
        return Ok(m);
    }
    let src = crate::fc::open(path)?;
    let info = src.info();
    let video = info.video.as_ref().ok_or("no video stream")?;
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    let fps = video.frame_rate.num as f64 / video.frame_rate.den as f64;
    if video.width == 0 || video.height == 0 || !fps.is_finite() || fps <= 0. {
        return Err("invalid video geometry/rate".into());
    }
    let mut result = Metadata {
        info: VideoInfo {
            path: path.to_owned(),
            width: video.width,
            height: video.height,
            fps,
            duration: info.duration.seconds().max(0.),
            codec: canonical_codec(&video.codec),
            pix_fmt: pix_label(&video.pixel_format),
            bit_rate: video.bitrate,
            file_size: size,
            rotation: None,
        },
        declared: Declared {
            hdr: video.hdr.is_some(),
            ..Default::default()
        },
    };
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" | "mov" | "3gp" => {
            let file =
                filmcraft_isobmff::open(crate::fc::reader(path)?).map_err(|e| e.to_string())?;
            let t = file
                .tracks
                .iter()
                .find(|t| t.kind == filmcraft_isobmff::TrackKind::Video)
                .ok_or("no video track")?;
            result.info.rotation = rotation(t.display_rotation());
            if t.duration > 0 && t.timescale > 0 {
                result.info.fps = t.samples.len() as f64 * t.timescale as f64 / t.duration as f64;
            }
            if file.timescale > 0 {
                result.info.duration = t.tkhd_duration as f64 / file.timescale as f64;
            }
            if let Some(e) = t.entries.first() {
                let (pix, color) = config(&e.codec);
                if !pix.is_empty() {
                    result.info.pix_fmt = pix;
                }
                result.declared.codes = color.codes;
                if let Some(v) = &e.video {
                    result.declared.hdr |=
                        v.mastering_display.is_some() || v.content_light.is_some();
                    match &v.color {
                        Some(ColorInfo::Nclc {
                            primaries,
                            transfer,
                            matrix,
                        })
                        | Some(ColorInfo::Nclx {
                            primaries,
                            transfer,
                            matrix,
                            ..
                        }) => result.declared.overlay([*primaries, *transfer, *matrix]),
                        Some(ColorInfo::Icc { .. }) => result.declared.icc = true,
                        None => {}
                    }
                }
            }
        }
        "mkv" | "webm" => {
            use filmcraft_matroska::{Codec, OpenOptions, TrackKind};
            let f = filmcraft_matroska::open_with(
                crate::fc::reader(path)?,
                &OpenOptions {
                    index: false,
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())?;
            let t = f
                .tracks
                .iter()
                .find(|t| t.kind == TrackKind::Video)
                .ok_or("no video track")?;
            if let Some(v) = &t.video {
                result.info.rotation = rotation(v.display_rotation());
                if let Some(c) = &v.colour {
                    result.declared.overlay([
                        c.primaries.unwrap_or(2) as u16,
                        c.transfer_characteristics.unwrap_or(2) as u16,
                        c.matrix_coefficients.unwrap_or(2) as u16,
                    ]);
                    result.declared.hdr |=
                        c.mastering.is_some() || c.max_cll.is_some() || c.max_fall.is_some();
                }
            }
            let c = match &t.codec {
                Codec::Avc { avcc } => filmcraft_isobmff::AvcConfig::parse(avcc)
                    .ok()
                    .map(CodecConfig::Avc),
                Codec::Hevc { hvcc } => filmcraft_isobmff::HevcConfig::parse(hvcc)
                    .ok()
                    .map(CodecConfig::Hevc),
                Codec::Av1 { av1c } => filmcraft_isobmff::Av1Config::parse(av1c)
                    .ok()
                    .map(CodecConfig::Av1),
                _ => None,
            };
            if let Some(c) = c {
                let (pix, color) = config(&c);
                if !pix.is_empty() {
                    result.info.pix_fmt = pix;
                }
                let container = result.declared.codes;
                result.declared.codes = color.codes;
                result.declared.overlay(container);
            }
            result.info.bit_rate = (result.info.duration > 0.)
                .then(|| (size as f64 * 8. / result.info.duration) as u64);
        }
        _ => {
            use filmcraft_color::{Matrix, Primaries, Transfer};
            result.declared.overlay([
                match video.color.primaries {
                    Primaries::Bt709 => 1,
                    Primaries::Bt601_625 => 5,
                    Primaries::Bt601_525 => 6,
                    Primaries::Bt2020 => 9,
                    Primaries::P3D65 => 12,
                },
                match video.color.transfer {
                    Transfer::Bt709 => 1,
                    Transfer::Srgb => 13,
                    Transfer::Linear => 8,
                    Transfer::Pq => 16,
                    Transfer::Hlg => 18,
                },
                match video.color.matrix {
                    Matrix::Bt709 => 1,
                    Matrix::Bt601 => 6,
                    Matrix::Bt2020Ncl => 9,
                },
            ]);
        }
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    fn fixture(name: &str, extra: &[&str]) -> Option<std::path::PathBuf> {
        if !Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return None;
        }
        let path = std::env::temp_dir().join(format!(
            "abner-filmcraft-meta-{}-{name}.mp4",
            std::process::id()
        ));
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x96:rate=25:duration=1",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-pix_fmt",
                "yuv420p",
            ])
            .args(extra)
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        Some(path)
    }
    #[test]
    fn unknown_color_is_not_inferred_sd_matrix_and_hdr_is_rejected() {
        for (name, tags, allowed) in [
            ("unknown", vec![], true),
            (
                "709",
                vec![
                    "-colorspace",
                    "bt709",
                    "-color_trc",
                    "bt709",
                    "-color_primaries",
                    "bt709",
                ],
                true,
            ),
            (
                "601",
                vec![
                    "-colorspace",
                    "smpte170m",
                    "-color_trc",
                    "smpte170m",
                    "-color_primaries",
                    "smpte170m",
                ],
                false,
            ),
            (
                "hdr",
                vec![
                    "-colorspace",
                    "bt2020nc",
                    "-color_trc",
                    "smpte2084",
                    "-color_primaries",
                    "bt2020",
                ],
                false,
            ),
        ] {
            let Some(path) = fixture(name, &tags) else {
                return;
            };
            let meta = read(&path).unwrap();
            assert_eq!((meta.info.width, meta.info.height), (160, 96));
            assert_eq!(meta.info.codec, "h264");
            assert_eq!(meta.info.pix_fmt, "yuv420p");
            assert_eq!(
                meta.check_sdr().is_ok(),
                allowed,
                "{name}: {:?}",
                meta.declared
            );
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn mp4_average_rate_and_rotation_follow_presentation_metadata() {
        let Some(path) = fixture(
            "vfr",
            &["-vf", "select=not(eq(mod(n\\,4)\\,3))", "-fps_mode", "vfr"],
        ) else {
            return;
        };
        let ours = crate::probe::probe(&path).unwrap();
        let raw = Command::new("ffprobe")
            .args(["-v", "error", "-show_streams", "-of", "json"])
            .arg(&path)
            .output()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&raw.stdout).unwrap();
        let rate = json["streams"][0]["avg_frame_rate"].as_str().unwrap();
        let (n, d) = rate.split_once('/').unwrap();
        let expected = n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap();
        assert!((ours.fps - expected).abs() < 1e-6);
        let rotated = path.with_extension("rotated.mov");
        assert!(
            Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-display_rotation", "90", "-i"])
                .arg(&path)
                .args(["-c", "copy"])
                .arg(&rotated)
                .status()
                .unwrap()
                .success()
        );
        let info = crate::probe::probe(&rotated).unwrap();
        assert_eq!((info.width, info.height), (96, 160));
        assert!(info.rotation.is_some());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(rotated).unwrap();
    }
}
#[cfg(test)]
mod header_only_tests {
    use super::{has_subtitles, timeline};
    #[test]
    fn valid_headers_do_not_require_decoding_the_media_payload() {
        use filmcraft_isobmff::{
            Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions,
        };
        let path =
            std::env::temp_dir().join(format!("abner-native-header-{}.mov", std::process::id()));
        let mut mux = Mp4Writer::new(
            std::fs::File::create(&path).unwrap(),
            WriterOptions::new(Brand::Mov),
        )
        .unwrap();
        let t = mux
            .add_track(TrackConfig::new(
                SampleEntry::prores(FourCc(*b"apch"), 160, 96),
                25,
            ))
            .unwrap();
        mux.write_sample(
            t,
            WriteSample {
                data: b"not a decodable frame",
                duration: 1,
                composition_offset: 0,
                is_sync: true,
            },
        )
        .unwrap();
        mux.finish().unwrap();
        let m = crate::probe::metadata(&path, std::time::Duration::from_secs(1)).unwrap();
        assert_eq!((m.info.width, m.info.height), (160, 96));
        assert_eq!(m.info.codec, "prores");
        assert_eq!(m.info.pix_fmt, "yuv422p10le");
        assert!(m.check_sdr().is_ok());
        let index = timeline(&path).unwrap().unwrap();
        assert_eq!(index.packets.len(), 1);
        assert!(index.chapters.is_empty());
        assert_eq!(has_subtitles(&path).unwrap(), Some(false));
        std::fs::remove_file(path).unwrap();
    }
}
#[derive(Clone)]
pub(crate) struct Packet {
    pub pts: f64,
    pub duration: f64,
    pub size: u64,
    pub key: bool,
}
pub(crate) struct Timeline {
    pub packets: Vec<Packet>,
    pub chapters: Vec<(f64, String)>,
    pub facts: serde_json::Value,
}
fn video_facts(m: &Metadata) -> serde_json::Value {
    let matrix = match m.declared.codes[2] {
        0 => "gbr",
        1 => "bt709",
        5 => "bt470bg",
        6 => "smpte170m",
        9 => "bt2020nc",
        _ => "unknown",
    };
    serde_json::json!({"codec_type":"video","codec_name":m.info.codec,"width":m.info.width,"height":m.info.height,"pix_fmt":m.info.pix_fmt,"avg_frame_rate":format!("{}/1",m.info.fps),"bit_rate":m.info.bit_rate,"color_space":matrix})
}
fn profile(codec: &CodecConfig, v: &mut serde_json::Value) {
    match codec {
        CodecConfig::Avc(a) => {
            v["level"] = a.level.into();
            v["profile"] = match a.profile {
                66 => "Baseline",
                77 => "Main",
                88 => "Extended",
                100 => "High",
                110 => "High 10",
                122 => "High 4:2:2",
                244 => "High 4:4:4 Predictive",
                _ => "Unknown",
            }
            .into();
        }
        CodecConfig::Hevc(a) => {
            v["level"] = (a.general_level_idc / 3).into();
            v["profile"] = match a.general_profile_idc {
                1 => "Main",
                2 => "Main 10",
                3 => "Main Still Picture",
                _ => "Unknown",
            }
            .into();
        }
        CodecConfig::Av1(a) => {
            v["level"] = (20 + (a.seq_level_idx_0 / 4) * 10 + a.seq_level_idx_0 % 4).into();
            v["profile"] = match a.seq_profile {
                0 => "Main",
                1 => "High",
                _ => "Professional",
            }
            .into();
        }
        CodecConfig::Vp9(a) => {
            v["profile"] = format!("Profile {}", a.profile).into();
        }
        CodecConfig::ProRes { fourcc } => {
            v["profile"] = match &fourcc.0 {
                b"apco" => "Proxy",
                b"apcs" => "LT",
                b"apcn" => "Standard",
                b"apch" => "HQ",
                b"ap4h" => "4444",
                b"ap4x" => "4444 XQ",
                _ => "Unknown",
            }
            .into();
        }
        _ => {}
    }
}
pub(crate) fn atoms(
    src: &impl filmcraft_isobmff::ByteSource,
    start: u64,
    end: u64,
) -> Result<Vec<([u8; 4], u64, u64)>, String> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        if end - pos < 8 {
            return Err("truncated metadata box".into());
        }
        let mut h = [0; 8];
        src.read_at(pos, &mut h).map_err(|e| e.to_string())?;
        let size = u32::from_be_bytes(h[..4].try_into().map_err(|_| "box size")?);
        let kind = h[4..].try_into().map_err(|_| "box type")?;
        let (size, head) = if size == 1 {
            let mut b = [0; 8];
            src.read_at(pos + 8, &mut b).map_err(|e| e.to_string())?;
            (u64::from_be_bytes(b), 16)
        } else if size == 0 {
            (end - pos, 8)
        } else {
            (u64::from(size), 8)
        };
        if size < head || size > end - pos {
            return Err("invalid metadata box extent".into());
        }
        out.push((kind, pos + head, pos + size));
        pos += size;
    }
    Ok(out)
}
fn nero_chapters(data: &[u8]) -> Result<Vec<(f64, String)>, String> {
    let version = *data.first().ok_or("empty chapter box")?;
    let mut p = if version == 0 {
        4
    } else if version == 1 {
        8
    } else {
        return Err("unknown chapter box version".into());
    };
    let count = usize::from(*data.get(p).ok_or("missing chapter count")?);
    p += 1;
    let mut out = Vec::new();
    for i in 0..count {
        let time = data.get(p..p + 8).ok_or("truncated chapter time")?;
        p += 8;
        let len = usize::from(*data.get(p).ok_or("missing chapter title length")?);
        p += 1;
        let title = data.get(p..p + len).ok_or("truncated chapter title")?;
        p += len;
        out.push((
            u64::from_be_bytes(time.try_into().map_err(|_| "chapter time")?) as f64 / 1e7,
            if title.is_empty() {
                format!("Chapter {}", i + 1)
            } else {
                String::from_utf8_lossy(title).into_owned()
            },
        ));
    }
    Ok(out)
}
fn chapter_text(text: &[u8]) -> String {
    if text.starts_with(&[0xfe, 0xff]) {
        String::from_utf16_lossy(
            &text[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_be_bytes(*b))
                .collect::<Vec<_>>(),
        )
    } else if text.starts_with(&[0xff, 0xfe]) {
        String::from_utf16_lossy(
            &text[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(text).into_owned()
    }
}
fn mp4_chapters(
    f: &filmcraft_isobmff::Mp4File,
    src: &crate::fc::FileReader,
    video: &filmcraft_isobmff::Track,
) -> Result<Vec<(f64, String)>, String> {
    let ids: Vec<_> = video
        .references
        .iter()
        .filter(|(kind, _)| kind.0 == *b"chap")
        .flat_map(|(_, ids)| ids.iter().copied())
        .collect();
    let mut out = Vec::new();
    for (ti, t) in f
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| ids.contains(&t.id))
    {
        if t.timescale == 0 {
            continue;
        }
        for (i, s) in t.samples.iter().enumerate() {
            if s.size > 65535 {
                return Err("oversized chapter title".into());
            }
            let b = f.read_sample(src, ti, i).map_err(|e| e.to_string())?;
            let Some(head) = b.get(..2) else { continue };
            let n = usize::from(u16::from_be_bytes([head[0], head[1]]));
            let Some(text) = b.get(2..2 + n) else {
                return Err("truncated chapter sample".into());
            };
            out.push((
                t.presentation_pts(i).unwrap_or(s.pts) as f64 / t.timescale as f64,
                chapter_text(text),
            ));
        }
    }
    if !out.is_empty() {
        return Ok(out);
    }
    let len = filmcraft_isobmff::ByteSource::len(src);
    for (_, a, b) in atoms(src, 0, len)?.into_iter().filter(|b| b.0 == *b"moov") {
        for (_, a, b) in atoms(src, a, b)?.into_iter().filter(|b| b.0 == *b"udta") {
            if let Some((_, a, b)) = atoms(src, a, b)?.into_iter().find(|b| b.0 == *b"chpl") {
                if b - a > 1024 * 1024 {
                    return Err("oversized chapter metadata".into());
                }
                let mut data = vec![0; (b - a) as usize];
                filmcraft_isobmff::ByteSource::read_at(src, a, &mut data)
                    .map_err(|e| e.to_string())?;
                return nero_chapters(&data);
            }
        }
    }
    Ok(out)
}
/// Native timeline packet and stream metadata for the two indexed container families.
/// `None` leaves other containers on their existing detailed-analysis compatibility path.
pub(crate) fn timeline(path: &Path) -> Result<Option<Timeline>, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "mp4" | "m4v" | "mov" | "3gp" | "mkv" | "webm") {
        return Ok(None);
    }
    let metadata = read(path)?;
    let mut streams = vec![video_facts(&metadata)];
    let (packets, mut chapters) = if matches!(ext.as_str(), "mkv" | "webm") {
        use filmcraft_matroska::{Codec, TrackKind};
        let f = filmcraft_matroska::open(crate::fc::reader(path)?).map_err(|e| e.to_string())?;
        let t = f
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .ok_or("no video track")?;
        match &t.codec {
            Codec::Avc { avcc } => {
                if let Ok(c) = filmcraft_isobmff::AvcConfig::parse(avcc) {
                    profile(&CodecConfig::Avc(c), &mut streams[0]);
                }
            }
            Codec::Hevc { hvcc } => {
                if let Ok(c) = filmcraft_isobmff::HevcConfig::parse(hvcc) {
                    profile(&CodecConfig::Hevc(c), &mut streams[0]);
                }
            }
            Codec::Av1 { av1c } => {
                if let Ok(c) = filmcraft_isobmff::Av1Config::parse(av1c) {
                    profile(&CodecConfig::Av1(c), &mut streams[0]);
                }
            }
            _ => {}
        }
        let scale = t.timebase.0 as f64 / t.timebase.1.max(1) as f64;
        let duration = t
            .default_duration_ns
            .map(|n| n as f64 / 1e9)
            .unwrap_or(1. / metadata.info.fps);
        let p = t
            .samples
            .iter()
            .map(|s| Packet {
                pts: s.pts as f64 * scale,
                duration: if s.duration > 0 {
                    s.duration as f64 * scale
                } else {
                    duration
                },
                size: s.size.into(),
                key: s.keyframe,
            })
            .collect();
        for t in &f.tracks {
            match t.kind{
            TrackKind::Audio=>{if let Some(a)=&t.audio{let name=match &t.codec{Codec::Aac{..}=>"aac",Codec::Opus{..}=>"opus",Codec::Vorbis{..}=>"vorbis",Codec::Flac{..}=>"flac",Codec::Pcm{..}=>"pcm",Codec::Mp3=>"mp3",Codec::Ac3=>"ac3",Codec::Eac3=>"eac3",_=>&t.codec_id};streams.push(serde_json::json!({"codec_type":"audio","codec_name":name,"channels":a.channels,"sample_rate":a.output_sampling_frequency.unwrap_or(a.sampling_frequency)}));}}
            TrackKind::Subtitle=>streams.push(serde_json::json!({"codec_type":"subtitle","codec_name":match &t.codec{Codec::SubRip=>"subrip",Codec::Ass{..}=>"ass",Codec::WebVtt=>"webvtt",_=>&t.codec_id}})),_=>{}
        }
        }
        fn add(c: &filmcraft_matroska::Chapter, out: &mut Vec<(f64, String)>) {
            if c.enabled && !c.hidden {
                out.push((
                    c.start_ns as f64 / 1e9,
                    c.title().unwrap_or("Chapter").into(),
                ));
            }
            for c in &c.children {
                add(c, out);
            }
        }
        let mut chapters = Vec::new();
        if let Some(e) = f
            .chapters
            .iter()
            .find(|e| e.default && !e.hidden)
            .or_else(|| f.chapters.iter().find(|e| !e.hidden))
        {
            for c in &e.chapters {
                add(c, &mut chapters);
            }
        }
        (p, chapters)
    } else {
        use filmcraft_isobmff::TrackKind;
        let src = crate::fc::reader(path)?;
        let f = filmcraft_isobmff::open(&src).map_err(|e| e.to_string())?;
        let t = f
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .ok_or("no video track")?;
        if let Some(e) = t.entries.first() {
            profile(&e.codec, &mut streams[0]);
        }
        let p = t
            .samples
            .iter()
            .enumerate()
            .map(|(i, s)| Packet {
                pts: t.presentation_pts(i).unwrap_or(s.pts) as f64 / t.timescale.max(1) as f64,
                duration: s.duration as f64 / t.timescale.max(1) as f64,
                size: s.size.into(),
                key: s.is_sync,
            })
            .collect();
        for t in &f.tracks {
            if t.kind == TrackKind::Audio {
                if let Some(e) = t.entries.first()
                    && let Some(a) = &e.audio
                {
                    let mut v = serde_json::json!({"codec_type":"audio","codec_name":e.codec.name().to_ascii_lowercase(),"channels":a.channels,"sample_rate":a.sample_rate});
                    if let CodecConfig::Aac(c) = &e.codec {
                        v["codec_name"] = "aac".into();
                        if c.object_type == 2 {
                            v["profile"] = "LC".into();
                        }
                    }
                    streams.push(v);
                }
            } else if t.kind == TrackKind::Subtitle {
                streams.push(serde_json::json!({"codec_type":"subtitle","codec_name":if t.entries.first().is_some_and(|e|matches!(&e.format.0,b"text"|b"tx3g")){"mov_text"}else{"subtitle"}}));
            }
        }
        (p, mp4_chapters(&f, &src, t)?)
    };
    chapters.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(Some(Timeline {
        packets,
        chapters,
        facts: serde_json::json!({"streams":streams,"format":{"bit_rate":metadata.info.bit_rate}}),
    }))
}
#[cfg(test)]
mod timeline_tests {
    use super::*;
    use std::process::Command;
    #[test]
    fn native_packets_and_chapters_match_reference_for_mp4_and_mkv() {
        if !Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("abner-native-timeline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("source.mp4");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=160x96:rate=30000/1001:duration=1",
                    "-c:v",
                    "libx264",
                    "-g",
                    "10",
                    "-bf",
                    "2",
                    "-sc_threshold",
                    "0",
                    "-threads",
                    "1"
                ])
                .arg(&src)
                .status()
                .unwrap()
                .success()
        );
        let labels = dir.join("chapters.txt");
        std::fs::write(&labels,";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=400\ntitle=First\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=400\nEND=1000\ntitle=Second\n").unwrap();
        for ext in ["mp4", "mkv"] {
            let path = dir.join(format!("chaptered.{ext}"));
            assert!(
                Command::new("ffmpeg")
                    .args(["-y", "-v", "error", "-i"])
                    .arg(&src)
                    .args(["-f", "ffmetadata", "-i"])
                    .arg(&labels)
                    .args(["-map", "0:v:0", "-map_chapters", "1", "-c", "copy"])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            let native = timeline(&path).unwrap().unwrap();
            let reference = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_packets",
                    "-show_chapters",
                    "-of",
                    "json",
                ])
                .arg(&path)
                .output()
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&reference.stdout).unwrap();
            let packets = json["packets"].as_array().unwrap();
            assert_eq!(native.packets.len(), packets.len());
            for (a, b) in native.packets.iter().zip(packets) {
                let pts = b["pts_time"].as_str().unwrap().parse::<f64>().unwrap();
                assert!((a.pts - pts).abs() < 2e-6, "{ext}: {} vs {pts}", a.pts);
                assert_eq!(a.size, b["size"].as_str().unwrap().parse::<u64>().unwrap());
                assert_eq!(a.key, b["flags"].as_str().unwrap().contains('K'));
            }
            assert_eq!(native.chapters.len(), 2, "{ext}");
            assert_eq!(native.chapters[0].1, "First");
            assert_eq!(native.chapters[1].1, "Second");
            assert!((native.chapters[1].0 - 0.4).abs() < 1e-6);
            assert_eq!(native.facts["streams"][0]["codec_name"], "h264");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn bounded_chapter_parser_rejects_truncation() {
        assert_eq!(
            chapter_text(&[0xfe, 0xff, 0, 65, 0xd8, 0x3d, 0xde, 0]),
            "A😀"
        );
        assert!(nero_chapters(&[]).is_err());
        assert!(nero_chapters(&[1, 0, 0, 0, 0, 0, 0, 0, 1]).is_err());
    }
}
#[cfg(test)]
pub(crate) fn has_subtitles(path: &Path) -> Result<Option<bool>, String> {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "mov" | "m4v" | "3gp" => {
            let f = filmcraft_isobmff::open(crate::fc::reader(path)?).map_err(|e| e.to_string())?;
            let chapter_ids: Vec<_> = f
                .tracks
                .iter()
                .flat_map(|t| &t.references)
                .filter(|(k, _)| k.0 == *b"chap")
                .flat_map(|(_, ids)| ids)
                .collect();
            Ok(Some(f.tracks.iter().any(|t| {
                t.kind == filmcraft_isobmff::TrackKind::Subtitle && !chapter_ids.contains(&&t.id)
            })))
        }
        "mkv" | "webm" => {
            let f = filmcraft_matroska::open_with(
                crate::fc::reader(path)?,
                &filmcraft_matroska::OpenOptions {
                    index: false,
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(Some(f.tracks.iter().any(|t| {
                t.kind == filmcraft_matroska::TrackKind::Subtitle
            })))
        }
        _ => Ok(None),
    }
}
