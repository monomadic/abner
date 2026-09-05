# abner — history

Completed work, newest first. Task numbers refer to [TASKS.md](TASKS.md) where a task
existed there before it landed; earlier entries predate the task list.

## 2026-09-05 — logo on the launch window, palette from the logo, transparent window

The launch window's wordmark is now `assets/logo.png` instead of tracked-out "ABNER"
type. The image already carries the "VIDEO QUALITY TESTING TOOLKIT" line, so the
second text run went with it.

- The renderer owns the mark, the way it owns the font: `include_bytes!` +
  the `png` crate (the Dock icon is decoded by AppKit, but a TEXTURE needs the pixels
  in-process, and nothing else in the tree can produce them), into a mipped
  `Rgba8UnormSrgb` texture — it is drawn at ~460 logical px from a 2056px source, so
  the mip chain is doing real work. Bound at slot 5 of EVERY bind group, so shader
  mode 7 rides whatever batch is current and needs no key. And it MEASURES the file:
  `decode_logo` takes the alpha bounding box and hands `App` the trimmed aspect plus
  the uv rect to draw, because logo.png carries ~12% margin at the top and ~18% at the
  bottom and drawn whole the mark sits visibly high in its own box. App-side code
  never guesses the proportions — the rule the text stack already follows.
- **Palette re-cut from the mark.** 2a's lime (#a6e22e) read as a different product
  sitting under the logo, so `LIME*` became `ACCENT*` = the mark's upper bar (#006dcf)
  and `ACCENT_B` = its lower bar (#e71b24, used as-is). The blue is the one deliberate
  departure from the file: at #006dcf it clears only ~4:1 against the HUD's black,
  which is under the bar for 10–11px mono, so the accent ships lifted to #1580de —
  next to the mark it still reads as the same blue, and illegible status text would
  not have. The launch window gives slot A the blue and slot B the red — the pair on
  screen is the pair on the mark
  directly above it. Both hues are far more saturated than lime, so the zone washes
  were re-weighted DOWN (0.05 → 0.035): blending is linear-space, and 5% of a primary
  already reads as a solid coloured panel. The "which slot fills next" affordance the
  original got from lime-vs-white is kept explicitly — the next free zone is the
  bright one, later empty zones sit at a twelfth of the fill and a fifth of the
  border.
- **The window is slightly transparent.** `with_transparent(true)`, a premultiplied
  surface alpha mode (falling back through post-multiplied to opaque), and
  `FrameDesc::clear` widened to RGBA — the clear colour is scaled by its own alpha at
  the `LoadOp`, since the blend state accumulates premultiplied. Backgrounds run at
  0.92 (frame) and 0.90 (launch). Video quads write alpha 1, so the picture itself is
  never see-through; only the app background, the letterbox and the launch plate let
  the desktop through. Verified from the window capture's own alpha channel
  (background pixel `07 07 09 e7`), not by eye — a capture composited on white looks
  identical to an opaque window.

## 2026-09-04 — no visible titlebar

Switchblade's glass titlebar: `setTitlebarAppearsTransparent` +
`NSWindowTitleHidden` + `FullSizeContentView`, so the wgpu surface fills the strip and
the frame runs edge to edge. The transparent bar on its own would show the default
system grey rather than the app's clear — the content view has to extend under it for
the strip to match. Traffic lights kept (they float over the content), so top-anchored
HUD rows — corner brackets, the A|B pill, the info block, the launch window's
brackets — are offset by `App::top_inset()`: `TITLEBAR_H` (28) windowed, 0 in fake
fullscreen, where the window is borderless. The window title is still set, only hidden.

## 2026-09-04 — drop-to-load (TASKS.md 1)

Drag clips onto the window and they load. Slots fill in drop order: one file fills
**A** and the launch window stays up half filled, two land as **A**/**B** and start
playing, more add C, D…. A drop onto a running comparison ADDS streams; ⌘ held at drop
time replaces the whole set. `abner one.mp4` now opens the same half-filled window
instead of erroring out.

- Event plumbing is switchblade's `FilesDropped` path (`sb-window/src/lib.rs`): winit
  reports one `DroppedFile` per file with no end-of-batch marker, so they accumulate in
  `window_event` and flush as ONE gesture from `about_to_wait` — without the batch,
  dropping a pair would land as two separate one-file loads. ⌘ is read from the
  hardware modifier state (`os_primary_modifier_down`), because a drag from Finder
  never focuses this window and so never sends a `ModifiersChanged`.
- `App::add_videos` rewinds EVERY stream to 0 on a load. A clip that kept its position
  while the arrivals decoded from the top would be silently unsynced — the one thing
  this product must never show. `App::ready()` (two or more streams) now gates the
  launch window, the transport hit-testing and the keymap.
- `Gpu::set_video_dims` rebuilds the per-video textures at runtime. Slots whose
  dimensions are unchanged KEEP their texture, so appending a C doesn't blank A and B
  until their next frame lands; any real change drops the pair-bind-group cache, whose
  entries hold views into those textures.
- Probing on the event loop is safe because `probe::run_deadlined` already bounds it —
  a dropped file on a dead mount fails in bounded time instead of hanging the window.
  A file that fails to probe or spawn is logged and skipped, not fatal: a drop is a
  guess by definition.
- The 2b launch window's targets are live state now: a filled slot shows the clip's
  name, `● WxH · codec` and a solid lime border, and a drag over the window brightens
  the empty ones (winit reports no drop POSITION, so both light together and the file
  fills the next free slot).
- New test `dropped_clips_fill_slots_in_order_and_stay_synced`: 0 → 1 (not ready) → 2
  (plays) → 3 (appends), asserting the clock rewinds and the three streams' shown pts
  stay inside a frame period.

## 2026-09-04 — .app bundling (TASKS.md 2)

- `packaging/build-app.sh` + `packaging/Info.plist.in`, switchblade's recipe minus
  document icons and asset folders: bundled ffmpeg dylibs rewritten to `@rpath`,
  ad-hoc codesign, PATH-fixing launcher, LaunchServices refresh, `--install`/`--open`/
  `--with-cli-tools`/`--sign`/`--debug`. Verified: the bundle runs with an empty PATH
  (dylibs + ffprobe lookup), and `open Abner.app` shows the launch window. The
  `cargo-bundle` metadata went — it can't rewrite dylib paths, so its bundle only ran
  on the machine that built it.
- `assets/app-icon.png` is now the icon slot for both the `.icns` and the bare
  binary's Dock icon. `set_app_icon()` returns early inside a bundle (the
  `setApplicationIconImage`-overrides-the-plist trap).
- `scripts/window-id.swift` matches the bundled app's owner name too.

## 2026-09-04 — switchblade catch-up

Assessment of abner against switchblade (forked 2026-07-23; switchblade gained ~166
commits since, of which only a handful touched shared code). Landed the fixes that were
real or latent defects; the rest became TASKS.md items 1–11.

- `0069bc8` docs: README, CLAUDE.md and `--help` brought in line with the code (no more
  drag-and-drop promise, Esc semantics, `--view` names, test list, bundle-icon trap);
  `scripts/window-id.swift` added for targeted window captures.
- `5fe8f57` `cargo update` (~60 transitive bumps, naga 30.0.1; no manifest changes —
  every direct dependency was already on its latest stable) and CLAUDE.md notes.
- `69a8a0c` `src/schedule.rs` lifted verbatim from sb-window. The redraw-cadence rules
  in `about_to_wait` were already a byte-for-byte inline of it; the point is the five
  tests pinning the occlusion / `MIN_FRAME` / idle-deadline invariants. Zero behaviour
  change.
- `1020d0b` text atlas: no mid-frame reset (earlier text in the frame had already baked
  UVs — switchblade's garbled-badge bug), a full atlas refuses + memoizes and wipes at
  `begin_frame()`; per-rect glyph uploads instead of two whole-atlas uploads per dirty
  frame (one of them dead code); 1024² → 2048².
- `380e9dd` decoder robustness from switchblade's post-fork fixes: `Drop` serialises
  with the reader's park loop (lost-wakeup thread leak); AVIO interrupt callback
  installed before `avformat_open_input` so a drop reaches a reader wedged in libav
  I/O on a dead mount (new FIFO-dribble test); a failed seek fails the player instead
  of silently desyncing; the startup `ffprobe` runs under a 30s deadline.

Deliberately NOT ported: per-player pacing anchors (abner's master clock makes the
whole class of bug impossible), the VT hardware scale chain (abner never scales), the
gamma-space surface flip (see TASKS.md 17), and the split text/tile pipeline (abner's
single pipeline cannot express the layering bugs it caused). abner is not joining
switchblade's workspace: the shared code is a few hundred lines that diverged on
purpose, and sb-window's `App` trait is shaped around a tile grid.

## 2026-07-27

- `797ef8f` feat(macos): embed and set Dock icon at startup — PNG `include_bytes!`'d
  and handed to `NSApp.setApplicationIconImage`, since a bare Mach-O has no
  `CFBundleIconFile` and running from a shell is the common case.

## 2026-07-23 — initial build

- `9097359` Window 2a, the Instrument HUD, from the Claude Design mockups: corner
  brackets, A|B toggle, per-clip info block, hover-revealed transport with seek bar,
  keycap legend.
- `fc93185` Launch window for the no-arguments / .app case (2b; drop targets
  decorative — TASKS.md 1).
- `741dcb5` Initial commit: N-player frame-locked sync on one master clock (no
  per-player pacing), exact-seek framestep with pts adoption, overlay / side-by-side /
  delta / split / checker / blend views, photo-style synced zoom, speed control, fake
  fullscreen, mip-chained video textures, ab_glyph text. Adapted from switchblade's
  media and render stack as of that date.
