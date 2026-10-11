//! Minimal glyph stack: system mono and SF Compact Rounded rasterized into
//! one R8 atlas (CPU-side shelf packer, per-glyph rect uploads). Glyphs
//! rasterize at physical pixel size and draw at logical size, so text
//! stays crisp on HiDPI.
//!
//! Two rules carried over from switchblade's atlas, both shipped bugs
//! there: the atlas is NEVER reset in the middle of a frame (earlier text
//! in the same frame has already baked its UVs, and a wipe under them
//! draws garbage), and a glyph that can't be placed is refused and
//! remembered, so it costs one raster attempt rather than one per frame.

use std::collections::HashMap;

use ab_glyph::{Font, FontVec, ScaleFont, VariableFont};

/// 2048² × R8 = 4 MiB, which is nothing; 1024 was ~40% full with two
/// sizes at 2× retina and one reset would have wiped the lot.
pub const ATLAS: u32 = 2048;

/// A freshly rasterized glyph's rect, for a partial texture upload. The
/// pixels live in `TextCtx::atlas` at this position.
#[derive(Debug, Clone, Copy)]
pub struct Upload {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One positioned glyph, in PHYSICAL pixels relative to the text origin
/// (top-left of the line box).
pub struct GlyphQuad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Normalized atlas UV rect (u0, v0, u1, v1).
    pub uv: [f32; 4],
}

pub struct LaidText {
    pub quads: Vec<GlyphQuad>,
    /// Physical px advance width / line height of the whole run.
    pub w: f32,
    pub h: f32,
}

struct Entry {
    /// Atlas placement in pixels.
    ax: u32,
    ay: u32,
    w: u32,
    h: u32,
    /// Bearing relative to the pen position (px_bounds.min).
    left: f32,
    top: f32,
}

pub struct TextCtx {
    font: Option<FontVec>,
    ui_font: Option<FontVec>,
    header_font: Option<FontVec>,
    regular_font: FontVec,
    inspector_fonts: [Option<FontVec>; 3],
    inspector: u8,
    regular: bool,
    header: bool,
    interface: bool,
    pub atlas: Vec<u8>,
    /// Rects rasterized since the renderer last drained them.
    pub pending: Vec<Upload>,
    /// Set when a glyph didn't fit; the reset is deferred to the next
    /// frame boundary (`begin_frame`), never done mid-layout.
    full: bool,
    cache: HashMap<(u8, u16, u32), Option<Entry>>,
    cur_x: u32,
    cur_y: u32,
    row_h: u32,
}

/// macOS system monospace candidates; first that parses wins. (This app is
/// macOS-first like switchblade; on other platforms add paths here.)
const FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/Monaco.ttf",
    "/System/Library/Fonts/Supplemental/Courier New.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
];

impl TextCtx {
    pub fn load() -> Self {
        let mut font = None;
        for cand in FONT_CANDIDATES {
            if let Ok(bytes) = std::fs::read(cand) {
                // index 0 handles both .ttf and .ttc collections.
                if let Ok(f) = FontVec::try_from_vec_and_index(bytes, 0) {
                    log::debug!("text: using {cand}");
                    font = Some(f);
                    break;
                }
            }
        }
        if font.is_none() {
            log::warn!("text: no usable system font found — UI text disabled");
        }
        let ui_font = std::fs::read("/System/Library/Fonts/SFCompactRounded.ttf").ok()
            .and_then(|bytes| FontVec::try_from_vec(bytes).ok()).map(|mut f| {
                f.set_variation(b"wght", 400.0);
                f
            });
        if ui_font.is_none() { log::warn!("SF Compact Rounded unavailable; interface uses the fallback font"); }
        let header_font = Some(FontVec::try_from_vec(
            include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf").to_vec()
        ).expect("bundled JetBrains Mono Medium must parse"));
        Self {
            inspector_fonts: [400.0, 500.0, 600.0].map(|weight| {
                std::fs::read("/System/Library/Fonts/SFNS.ttf").ok()
                    .and_then(|bytes| FontVec::try_from_vec(bytes).ok()).map(|mut f| {
                        f.set_variation(b"wght", weight); f.set_variation(b"opsz", 12.0); f
                    })
            }),
            inspector: 0,
            font, ui_font, header_font,
            regular_font: FontVec::try_from_vec(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").to_vec()).expect("bundled JetBrains Mono Regular must parse"),
            regular: false, header: false, interface: false,
            atlas: vec![0; (ATLAS * ATLAS) as usize],
            pending: Vec::new(),
            full: false,
            cache: HashMap::new(),
            cur_x: 0,
            cur_y: 0,
            row_h: 0,
        }
    }

    pub fn set_interface(&mut self, interface: bool) { self.interface = interface; self.header = false; self.regular = false; }
    pub fn set_header(&mut self, header: bool) { self.header = header; }

    pub fn set_regular(&mut self, regular: bool) { self.regular = regular; }

    pub fn set_inspector(&mut self, weight: u8) { self.inspector = weight.min(3); }

    fn face(&self) -> Option<&FontVec> {
        if self.inspector > 0 { return self.inspector_fonts[(self.inspector - 1) as usize].as_ref().or(self.ui_font.as_ref()).or(self.font.as_ref()); }
        if self.regular { return Some(&self.regular_font); }
        if self.header { return self.header_font.as_ref().or(self.ui_font.as_ref()).or(self.font.as_ref()); }
        if self.interface { self.ui_font.as_ref().or(self.font.as_ref()) } else { self.font.as_ref() }
    }

    // Figma sizes are em sizes; ab_glyph's scale is ascent minus descent.
    fn raster_size(&self, px: f32) -> f32 {
        if self.inspector > 0 || self.regular || (self.interface && self.ui_font.is_some()) || (self.header && self.header_font.is_some()) {
            let font = self.face().unwrap();
            px * font.height_unscaled() / font.units_per_em().unwrap_or(1.0)
        } else { px }
    }

    /// Frame boundary. If the last frame ran the atlas out of room, wipe
    /// it NOW — before anything in this frame has baked a UV — and tell
    /// the renderer the whole texture needs re-uploading. Returns true on
    /// a reset. Only a pathological glyph-size churn ever gets here; a
    /// stream of these in the log is the signature to look for.
    pub fn begin_frame(&mut self) -> bool {
        if !self.full {
            return false;
        }
        log::debug!("text: atlas full, resetting ({} entries)", self.cache.len());
        self.cache.clear();
        self.atlas.fill(0);
        self.pending.clear();
        self.cur_x = 0;
        self.cur_y = 0;
        self.row_h = 0;
        self.full = false;
        true
    }

    fn pack(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w > ATLAS || h > ATLAS {
            return None;
        }
        if self.cur_x + w + 1 > ATLAS {
            self.cur_x = 0;
            self.cur_y += self.row_h + 1;
            self.row_h = 0;
        }
        if self.cur_y + h + 1 > ATLAS {
            // No room. Refuse rather than reset: quads earlier in this
            // frame already point into the atlas. The miss is memoized by
            // the caller and the reset happens at the next frame start.
            self.full = true;
            return None;
        }
        let at = (self.cur_x, self.cur_y);
        self.cur_x += w + 1;
        self.row_h = self.row_h.max(h);
        Some(at)
    }

    fn entry(&mut self, ch: char, px: f32) -> Option<&Entry> {
        let font = self.face()?;
        let gid = font.glyph_id(ch);
        let key = (if self.inspector > 0 { 3 + self.inspector } else if self.regular { 3 } else if self.header { 2 } else { u8::from(self.interface) }, gid.0, px.to_bits());
        if !self.cache.contains_key(&key) {
            let glyph = gid.with_scale_and_position(px, ab_glyph::point(0.0, 0.0));
            let made = self.face()?.outline_glyph(glyph).and_then(|og| {
                let b = og.px_bounds();
                let (w, h) = (b.width().ceil() as u32, b.height().ceil() as u32);
                if w == 0 || h == 0 {
                    return None;
                }
                let (ax, ay) = self.pack(w, h)?;
                let atlas = &mut self.atlas;
                og.draw(|x, y, c| {
                    let (px_, py_) = (ax + x, ay + y);
                    if px_ < ATLAS && py_ < ATLAS {
                        let i = (py_ * ATLAS + px_) as usize;
                        atlas[i] = atlas[i].max((c * 255.0) as u8);
                    }
                });
                self.pending.push(Upload { x: ax, y: ay, w, h });
                Some(Entry { ax, ay, w, h, left: b.min.x, top: b.min.y })
            });
            self.cache.insert(key, made);
        }
        self.cache.get(&key).and_then(|e| e.as_ref())
    }

    /// Fit a label before rasterizing it. Count real font advances, including
    /// tracking, rather than guessing a character limit in the app.
    pub fn fit(&self, text: &str, px: f32, tracking: f32, limit: f32) -> String {
        let px = self.raster_size(px);
        let Some(font) = self.face() else { return String::new() };
        let sf = font.as_scaled(px);
        let advance = |c| sf.h_advance(sf.glyph_id(c)) + tracking;
        let width = |s: &str| s.chars().map(advance).sum::<f32>() - if s.is_empty() { 0.0 } else { tracking };
        if width(text) <= limit { return text.to_string(); }
        let ellipsis = "…";
        if width(ellipsis) > limit { return String::new(); }
        let mut result = String::new();
        let mut used = 0.0;
        for c in text.chars() {
            if used + advance(c) + width(ellipsis) > limit { break; }
            used += advance(c);
            result.push(c);
        }
        result.push('…');
        result
    }

    /// Lay out one line at `px` physical pixels, adding `tracking` px of
    /// extra advance per glyph (CSS letter-spacing). Quads are relative
    /// to the line box's top-left.
    pub fn layout(&mut self, text: &str, px: f32, tracking: f32) -> LaidText {
        let px = self.raster_size(px);
        let Some(font) = self.face() else {
            return LaidText { quads: Vec::new(), w: 0.0, h: 0.0 };
        };
        let sf = font.as_scaled(px);
        let (ascent, descent) = (sf.ascent(), sf.descent());
        let line_h = ascent - descent;
        let mut quads = Vec::new();
        let mut pen = 0.0f32;
        for ch in text.chars() {
            let adv = {
                let sf = self.face().unwrap().as_scaled(px);
                sf.h_advance(sf.glyph_id(ch))
            };
            if !ch.is_whitespace()
                && let Some(e) = self.entry(ch, px)
            {
                let inv = 1.0 / ATLAS as f32;
                quads.push(GlyphQuad {
                    x: pen + e.left,
                    y: ascent + e.top,
                    w: e.w as f32,
                    h: e.h as f32,
                    uv: [
                        e.ax as f32 * inv,
                        e.ay as f32 * inv,
                        (e.ax + e.w) as f32 * inv,
                        (e.ay + e.h) as f32 * inv,
                    ],
                });
            }
            pen += adv + tracking;
        }
        // Trailing tracking isn't part of the visible run.
        if tracking != 0.0 && !text.is_empty() {
            pen -= tracking;
        }
        LaidText { quads, w: pen, h: line_h }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interface_font_and_fractional_sizes_have_distinct_cached_glyphs() {
        let mut text = TextCtx::load();
        if text.font.is_none() { return; }
        text.layout("Sources", 24.1, 0.0);
        let first = text.cache.len();
        text.layout("Sources", 24.2, 0.0);
        assert_eq!(text.cache.len(), first * 2, "nearby raster sizes must not alias");
        text.set_interface(true);
        #[cfg(target_os = "macos")]
        assert!(text.ui_font.is_some(), "the Figma font must load on macOS");
        if text.ui_font.is_some() {
            let narrow = text.layout("iii", 24.0, 0.0).w;
            let wide = text.layout("WWW", 24.0, 0.0).w;
            assert!(wide > narrow * 2.0, "interface labels use the proportional face");
            for scale in [1.0, 1.5, 2.0] {
                let fitted = text.fit("Sources Timeline Crop Mask", 12.0 * scale, 0.0, 90.0 * scale);
                assert!(text.layout(&fitted, 12.0 * scale, 0.0).w <= 90.0 * scale + 0.001);
            }
        }
        text.set_header(true);
        #[cfg(target_os = "macos")]
        assert!(text.header_font.is_some(), "bundled JetBrains Mono header font loads");
        assert!(!text.layout("Sources", 24.0, 0.0674).quads.is_empty());
        text.set_interface(false);
        let narrow = text.layout("iii", 24.0, 0.0).w;
        let wide = text.layout("WWW", 24.0, 0.0).w;
        assert!((narrow - wide).abs() < 0.01, "numeric text retains the mono face");
    }

    #[test]
    fn fitted_unicode_labels_respect_measured_width() {
        let mut text = TextCtx::load();
        let label = "長いファイル名 — résumé clip.mp4";
        for (px, tracking, width) in [(11.0, 0.0, 85.0), (22.0, 1.2, 170.0), (11.0, 0.0, 0.0)] {
            let fit = text.fit(label, px, tracking, width);
            assert!(text.layout(&fit, px, tracking).w <= width + 0.001);
            if !fit.is_empty() { assert!(fit.ends_with('…')); }
        }
        assert_eq!(text.fit("clip.mp4", 11.0, 0.0, 400.0), "clip.mp4");
    }
}
