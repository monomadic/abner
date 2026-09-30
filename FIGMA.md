# Figma → abner: design system rules

How to turn a Figma design into abner code (and back) through the Figma MCP.
Read this before implementing anything from Figma. CLAUDE.md has the wider
architecture; this file is only the design ↔ code contract.

**The Figma file:** [Abner — Screens](https://www.figma.com/design/a5aV8WGfzSy9nWBea6Oqyb)
(file key `a5aV8WGfzSy9nWBea6Oqyb`). Sections: *Launch window (Splash)*,
*Workspace* (Single, Side by side, Difference, Mask, Crop), *Components, tokens
& assets*. Every frame is 1280×800, the default window (`abner.default.toml`
`[window]`), so Figma x/y ARE the app's logical px at that size.

## 0. The one thing that is different here

abner is **not a web app**. There is no React/Vue, no CSS, no DOM, no bundler,
no component tree. The whole UI is one immediate-mode list of draw items,
rebuilt every frame in `src/app.rs` and drawn by a single wgpu pipeline
(`src/render.rs` + `src/shader.wgsl`). So:

- `get_design_context` output (React + Tailwind) is **a spec to read, never
  code to paste**. Translate it into `Item`s.
- A Figma *component* maps to a **builder function** in `app.rs`, not a class.
- Figma *auto-layout* has no runtime equivalent: layout is explicit geometry,
  computed once in `Workspace::new` / `controls()` and shared by drawing AND
  hit testing. Read the resolved x/y/w/h from Figma, then express them the way
  the code already does (offsets from `rail`, `HEADER_H`, etc.), not as magic
  absolute numbers.
- `generate_figma_design` (web capture) does not apply — there is no page to
  capture. To refresh a screen, capture the real window (§8) and rebuild with
  `use_figma`.

## 1. Tokens

### Where they live

| Kind | Code (source of truth) | Figma |
|---|---|---|
| Colours | `const NAME: [f32; 4]` at the bottom of `src/app.rs` (workspace block ~`HEADER_H`…`TOOL_BG`; launch block `LAUNCH_BG`…`FORMATS`; crop `CROP_*`) | Variable collection **Abner** (mode *Dark*): `shell/*`, `text/*`, `accent`, `mask-red`, `transport/track`, `crop/*`, `mask/*`, `launch/*`, `clip/1–9` |
| Clip palette | `clip_color()` in `src/app.rs` (9 hex values, cycles) | `clip/1` … `clip/9` |
| Type | `ui_label(items, x, y, px, color, text, align, max_width)`; sizes 9/10/11/13 | Text styles `mono/13`, `mono/11`, `mono/11 medium`, `mono/10`, `mono/10 tracked`, `mono/9` |
| Metrics | `HEADER_H 38`, `CONTEXT_H 40`, `SOURCE_H 80`, `TRANSPORT_H 60`, `STATUS_H 28`, `TITLEBAR_H 28`, `CROP_*`, `SEEK_GRAB` | Resolved geometry in the frames |
| Behavioural defaults | `abner.default.toml` (window size, view, gain, blend, checker, brush) | — |

Each Figma variable's **description names its code constant** — use it to find
the line to change.

### Format and the one transformation

Colours are authored as **sRGB components in 0–1** (`[r, g, b, a]`), usually
written from hex: `[29.0 / 255.0, 27.0 / 255.0, 27.0 / 255.0, 1.0]` for
`#1d1b1b`, or via `hex_color(0x61b5ee)`. The shader's `ui_color()`
(`src/shader.wgsl`) decodes them to linear because the surface is
`*UnormSrgb` and re-encodes on write. Opaque colours therefore copy 1:1
between Figma and code.

**Alpha does not copy 1:1.** Blending on the GPU is linear-space; Figma blends
in sRGB. A translucent black in code darkens far less than the same alpha in
Figma. The Figma variables carry the *visually equivalent* alpha and the
description carries the code value:

| Token | Code | Figma |
|---|---|---|
| `crop/dim` | `CROP_DIM` black @ **0.95** | @ 0.74 |
| `transport/track` | `TRACK` white @ **0.14** | @ 0.36 |
| marquee label chip | `CROP_CHIP` white @ **0.03** | white @ 0.12 (unbound fill) |

Rule of thumb when going Figma → code for a dark overlay of Figma alpha `a`:
code alpha ≈ `1 − (1 − a)^2.2` (the same lift CLAUDE.md documents for the
Splash card's CSS alphas). Verify with a window capture — it is an
approximation, and bright footage is where it shows.

```rust
// src/app.rs — how a token is declared and used
const CONTROL_BG: [f32; 4] = [26.0 / 255.0, 26.0 / 255.0, 26.0 / 255.0, 1.0]; // #1a1a1a
items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(control.r, CONTROL_BG) }));
```

Adding a token: add the `const` in the right block of `app.rs` with a `///`
comment saying what it is and why, AND add the Figma variable with that
constant's name in its description. There is no generator — keep them in step
by hand.

## 2. Components

| Figma component (Components board) | Code |
|---|---|
| **Badge** (`Clip=1…9`) | `number_badge()` — fill `clip × 0.17 + 0.04`, 1px border clip @0.12, r5 |
| **Tool tab** (`Selected`/`Idle`) | `controls()` → `Action::Tool`, drawn in `build_hud`; `TOOL_BG` + `ACCENT` when selected |
| **Segment** (`Selected`/`Idle`/`Disabled`) | `controls()` → `Action::View/Brush/Save/Export/Param`; `CONTROL_BG` r7 selected, r6 hover; `WORKSPACE_MUTED` while its worker runs |
| **Source row** | the rail loop in `build_hud` (`source_row()`, `SOURCE_H`) |
| **Inspector row** | the inspector loop in `build_hud` (rows 23px apart) |
| **Transport** (`Playing=Yes/No`) | `build_transport`, `btn_prev/btn_play/btn_next/seek_rect` |
| **Status bar** (`Mode=Compare/Mask/Crop`) | `build_status_line` |
| **Crop marquee** | `build_crop_layer` + `build_crop_labels` (size/ratio chip, corner-coordinate chips; `CROP_*` consts) |
| **Traffic lights** | *not drawn by abner* — macOS draws them; shown for layout only |

Architecture rules that a new component must follow:

1. **One rect, two uses.** Anything clickable gets its rectangle from a
   function (`controls()`, `btn_play()`, `source_row()`, `Workspace`) that BOTH
   the builder and the pointer handler call. Never compute a hit rect
   separately from the drawn one.
2. **The renderer measures.** Text width, ellipsis, the logo's ink box and the
   plate's horizon are measured by `render.rs`/`text.rs`. Pass `max_width` to
   `ui_label` and let it truncate; never estimate glyph widths app-side
   (`MONO_ADV` only steps between runs).
3. **Glyph-free controls.** Transport/step/play glyphs are geometry
   (`Item::Triangle`, `Item::Rect`), never font glyphs — the system mono fonts
   have no media-control glyphs and a font's ▶ never centres.
4. **Keys and CLI too.** Every control needs a key (handled in `App::key`) and,
   for anything visual, a way to reach it from the command line (`--view`,
   `--mask`, `--crop`) so it can be verified without injecting keystrokes.

```rust
// A context-row control: geometry in controls(), drawing in build_hud().
out.push(Control::new(RectPx { x, y, w: 122.0, h: 27.0 }, "Export ProRes", false, Action::Export));
// …then build_hud draws every Control the same way (fill if selected/hovered,
// centred 11px label) and mouse_down() hit-tests the same Vec<Control>.
```

There is no Storybook. The component documentation is the Figma
**Components** board plus the doc comments on the builder functions.

## 3. Frameworks & build

- **Language:** Rust 2024, one crate (`Cargo.toml`). Build: `cargo build` /
  `cargo test`; the `.app` is `packaging/build-app.sh`.
- **Windowing:** winit 0.30 (`src/main.rs`). **GPU:** wgpu 30, one pipeline,
  instanced quads in logical px, WGSL in `src/shader.wgsl` (`include_str!`).
- **Text:** ab_glyph over system fonts, R8 atlas (`src/text.rs`).
- **Shader modes** are the "styling primitives": 0 rect (SDF rounded box,
  border, fades), 1 texture, 2–5 compare views, 6 glyph, 7 logo, 8 mask,
  9 triangle, 10–13 launch plate/floor/shadow/overlays. A new visual effect is
  a new mode or a new `RectItem` flag, not a new pipeline.
- Needs ffmpeg **8.x** dev libraries (see CLAUDE.md → Rules).

## 4. Assets

- Everything shipped is **baked into the binary** — `include_bytes!` /
  `include_str!`: `assets/app-icon.png` (`main.rs`), `assets/logo.png` and
  `assets/banner/background-02.png` (`render.rs`), `abner.default.toml`
  (`config.rs`). The one runtime file is the launch loop video
  (`assets/banner/background-02-loop.mp4`, bundled as
  `Contents/Resources/background.mp4`, found via `backdrop_path`).
- **Slots, not references.** `assets/logo.png` and `assets/app-icon.png` are
  SLOTS: an alternate becomes the mark by `cp` (logo) or by
  `scripts/trim-icon.py` (icon — square, full-bleed, opaque, unmasked 1024²).
  Code never names `assets/logo/logo-05.png`.
- Images are decoded once with the `png` crate into mipped textures bound at
  fixed slots of every bind group (logo 5, plate 7). Optimisation = mips +
  measuring the alpha bbox at load (`decode_logo`), so exported padding never
  matters. No CDN; nothing is fetched at runtime.
- Figma's image fills (video frames, logo, plate) are **illustrations**. Don't
  export them back into `assets/`; the real assets are already there.

## 5. Icons

There is no icon set. The UI has no icon glyphs by design; the few pictograms
are geometry:

- transport step/play/pause → `Item::Triangle` + `Item::Rect` in
  `build_transport`;
- crop handles → rects in `build_crop_layer`;
- the READY lamp → two rounded rects.

If a Figma design introduces an icon, draw it from `Rect`/`Triangle` items (or
add a shader mode for a genuinely new shape). Do not add an icon font or SVG
rasteriser. In Figma, such icons are imported SVGs named `icon/<name>` or
live inside the component that owns them (e.g. *Transport → Step back*).

## 6. Styling approach

- No CSS. "Style" is: the token `const`s, the `RectItem` fields
  (`radius`, `border_w`, `border_color`, `fade_up/down/x`), the `TextItem`
  fields (`px`, `tracking`, `align`, `valign`, `max_width`, `bg`), and shader
  modes.
- **Global styles:** the opaque shell surfaces (`WORKSPACE_PANEL`), the canvas
  (`CANVAS_BG`), the clear, and the rule that UI colours are sRGB decoded by
  `ui_color()`.
- **Font:** SF Mono (`FONT_CANDIDATES` in `text.rs`, falling back to Menlo,
  Monaco…). Figma has no SF Mono, so the text styles use **JetBrains Mono** as
  a stand-in — compare spacing, not glyph shapes. Sizes and tracking in the
  text styles are the code's.
- **Responsive rules** live in code, not in Figma frames:
  - rail 280px, 210px (≤ 30% of width) below 900px wide; `Tab` hides the whole
    shell (`Workspace::new(…, visible=false)`);
  - inspector hides below 600px high; minimum window 720×480;
  - transport drops the `frame · speed` readout below 720px; the status bar
    drops key hints below 1000px;
  - the launch window falls back from Splash to the bare mark under 900×520
    (`SPLASH_MIN_W/H`).
  Figma only shows 1280×800. If a design needs a breakpoint, express it as one
  of these threshold checks and mention it in the frame's description.
- **Z-order is push order.** Later `Item`s draw on top; `Item::Clip` scissors
  (the canvas clip keeps zoomed video and mask/crop dimming out of the shell).

## 7. Project structure

```
src/main.rs      CLI, winit loop, macOS window/titlebar/icon, drops, Open With
src/schedule.rs  redraw cadence (switchblade's, verbatim)
src/app.rs       master clock, input, ALL UI layout + drawing, tokens (consts)
src/render.rs    wgpu pipeline, Item types, textures, logo/plate decoding
src/shader.wgsl  every visual primitive (modes 0–13), ui_color()
src/text.rs      font loading + glyph atlas
src/mask.rs      masks, crop geometry (Crop), PNG/ProRes export
src/player.rs    libav decode, one player per clip
src/probe.rs     ffprobe at startup
src/config.rs    abner.default.toml overlay
src/open.rs + open_shim.m   Open With / double-click
assets/          baked-in images (slots) + the launch loop video
packaging/       .app build, icon check
scripts/         window-id.swift (captures), trim-icon.py
```

Feature organisation is by module, and inside `app.rs` by builder:
`build_hud` (shell), `build_transport`, `build_status_line`,
`build_mask_layer` / `build_crop_layer`, `launch_frame`. A new surface gets its
own `build_*` function called from `tick`; its layout rects come from
`Workspace` or a small `fn …_rect(&self) -> RectPx`.

## 8. Workflow: implementing a Figma change

1. `get_screenshot` / `get_metadata` on the frame or component (file key
   above). Treat `get_design_context` code as a spec only (§0).
2. Map every colour to a variable → its code `const` (the variable
   description names it). New colour ⇒ new `const` + new variable.
3. Map geometry to the existing layout functions; change `Workspace`,
   `controls()` or the `*_H` consts rather than hard-coding positions in a
   builder.
4. Implement, `cargo test` (sync is the product — keep it green).
5. Verify with a targeted window capture, never injected keystrokes:
   ```bash
   screencapture -x -o -l "$(swift scripts/window-id.swift | head -1)" shot.png
   ```
   Use `--view <mode>`, `--mask`, `--crop x,y,w,h` to reach the state; make
   test clips with `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -t 6 -g 30 a.mp4`.
   Compare the capture (2560×1600 on a retina display = 2× the frame) with the
   Figma frame.
6. **Code → Figma:** when the UI changes in code, update the Figma screen with
   `use_figma` (edit the component, not each screen, where possible) so the file
   stays the picture of what ships. Keep alpha tokens in their sRGB-equivalent
   form (§1).
