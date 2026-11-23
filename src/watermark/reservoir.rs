//! Keep diverse native pictures from one forward scan, including for refinement.
use super::search::Region;
use filmcraft_frame::{PixelData, VideoFrame};
use std::sync::Arc;

const MAX_FRAMES: usize = 64;
const THUMB_W: usize = 64;
const THUMB_H: usize = 36;

type Thumbnail = [u16; THUMB_W * THUMB_H];

struct Sample {
    index: usize,
    picture: Arc<VideoFrame>,
    thumb: Thumbnail,
}

pub(super) struct Reservoir {
    region: Region,
    crop: filmcraft_frame::Region,
    samples: Vec<Sample>,
    distances: [[u32; MAX_FRAMES]; MAX_FRAMES],
    closest: Option<(u32, usize, usize)>,
    bytes: usize,
    budget: usize,
}

impl Reservoir {
    pub fn new(region: Region, [x, y, w, h]: Region, budget: usize) -> Self {
        // Preserve the chroma phase and include the full detector halo.
        let crop = filmcraft_frame::Region {
            x: x & !1,
            y: y & !1,
            w: w + x % 2,
            h: h + y % 2,
        };
        Self {
            region,
            crop,
            samples: Vec::new(),
            distances: [[0; MAX_FRAMES]; MAX_FRAMES],
            closest: None,
            bytes: 0,
            budget,
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn origin(&self) -> [usize; 2] {
        [self.crop.x, self.crop.y]
    }

    pub fn pictures(&self, count: usize) -> impl Iterator<Item = &VideoFrame> {
        (0..count).map(move |slot| {
            let index = slot * (self.len() - 1) / (count - 1);
            self.samples[index].picture.as_ref()
        })
    }

    pub fn into_pictures(self) -> impl Iterator<Item = Arc<VideoFrame>> {
        self.samples.into_iter().map(|sample| sample.picture)
    }

    pub fn finish(&mut self) {
        // Refinement subsets should advance in source order.
        self.samples.sort_unstable_by_key(|sample| sample.index);
    }

    pub fn consider(&mut self, index: usize, picture: Arc<VideoFrame>) {
        let thumb = thumbnail(&picture, self.region);
        let len = self.samples.len();
        let mut distances = [0; MAX_FRAMES];
        for (distance, sample) in distances.iter_mut().zip(&self.samples) {
            *distance = thumb
                .iter()
                .zip(&sample.thumb)
                .map(|(&a, &b)| u32::from(a.abs_diff(b)))
                .sum();
        }
        let replace = self.closest.and_then(|(pair_distance, a, b)| {
            let nearest = *distances[..len].iter().min()?;
            // Remove whichever of the redundant pair the incoming picture is closest to.
            (nearest > pair_distance).then_some(if distances[a] <= distances[b] { a } else { b })
        });
        if len == MAX_FRAMES && replace.is_none() {
            return;
        }
        let picture = if self
            .crop
            .is_full(picture.width as usize, picture.height as usize)
        {
            picture
        } else {
            Arc::new(picture.cropped(self.crop))
        };
        if distances[..len]
            .iter()
            .zip(&self.samples)
            .any(|(&distance, sample)| distance == 0 && same_pixels(&picture, &sample.picture))
        {
            return;
        }

        let size = picture.byte_size();
        let slot = if len < MAX_FRAMES && self.bytes + size <= self.budget {
            self.samples.push(Sample {
                index,
                picture,
                thumb,
            });
            self.bytes += size;
            len
        } else if let Some(slot) = replace {
            let bytes = self.bytes - self.samples[slot].picture.byte_size() + size;
            if bytes > self.budget {
                return;
            }
            self.samples[slot] = Sample {
                index,
                picture,
                thumb,
            };
            self.bytes = bytes;
            slot
        } else {
            return;
        };
        for (other, &distance) in distances.iter().take(self.len()).enumerate() {
            if other != slot {
                self.distances[slot][other] = distance;
                self.distances[other][slot] = distance;
            }
        }
        self.closest = (0..self.len())
            .flat_map(|a| (a + 1..self.len()).map(move |b| (a, b)))
            .map(|(a, b)| (self.distances[a][b], a, b))
            .min();
    }
}

fn thumbnail(picture: &VideoFrame, [x, y, w, h]: Region) -> Thumbnail {
    let width = picture.width as usize;
    let index = |i: usize| {
        let px = x + (2 * (i % THUMB_W) + 1) * w / (2 * THUMB_W);
        let py = y + (2 * (i / THUMB_W) + 1) * h / (2 * THUMB_H);
        py * width + px
    };
    match &picture.data {
        PixelData::Yuv8 { planes, .. } => {
            std::array::from_fn(|i| u16::from(planes[0][index(i)]) << 8)
        }
        PixelData::Yuv16 { planes, bits, .. } => {
            let shift = 16u32.saturating_sub(*bits);
            std::array::from_fn(|i| planes[0][index(i)].checked_shl(shift).unwrap_or(0))
        }
        PixelData::Rgba8(rgba) => std::array::from_fn(|i| {
            let p = index(i) * 4;
            let luma = 77 * u32::from(rgba[p])
                + 150 * u32::from(rgba[p + 1])
                + 29 * u32::from(rgba[p + 2]);
            (luma >> 8) as u16 * 256
        }),
        PixelData::RgbaF32(rgba) => std::array::from_fn(|i| {
            let p = index(i) * 4;
            let luma = 0.299 * rgba[p] + 0.587 * rgba[p + 1] + 0.114 * rgba[p + 2];
            ((luma.clamp(0., 1.) * 255.).round() as u16) << 8
        }),
    }
}

fn same_pixels(a: &VideoFrame, b: &VideoFrame) -> bool {
    if a.color != b.color {
        return false;
    }
    match (&a.data, &b.data) {
        (PixelData::Rgba8(a), PixelData::Rgba8(b)) => Arc::ptr_eq(a, b) || a == b,
        (PixelData::RgbaF32(a), PixelData::RgbaF32(b)) => Arc::ptr_eq(a, b) || a == b,
        (
            PixelData::Yuv8 {
                planes: a,
                chroma: ca,
                alpha: aa,
            },
            PixelData::Yuv8 {
                planes: b,
                chroma: cb,
                alpha: ab,
            },
        ) => ca == cb && aa == ab && a.iter().zip(b).all(|(a, b)| Arc::ptr_eq(a, b) || a == b),
        (
            PixelData::Yuv16 {
                planes: a,
                chroma: ca,
                bits: ba,
                alpha: aa,
            },
            PixelData::Yuv16 {
                planes: b,
                chroma: cb,
                bits: bb,
                alpha: ab,
            },
        ) => {
            ca == cb
                && ba == bb
                && aa == ab
                && a.iter().zip(b).all(|(a, b)| Arc::ptr_eq(a, b) || a == b)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watermark::search::{DETECTOR_BUDGET, Search};

    #[test]
    fn bounded_reservoir_replaces_a_redundant_picture_and_ignores_duplicates() {
        let mut reservoir = Reservoir::new([0, 0, 2, 2], [0, 0, 2, 2], 3 * 16);
        for (i, gray) in [20, 21, 21, 80, 130, 129].into_iter().enumerate() {
            reservoir.consider(i, Arc::new(VideoFrame::rgba8(2, 2, vec![gray; 16])));
        }
        assert_eq!(reservoir.len(), 3);
        assert_eq!(reservoir.bytes(), 48);
        let mut values: Vec<_> = reservoir
            .samples
            .iter()
            .map(|sample| sample.thumb[0] >> 8)
            .collect();
        values.sort_unstable();
        assert_eq!(values, [20, 80, 130]);
    }

    #[test]
    fn retained_crop_preserves_native_rgb_at_an_odd_context_origin() {
        let size = [240, 240];
        let region = [99, 101, 20, 20];
        let picture = Arc::new(VideoFrame {
            width: size[0] as u32,
            height: size[1] as u32,
            data: PixelData::Yuv8 {
                planes: [
                    Arc::new(vec![128; 240 * 240]),
                    Arc::new((0..120 * 120).map(|i| i as u8).collect()),
                    Arc::new(vec![128; 120 * 120]),
                ],
                chroma: filmcraft_frame::Chroma::C420,
                alpha: None,
            },
            color: filmcraft_color::ColorInfo::REC709,
            par: (1, 1),
            pts: filmcraft_time::Tick::ZERO,
        });
        let mut reservoir = Reservoir::new(region, Search::context(size, region), DETECTOR_BUDGET);
        reservoir.consider(0, picture.clone());
        assert_eq!(reservoir.origin(), [2, 4]);
        let mut rgba = Vec::new();
        let convert = |picture: &VideoFrame, rgba: &mut Vec<u8>, origin| {
            let (w, h) = (picture.width, picture.height);
            crate::fc::fit_rgba_into(picture, crate::fc::Fit::Exact { w, h }, rgba);
            let mut search = Search::new(size, region, 8, DETECTOR_BUDGET);
            search.add(rgba, w as usize, origin).unwrap();
            search.frames.pop().unwrap().rgb().unwrap().to_vec()
        };
        let full = convert(&picture, &mut rgba, [0, 0]);
        let origin = reservoir.origin();
        let cropped = convert(
            &reservoir.into_pictures().next().unwrap(),
            &mut rgba,
            origin,
        );
        assert_eq!(full, cropped);
    }
}
