# Theme
Black video canvas; blue #1580de focus; red #e71b24 mask; system monospace. Logical px; 28px titlebar, 34px status bar. Existing 430px metadata strips and giant letters compete with video.
```rust
// ---- 2a "Instrument HUD, remixed" palette ----
/// Signal accent — the one hot colour in the design, and where the whole
/// palette comes from: 2a's lime read as a different product next to the
/// logo. This is the mark's upper bar (`assets/logo.png`, #006dcf) lifted
/// to #1580de, the one deliberate departure from the file: the bar itself
/// clears only ~4:1 against the HUD's black, which is under the bar for
/// 10–11px mono. Side by side with the mark it still reads as the same
/// blue; illegible status text would not.
const ACCENT: [f32; 4] = [0.082, 0.502, 0.871, 1.0];
/// Hairlines and pill outlines drawn in the accent, well under full.
const ACCENT_EDGE: [f32; 4] = [0.082, 0.502, 0.871, 0.28];
/// Frame background / ink on the accent (#050506). Opaque: the window
/// no longer lets the desktop through.
const FRAME_BG: [f32; 4] = [0.0196, 0.0196, 0.0235, 1.0];
const FRAME_INK: [f32; 4] = [0.0196, 0.0196, 0.0235, 1.0];
const TEXT: [f32; 4] = [0.941, 0.941, 0.949, 1.0];
const TEXT_OFF: [f32; 4] = [0.784, 0.784, 0.824, 0.85];
const DETAIL: [f32; 4] = [0.824, 0.824, 0.863, 0.95];
const DIM_PATH: [f32; 4] = [0.588, 0.588, 0.627, 0.75];
const DETAIL_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.82];
const DIM: [f32; 4] = [0.588, 0.588, 0.627, 0.9];
const LABEL: [f32; 4] = [0.784, 0.784, 0.804, 0.85];
const INACTIVE: [f32; 4] = [0.706, 0.706, 0.745, 0.9];
const SEG_OFF: [f32; 4] = [0.902, 0.902, 0.922, 0.8];
const TIME_OFF: [f32; 4] = [0.471, 0.471, 0.510, 0.85];
const GLYPH: [f32; 4] = [0.824, 0.824, 0.843, 0.85];
const ERR: [f32; 4] = [1.0, 0.35, 0.3, 1.0];
const PILL_BG: [f32; 4] = [0.016, 0.016, 0.024, 0.62];
// Panel alphas run high on purpose: see the scrim note in shader.wgsl —
// linear-space blending means 0.6 alpha barely dims bright footage.
const ROW_BG_ON: [f32; 4] = [0.0, 0.0, 0.0, 0.90];
const ROW_BG_OFF: [f32; 4] = [0.0, 0.0, 0.0, 0.82];
const RULE_OFF: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
const SCRIM: [f32; 4] = [0.016, 0.016, 0.024, 0.97];
const TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
const KNOB: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// Status line, tinted by mode and only just see-through: dark blue in
/// A/B (#0b1d3a), dark red in mask (#3a0b10). Mask's chip takes the
/// logo's lower bar (#e71b24), as A/B's takes the upper.
const STATUS_BG_AB: [f32; 4] = [0.043, 0.114, 0.227, 0.93];
const STATUS_BG_MASK: [f32; 4] = [0.227, 0.043, 0.063, 0.93];
const MASK_RED: [f32; 4] = [0.906, 0.106, 0.141, 1.0];
/// The hairline over the transport: faint grey, not the accent.
const TRANSPORT_RULE: [f32; 4] = [1.0, 1.0, 1.0, 0.07];
/// Keycaps — switchblade's design system (`switchblade.toml` [theme] and
/// [theme.keycap], the inline 22px cap): surface, hairline, highlight,
/// shadow and ink are its tokens verbatim.
const KEY_SURFACE: [f32; 4] = [0.043, 0.051, 0.067, 1.0];
/// Fainter than switchblade's 0.11 hairline token: the outline should barely
/// register; the face, highlight and drop shadow carry the cap.
/// Blending is linear-space, so even 0.05 white lands as a clearly grey edge.
const KEY_HAIRLINE: [f32; 4] = [1.0, 1.0, 1.0, 0.018];
const KEY_HIGHLIGHT: [f32; 4] = [1.0, 1.0, 1.0, 0.07];
const KEY_SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.55];
const KEY_INK: [f32; 4] = [0.957, 0.961, 0.969, 0.9];
const CAP_H: f32 = 22.0;
const CAP_R: f32 = 5.0;
const CAP_FONT: f32 = 13.5;
const CAP_PAD_EM: f32 = 0.58;
const CAP_DROP: f32 = 3.0;
/// The word beside each cap: full ink, a touch larger than the old dim
/// label, so what a key DOES reads as easily as the key.
const KEY_LABEL_PX: f32 = 12.0;

// Launch window — the design system's `Splash` surface.
const LAUNCH_BG: [f32; 4] = [0.027, 0.027, 0.035, 1.0];
/// Smallest window that still gets the plate. Under it the vanishing
/// point leaves the frame and the horizon stops reading, so the launch
/// window falls back to the mark centred on the flat ground.
const SPLASH_MIN_W: f32 = 900.0;
const SPLASH_MIN_H: f32 = 520.0;
/// Where the plate's horizon is driven to, down the window.
const HORIZON_Y: f32 = 0.578;
/// Clear space each side of the mark at the plate's size.
const LOCKUP_CLEAR: f32 = 80.0;
/// The lockup's width, as a fraction of the frame (the card's 61%).
const LOCKUP_W: f32 = 0.61;
/// The mark stands ON the line: its baseline sits this fraction of its
/// own height above it — the card's 5px under a 130px lockup.
const LOCKUP_LIFT: f32 = 0.04;
/// Three stacked shadows, (dy, blur, alpha) in the card lockup's px: a
/// tight contact shadow, a wider fall, and an ambient halo that
/// separates the metal from the lit horizon. Tighter than the card's
/// (6/9, 14/20, 0/16): at the card's drops the tagline's shadow parts
/// from the tagline and reads as a second, blurred line of type. Alphas
/// run above the card's for the linear-blending reason the scrims do.
const SHADOW_PASSES: [(f32, f32, f32); 3] = [(2.0, 4.0, 0.80), (5.0, 10.0, 0.55), (0.0, 9.0, 0.65)];
/// The floor projection: pinhole distance as a multiple of the mark's
/// width, the floor's tilt from the screen plane, and how much light
/// the floor gives back.
const FLOOR_PERSP: f32 = 1.36;
const FLOOR_TILT: f32 = 1.0472; // 60° — shallower than the card's 72°, so it runs further toward the viewer
const FLOOR_ALPHA: f32 = 0.15;
/// How far below the horizon the reflection starts, as a fraction of the
/// mark's height — a gap of floor between the tagline and its mirror,
/// so the two never read as one doubled line.
const FLOOR_DROP: f32 = 0.10;
/// The brand's `void` (#06070a): the ground every Splash layer darkens to.
const VOID: [f32; 4] = [0.024, 0.027, 0.039, 1.0];
/// Ground under the traffic lights and under the message.
const SPLASH_SCRIM: [f32; 4] = [0.024, 0.027, 0.039, 0.80];
/// The brand's `scanline` token is white at 0.07, composited in sRGB;
/// blended in linear space that would glow, so it runs far lower here.
const SCANLINE: [f32; 4] = [1.0, 1.0, 1.0, 0.012];
/// How far the foot's last line sits off the bottom edge, as a fraction
/// of the frame (the card's 41px of 400).
const FOOT_BOTTOM: f32 = 0.10;
/// The foot's height (rule, instruction, formats and the two steps
/// between them) — what centres the lockup in the bare fallback.
const FOOT_H: f32 = 51.0;
/// The instruction (#c3ccd6), the design system's `ink-muted` for the
/// labels, and the hairline above them (#616b7b at half).
const INSTRUCTION: [f32; 4] = [0.765, 0.800, 0.839, 1.0];
const INK_MUTED: [f32; 4] = [0.604, 0.651, 0.706, 1.0];
const RULE: [f32; 4] = [0.380, 0.420, 0.482, 0.5];
/// The logo's own blue and red (the brand's `blue` #00adfa and `red`
/// #fc0a1c), for the ticks either side of the formats.
const BRAND_BLUE: [f32; 4] = [0.0, 0.678, 0.980, 1.0];
const BRAND_RED: [f32; 4] = [0.988, 0.039, 0.110, 1.0];
/// The status lamp (`green` #16d97e) and its glow.
const LAMP: [f32; 4] = [0.086, 0.851, 0.494, 1.0];
const LAMP_GLOW: [f32; 4] = [0.086, 0.851, 0.494, 0.07];
/// What the launch window says it opens.
const FORMATS: &str = "MP4 · MOV · MKV · Y4M";

// Crop marquee. The dimmer runs high for the reason the HUD's panels do
// (see shader.wgsl): linear-space blending means a modest alpha barely
// touches bright footage, and this one has to read as "not exported".
const CROP_DIM: [f32; 4] = [0.0, 0.0, 0.0, 0.72];
const CROP_LINE: [f32; 4] = [1.0, 1.0, 1.0, 0.95];
const CROP_INK: [f32; 4] = [0.0, 0.0, 0.0, 0.75];
const CROP_LINE_W: f32 = 1.5;
const CROP_DASH: f32 = 7.0;
const CROP_GAP: f32 = 5.0;
const CROP_HANDLE: f32 = 9.0;
/// Half-size of a corner's grab square — a little wider than it is drawn.
const CROP_GRAB: f32 = 11.0;

/// Info block width, and how many monospace chars fit inside it.
const INFO_W: f32 = 430.0;
const INFO_CH: usize = ((INFO_W - 16.0) / (10.5 * MONO_ADV)) as usize;

/// Transport strip: 13px pad + 32px controls + 11px gap + the status line.
const TRANSPORT_H: f32 = 56.0 + STATUS_H;
/// Bottom status line (mode chip + keycaps), and its mode chip's fixed
/// width — constant across modes, like helix's, so the keys never shift.
const STATUS_H: f32 = 34.0;
const MODE_W: f32 = 92.0;
/// The play triangle, matched to the 12px pause bars it swaps with.
const PLAY_W: f32 = 11.5;
const PLAY_H: f32 = 13.0;
/// Extra scrim drawn above the strip so the gradient's transparent end
/// falls on bare frame rather than on the controls.
const SCRIM_LEAD: f32 = 54.0;
/// Width reserved right of the seek bar for the status readout.
const STATUS_RESERVE: f32 = 330.0;
/// Extra grab margin above/below the 5px seek track.
const SEEK_GRAB: f32 = 9.0;
/// Pointer stillness before the transport starts fading, and the fade.
const TRANSPORT_HOLD_S: f32 = 2.6;
const TRANSPORT_FADE_S: f32 = 0.45;
/// Advance width of the monospace UI font, in em — used only to step
/// between right-aligned status segments and keycap chips, never to
/// place a glyph (the renderer measures those exactly).
const MONO_ADV: f32 = 0.60;

/// Logical height of a standard macOS titlebar. The window has no visible
/// bar (`set_titlebar_glass` makes it transparent and runs the content
/// under it), but the traffic-light buttons still float in that strip, so
/// the top HUD row is pushed below them — everything else goes edge to
/// edge. Zero in fake fullscreen: that window is borderless, buttons and
/// all.
const TITLEBAR_H: f32 = 28.0;

/// Width of a keycap for `label` — switchblade's rule: `pad_em` of air
/// each side of a monospace run, floored at the cap height so a single
/// glyph stays square.
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
