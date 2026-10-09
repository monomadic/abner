# abner — history

Completed work, newest first. Task numbers refer to [TASKS.md](TASKS.md) where a task
existed there before it landed; earlier entries predate the task list.

## 2026-10-09 — Timeline transport styling

Matched the selected Figma `transport/toolbar` frame at 1440px: black 50px ground,
more widely spaced playback controls, labeled time and frame readouts, and edit
tools shifted right. The shared control rectangles still drive both drawing and
hit testing. Narrow windows keep the compact arrangement, including chapter
navigation. The existing geometric icons and snap/zoom controls remain live.

## 2026-10-08 — updated shared header alignment

Refreshed Figma Header `12:5`: the mode selector is now left aligned at x=85
(x=14 in fullscreen), 356px wide, with Sources / Timeline / Crop / Mask labels.
All four modes share the 44px header, outlined housing and blue active button,
so switching modes keeps the selector in place. Timeline retains its existing
Undo/Export controls and status. Drawing and hit testing use the same controls.

## 2026-10-08 — Timeline Edit header

Implemented Figma Timeline Edit header `12:5`: a 44px #050506 bar,
31px outlined tab housing, blue TIMELINE selection, green stream-copy status, and
right-aligned Undo (with ⌘Z keycap) / Export controls at the board's sizes.
The clip filename leaves the header; it remains in the native window title.
The timeline canvas and inspector start below the new header. The status yields
below 1000px so the tabs and actions fit at 720px; native traffic lights retain
100px clearance (14px in fullscreen). T / Escape still return to Sources.
The header keeps Abner’s SOURCES, TIMELINE, CROP and MASK modes, wired to
their existing actions. The existing mono text renderer is retained.
Undo/export keep their existing availability and actions. All 62 tests pass;
window captures verified the header at 1440×800 and 720×480. The existing
scrub-toolbar overlap at 720px is outside this header change.

## 2026-10-07 — the scrub panel brought level with the Figma board

Implemented from the Figma file "Abner — Timeline Edit", node 25-24 (the toolbar and
media timeline), against the build of 2026-10-05. The geometry was already the board's;
this is the detail pass:

- **Grounds**: the toolbar is `CUT_BAR` (#100f0e) and the lanes `CUT_TL` (#1a1a18), the
  board's two surfaces. The tool buttons carry the board's 1px #232323 border at an 8px
  radius; the zoom −/+ are its magnifiers (`Icon::ZoomOut`/`ZoomIn`, the handle a stair
  of squares — no diagonal primitive). The snap lamp is #20c45f at 42% with a faint halo.
- **Chapters** have three states: passed (a #393a3a flag, 2px pole down to the clips),
  current (amber, a short pole), still to come (a faint flag over a 6px hairline). The
  pole no longer runs up past the flag.
- **Clips**: the sound row is 39px (was 52) and a sound clip is the board's vertical
  gradient (#112a42 → #0c1d2f, `fade_down`) under its blue border; waveform peaks scale
  to the row with a 6px floor. Each end of a clip has the 8px trim glow and the grips
  inside it (38px on picture, 27 on sound). An idle thumbnail keeps a 16% white border
  at a 5px radius. Cues are #c1a9ee.
- **Marks**: in is a 5px bar with a foot at the top and the bottom; out keeps its tab. The
  snapped keyframes stand tall at 1px. The razor's disc sits 4px higher. The playhead's
  pin is the board's 11×13 tab with three dark grooves, and its timecode is a WHITE chip
  with black type centred under the line on the keyframe row, not a dark one beside it
  on the ruler.
- **Dark type on light chips** (flag numbers, the timecode chip, the lit tool buttons)
  read brown/grey: glyph coverage blends in linear light, so a thin black stroke's
  edges over amber came back far too bright. Shader mode 6 now lifts a dark glyph's
  coverage by the same `1 − (1 − a)^2.2` the launch overlays use (faded in below ~0.3
  luminance, so light type on dark ground is untouched), and the flag numbers are
  drawn twice half a pixel apart to stand in for the board's Bold weight.
- Not taken from the board: it draws only the play disc before the clock, where the
  toolbar keeps the chapter / keyframe / frame skips added since (they get the same
  border); the gutter icons are white as drawn there, but built from rects as before.

## 2026-10-06 — a lone file opens on the timeline

Launch with one file, drop one on an empty window, or open one from Finder / recents, and
Abner opens in TIMELINE mode; two or more files stay in SOURCES (the compare view).
`App::set_cut_default(true)` (main.rs) turns it on and `settle_cut_default` runs at launch
and after every drop (replacing with one file counts); `--mask` and `--crop` still take
their own tool, and tests leave the flag off so Sources stays what they exercise. A file
with no duration stays in Sources (the timeline needs one).

## 2026-10-06 — toolbar tabs renamed and reordered

`INPUT` → `SOURCES`, `CUT` → `TIMELINE`, shown as SOURCES · TIMELINE · CROP · MASK (the
status line's mode word follows). Only the labels and their order moved: `Action::Tool`
indices are unchanged (0 sources, 1 mask, 2 crop, 3 timeline), so nothing keyed on them
did. The code still says "cut mode" and "input mode" internally.

## 2026-10-06 — Cut mode: `+` adds a chapter, and the export writes them

A `+` beside the list / thumbnails toggle starts a chapter at the playhead ("Chapter N",
refused within half a second of an existing one). The export now carries chapters:
`cut::output_chapters` moves each by what was cut before it and drops those whose start
is inside a cut, and an ffmetadata input (`-map_chapters 1`, `-map_metadata 0` so the
file's own tags survive) writes them for both the stream-copy and the re-encode paths.
Adding a chapter alone is enough to export. No rename or delete yet, and no undo for them.

## 2026-10-06 — Cut mode: U undoes, the wheel zooms the track, a backgrounded app idles

`U` is undo as well as ⌘Z. Over the timeline, scrolling up zooms in and down zooms out
about the pointer (exponential, `cut.zoom`), a sideways swipe pans, pinch still zooms.

Cmd-tabbing back to a PLAYING clip was slow while a paused one was instant. The
difference is the loop: playing runs the Poll cadence, presenting 4K frames as fast as the
display allows, and the OS's activation work waits behind it. A backgrounded window
(`Focused(false)`) now schedules like an occluded one — idle ticks, no Poll — and its
clock is held (`dt = 0`), so playback pauses where it was instead of running on unseen;
focusing again restarts the clock and redraws at once. Not measured: this was found by
reading the loop, not by profiling, because the capture tooling here sees a black screen.

## 2026-10-06 — cut mode: the chosen inspector, a thinner playhead

The right-hand panel is the "Cut inspector (chosen)" board: the Selection block is gone
(the context row and the timeline's in/out ghosts already say it), and Segments became
chapters. Two icon tabs on top — chapters and streams. **Chapters**: a chip strip of the
stream facts (`H.264 · 2160p · 23.976 · AAC 2ch · SRT`), a count, a list / thumbnails
toggle, and one row (or card) per chapter, the one under the playhead in the highlight
yellow with a dot for whether it lands on a keyframe (green) or not (coral); a click seeks
there. **Streams**: video, audio, subtitle and keyframe sections (count, median and
longest GOP). The footer carries the result (`01:41:42  −6:30  0 re-enc`), or the file's
duration, size and container on the Streams tab. The snapping warning stays, as a coral
note above the footer: it is information the design's panel had no room for.

The facts come from one more `ffprobe -show_format -show_streams` in the scan worker
(`cut::parse_facts`, unit-tested on a JSON sample), so entering the mode still never
blocks. A removed clip is now selected by clicking its ghost on the timeline (X restores
it) — that used to be a Segments row.

The playhead is 1px (was 2) with a shorter pin (9px tab + 7px point, was 14 + 10) and a
softer glow, as the board's last edit has it.

Not built: the thumbnails view draws the film ground, not frames (nothing extracts a frame
per chapter yet); no `+` to add a chapter (the model reads chapters from the file and the
export does not write them).

## 2026-10-05 — cut mode's scrub panel rebuilt to the board's variation A

The Cut board changed after it was first built: the transport row and the six
labelled lanes gave way to one scrub panel, "clips on tracks" (variation A of the new
board 1b). Rebuilt to it; the model, the export, the header and the inspector stand.

- **Toolbar** (50px): play, timecode, then icon buttons — set in (blue), set out
  (red), split, cut selection, snap to keyframes — a snap lamp and a zoom slider
  (log scale between "whole clip" and 600 px/s; − / + step it). The buttons carry no
  words, so the hovered one names itself and its key.
- **Rows** (222px, 56px icon gutter): a time ruler, numbered chapter flags, video and
  audio as one rounded CLIP per kept piece with a grip at each end, a dashed ghost
  per cut, subtitle cues as thin bars, keyframe ticks (the two the selection snapped
  to stand tall). Short windows drop the chapter and audio rows. The overview lane,
  the legend and the long-GOP bracket are gone with the old board.
- **Grips work**: dragging a clip's edge resizes the cut beside it, slides a split, or
  trims a new cut in from the clip's own start/end — nearest keyframe, one undo step
  per drag, never across a neighbour (`Cut::move_edge`). The canvas follows the edge.
- **Razor**: over the clips the pointer shows where `S` will split (snapped), and `S`
  splits there; elsewhere it still splits at the playhead.
- In/out are a blue and a red bar with a tab; the playhead is white with a pin and a
  timecode chip. The pin's point is `Item::TriangleDown` (shader mode 9, `p1 = 2`).
- Icons are built from rects and triangles (`draw_icon`) — the renderer has no paths.
  Chapter titles and cue text, which the board no longer prints, show on hover.

## 2026-10-03 — cut mode (the Timeline Edit design's "Cut" board)

`T` (or the new CUT tab, or `--cut [in,out[,in,out…]]`) puts the focused clip on a
timeline: the Claude Design canvas "Abner Timeline Edit", board 1. Same shell as
Compare with the source rail traded for a 300px inspector on the right (Selection,
Segments, Result) and a lanes panel under the transport (rebuilt 2026-10-05, above).

`src/cut.rs` is the model: removed ranges over SOURCE time (nothing shifts), splits,
an undo stack, and the snap. `I`/`O` set in/out; with the default keyframe snap the
in point moves BACK and the out point FORWARD to keyframes, so every kept range starts
on one and `E` exports `<name>.cut.<ext>` as a stream copy (concat demuxer,
`inpoint`/`outpoint`). `K` switches to frame snap, which re-encodes (select filters,
H.264 CRF 16 + AAC) and says so in the header. `X` cuts the selection (or restores
it when it names an existing cut — click a segment row to select one), `S` splits,
⌘Z undoes, ← → step a frame, ⇧← ⇧→ a keyframe. Playback jumps over cuts.

Keyframes and chapters come from `ffprobe` (packet flags — nothing decoded), the
waveform from a streaming `ffmpeg` decode to 20 peaks/s, subtitles from the first
text stream as SRT; each on its own worker, drained in `tick`.

Departures from the board, on purpose: the warning reads "snapping also cuts N s you
meant to keep" (the board's "keeps … you meant to cut" has it backwards for an out
point that snaps forward); the Keyframes/Subtitles/Chapters tabs are not built, so
they are not drawn; the video lane has no thumbnails and there is no scene detection
(so no "detected scene" marker); cut rows are dimmed and tagged, not struck through
(the renderer has no strike-through and text is never measured app-side). The design's
translucent whites are baked to opaque colours over their known ground (`mix`) —
blending here is linear.

Also fixed: `launch_frame`'s logo-only `FrameDesc` was missing `thumbs` (the tree did
not compile at 9bb5b61).

## 2026-09-30 — recent files on the launch window

The Splash floor carries the last four clips opened, as the Design canvas "abner —
Splash, recent files" draws them: 132×74 rounded (8px) 16:9 thumbnails 64px apart,
a white hairline bevel lit from the top, the container (from the extension) and
`HH:MM:SS` duration on corner chips, `W×H` and fps underneath, under a `RECENT` /
`⌘1–n` header. Click or ⌘1–4 opens a tile through the drop path (so it is recorded
again and moves to the front); with clips up ⌘-digit stays the bare digit. Hover
turns the bevel brand blue and shows a pointer.

`src/recent.rs`: `~/.config/abner/recent`, one absolute (not canonical — no disk
access, no `/tmp` vs `/private/tmp` doubles) path per line, 12 kept, written
atomically by the RUNNER on every load (CLI or drop), never by `App`, so tests can't
touch it. No cache, per the project rule: each launch window re-probes up to 8
candidates and pulls one 320×180 cover-cropped frame at 10% in with an `ffmpeg` child
per file, on worker threads under the probe's deadline; a missing or unreadable file
drops out and the next candidate fills its place, pending tiles hold their slot as
an empty well. Thumbnails live in one 1280×180 atlas bound at slot 8 of every group
(keyless, like the plate), drawn by shader mode 14 with mode 0's rounded-box SDF;
`App` tracks which path each cell holds, so a cell uploads only when it changes.
Only the plate layout gets the row (it needs the floor between horizon and foot).

## 2026-09-28 — config file

`abner.default.toml` (compiled in) holds every setting; the first of `--config`,
`./abner.toml`, `~/.config/abner/abner.toml`, `~/.config/abner.toml` overlays it
by deep table merge. Strict: unknown keys, bad types and out-of-range values refuse
startup with the file and dotted key. Covers window size, starting view, start
paused, seek step, delta gain, blend, checker size and brush size (`src/config.rs`).

## 2026-09-28 — native comparison and editing workspace

Implemented the approved Superdesign workspace in the native renderer: left-aligned
Compare/Mask/Crop tools, colored numbered source rows with metadata, focused
inspector, neutral comparison controls, charcoal canvas, persistent transport and
compact status footer. Replaced the giant letter and overlay panels. All controls
use the existing comparison, brush, crop and export actions; `C` opens crop directly.

Drawing and pointer routing share the workspace geometry. Pan/zoom, frame-locked
comparison, painting and crop exports retain a single image transform; GPU scissors
keep zoomed images and mask overlays inside each canvas cell. Text truncation uses
actual font advances. Responsive layout narrows the source rail below 900px and
hides the inspector below 600px high; `Tab` gives the image the full window.

Validation: all 30 tests pass, including workspace input routing, clip scrolling,
side-by-side zoom anchoring, text fitting, GPU scissor bounds, sync and crop exports.
The native comparison workspace was checked with a targeted window capture.

## 2026-09-28 — `E` exports the crop as ProRes

With the marquee up, `E` re-encodes the focused clip — every frame, audio as PCM —
cut to the marquee, as ProRes 422 Proxy beside the source: `clip.mp4` →
`clip.crop.mov` (ProRes lives in QuickTime, so `.mov` whatever the source was).
It shells out to `ffmpeg` on a worker (`mask::export_prores`) rather than going
through the in-process decoder: it's an offline transcode, and ffmpeg's autorotate
puts the crop filter in the same display-pixel space as the marquee. Written to a
temporary and renamed on success, like the PNGs. The rectangle is the same one `S`
cuts, floored to even sides (`mask::even_rect`) because 4:2:2 needs whole chroma
pairs; the origin doesn't move. Progress/result ride the status line through their
own receiver (`crop_export`), so a PNG save never queues behind a long encode.

## 2026-09-27 — skip the splash for command-line launches

Any arguments disable the animated launch splash and skip probing its video asset.
Video paths go straight to playback; `--no-video-splash` alone opens a static,
centered logo without the plate, reflection, status lamp, or footer. Drag-and-drop
still loads clips, and closing the last clip returns to that same logo-only window.
Launching without arguments keeps the existing animated splash.

## 2026-09-23 — the launch window becomes the Splash surface

The launch window now stands on the brand's grid-floor plate instead of a flat
clear: `assets/banner/background-02.png` baked in beside the wordmark (a bare
binary still has no resource directory), cover-fitted and slid until its lit
horizon sits at 57.8% of the window, with the mark standing on that line and its
reflection running out across the floor. This is the design system's `Splash`
surface (Abner > Components > Splash); the geometry notes there and the constants
in `app.rs` are the same numbers.

The horizon is MEASURED, not assumed: `decode_plate` takes the plate's brightest
row and hands `App` its v, the same rule the wordmark's ink box and the glyph
metrics already follow — a different render in the slot moves the layout with it,
and `launch_frame` reads back where the cover fit actually left the line rather
than trusting the fraction it asked for (the clamp that stops the fit opening a
gap can move it).

Three renderer modes, all keyless like the wordmark (the plate is bound at slot 7
of every group): **10** draws the plate, **11** projects the mark onto the floor,
**12** is its contact shadow. Mode 11 inverts a pinhole projection of a plane
hinged at the horizon and tilted 72° — a screen row becomes a distance along the
ground, which is the same hyperbola that makes the grid converge. A straight
vertical flip is a mirror on glass; this is light on a floor. It samples an
explicit LOD (non-uniform control flow forbids derivatives there, and the
softening toward the viewer is wanted anyway), fades in from the horizon as well
as out toward the viewer, and breaks into bands at the plate's own 3px pitch.

Two shipped mistakes, both found in a window capture and worth keeping written
down: at full strength the reflection's first row is barely foreshortened, so it
landed as a second copy of the tagline directly under the real one (hence the
fade-IN); and a contact shadow dropped a twentieth of the mark's height does the
same thing, because the tagline's shadow clears the tagline. The drop is now
1.4%, and the shadow is the one place the brand book allows the lockup a shadow —
the plate's lit horizon runs straight behind the wordmark and leaves the metal
nothing to sit against.

`RectItem` gained `fade_down`, the mirror of `fade_up`: the plate needs a ground
under the traffic lights as well as one under its message. Under 900×520 the
window falls back to the bare mark on the flat clear — below that the vanishing
point leaves the frame and the horizon stops reading, which is the Splash card's
own rule. The message, the drag-hover accent swap and the launch window's alpha
are unchanged; the plate is drawn AT that alpha, so the desktop still shows
faintly through.

## 2026-09-20 — 19. per-video mask painting and grayscale export

M pauses and opens a mask over the focused video, temporarily showing it full-window.
A native-resolution binary raster supplies one 50%-alpha red/blue overlay: painted
blue replaces untouched red. Circular strokes sweep between pointer events, so quick
drags have no gaps. The brush is measured in image pixels; the overlay, pointer
outline, and hit mapping share the existing zoom/pan transform. +/- changes diameter,
Enter switches per-video masks, and M/Esc returns to the previous comparison view.

S snapshots the focused mask and saves `<video-stem>.mask.png` on a worker thread,
with white painted pixels and black untouched pixels. PNG encoding finishes before
an atomic sibling-file replacement; failures are visible in the mask strip. No new
dependencies. `--mask` makes the state directly reachable for window-capture checks.

Regression tests cover continuous/clipped strokes, exact grayscale PNG bytes and
polarity, zoom mapping, letterbox/status hit rejection, independent video masks,
focused export, and drop/replacement lifetime; all 18 tests pass. The release bundle
builds and passes codesign verification. A live targeted-window drag confirmed the
continuous blue stroke and brush ring, and S produced a verified 1280×720 grayscale
PNG with the save confirmation visible in the HUD. Clippy reports pre-existing lints.

## 2026-09-16 — Open With / double-click on the .app (TASKS.md 3)

Opening two mp4s on the installed bundle produced AppKit's own dialog — *"Abner cannot
open files in the “MPEG-4 movie” format"* — because the plist declared no document types
and nothing answered the odoc Apple Event: LaunchServices delivers opened files to the
application delegate, never as argv, so without a handler the default
`NSDocumentController` answers for the app.

**Fix.** switchblade's `open.rs` + `open_shim.m`, ported as `src/open.rs` +
`src/open_shim.m` (compiled by a new `build.rs` via `cc`). winit 0.30 owns the
`NSApplicationDelegate` and panics if it is replaced, so the shim grafts
`application:openURLs:` onto winit's delegate CLASS with `class_addMethod`, right after
`EventLoop::new` and before the run loop starts (AppKit checks `respondsToSelector` at
`finishLaunching`, and the cold-launch open event fires then). Paths buffer in a static
and `about_to_wait` drains them into `Runner::files_dropped(paths, false)` — the drop
path already does the right thing, and it is always an ADD, never the ⌘-replace, since
a ⌘ held while picking a menu item must not wipe the set. The player `Notify` hook wakes
the loop, so an open into an idling app redraws promptly.

`packaging/Info.plist.in` now carries `CFBundleDocumentTypes` (mp4/m4v, mov, mkv, webm,
avi, mpeg/ts, wmv, flv, 3gp and `public.movie`) plus `UTImportedTypeDeclarations` for
the containers macOS has no UTI for, using the identifiers VLC/IINA already import so
LaunchServices unifies rather than forks the type. No `CFBundleTypeIconFile` — the app
icon stands in. Verified with `open -a /Applications/Abner.app a.mp4 b.mp4` (title
`a.mp4 vs b.mp4`, both slots filled) and a second `open` of `c.mp4` into the running
instance (same pid, title gains `vs c.mp4`).

## 2026-09-06 — build against the keg-only ffmpeg@8

A clean `cargo build --release` stopped compiling, with five errors reported inside
`rsmpeg 0.18.0+ffmpeg.8.0`'s own source — `attempted to take value of method pix_fmts`,
`supported_framerates`, `sample_fmts`, and an `AVCodecID` expected `u32`, found `i32`.
Nothing in this repo had changed.

**Cause.** Homebrew's `ffmpeg` moved to 9.0.1 (libavcodec 63). rusty_ffmpeg generates its
bindings with bindgen from whatever headers pkg-config finds, so the build silently
retargeted ffmpeg 9, where the `AVCodec` array fields are accessor functions and
`AVCodecID` changed signedness. rsmpeg's source is written against the 8.x layout, so the
mismatch surfaces as a broken dependency crate rather than as "wrong ffmpeg" — which is
the whole trap: the errors point at a file nobody here wrote. 0.18 is rsmpeg's newest
release; there is no ffmpeg 9 support upstream to upgrade to.

**Fix.** `brew install ffmpeg@8` (8.1.2, keg-only, so it doesn't unlink ffmpeg 9) and a
new `.cargo/config.toml` that puts its pkgconfig dir on `PKG_CONFIG_PATH` for both
Homebrew prefixes. Not `force`d, so an explicit `PKG_CONFIG_PATH` still wins, and
pkg-config ignores the prefix that doesn't exist. A bare `cargo build --release` works
again with no shell setup — which matters, because the failure mode is confusing enough
that a required export would eventually be forgotten.

The two ffmpegs coexist deliberately. Decoding links the 8.x dylibs (the bundle now
carries `libavcodec.62`, and `build-app.sh` picks that up unchanged since it walks
`otool -L`); the startup probe shells out to `ffprobe` from PATH, which is ffmpeg 9 and
parses identically. Suite green, both sync tests included: the test clips are generated
by the ffmpeg 9 CLI and decoded by the ffmpeg 8 libs.

## 2026-09-05 — 18. app icon conforms to the macOS 26 icon guideline

The Dock drew the app icon shrunk inside a lighter rounded plate, visibly smaller than
every neighbouring icon, and re-centring or trimming the PNG changed nothing on screen.

**Cause, and it is documented.** Apple documents this outright (HIG > App icons > Icon shape, rev. 2026-06-08): *"Produce appropriately shaped, unmasked layers. The system masks all layer edges to produce an icon's final shape. For iOS, iPadOS, and macOS icons, provide square layers so the system can apply rounded corners. Providing layers with pre-defined masking negatively impacts specular highlight effects and makes edges look jagged."* and *"If you do import a background layer, make sure it's full-bleed and opaque."* macOS 26 applies the mask itself, so an app
icon must be **square, full-bleed, opaque and unmasked at 1024²**. The renders in
`assets/icons/` are none of those: a rounded body floating in a transparent margin at ~89%
of its canvas, carrying its own bevel and glow (the guideline separately says to avoid soft
feathered edges and to leave highlights, bevels and glows to the system). Not meeting the
spec, it was adapted by the system rather than masked by it — hence the plate.

**Fix.** `scripts/trim-icon.py` trims to the alpha bounding box and scales the artwork up
until the 1024² centre crop is opaque corner to corner, then flattens to RGB. `--zoom`
defaults to the smallest value that achieves this, found by binary search — 1.198 for this
render, keeping 83% — so the crop is never tighter than the guideline requires. The
render's own rounded corners are cropped away and the system's mask supplies the
silhouette. `assets/app-icon.png` is regenerated through it.

Switchblade's slot icon already satisfied the guideline (1254², fully opaque, edge to
edge), which is why its Dock icon was always right on the same `CFBundleIconFile`-only
bundle layout. Its `assets/variants/` alternates do not — `make-variants.sh` seats a
pre-masked superellipse at `BODY=824` on a 1024 canvas — and are filed as a task there.

**The process failure is the point.** Two confident causes were written into this file and
then disproven, and a third was reached by building throwaway .app bundles and measuring
what `NSWorkspace.icon(forFile:)` composited: synthetic squircles at 824 vs 1024, crisp vs
soft alpha, cream vs magenta. A wrong turn before that had blamed `CFBundleIconName` /
`Assets.car` and would have cost a full Xcode install and an Icon Composer redraw. All of
it was one paragraph of published guideline. **Read the platform's own documentation
before building a test rig against the platform.** The one artefact worth keeping is
switchblade's `packaging/check-icon.swift`, which renders what the OS actually composites
for a built bundle — useful for verifying a change, not for deriving the rule.

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
