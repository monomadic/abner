//! Native-resolution binary masks. Screen coordinates belong to App; this module
//! only paints image pixels and writes the same pixels as an 8-bit grayscale PNG.
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Mask {
    pub id: u64,
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<Vec<u8>>,
}

impl Mask {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            revision: 0,
            width,
            height,
            pixels: Arc::new(vec![0; width as usize * height as usize]),
        }
    }

    /// A swept circle (capsule), so sparse pointer events cannot leave gaps.
    pub fn paint(&mut self, from: (f32, f32), to: (f32, f32), radius: f32) {
        let x0 = (from.0.min(to.0) - radius).floor().max(0.0) as u32;
        let y0 = (from.1.min(to.1) - radius).floor().max(0.0) as u32;
        let x1 = ((from.0.max(to.0) + radius).ceil().max(0.0) as u32).min(self.width);
        let y1 = ((from.1.max(to.1) + radius).ceil().max(0.0) as u32).min(self.height);
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let length2 = dx * dx + dy * dy;
        let pixels = Arc::make_mut(&mut self.pixels);
        let mut changed = false;
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5 - from.0, y as f32 + 0.5 - from.1);
                let t = if length2 > 0.0 {
                    ((px * dx + py * dy) / length2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if (px - t * dx).powi(2) + (py - t * dy).powi(2) <= radius * radius {
                    let p = &mut pixels[(y * self.width + x) as usize];
                    changed |= *p != 255;
                    *p = 255;
                }
            }
        }
        if changed {
            self.revision += 1;
        }
    }
}

/// The crop marquee, in image pixels — the same grid the mask uses, so a
/// crop cuts the mask and the video frame to identical rectangles. App owns
/// the dragging (screen space); this owns the geometry and the pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crop {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Small enough to crop tightly, big enough that the handles stay grabbable
/// and the export is still an image.
const MIN_CROP: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    Nw,
    Ne,
    Sw,
    Se,
}

impl Corner {
    /// This corner's point on `c` — where the handle is drawn and grabbed.
    pub fn of(self, c: Crop) -> (f32, f32) {
        let (x1, y1) = (c.x + c.w, c.y + c.h);
        match self {
            Corner::Nw => (c.x, c.y),
            Corner::Ne => (x1, c.y),
            Corner::Sw => (c.x, y1),
            Corner::Se => (x1, y1),
        }
    }
}

impl Crop {
    /// The whole frame — where `C` starts, sitting directly on the video.
    pub fn full(width: u32, height: u32) -> Self {
        Self { x: 0.0, y: 0.0, w: width as f32, h: height as f32 }
    }

    /// Slide without resizing, staying inside the image.
    pub fn moved_to(self, x: f32, y: f32, width: u32, height: u32) -> Self {
        Self {
            x: x.clamp(0.0, (width as f32 - self.w).max(0.0)),
            y: y.clamp(0.0, (height as f32 - self.h).max(0.0)),
            ..self
        }
    }

    /// Drag one corner to (x, y); the opposite corner is the anchor. The
    /// pointer is clamped to the image first, so the rect can never be
    /// dragged outside it, and `MIN_CROP` stops it collapsing.
    pub fn with_corner(self, corner: Corner, x: f32, y: f32, width: u32, height: u32) -> Self {
        let (ax, ay) = match corner {
            Corner::Nw => (self.x + self.w, self.y + self.h),
            Corner::Ne => (self.x, self.y + self.h),
            Corner::Sw => (self.x + self.w, self.y),
            Corner::Se => (self.x, self.y),
        };
        let x = x.clamp(0.0, width as f32);
        let y = y.clamp(0.0, height as f32);
        let (x0, x1) = (ax.min(x), ax.max(x));
        let (y0, y1) = (ay.min(y), ay.max(y));
        Self {
            x: x0.min((width as f32 - MIN_CROP).max(0.0)),
            y: y0.min((height as f32 - MIN_CROP).max(0.0)),
            w: (x1 - x0).max(MIN_CROP).min(width as f32),
            h: (y1 - y0).max(MIN_CROP).min(height as f32),
        }
    }

    /// A rect from outside (`--crop x,y,w,h`) pulled inside the image.
    pub fn clamped(self, width: u32, height: u32) -> Self {
        Self {
            w: self.w.clamp(MIN_CROP, width as f32),
            h: self.h.clamp(MIN_CROP, height as f32),
            ..self
        }
        .moved_to(self.x, self.y, width, height)
    }

    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    /// Rounded to whole pixels and clipped to the image, for export:
    /// (x, y, w, h), always at least one pixel each way.
    pub fn pixels(self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let x = (self.x.round().max(0.0) as u32).min(width.saturating_sub(1));
        let y = (self.y.round().max(0.0) as u32).min(height.saturating_sub(1));
        let w = (self.w.round().max(1.0) as u32).min(width - x);
        let h = (self.h.round().max(1.0) as u32).min(height - y);
        (x, y, w, h)
    }
}

/// Copy a sub-rectangle out of a tightly packed image (`channels` bytes per
/// pixel, `width * channels` per row) — the mask at 1, an RGBA frame at 4.
pub fn crop_pixels(
    pixels: &[u8],
    width: u32,
    channels: usize,
    (x, y, w, h): (u32, u32, u32, u32),
) -> Vec<u8> {
    let row = width as usize * channels;
    let (x, w) = (x as usize * channels, w as usize * channels);
    (y as usize..(y + h) as usize)
        .flat_map(|r| pixels[r * row + x..r * row + x + w].iter().copied())
        .collect()
}

pub fn output_path(video: &Path) -> PathBuf {
    video.with_extension("mask.png")
}

/// Where a crop's video pixels land, beside the mask they match.
pub fn crop_output_path(video: &Path) -> PathBuf {
    video.with_extension("crop.png")
}

/// Write beside the source, then atomically replace the destination only after
/// PNG encoding succeeds. A failed save leaves an existing mask intact.
pub fn save(path: &Path, width: u32, height: u32, pixels: &[u8]) -> anyhow::Result<()> {
    write_png(path, width, height, pixels, png::ColorType::Grayscale)
}

/// The same atomic write for a crop's colour pixels (the decoder hands the
/// app RGBA, so that is what lands on disk).
pub fn save_rgba(path: &Path, width: u32, height: u32, pixels: &[u8]) -> anyhow::Result<()> {
    write_png(path, width, height, pixels, png::ColorType::Rgba)
}

fn write_png(
    path: &Path,
    width: u32,
    height: u32,
    pixels: &[u8],
    color: png::ColorType,
) -> anyhow::Result<()> {
    use std::io::Write;
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
        writer.finish()?;
    }
    let nonce = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("png.{}.{nonce}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> anyhow::Result<()> {
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swept_circle_is_connected_clipped_and_binary() {
        let mut mask = Mask::new(32, 16);
        mask.paint((-4.0, 8.0), (36.0, 8.0), 2.0);
        for x in 0..32 {
            assert_eq!(mask.pixels[8 * 32 + x], 255);
        }
        assert!(mask.pixels[..32].iter().all(|p| *p == 0));
        assert!(mask.pixels.iter().all(|p| *p == 0 || *p == 255));
        let revision = mask.revision;
        mask.paint((-4.0, 8.0), (36.0, 8.0), 2.0);
        assert_eq!(mask.revision, revision);
    }
    /// The crop is the contract between the two exported files: whatever
    /// rectangle it names must cut the mask and the RGBA frame to the same
    /// pixels, and it can never leave the image.
    #[test]
    fn crop_stays_inside_the_image_and_cuts_both_planes_alike() {
        let full = Crop::full(32, 16);
        assert_eq!(full.pixels(32, 16), (0, 0, 32, 16));
        // Dragged past the edges, a move keeps its size and stops at them.
        assert_eq!(full.moved_to(9.0, 9.0, 32, 16), full);
        let c = Crop { x: 8.0, y: 4.0, w: 8.0, h: 4.0 };
        assert_eq!(c.moved_to(100.0, -100.0, 32, 16), Crop { x: 24.0, y: 0.0, ..c });
        // A corner drag anchors the opposite corner, clamps to the image…
        let d = c.with_corner(Corner::Se, 100.0, 100.0, 32, 16);
        assert_eq!(d, Crop { x: 8.0, y: 4.0, w: 24.0, h: 12.0 });
        assert_eq!(Corner::Se.of(d), (32.0, 16.0));
        // …and refuses to collapse past MIN_CROP, even dragged inside out.
        let e = c.with_corner(Corner::Nw, 40.0, 40.0, 32, 16);
        assert!(e.w >= MIN_CROP && e.h >= MIN_CROP);
        assert!(e.x + e.w <= 32.0 && e.y + e.h <= 16.0);

        // Same rect, one channel and four: the crop's pixel (i, j) is the
        // image's (x + i, y + j) in both.
        let rect = (3, 2, 4, 3);
        let gray: Vec<u8> = (0..32u32 * 16).map(|i| i as u8).collect();
        let rgba: Vec<u8> = gray.iter().flat_map(|p| [*p, 0, 0, 255]).collect();
        let cut_gray = crop_pixels(&gray, 32, 1, rect);
        let cut_rgba = crop_pixels(&rgba, 32, 4, rect);
        assert_eq!(cut_gray.len(), 4 * 3);
        assert_eq!(cut_rgba.len(), 4 * 3 * 4);
        for j in 0..3usize {
            for i in 0..4usize {
                let source = gray[(2 + j) * 32 + 3 + i];
                assert_eq!(cut_gray[j * 4 + i], source);
                assert_eq!(cut_rgba[(j * 4 + i) * 4..][..4], [source, 0, 0, 255]);
            }
        }
    }

    #[test]
    fn saved_png_preserves_dimensions_polarity_and_pixels() {
        let video =
            std::env::temp_dir().join(format!("abner-mask-test-{}.mov", std::process::id()));
        let path = output_path(&video);
        assert_eq!(
            path.file_name().unwrap(),
            format!("abner-mask-test-{}.mask.png", std::process::id()).as_str()
        );
        let mut mask = Mask::new(17, 11);
        mask.paint((8.5, 5.5), (8.5, 5.5), 2.0);
        save(&path, mask.width, mask.height, &mask.pixels).unwrap();
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
                .read_info()
                .unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((info.width, info.height), (17, 11));
        assert_eq!(info.color_type, png::ColorType::Grayscale);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        assert_eq!(&decoded[..info.buffer_size()], mask.pixels.as_slice());
        assert_eq!(decoded[5 * 17 + 8], 255);
        assert_eq!(decoded[0], 0);
        // A failed replacement must not destroy the successful export.
        assert!(save(&path, 17, 11, &[0]).is_err());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        std::fs::remove_file(path).unwrap();
    }
}
