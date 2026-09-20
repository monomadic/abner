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

pub fn output_path(video: &Path) -> PathBuf {
    video.with_extension("mask.png")
}

/// Write beside the source, then atomically replace the destination only after
/// PNG encoding succeeds. A failed save leaves an existing mask intact.
pub fn save(path: &Path, width: u32, height: u32, pixels: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Grayscale);
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
