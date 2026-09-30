# abner — agent notes

A/B video comparison player: N videos decoded in frame-locked sync, flipped/diffed
on screen. Deliberately slim — one crate, a handful of modules, one config file, no cache.
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
  second copy of the rules there. Fake fullscreen = `set_simple_fullscreen`. **No window
  border**: macOS Tahoe draws a ~1px contour around every window WITH the window
  shadow, so `setHasShadow(false)` at creation kills it for good, windowed included
  (switchblade's trick) — the shadow is never restored.
  **No visible titlebar** (`set_titlebar_glass`, switchblade's): transparent bar,
  hidden title, and `FullSizeContentView` so the wgpu surface runs UNDER the strip —
  a transparent bar alone shows the default system grey, not the app's clear, so the
  only way to match exactly is to let the same GPU clear paint it. The traffic lights
  stay, floating over the content; the workspace toolbar starts at x=100 to clear
  them (x=16 in borderless fullscreen).
  The title is still SET, just hidden, so anything reading it sees the clip names.
  The Dock icon for the BARE binary is `include_bytes!`'d from `assets/app-icon.png`
  (the icon SLOT — `packaging/build-app.sh` renders the bundle's `.icns` from the same
  file, so the two can't drift; the alternates in `assets/icons/` become the icon by
  being run through `scripts/trim-icon.py <alternate> assets/app-icon.png`, not
  `cp`. The rule is **square, full-bleed, opaque, UNMASKED, 1024²** — macOS 26
  applies the rounded-rectangle mask itself. Apple documents this outright (HIG > App icons > Icon shape, rev. 2026-06-08): *"Produce appropriately shaped, unmasked layers. The system masks all layer edges to produce an icon's final shape. For iOS, iPadOS, and macOS icons, provide square layers so the system can apply rounded corners. Providing layers with pre-defined masking negatively impacts specular highlight effects and makes edges look jagged."* and *"If you do import a background layer, make sure it's full-bleed and opaque."*
  An icon that doesn't meet it gets adapted by the system instead: abner's Dock
  icon came back shrunk inside a lighter plate, which reads as padding that no
  amount of re-centring the PNG fixes. The renders in `assets/icons/` never meet
  it — a rounded body floating in a transparent margin at ~89% of its canvas,
  with its own bevel and glow (the HIG says to avoid soft feathered edges and to
  leave highlights, bevels and glows to the system, so bear that in mind when
  picking the next render). The script trims to the alpha bbox and scales up
  until the 1024² centre crop is opaque corner to corner — `--zoom` defaults to
  the smallest such value, found by search, so the crop is never tighter than the
  guideline needs. `packaging/check-icon.swift` (switchblade's, extended to take a PNG) renders
  what the OS composites — `swift packaging/check-icon.swift assets/app-icon.png`
  checks the slot BEFORE a build; run it after every icon change and look for the
  plate. Do NOT port switchblade's `seat-icon.sh`/`squircle.sh`: they seat a
  pre-masked 824px body on a transparent 1024 canvas, the opposite rule, and
  switchblade's own slot has been plated since it adopted them (checked 2026-09-22
  — the older claim here that its slot was opaque edge to edge had gone stale)) and pushed to `NSApp.setApplicationIconImage` at startup — a
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
- `src/config.rs` — `abner.default.toml` (repo root, `include_str!`'d, so the bundle
  needs no file on disk) is the COMPLETE key set; a user file is an overlay, deep-merged
  table by table, then the result is deserialized with `deny_unknown_fields` and
  range-checked against the same bounds as `App`'s live clamps. Only the FIRST existing
  file is read: `--config <path>` (must exist), `./abner.toml`,
  `~/.config/abner/abner.toml`, `~/.config/abner.toml`. Any error exits 2 before the
  window opens, naming the file and dotted key. A new setting = a key in the default
  file + a field in the struct (the `default_is_complete_and_valid` test catches a
  mismatch); CLI flags (`--view`) override the config.
- `src/probe.rs` — one ffprobe per input at startup, synchronous on the main thread
  before any window exists, so it runs under a hard deadline (`run_deadlined`): a child
  stuck on a dead volume otherwise looks exactly like a crash.
- `src/recent.rs` — the launch window's recent files: `~/.config/abner/recent`
  (written by main.rs on every load, never by `App` — tests must not touch it) and the
  per-file thumbnail workers (probe + one `ffmpeg` frame, no cache). `App::RecentRow`
  owns the tiles, the atlas-cell bookkeeping and hit testing; the renderer's `thumbs`
  atlas (binding 8, mode 14) holds the frames. See HISTORY 2026-09-30.
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
  **Keys**: `1`–`9` pick a clip directly (`select`), `V`/Shift-V cycle the view.
  ⌘W (`Key::Close`, from winit's `ModifiersChanged` state) closes the focused clip
  (`close_active`): survivors shift down a slot and are exact-seeked to the current `t`
  so each re-delivers into its new texture slot; `started` is NOT reset (the seek lands
  just past `t`, so a delivery-gated clock would deadlock). `Cmd::VideosChanged` makes
  the runner re-sync textures + title. Last clip closed = launch window; ⌘W there quits.
  The **workspace** (`Workspace`, `controls`, `build_hud`) shares geometry between
  drawing and pointer hit testing: a numbered source rail, focused inspector,
  Compare/Mask/Crop toolbar, contextual controls, canvas, persistent transport and
  status line. The rail is 280px (210 below 900px wide), the inspector hides below
  600px high, and the minimum window is 720×480. `Tab` hides the whole shell.
  `base_rect` fits within the canvas (or an individual side-by-side cell); every
  gesture and crop/mask transform uses that same rect. Shell clicks cannot paint.
  `Item::Clip` scissors video and overlays to the canvas/cell, including when zoomed.
  Source badges use numbers and nine distinct colors; keyboard selection scrolls
  the rail to keep the selected row visible. Text truncation uses measured font
  advances (`TextItem::max_width`), not character counts. Transport glyphs remain
  geometry (`Item::Triangle`), not font glyphs.
- `src/mask.rs` — per-video native-resolution binary masks, the crop marquee's geometry,
  and atomic PNG export. `App` owns lazy masks and converts pointer positions through
  `content_rect`; never invent a second zoom transform. `M` temporarily draws the focused
  video alone, `+`/`-` (or `[`/`]`) size the image-pixel brush, `S` snapshots to a save
  worker. Blue = 255, red = 0. Renderer mode 8 samples one R8 mask texture;
  `(id, revision)` avoids idle uploads. Mask/crop controls occupy the context row and results appear in the
  footer. The mask layer is clipped to the canvas, so dimming never touches the shell. **`C` is the crop marquee** (2026-09-20): a `Crop` in IMAGE pixels
  — the mask's own grid, so one rectangle cuts both planes — drawn as dashed rects
  (the renderer has no line primitive) with white corner handles, everything outside it
  dimmed. While it is up the pointer moves/resizes it and `brush_cursor_visible()` is
  false, so painting can't run into a drag; `C` again drops it. `S` then writes the mask
  AND the video pixels under it at the same size (`<name>.mask.png` + `<name>.crop.png`). `E` (marquee up) re-encodes the whole clip cut to the same rectangle
  (floored to even sides) as ProRes 422 Proxy, `<name>.crop.mov`, via an `ffmpeg` child
  on a worker (`mask::export_prores`, its own `crop_export` receiver). With the marquee up the mask
  tint is NOT drawn (the footage shows plain, everything outside dimmed near-black at
  `CROP_DIM` 0.95 — linear blending, so lower reads grey). `A`/Shift-A step the ratio
  presets (`ASPECTS`: free, 16:9, 9:16, 4:3, 1:1, 2.39:1); a preset snaps to its largest
  fit about the marquee's centre (keeping the size ratchets smaller), and corner drags
  keep the ratio (`Crop::with_corner_locked`). The marquee carries the Figma design's
  three readouts (`build_crop_labels`): `WxH - ratio` in the middle, the image-pixel
  corners `x, y` / `x+w, y+h` pinned inside the top-left and bottom-right, on flat chips
  the renderer measures (`TextBg`, `VAlign::Bottom`); small marquees drop them.
  That second file is why `Video::last_frame` exists: the GPU's copy can't be read back,
  so mask mode keeps one RGBA frame per video (cheap — it's paused, so the copy happens
  on entry and on seeks, and entering mask mode re-seeks to re-deliver the frame already
  handed back to the decoder). `--mask a.mp4 b.mp4` reaches this state for targeted
  captures without global keys, and `--crop [x,y,w,h]` reaches the marquee the same way.
- `src/render.rs` — one wgpu pipeline for everything (rects, video quads, compare
  modes, glyphs, the logo), instanced quads in logical px. Per-video textures carry a blit-filled
  mip chain (4K fit-to-window without shimmer). Bind groups are cached per (A,B) texture
  pair; keyless items (rects/text) ride the current batch. `TextItem` carries
  align/valign/tracking and an optional rounded chip: the renderer owns the font, so
  it MEASURES each run — never estimate glyph positions app-side (`MONO_ADV` exists
  only to step *between* runs). The **wordmark** (`assets/logo.png`, the SLOT —
  currently a straight copy of `assets/logo/logo-05.png`; an alternate becomes the
  mark by `cp`, since the trim below makes its margins irrelevant) is
  `include_bytes!`'d and decoded with the `png` crate — the Dock icon goes through
  AppKit, but a texture needs the pixels in-process — into a mipped texture bound at
  slot 5 of EVERY bind group, so `Item::Logo` needs no batch key of its own. The
  renderer owns the image, so it MEASURES it: `decode_logo` takes the alpha bounding
  box and hands `App` the trimmed aspect plus the uv rect to draw, so the launch
  layout doesn't inherit whatever margin the export left (the exports carry
  uneven transparent padding — drawn whole, the mark sits visibly off-centre in its
  own box). Same rule as
  text: never estimate what the renderer can measure.
  The **launch plate** (`assets/banner/background-02.png`, the brand's bare grid-floor
  ground) is baked in the same way and bound at slot 7 of every group, and it is
  measured the same way too: `decode_plate` takes its BRIGHTEST ROW as the horizon and
  hands `App` that v, so a different render in the slot moves the launch layout with it
  instead of needing a hard-coded fraction. Modes 10 (the plate), 11 (the mark projected
  onto its floor) and 12 (the mark as a soft shadow) are keyless for the same reason
  mode 7 is.
  **The plate moves** (2026-09-23): `assets/banner/background-02-loop.mp4` plays as
  the launch floor — `App::tick_backdrop` runs its own `Player` and its own clock
  while there are no clips (a loaded clip drops the decoder), wraps with an exact
  seek to 0, and hands each frame over as `FrameDesc::plate`; `Gpu::upload_plate`
  rebuilds the plate texture at the video's size on the first frame, and main re-reads
  `plate_size` after it. The redraw is paced by `redraw_at` at the clip's fps, not
  the display's. The still PNG stays baked in: it's the horizon measurement (same
  scene, same lit line) and the fallback when the video is missing or fails. The loop
  file is DERIVED from the camera move `background-02.MP4`, whose last frame doesn't
  match its first: its last second is crossfaded into its first —
  `ffmpeg -i background-02.MP4 -filter_complex "[0:v]split[a][b];[a]trim=start=1,setpts=PTS-STARTPTS[main];[b]trim=end=1,setpts=PTS-STARTPTS[head];[main][head]xfade=transition=fade:duration=1:offset=3.0417,format=yuv420p[v]" -map "[v]" -c:v libx264 -crf 18 -preset slow -g 24 -movflags +faststart -an background-02-loop.mp4`.
  The bundle carries it as `Contents/Resources/background.mp4` (build-app.sh); a bare
  binary finds it in the source tree (`backdrop_path`).
  The surface is **opaque** (`with_transparent(false)` in main.rs, `Opaque` alpha
  mode). It used to be transparent, the desktop showing faintly through the letterbox
  and the launch window; dropped 2026-09-23. `FrameDesc::clear` still carries an alpha
  (now 1) for the plumbing that remains.
  The **floor reflection is hard-light blended** onto the plate IN the shader: fixed-
  function blending can't read the destination, but the plate is our own texture, so
  mode 11 samples it where the fragment lands (the plate's rect rides the uniforms,
  `Uniforms::plate`) and writes the blended result. That's also why `LogoFloor` is
  pushed straight after `Plate`: what's under it must BE the plate, not the plate
  under the vignette.
- `src/shader.wgsl` — modes: 0 rect, 1 tex, 2 delta, 3 split, 4 checker, 5 blend,
  6 glyph, 7 logo, 8 mask, 9 triangle, 10 launch plate, 11 the wordmark projected onto
  the plate's floor, 12 the wordmark as a soft shadow, 13 the launch overlays (radial
  vignette / scanlines). Mode 11 inverts a pinhole
  projection of a plane hinged at the horizon and tilted 72°, so a screen row becomes a
  distance along the ground — the same hyperbola that makes the grid converge, which is
  why it reads as light on a floor rather than a mirror on glass. It samples an EXPLICIT
  LOD: implicit derivatives are illegal in non-uniform control flow (a switch arm is
  exactly that), and the blur toward the viewer is wanted anyway. Mode 0's `pad` is
  four-valued: 0 none, 1 fade up (the bottom-anchored scrim), 2 fade down, 3 fade out
  to both sides (the Splash foot's hairline). The reflection deliberately does NOT follow the
  card's dark ripple/falloff group (tried 2026-09-23: the mirrored tagline under the
  real one read as weird) — it stays a faint banded mirror. Dark overlays the card
  specifies in CSS alphas are lifted to `1 − (1 − a)^2.2` (and the white scanline cut
  to ~0.012) because blending here is linear-space. Textures are sampled unconditionally then selected (uniform-control-flow
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

**Every screen is in Figma**: [Abner — Screens](https://www.figma.com/design/a5aV8WGfzSy9nWBea6Oqyb)
(launch + drag-over, Single, Side by side, Difference, Mask, Crop, and the component
board), rebuilt as editable layers from this code on 2026-09-28. **[FIGMA.md](FIGMA.md)
is the design ↔ code contract** — read it before implementing from Figma: tokens are
`app.rs` consts mirrored as Figma variables (alphas differ — linear vs sRGB blending),
components are `build_*` functions, and MCP-generated React/CSS is a spec, never code.

The loaded workspace implements the approved Superdesign draft
`https://p.superdesign.dev/draft/2e39910e-e4b1-4236-bc2f-c1e0d5ce6f0c`
(local reference: `.superdesign/proposals/canvas.html`). It replaces the old 2a
Instrument HUD: no giant letter, Sources/View headings or fading transport.
The canvas surround is #1d1b1b; controls use neutral #1a1a1a fills and #d1d1d1
selected text. The user-requested negative border width is represented as no border.
The source palette starts #61b5ee, #e6ac69, #b89ae8 and repeats after nine slots.

**One clip is enough to play**: `App::ready()` is "any video", a lone clip lands in
slot 1 and `shown_mode()` draws it plain while keeping the selected comparison mode
for when another clip arrives. The whole empty window remains the drop target.

The launch window is the design system's **`Splash`** surface (Abner > Components >
Splash, in the Claude design system at `claude.ai/artifact/CqpfyGsUrKR6f7ZeV9fAdV`):
the brand's grid-floor plate under a radial vignette, the mark standing on its measured
horizon with the card's three stacked shadows and a floor reflection, a 3px scanline
over the art, a `READY` lamp level with the traffic lights, and the foot on the near
floor (hairline, instruction, formats between a blue and a red tick). The card's geometry notes
and `app.rs`'s launch constants are the same numbers, so change them together. Under
900×520 it falls back to the bare mark on the flat clear — the card's own rule, because
below that the plate's vanishing point leaves the frame. The shadow is the single
documented exception to the brand book's "no drop shadow on the lockup".

**The palette comes from the logo, not the mock.** 2a's lime (#a6e22e) read as a
different product next to `assets/logo.png`, so `ACCENT` is the mark's upper bar
(#006dcf, lifted to #1580de — the bar itself clears only ~4:1 against the HUD's black,
under the bar for 10–11px mono). The lower bar (#e71b24) was `ACCENT_B`, slot B's
colour on the old launch zones; it went with them. The launch window's wordmark is the logo IMAGE, not type — it already carries
the "VIDEO QUALITY TESTING TOOLKIT" line that used to be a second text run.

## Rules

- `cargo test` generates tiny ffmpeg test clips; the suite covers master-clock
  draining, exact seek, two-player sync, framestep adoption, reader-thread cleanup
  (condvar-parked AND wedged-in-libav-I/O, the latter via a mkfifo dribble), and the
  redraw cadence (`schedule::tests`), crop drags and the paired crop export, and
  slot-filling drops (0 → 1 → 2 → 3, asserting
  a lone clip plays, the clock rewinds and the streams stay inside a frame period of each other).
  Keep it green — sync IS the product.
- Building needs the ffmpeg **8.x** dev libraries — `brew install ffmpeg@8`, which is
  keg-only, so `.cargo/config.toml` puts its pkgconfig dir on `PKG_CONFIG_PATH` (not
  forced: an explicit `PKG_CONFIG_PATH` still wins). Plain `brew ffmpeg` is 9.x now and
  **rsmpeg does not build against it** — 0.18 is the newest release, it generates its
  bindings from whatever headers pkg-config finds, and on 9.x `AVCodec::pix_fmts` and
  friends became accessor functions while `AVCodecID` changed signedness, so the errors
  land inside rsmpeg's own source and read like a broken crate rather than a wrong
  ffmpeg. Keep ffmpeg 9 linked in PATH — the startup `ffprobe` is a separate process and
  is happy on either. The only other non-obvious dependency is `png`, for the wordmark
  texture.
- **`./packaging/build-app.sh [--open|--install]` builds `Abner.app`** — switchblade's
  recipe: release build, `assets/app-icon.png` → `AppIcon.icns`, `Info.plist.in` with
  version + a NUMERIC build stamp (`CFBundleVersion` = UTC `YYYYMMDD.HHMMSS`; the git
  hash rides in `AbnerGitHash` — when two bundles share an id `open -a Abner` goes to
  the higher version, a hash parses as ~0, and a stale cargo-bundle leftover in
  `target/` answered for the installed app and exited with usage; `--install` also
  unregisters the build-tree twin), every non-system dylib copied into `Contents/Frameworks` with
  load paths rewritten to `@rpath`, ad-hoc codesign (mandatory on Apple Silicon after
  `install_name_tool`), then `lsregister -f` so an in-place reinstall doesn't keep the
  old icon. `CFBundleExecutable` is a thin launcher that prepends the Homebrew bin dirs
  to PATH, because a Finder-launched app gets no PATH and the startup `ffprobe` would
  fail. **Open With / double-click** is `src/open.rs` + `src/open_shim.m` (switchblade's,
  compiled by `build.rs`): LaunchServices delivers opened files as an Apple Event, never
  argv, and winit 0.30 panics if its `NSApplicationDelegate` is replaced, so the shim
  grafts `application:openURLs:` onto winit's delegate class before the run loop starts.
  Opened paths drain in `about_to_wait` into `files_dropped(paths, false)` — always an
  add, never the ⌘-replace. The plist's `CFBundleDocumentTypes` are the other half; without
  the handler, declaring them makes AppKit's `NSDocumentController` answer with "Abner
  cannot open files in the “MPEG-4 movie” format".
- Verify visual changes with a targeted window capture, never by injecting global
  keystrokes — a `--view` flag exists so every mode is reachable from the CLI.
  `scripts/window-id.swift` prints the window id:
  `screencapture -x -l "$(swift scripts/window-id.swift | head -1)" shot.png`.
  `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -t 6 -g 30 a.mp4` makes a clip
  (vary `eq=brightness` for a B that actually differs).
