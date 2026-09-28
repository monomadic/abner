# Native UI primitives
Rust / wgpu, no web frontend.
```rust
pub struct RectPx {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A filled rect: optionally rounded, optionally outlined, optionally
/// faded out toward its top edge (the transport scrim).
#[derive(Debug, Clone, Copy)]
pub struct RectItem {
    pub r: RectPx,
    pub color: [f32; 4],
    pub radius: f32,
    pub border_w: f32,
    pub border_color: [f32; 4],
    /// Alpha ramps to zero at the top edge — a bottom-anchored gradient.
    pub fade_up: bool,
    /// The mirror of `fade_up`: opaque at the top edge, gone at the
    /// bottom. The launch plate needs a ground under the traffic lights
    /// as well as one under the message.
    pub fade_down: bool,
    /// Alpha peaks at the horizontal centre and runs out to zero at both
    /// ends — a hairline rule that fades into the ground.
    pub fade_x: bool,
}

impl RectItem {
    pub fn new(r: RectPx, color: [f32; 4]) -> Self {
        Self { r, color, ..Default::default() }
    }
}

impl Default for RectItem {
    fn default() -> Self {
        Self {
            r: RectPx { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            color: [0.0; 4],
            radius: 0.0,
            border_w: 0.0,
            border_color: [0.0; 4],
            fade_up: false,
            fade_down: false,
            fade_x: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VAlign {
    /// `y` is the top of the line box.
    Top,
    /// `y` is the line box's vertical centre.
    Middle,
}

/// Chip drawn behind a text run. Sized from the run's real measured
/// width, so keycaps and pills fit their label exactly.
#[derive(Debug, Clone, Copy)]
pub struct TextBg {
    pub color: [f32; 4],
    pub radius: f32,
    pub pad_x: f32,
    pub pad_y: f32,
    /// Solid offset shadow beneath the chip (keycap depth); alpha 0 = none.
    pub shadow: [f32; 4],
    pub shadow_dy: f32,
}

impl TextBg {
    #[allow(dead_code)] // no chip-backed text right now (keycaps are drawn as shapes)
    pub fn new(color: [f32; 4]) -> Self {
        Self {
            color,
            radius: 0.0,
            pad_x: 6.0,
            pad_y: 3.0,
            shadow: [0.0; 4],
            shadow_dy: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextItem {
    /// Anchor point; `align` decides which edge of the run sits here.
    pub x: f32,
    pub y: f32,
    /// Logical px size.
    pub px: f32,
    pub color: [f32; 4],
    pub text: String,
    pub align: Align,
    pub valign: VAlign,
    /// Extra advance per glyph, logical px (CSS letter-spacing).
    pub tracking: f32,
    pub bg: Option<TextBg>,
}

impl TextItem {
    pub fn new(x: f32, y: f32, px: f32, color: [f32; 4], text: impl Into<String>) -> Self {
        Self {
            x,
            y,
            px,
            color,
            text: text.into(),
            align: Align::Left,
            valign: VAlign::Top,
            tracking: 0.0,
            bg: None,
        }
    }
}

fn cap_width(label: &str) -> f32 {
    (CAP_FONT * CAP_PAD_EM * 2.0 + CAP_FONT * MONO_ADV * label.chars().count() as f32).max(CAP_H)
}

/// One keycap, top-left at `(x, y)`: switchblade's design-system cap
/// (`theme.rs::keycap`, inline size) — a hard drop shadow, a near-black
/// face with a hairline outline, and an inset top highlight held off the
/// corners. That highlight is the whole difference between "a dark
/// rectangle" and "a key".
fn keycap(items: &mut Vec<Item>, x: f32, y: f32, label: &str) {
    let (w, h, r) = (cap_width(label), CAP_H, CAP_R);
    items.push(Item::Rect(RectItem {
        radius: r,
        ..RectItem::new(RectPx { x, y: y + CAP_DROP, w, h }, KEY_SHADOW)
    }));
    items.push(Item::Rect(RectItem {
        radius: r,
        border_w: 1.0,
        border_color: KEY_HAIRLINE,
        ..RectItem::new(RectPx { x, y, w, h }, KEY_SURFACE)
    }));
    let inset = r * 0.55;
    items.push(Item::Rect(RectItem {
        radius: 0.5,
        ..RectItem::new(RectPx { x: x + inset, y: y + 1.0, w: (w - inset * 2.0).max(0.0), h: 1.0 },
            KEY_HIGHLIGHT)
    }));
    items.push(Item::Text(TextItem {
        align: Align::Center,
        valign: VAlign::Middle,
        ..TextItem::new(x + w / 2.0, y + h / 2.0, CAP_FONT, KEY_INK, label)
    }));
}

/// Clip a run to `max` characters, marking the cut with an ellipsis.
fn name_of(path: &std::path::Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// Same, but keeps the TAIL (for paths, where the leaf matters).
fn ellipsize_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max.saturating_sub(1);
    "…".to_string() + &s.chars().skip(skip).collect::<String>()
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0) as u64;
    format!("{m:02}:{:06.3}", t - m as f64 * 60.0)
}

fn fmt_size(b: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let b = b as f64;
    if b >= 1024.0 * MIB {
        format!("{:.2} GiB", b / (1024.0 * MIB))
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}

```
