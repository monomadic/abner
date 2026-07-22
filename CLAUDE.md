# abner — agent notes

A/B video comparison player: N videos decoded in frame-locked sync, flipped/diffed
on screen. Deliberately slim — one crate, five modules, no config file, no cache.
Sibling project: `~/src/switchblade` (the graphics learnings came from there; its
CLAUDE.md documents the deeper media/render rationale).

## Architecture

- `src/main.rs` — CLI + winit loop. Carries switchblade's loop rules: input wakes are
  optimistic, `animating` decides if the loop stays hot, occluded windows never run the
  continuous path (no vsync present to pace them = pegged core), `MIN_FRAME` floors the
  Poll cadence, idle ticks at 100ms. Fake fullscreen = `set_simple_fullscreen` +
  `setHasShadow(false)` (macOS Tahoe draws its window contour with the shadow).
- `src/player.rs` — adapted from switchblade's `SeekablePlayer` (in-process libav via
  rsmpeg, VT decode for h264/hevc/prores only, content-relative time, bounded queue,
  drop-wakes-the-parked-reader). **Key difference: no per-player pacing.** Players queue
  `(pts, rgba)`; the app owns ONE master clock and drains each player with
  `take_upto(t)` (pop all due, newest wins). Sync is by construction, pause is "stop
  advancing t" (backpressure stalls decoders), EOF parks the reader until a seek.
  Decode is at native resolution — pixels are the product here, nothing scales.
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
- `src/render.rs` — one wgpu pipeline for everything (flat rects, video quads, compare
  modes, glyphs), instanced quads in logical px. Per-video textures carry a blit-filled
  mip chain (4K fit-to-window without shimmer). Bind groups are cached per (A,B) texture
  pair; keyless items (text) ride the current batch. sRGB end to end.
- `src/shader.wgsl` — modes: 0 flat, 1 tex, 2 delta, 3 split, 4 checker, 5 blend,
  6 glyph. Textures are sampled unconditionally then selected (uniform-control-flow
  rule), `mode` is a flat varying.
- `src/text.rs` — ab_glyph over system fonts (SF Mono/Menlo/…), R8 shelf-packed atlas,
  glyphs rasterized at physical px and drawn at logical size.

## Rules

- `cargo test` generates tiny ffmpeg test clips; the suite covers master-clock
  draining, exact seek, two-player sync, framestep adoption, reader-thread cleanup.
  Keep it green — sync IS the product.
- Building needs the ffmpeg 8.x dev libraries (brew ffmpeg) — same as switchblade.
- Verify visual changes with a targeted window capture
  (`screencapture -l $(window id)`), never by injecting global keystrokes — a
  `--view` flag exists so every mode is reachable from the CLI.
