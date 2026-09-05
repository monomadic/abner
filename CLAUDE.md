# abner — agent notes

A/B video comparison player: N videos decoded in frame-locked sync, flipped/diffed
on screen. Deliberately slim — one crate, six modules, no config file, no cache.
Sibling project: `~/src/switchblade` (the graphics learnings came from there; its
CLAUDE.md documents the deeper media/render rationale).

**[TASKS.md](TASKS.md) is the numbered, priority-ordered list of open work;
[HISTORY.md](HISTORY.md) records what landed and why.** When a task lands, move it
from one to the other, keeping its number.

## Architecture

- `src/main.rs` — CLI + winit loop. The redraw cadence lives in `src/schedule.rs`
  (switchblade's module, verbatim, with its tests): input wakes are optimistic,
  `animating` decides if the loop stays hot, occluded windows never run the continuous
  path (no vsync present to pace them = pegged core), `MIN_FRAME` floors the Poll
  cadence, idle ticks at 100ms. `about_to_wait` is the only caller — don't grow a
  second copy of the rules there. Fake fullscreen = `set_simple_fullscreen` +
  `setHasShadow(false)` (macOS Tahoe draws its window contour with the shadow).
  **No visible titlebar** (`set_titlebar_glass`, switchblade's): transparent bar,
  hidden title, and `FullSizeContentView` so the wgpu surface runs UNDER the strip —
  a transparent bar alone shows the default system grey, not the app's clear, so the
  only way to match exactly is to let the same GPU clear paint it. The traffic lights
  stay, floating over the content, which is why the top HUD row is offset by
  `App::top_inset` (`TITLEBAR_H`, zero in fake fullscreen — that window is borderless).
  The title is still SET, just hidden, so anything reading it sees the clip names.
  The Dock icon for the BARE binary is `include_bytes!`'d from `assets/app-icon.png`
  (the icon SLOT — `packaging/build-app.sh` renders the bundle's `.icns` from the same
  file, so the two can't drift; the alternates in `assets/icons/` become the icon by
  being run through `scripts/trim-icon.py <alternate> assets/app-icon.png`, not
  `cp`. **macOS 26 masks the icon to its own squircle only if the icon FILLS its
  canvas**; hand it artwork floating in a margin — which is how every render in
  `assets/icons/` arrives, a shape at ~89% of its canvas with transparent
  surround — and the system instead drops it on a white plate and shrinks it
  inside, a padded undersized Dock tile that no amount of trimming or centring
  fixes, because the margin IS the trigger. The MARGIN is the trigger, not the
  alpha — artwork whose alpha follows the tile edge composites fine — so the
  script trims to the alpha bbox, scales just past the canvas (`--zoom`, default
  1.05, the measured point where the art covers the mask; 1.00 leaves 0.9% bare
  and the plate returns), centre-crops to 1024² and masks to a superellipse cut
  slightly WIDER than Apple's corner. Surplus alpha the system mask cuts; a
  deficit brings the plate back. Only the render's rounded corners are lost, and
  pre-Tahoe still gets a rounded icon. Switchblade solved the same problem by
  brute force — its `AppIcon.icns` is 1024² with zero alpha, fully opaque — which
  is why its Dock icon always looked right on an identical
  `CFBundleIconFile`-only bundle. Verify a change by asking macOS what it
  composites (`NSWorkspace.icon(forFile:)` on the built .app, rendered to a PNG)
  rather than by eye: the plate is invisible in the source asset) and pushed to `NSApp.setApplicationIconImage` at startup — a
  bare Mach-O has no `CFBundleIconFile`, and running from a shell is the common case
  here. **Drops** are switchblade's `FilesDropped` path: winit sends one `DroppedFile`
  per file with no end-of-batch marker, so `window_event` accumulates into `dropped`
  and `about_to_wait` flushes the whole gesture at once (without the batch, a pair
  dropped together would land as two one-file loads). ⌘ (replace, not add) is read
  from the HARDWARE modifier state — a drag from Finder never focuses this window, so
  no `ModifiersChanged` ever reported the key. `Runner::files_dropped` probes and
  spawns right there on the loop, which `probe`'s deadline makes safe, and logs-and-
  skips anything that fails: a drop is a guess by definition. **`set_app_icon()` returns early when the executable sits under
  `Contents/MacOS`**: inside a bundle that call OVERRIDES `AppIcon.icns` at launch, and
  switchblade lost a day to it (a stale baked-in PNG read exactly like an icon cache
  that wouldn't clear). AppKit decodes the PNG, so no image crate is pulled in; other
  platforms want an already-decoded RGBA buffer, so it's a no-op there.
- `src/player.rs` — adapted from switchblade's `SeekablePlayer` (in-process libav via
  rsmpeg, VT decode for h264/hevc/prores only, content-relative time, bounded queue,
  drop-wakes-the-parked-reader). **Key difference: no per-player pacing.** Players queue
  `(pts, rgba)`; the app owns ONE master clock and drains each player with
  `take_upto(t)` (pop all due, newest wins). Sync is by construction, pause is "stop
  advancing t" (backpressure stalls decoders), EOF parks the reader until a seek.
  Decode is at native resolution — pixels are the product here, nothing scales.
  Three robustness rules ported from switchblade (2026-09-04), each a shipped bug there:
  `Drop` takes the `frames` lock for an instant before notifying (a store+notify between
  the reader's `closed` check and its wait is a lost wakeup = leaked thread); an AVIO
  interrupt callback is installed BEFORE `avformat_open_input` so a drop reaches a
  reader wedged in libav I/O on a dead mount (`dropped_player_interrupts_a_reader_blocked_in_libav_io`);
  a failed seek FAILS the player rather than continuing from wherever it was, because a
  silently unsynced stream is the one thing this product must never show.
- `src/probe.rs` — one ffprobe per input at startup, synchronous on the main thread
  before any window exists, so it runs under a hard deadline (`run_deadlined`): a child
  stuck on a dead volume otherwise looks exactly like a crash.
- `src/app.rs` — master clock, modes, input, UI overlay. **Zoom** is photo-style: one
  shared `(zoom, center)` where `center` is the content point (0..1) held mid-view —
  every video applies it to its own fit rect, so pan/zoom position stays synced across
  streams and side-by-side cells; pinch anchors on the pointer (solve for the content
  point under the cursor, keep it there), `clamp_center` pins the view inside the
  content, Z resets. **Speed** (`[`/`]`, Backspace) just scales the master clock's dt —
  decoders need no notion of rate (backpressure absorbs slow, frame-dropping in
  `take_upto` absorbs fast). Framestep = exact seek to
  `t + 0.5/fps` (forward) / `t − 1.5/fps` (back) — half-period offsets so pts rounding
  can't re-land on the same frame — then the delivered frame's true pts is ADOPTED as
  `t` (`pending` flags + `take_next`). The clock wraps at the shortest stream duration
  and exact-seeks everyone to 0.
- `src/render.rs` — one wgpu pipeline for everything (rects, video quads, compare
  modes, glyphs, the logo), instanced quads in logical px. Per-video textures carry a blit-filled
  mip chain (4K fit-to-window without shimmer). Bind groups are cached per (A,B) texture
  pair; keyless items (rects/text) ride the current batch. `TextItem` carries
  align/valign/tracking and an optional rounded chip: the renderer owns the font, so
  it MEASURES each run — never estimate glyph positions app-side (`MONO_ADV` exists
  only to step *between* runs). The **wordmark** (`assets/logo.png`) is
  `include_bytes!`'d and decoded with the `png` crate — the Dock icon goes through
  AppKit, but a texture needs the pixels in-process — into a mipped texture bound at
  slot 5 of EVERY bind group, so `Item::Logo` needs no batch key of its own. The
  renderer owns the image, so it MEASURES it: `decode_logo` takes the alpha bounding
  box and hands `App` the trimmed aspect plus the uv rect to draw, so the launch
  layout doesn't inherit whatever margin the export left (logo.png is padded ~12% top,
  ~18% bottom — drawn whole, the mark sits visibly high in its own box). Same rule as
  text: never estimate what the renderer can measure.
  The surface is **transparent** (`with_transparent` in main.rs + a premultiplied
  alpha mode): `FrameDesc::clear` carries an alpha and the clear colour is scaled by
  it, so the desktop shows faintly through the app background and the letterbox while
  opaque video quads (they write alpha 1) stay solid.
- `src/shader.wgsl` — modes: 0 rect, 1 tex, 2 delta, 3 split, 4 checker, 5 blend,
  6 glyph, 7 logo. Textures are sampled unconditionally then selected (uniform-control-flow
  rule), `mode` is a flat varying. Mode 0 is an SDF rounded box with `fwidth`-based
  1px AA, an optional border (colour smuggled through the unused `uv` slot) and a
  bottom-anchored scrim ramp. **UI colours are authored as sRGB hex and decoded by
  `ui_color()`** — the surface is `*UnormSrgb` and re-encodes on write, so a raw sRGB
  value lands pale. Same reason panel/scrim alphas run high (0.8–0.97): blending is
  linear-space, so 0.6 alpha barely dims bright footage.
- `src/text.rs` — ab_glyph over system fonts (SF Mono/Menlo/…), 2048² R8 shelf-packed
  atlas, glyphs rasterized at physical px and drawn at logical size, optional tracking.
  New glyphs upload as their own rects (`pending`), never the whole atlas. The atlas is
  NEVER reset mid-frame — earlier text in the frame has already baked its UVs — so a
  full atlas refuses the glyph, memoizes the miss, and `begin_frame()` wipes at the next
  frame start (then the renderer re-uploads the whole texture).
  The system mono fonts have NO media-control glyphs (⏮ ⏸ ⏭ ⏎ render as nothing) —
  use the geometric block (◀ ▶ ●) or draw the shape from rects.

## Design source

The HUD implements **2a** from the Claude Design project "A/B testing window mockups
for Abner" (`Abner AB Window.dc.html`, project `e16025af-9465-4a81-bdc3-97780f3399eb`,
read via the DesignSync tool). 2b (launch/empty state) is implemented too, and its drop
targets are live: a filled slot shows the clip's name and `● WxH · codec` behind a
solid border, and a drag over the window brightens the empty ones (winit gives no
drop POSITION, so both light together and the file fills the next free slot).
Deliberate deviations from the mock are noted where they occur: higher panel alphas
and a saturating scrim (bright real footage, not the mock's dark plate), `ENTER`
instead of ⏎, and solid rather than dashed drop-zone borders.

**The palette comes from the logo, not the mock.** 2a's lime (#a6e22e) read as a
different product next to `assets/logo.png`, so `ACCENT` is the mark's upper bar
(#006dcf, lifted to #1580de — the bar itself clears only ~4:1 against the HUD's black,
under the bar for 10–11px mono) and `ACCENT_B` its lower one (#e71b24, used as-is) —
the launch window gives slot A the blue and slot B the red, so the pair on screen is
the pair on the mark above it.
Both are far more saturated than lime, so the drop-zone washes run thinner than the
mock's: blending is linear-space, and 5% of a primary already reads as a coloured
panel. The zone that the NEXT drop lands in is the bright one; later empty slots stay
quiet. The launch window's wordmark is the logo IMAGE, not type — it already carries
the "VIDEO QUALITY TESTING TOOLKIT" line that used to be a second text run.

## Rules

- `cargo test` generates tiny ffmpeg test clips; the suite covers master-clock
  draining, exact seek, two-player sync, framestep adoption, reader-thread cleanup
  (condvar-parked AND wedged-in-libav-I/O, the latter via a mkfifo dribble), and the
  redraw cadence (`schedule::tests`), and slot-filling drops (0 → 1 → 2 → 3, asserting
  the clock rewinds and the streams stay inside a frame period of each other).
  Keep it green — sync IS the product.
- Building needs the ffmpeg 8.x dev libraries (brew ffmpeg) — same as switchblade.
  The only other non-obvious dependency is `png`, for the wordmark texture.
- **`./packaging/build-app.sh [--open|--install]` builds `Abner.app`** — switchblade's
  recipe: release build, `assets/app-icon.png` → `AppIcon.icns`, `Info.plist.in` with
  version + git hash, every non-system dylib copied into `Contents/Frameworks` with
  load paths rewritten to `@rpath`, ad-hoc codesign (mandatory on Apple Silicon after
  `install_name_tool`), then `lsregister -f` so an in-place reinstall doesn't keep the
  old icon. `CFBundleExecutable` is a thin launcher that prepends the Homebrew bin dirs
  to PATH, because a Finder-launched app gets no PATH and the startup `ffprobe` would
  fail. The plist declares NO document types yet — that lands with the open-files
  delegate (TASKS.md 3), or Open With would show an empty launch window.
- Verify visual changes with a targeted window capture, never by injecting global
  keystrokes — a `--view` flag exists so every mode is reachable from the CLI.
  `scripts/window-id.swift` prints the window id:
  `screencapture -x -l "$(swift scripts/window-id.swift | head -1)" shot.png`.
  `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -t 6 -g 30 a.mp4` makes a clip
  (vary `eq=brightness` for a B that actually differs).
