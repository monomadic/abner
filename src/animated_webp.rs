//! Animated WebP on the same media-source interface as the video decoders.
use std::{
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::{
    FrameRequest, MediaError, MediaInfo, MediaKind, MediaSource, VideoStreamInfo,
};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use image_webp::{ColorType, WebPDecoder, WebPEncoder};

pub(crate) fn is_webp(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("webp"))
}

const TICKS_PER_MS: i64 = TICKS_PER_SECOND / 1000;

pub(crate) struct Animation {
    info: MediaInfo,
    pub starts_ms: Arc<[u64]>,
    pub duration_ms: u64,
    frames: Vec<FrameChunk>,
    dims: [u32; 2],
    decoder: Mutex<Decode>,
}

struct Decode {
    file: BufReader<File>,
    next: usize,
    pixels: Vec<u8>,
    last: Option<Arc<VideoFrame>>,
}

struct FrameChunk {
    offset: u64,
    len: usize,
    rect: [u32; 4],
    replace: bool,
    dispose: bool,
}

const MAX_DECODE_BYTES: usize = 256 * 1024 * 1024;

fn u24(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

impl Animation {
    pub(crate) fn timings(&self) -> impl Iterator<Item = (f64, f64)> + '_ {
        self.starts_ms
            .iter()
            .copied()
            .zip(
                self.starts_ms
                    .iter()
                    .copied()
                    .skip(1)
                    .chain(std::iter::once(self.duration_ms)),
            )
            .map(|(start, end)| (start as f64 / 1000., (end - start) as f64 / 1000.))
    }

    pub(crate) fn at(&self, secs: f64) -> filmcraft_media::Result<Arc<VideoFrame>> {
        self.video_frame(FrameRequest::full(Tick(
            (secs * TICKS_PER_SECOND as f64).round() as i64,
        )))
    }

    pub(crate) fn open(path: &Path) -> Result<Option<Self>> {
        if !is_webp(path) {
            return Ok(None);
        }
        let stat = std::fs::metadata(path)?;
        ensure!(stat.is_file(), "WebP source is not a regular file");
        let reader = WebPDecoder::new(BufReader::new(File::open(path)?))?;
        if !reader.is_animated() {
            return Ok(None);
        }
        let (width, height) = reader.dimensions();
        let mut file = BufReader::new(File::open(path)?);
        let mut riff = [0; 12];
        file.read_exact(&mut riff)?;
        let end = 8 + u32::from_le_bytes(riff[4..8].try_into()?) as u64;
        ensure!(end <= stat.len(), "Truncated WebP animation");
        let mut starts = Vec::new();
        let mut frames = Vec::new();
        let mut duration_ms = 0;
        let mut pos = 12;
        while pos + 8 <= end {
            if filmcraft_media::cancel::cancelled() {
                return Err(MediaError::Cancelled.into());
            }
            file.seek(SeekFrom::Start(pos))?;
            let mut chunk = [0; 8];
            file.read_exact(&mut chunk)?;
            let size = u32::from_le_bytes(chunk[4..].try_into()?) as u64;
            pos += 8 + size + size % 2;
            ensure!(pos <= end, "Truncated WebP chunk");
            if &chunk[..4] == b"ANMF" {
                ensure!(size >= 16, "Invalid WebP animation frame");
                let mut header = [0; 16];
                file.read_exact(&mut header)?;
                let [x, y, w, h] = [
                    u24(&header[0..]) * 2,
                    u24(&header[3..]) * 2,
                    u24(&header[6..]) + 1,
                    u24(&header[9..]) + 1,
                ];
                ensure!(
                    x + w <= width && y + h <= height,
                    "WebP frame lies outside the canvas"
                );
                frames.push(FrameChunk {
                    offset: file.stream_position()?,
                    len: (size - 16) as usize,
                    rect: [x, y, w, h],
                    replace: header[15] & 2 != 0,
                    dispose: header[15] & 1 != 0,
                });
                let delay = u24(&header[12..]);
                starts.push(duration_ms);
                duration_ms += u64::from(delay.max(1));
            }
        }
        ensure!(
            !starts.is_empty() && starts.len() == reader.num_frames() as usize,
            "Invalid WebP frame count"
        );
        let duration = Tick(
            i64::try_from(duration_ms)?
                .checked_mul(TICKS_PER_MS)
                .context("WebP duration overflow")?,
        );
        let info = MediaInfo {
            name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            kind: MediaKind::Movie,
            duration,
            video: Some(VideoStreamInfo {
                width,
                height,
                frame_rate: FrameRate::new(starts.len() as i64 * 1000, duration_ms as i64),
                par: (1, 1),
                codec: "WebP".into(),
                pixel_format: "RGBA 8-bit".into(),
                color: filmcraft_color::ColorInfo::SRGB_FULL,
                has_alpha: reader.has_alpha(),
                bitrate: None,
                hdr: None,
            }),
            audio_streams: Vec::new(),
            container: "WebP".into(),
            start_timecode: None,
            file_size: Some(stat.len()),
        };
        Ok(Some(Self {
            info,
            starts_ms: starts.into(),
            duration_ms,
            frames,
            dims: [width, height],
            decoder: Mutex::new(Decode {
                file,
                next: 0,
                pixels: Vec::new(),
                last: None,
            }),
        }))
    }
}

impl MediaSource for Animation {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        let ms = (req.time.0.max(0) / TICKS_PER_MS) as u64;
        let index = self
            .starts_ms
            .partition_point(|&start| start <= ms)
            .saturating_sub(1);
        let mut decoder = self.decoder.lock().unwrap();
        let Decode {
            file,
            next,
            pixels,
            last,
        } = &mut *decoder;
        if *next == index + 1
            && let Some(frame) = last
        {
            return Ok(frame.clone());
        }
        last.take();
        let [w, h] = self.dims;
        let len = (w as usize * h as usize)
            .checked_mul(4)
            .filter(|&size| size <= MAX_DECODE_BYTES)
            .ok_or_else(|| MediaError::Decode("WebP canvas exceeds the decode limit".into()))?;
        pixels.resize(len, 0);
        if *next > index {
            pixels.fill(0);
            *next = 0;
        }
        while *next <= index {
            if filmcraft_media::cancel::cancelled() {
                return Err(MediaError::Cancelled);
            }
            let chunk = &self.frames[*next];
            let frame = chunk
                .decode(file)
                .map_err(|error| MediaError::Decode(error.to_string()))?;
            if let Some(previous) = next
                .checked_sub(1)
                .map(|i| &self.frames[i])
                .filter(|p| p.dispose)
            {
                for row in canvas_rows(pixels, w, previous.rect) {
                    row.fill(0);
                }
            }
            let frame_stride = chunk.rect[2] as usize * if frame.1 { 4 } else { 3 };
            for (dst, src) in
                canvas_rows(pixels, w, chunk.rect).zip(frame.0.chunks_exact(frame_stride))
            {
                if !frame.1 {
                    for (dst, src) in dst.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                        dst.copy_from_slice(&[src[0], src[1], src[2], 255]);
                    }
                } else if chunk.replace {
                    dst.copy_from_slice(src);
                } else {
                    for (dst, src) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                        blend_pixel(dst, src);
                    }
                }
            }
            *next += 1;
        }
        let rgba = pixels.clone();
        let pts = Tick(self.starts_ms[index] as i64 * TICKS_PER_MS);
        let frame = Arc::new(VideoFrame::rgba8(w, h, rgba).with_pts(pts));
        *last = Some(frame.clone());
        Ok(frame)
    }

    fn audio(&self, _: i64, _: usize, _: u32) -> filmcraft_media::Result<AudioBuffer> {
        Err(MediaError::NoStream("audio"))
    }
}

// Decode each ANMF payload as a still. image-webp 0.2.4 skips disposal with
// its default background and can clear RGB frames using an RGBA canvas's wrong stride.
impl FrameChunk {
    fn decode(&self, file: &mut BufReader<File>) -> Result<(Vec<u8>, bool)> {
        ensure!(
            (8..=MAX_DECODE_BYTES).contains(&self.len),
            "Invalid WebP frame size"
        );
        file.seek(SeekFrom::Start(self.offset))?;
        let mut first = [0; 4];
        file.read_exact(&mut first)?;
        let alpha = &first == b"ALPH";
        let mut header = Vec::with_capacity(30);
        header.extend_from_slice(b"RIFF");
        header
            .extend_from_slice(&((self.len + 4 + if alpha { 18 } else { 0 }) as u32).to_le_bytes());
        header.extend_from_slice(b"WEBP");
        if alpha {
            header.extend_from_slice(b"VP8X\x0a\0\0\0\x10\0\0\0");
            header.extend_from_slice(&(self.rect[2] - 1).to_le_bytes()[..3]);
            header.extend_from_slice(&(self.rect[3] - 1).to_le_bytes()[..3]);
        }
        header.extend_from_slice(&first);
        let head = header.len();
        header.resize(head + self.len - 4, 0);
        file.read_exact(&mut header[head..])?;
        let mut reader = WebPDecoder::new(Cursor::new(header))?;
        ensure!(
            !reader.is_animated() && reader.dimensions() == (self.rect[2], self.rect[3]),
            "Invalid WebP frame dimensions"
        );
        reader.set_memory_limit(MAX_DECODE_BYTES);
        let len = reader
            .output_buffer_size()
            .filter(|&size| size <= MAX_DECODE_BYTES)
            .context("WebP frame exceeds the decode limit")?;
        let mut pixels = vec![0; len];
        reader.read_image(&mut pixels)?;
        Ok((pixels, reader.has_alpha()))
    }
}

fn canvas_rows(
    pixels: &mut [u8],
    width: u32,
    [x, y, w, h]: [u32; 4],
) -> impl Iterator<Item = &mut [u8]> {
    pixels
        .chunks_exact_mut(width as usize * 4)
        .skip(y as usize)
        .take(h as usize)
        .map(move |row| &mut row[x as usize * 4..(x + w) as usize * 4])
}

fn blend_pixel(dst: &mut [u8], src: &[u8]) {
    let a = u32::from(src[3]);
    if a == 0 {
        return;
    }
    if a == 255 || dst[3] == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let source = a * 255;
    let behind = u32::from(dst[3]) * (255 - a);
    let alpha = source + behind;
    for c in 0..3 {
        dst[c] =
            ((u32::from(src[c]) * source + u32::from(dst[c]) * behind + alpha / 2) / alpha) as u8;
    }
    dst[3] = ((alpha + 127) / 255) as u8;
}

pub(crate) struct Writer<W> {
    out: W,
    dims: [u32; 2],
    encoded: Vec<u8>,
    alpha: bool,
}

impl<W: Write + Seek> Writer<W> {
    pub(crate) fn new(mut out: W, dims: [u32; 2]) -> Result<Self> {
        let [w, h] = dims;
        ensure!(
            (1..=16384).contains(&w) && (1..=16384).contains(&h),
            "WebP dimensions must be 1–16384 pixels"
        );
        out.write_all(b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0")?;
        out.write_all(&(w - 1).to_le_bytes()[..3])?;
        out.write_all(&(h - 1).to_le_bytes()[..3])?;
        // Transparent background and zero loop count (infinite).
        out.write_all(b"ANIM\x06\0\0\0\0\0\0\0\0\0")?;
        Ok(Self {
            out,
            dims,
            encoded: Vec::new(),
            alpha: false,
        })
    }

    pub(crate) fn frame(&mut self, rgba: &[u8], dims: [u32; 2], ms: u64) -> Result<()> {
        ensure!(dims == self.dims, "WebP canvas dimensions changed");
        ensure!(
            (1..=0xff_ffff).contains(&ms),
            "WebP frame duration is out of range"
        );
        let [w, h] = dims;
        self.alpha |= rgba.chunks_exact(4).any(|pixel| pixel[3] != 255);
        self.encoded.clear();
        WebPEncoder::new(&mut self.encoded).encode(rgba, w, h, ColorType::Rgba8)?;
        // The still encoder writes a padded VP8L chunk after the 12-byte RIFF header.
        let payload = &self.encoded[12..];
        let size = u32::try_from(16 + payload.len())?;
        self.out.write_all(b"ANMF")?;
        self.out.write_all(&size.to_le_bytes())?;
        self.out.write_all(&[0; 6])?;
        self.out.write_all(&(w - 1).to_le_bytes()[..3])?;
        self.out.write_all(&(h - 1).to_le_bytes()[..3])?;
        self.out.write_all(&ms.to_le_bytes()[..3])?;
        self.out.write_all(&[2])?; // Replace the full canvas, including transparent pixels.
        self.out.write_all(payload)?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        let size = u32::try_from(self.out.stream_position()? - 8)
            .context("WebP exceeds the RIFF size limit")?;
        self.out.seek(SeekFrom::Start(4))?;
        self.out.write_all(&size.to_le_bytes())?;
        if self.alpha {
            self.out.seek(SeekFrom::Start(20))?;
            self.out.write_all(&[0x12])?;
        }
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_frames_clear_disposed_rectangles_and_restart_cleanly() {
        fn frame_chunk(dims: [u32; 2], pixel: [u8; 4]) -> Vec<u8> {
            let mut file = Cursor::new(Vec::new());
            let mut writer = Writer::new(&mut file, dims).unwrap();
            writer
                .frame(&pixel.repeat((dims[0] * dims[1]) as usize), dims, 40)
                .unwrap();
            writer.finish().unwrap();
            file.into_inner().split_off(44)
        }

        let mut file = Cursor::new(Vec::new());
        let mut writer = Writer::new(&mut file, [4, 2]).unwrap();
        writer.alpha = true;
        let mut red = frame_chunk([2, 2], [255, 0, 0, 255]);
        red[23] = 3; // Replace, then dispose.
        let mut green = frame_chunk([2, 2], [0, 255, 0, 255]);
        green[8] = 1; // x = 2
        let mut blue = frame_chunk([2, 2], [0, 0, 255, 128]);
        blue[8] = 1;
        blue[23] = 0; // Blend over the green rectangle.
        for chunk in [red, green, blue] {
            writer.out.write_all(&chunk).unwrap();
        }
        writer.finish().unwrap();
        let path =
            std::env::temp_dir().join(format!("abner-webp-disposal-{}.webp", std::process::id()));
        std::fs::write(&path, file.into_inner()).unwrap();
        let animation = Animation::open(&path).unwrap().unwrap();
        let pixels = |t| animation.at(t).unwrap().to_rgba8();
        assert_eq!(&pixels(0.)[..8], &[255, 0, 0, 255].repeat(2));
        let second = pixels(0.04);
        assert_eq!(&second[..8], &[0; 8]);
        assert_eq!(&second[8..16], &[0, 255, 0, 255].repeat(2));
        assert_eq!(&pixels(0.08)[8..16], &[0, 127, 128, 255].repeat(2));
        assert_eq!(&pixels(0.)[8..16], &[0; 8]);
        assert_eq!(pixels(0.04), second);
        std::fs::remove_file(path).unwrap();
    }
}
