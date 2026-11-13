//! Generic source selection for processing. The first processor may supply an
//! exact decoder; playback and other processors keep the host's native reader.
use crate::player::{NativeFrame, Player};
use abner_processor_api::{Cancellation, FrameSource, Registry, Result, VideoFrame};
pub struct Source {
    custom: Option<Box<dyn FrameSource>>,
    player: Option<Player>,
    pending: Option<(f64, NativeFrame)>,
    fps: f64,
    dims: [usize; 2],
}
impl Source {
    pub fn open(
        registry: &Registry,
        first: Option<&str>,
        path: &std::path::Path,
        w: u32,
        h: u32,
        fps: f64,
    ) -> Result<Self> {
        let custom = match first {
            Some(id) => registry
                .get(id)?
                .open_source(path, w as usize, h as usize)?,
            None => None,
        };
        let player = if custom.is_none() {
            Some(
                Player::spawn_native(path, w, h, 16 * 1024 * 1024)
                    .ok_or("Cannot open processing decoder")?,
            )
        } else {
            None
        };
        Ok(Self {
            custom,
            player,
            pending: None,
            fps,
            dims: [w as usize, h as usize],
        })
    }
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        self.pending = None;
        if let Some(s) = &mut self.custom {
            s.seek(seconds)
        } else {
            self.player.as_ref().unwrap().seek(seconds, true);
            Ok(())
        }
    }
    fn native(&self, cancel: &Cancellation) -> Result<Option<(f64, NativeFrame)>> {
        let player = self.player.as_ref().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            cancel.check()?;
            if let Some(f) = player.take_next_native() {
                return Ok(Some(f));
            }
            if player.failed() {
                return Err("Processing decode failed".into());
            }
            if player.exhausted() {
                return Ok(None);
            }
            if std::time::Instant::now() > deadline {
                return Err("Processing decoder timed out".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    pub fn read(&mut self, cancel: &Cancellation) -> Result<Option<VideoFrame>> {
        cancel.check()?;
        if let Some(s) = &mut self.custom {
            let frame = s.read()?;
            if let Some(f) = &frame {
                f.validate()?;
                if [f.width, f.height] != self.dims {
                    return Err("Source decoder changed dimensions".into());
                }
            }
            return Ok(frame);
        }
        let next = if self.pending.is_some() {
            self.pending.take()
        } else {
            self.native(cancel)?
        };
        let Some((pts, f)) = next else {
            return Ok(None);
        };
        self.pending = self.native(cancel)?;
        let p = (pts * 1e6).round() as i64;
        let nominal = (1e6 / self.fps).round().max(1.0) as i64;
        let duration = self
            .pending
            .as_ref()
            .map_or(nominal, |(t, _)| (t * 1e6).round() as i64 - p);
        if duration <= 0 || (duration - nominal).abs() > 2000 {
            return Err("Processing currently requires constant-rate input".into());
        }
        abner_processing::color::from_decoded(&f, p, duration, [1, 1_000_000]).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abner_processor_api::{Descriptor, Factory, FramePixels, PixelFormat, Session};
    use std::sync::Arc;
    struct Exact {
        index: i64,
    }
    impl FrameSource for Exact {
        fn seek(&mut self, t: f64) -> Result<()> {
            self.index = (t * 24.).round() as i64;
            Ok(())
        }
        fn read(&mut self) -> Result<Option<VideoFrame>> {
            if self.index >= 3 {
                return Ok(None);
            }
            let f = VideoFrame {
                width: 16,
                height: 16,
                format: PixelFormat::RgbF32,
                pixels: FramePixels::RgbF32(Arc::new([-0.25, 0.12345679, 1.25].repeat(256))),
                pts: self.index,
                duration: 1,
                time_base: [1, 24],
            };
            self.index += 1;
            Ok(Some(f))
        }
    }
    struct Provider;
    impl Factory for Provider {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "exact".into(),
                name: "Exact".into(),
                settings_schema: serde_json::json!({}),
                presets: vec![],
            }
        }
        fn create(&self, _: usize, _: usize, _: &serde_json::Value) -> Result<Box<dyn Session>> {
            Err("not used".into())
        }
        fn open_source(
            &self,
            _: &std::path::Path,
            _: usize,
            _: usize,
        ) -> Result<Option<Box<dyn FrameSource>>> {
            Ok(Some(Box::new(Exact { index: 0 })))
        }
    }
    #[test]
    fn preferred_source_bypasses_player_preserves_timing_and_obeys_cancel() {
        let mut registry = Registry::default();
        registry.register(Box::new(Provider)).unwrap();
        // Deliberately no file: fallback to Player would fail.
        let mut source = Source::open(
            &registry,
            Some("exact"),
            std::path::Path::new("/nonexistent/float-probe.mov"),
            16,
            16,
            24.,
        )
        .unwrap();
        source.seek(1. / 24.).unwrap();
        let cancel = Cancellation::default();
        let f = source.read(&cancel).unwrap().unwrap();
        assert_eq!(f.pts, 1);
        assert_eq!(f.time_base, [1, 24]);
        let FramePixels::RgbF32(p) = f.pixels else {
            panic!()
        };
        assert_eq!(p[0], -0.25);
        assert_eq!(p[2], 1.25);
        cancel.cancel();
        assert!(source.read(&cancel).is_err());
    }
}
