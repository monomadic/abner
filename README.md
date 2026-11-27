# abner

**A/B testing for videos.** Point it at two (or more) video files — different encodes,
different upscalers, different grades of the same content — and flip between them while
they play in frame-locked sync.

```sh
abner original.mp4 encoded.mp4
abner --view delta original.mp4 encoded.mp4
abner a.mp4 b.mp4 c.mp4          # three-way works too
abner --export original.mp4       # minimal pipeline export workspace
abner                            # launch window — drag clips onto it
abner --no-video-splash           # just the centered logo — drag clips onto it
```

`--view` takes `overlay`, `sbs`, `delta`, `split`, `checker` or `blend`, so every
mode is reachable from the command line.

The animated splash appears only when launched with no arguments. Video paths
start playback directly without probing or decoding the splash video. Any arguments
also make the empty window logo-only, including after closing the last clip.

## Processor presets

On Process, use **Save** beside the preset arrows, enter a name in the preset
field, then press Enter or Save again. The arrows cycle through Default, built-in
presets and your saved presets. Saving an existing custom name updates that preset;
Delete removes the selected custom preset. Built-in presets cannot be deleted.

Presets are individual, pretty-printed JSON files under
`~/.config/abner/presets/processor-<processor-id>/`. Each records a version, processor
ID, name and settings, including nested values and automatic/null settings. Files
can be edited by hand and are loaded when the app starts. Presets affect settings;
loading one does not turn a bypassed processor on.

## Export

Open the **Export** tab to write the enabled processor pipeline, or launch with
`abner --export INPUT`. Choose MP4, MOV, MKV, or WebP, then Export to pick
a destination. ProRes is available in MOV and MKV. Selecting ProRes while MP4
is active switches the container to MOV. The large output viewer follows
new rendered frames; scrubbing or arrow keys holds a recent frame for inspection,
TAB compares its original, and Follow render returns to the latest frame. Progress
shows encoded frames, FPS, elapsed time and estimated remaining time.

Abner opens animated WebP as a source with its original per-frame timing.
WebP export writes a lossless, infinitely looping animation without audio.
WebP-to-WebP trims and direct crops retain transparency. Processor chains
require opaque input.

This initial exporter supports SDR constant-rate video, current Timeline kept ranges,
an active crop and streaming processors. Recent-frame review is memory bounded;
see [native media limits](docs/NATIVE-MEDIA.md#minimal-export-workspace).

## Configuration

[`abner.default.toml`](abner.default.toml) lists every setting with its default
(window size, starting view, start paused, seek step, delta gain, blend, checker
size, brush size) and is built into the binary. To change something, create a
TOML file with just those keys, in the same tables:

```toml
[playback]
view = "delta"
seek_step = 5.0
# Decoded frames kept per clip, in MB (default 1024): the GOP on screen and
# the next one stay resident, so seeks and scrubs inside them are instant.
frame_cache_mb = 2048
```

abner reads the first of these that exists — `--config <file>` (or
`--config=<file>`), `./abner.toml`, `~/.config/abner/abner.toml`,
`~/.config/abner.toml` — and lays it over the defaults. An unknown key, wrong type
or out-of-range value stops startup with the file and key in the error.
Command-line flags such as `--view` win over the config.

## Drag and drop

Run `abner` with no arguments and drag clips onto the launch window. One file fills
slot **1** and starts playing; two land as **1** and **2**;
more add 3, 4… — in the order the platform hands them over, which for a Finder
multi-select is the order they appear in that window, so drop them one at a time if it
matters which is first. Dropping onto a running comparison **adds** streams — hold **⌘** while
dropping to replace the whole set instead. A single path on the command line
(`abner reference.mp4`) opens the same half-filled window.

Every load rewinds all streams to 0, so an arrival is frame-locked with what was
already playing rather than joining mid-flight.

Born out of the [switchblade](../switchblade) project's graphics stack: in-process pure-Rust
decode ([FilmCraft](https://github.com/storytold/filmcraft)'s codecs, VideoToolbox underneath for
h264/hevc/vp9/av1), a wgpu renderer with mip-chained video
textures (no minification shimmer on 4K sources), idle-throttled render loop, and
mpv-style fake fullscreen.

## Sync model

One master clock drives every stream. Players decode into small bounded queues; each
frame the app pops everything due and shows the newest. Flipping the displayed video
(`Tab`) switches *textures*, not players — the other stream was already decoding the
same instant, so the flip is seamless and time never jumps. Pause stops the clock
(backpressure stalls every decoder for free); framesteps are exact seeks whose landing
frame's true pts is adopted back into the clock, so stepping can't accumulate drift.

## Keys

| Key | Action |
|---|---|
| `Tab` | flip to the next video (in overlay mode) |
| `Space` | pause / play |
| `<` `>` (or `,` `.`) | frame-step back / forward |
| `←` `→` | seek ±1s |
| `1`…`9` | show clip 1, 2, … directly |
| `V` / `Shift-V` | next / previous view, cycling through: |
| | **overlay** — videos stacked, flip with Tab (the classic A/B) |
| | **side-by-side** — all videos in a row |
| | **delta** — amplified \|A−B\| difference (`-`/`=` adjusts gain) |
| | **split** — vertical wipe, divider follows the pointer |
| | **checker** — checkerboard mix (`-`/`=` adjusts tile size) |
| | **blend** — dissolve between A and B (`-`/`=` adjusts mix) |
| pinch | photo-style zoom on the pointer — every video pans/zooms to the same spot |
| drag / scroll | pan while zoomed (synced across videos) |
| `M` | open a temporary mask editor on the focused source (pauses on entry) |
| `+` / `-` | enlarge / shrink the brush in mask mode (`=` also enlarges) |
| `S` | save the focused mask in mask mode |
| `Z` | reset zoom |
| `[` `]` | slow down / speed up playback (0.25×–4×) |
| `Backspace` | Sources: close selected clips (or focused clip if none selected); Timeline: delete picked piece; otherwise reset playback speed |
| `Option`-drag | Duplicate the dragged clip at the drop position in Sources; upper/lower card halves insert before/after, blank space below appends |
| `F` | fullscreen (borderless, same Space, instant) |
| `Enter` | the current video alone, fullscreen (again or `Esc` to return) |
| `Q` | quit |
| `Esc` | leave fullscreen, else quit |

In compare modes (delta/split/checker/blend) the pair is the active video vs the next
one; `Tab` rotates which pair you're looking at. Small colored number badges match
the source list and the `1`–`9` shortcuts. With the controls hidden, `Tab` still
flashes the number briefly so you know where you are.

## Mask painting

Press **M** with a comparison loaded. The focused video fills the canvas with a
50% red overlay. Click or drag to paint with the circular brush: painted pixels
become 50% blue, replacing red rather than stacking another tint. **+ / -** changes
brush diameter (shown in source-image pixels); pinch to zoom and scroll to pan.
The brush outline follows the same transform as the image. **Apply** or **Enter**
saves the draft to its caller; **Cancel**, **M**, or **Esc** discards it. The temporary
tab closes and returns to Sources, Timeline, or Process. The editor stays attached
to its original source until closed. Applied source masks persist in memory, and
playback stays paused until Space.

**Autodetect** (or **D**) opens a compact panel beneath the mask toolbar.
Choose the exact time range, then choose **Frames**: 8, 16, 32, **64** (default),
128, 256, 512, or **∞**. Whole clip resets the range; moving the playhead does not
change it. **Detection area** shows a movable, resizable rectangle; click it again
to hide the handles while retaining the selected area. Double-click inside the
rectangle to reset it. The area limits detection and retained sample memory,
without cropping the source or changing existing paint.

Press **Detect** in the panel to run, or **Stop** to cancel. The panel can be hidden
with Autodetect. Changing either slider or the area cancels pending detection.
**∞** examines every frame in the range in bounded batches distributed across that
range, keeping pixels supported by at least 60% of batches. It takes longer and
is not a guarantee of a better mask. Finite counts use one statistical fit over
all selected samples; memory limits report an error rather than silently dropping
samples. Samples retain native pixel resolution with context around the area.

Detection adds to the draft; Undo restores the previous mask, Apply keeps it, and
Cancel restores the original. A weaker static-scene fallback is used only when
the temporal detector finds nothing inside an explicitly drawn area.

Detection needs background variation and can mistake stationary scenery for an
overlay. Inspect the result before applying. Sampling stays at native resolution;
a memory limit and cancellation bound background work. Crop mode does not offer
the mask detector.

**S** saves beside the focused video: `example.mov` becomes `example.mask.png`.
The PNG is 8-bit grayscale at the displayed video's native resolution (including
its display rotation), with **painted blue = white (255)** and **untouched red =
black (0)**. Saving runs in the background and reports success or failure on the
right of the status line; a successful save replaces an existing file at that path. Masks apply
to the whole clip, not an individual frame. They start blank each session; existing
PNG masks are not loaded automatically. Replacing the video set clears its masks.

For a direct visual check without keyboard automation:
`abner --mask a.mp4 b.mp4`. The ordinary `--view` selection is restored when leaving
mask mode.

## Cutting

`T` (or the CUT tab, or `--cut`) opens the focused clip on a timeline: a ruler,
chapters, the picture and sound as clips, subtitles and keyframes on one time axis.

| Key | |
|---|---|
| `I` / `O` | set in / out at the playhead |
| `X` | cut the selection (or restore it, when it is an existing cut) |
| `S` | split at the playhead — or under the pointer, where the razor line shows |
| `K` | snap to keyframes (default) or to frames |
| `⌘Z` | undo |
| `←` `→` / `⇧←` `⇧→` | step a frame / a keyframe |
| `E` | export `<name>.cut.<ext>` beside the source |
| `Esc` | clear the selection, then leave |

With keyframe snap the in point moves back and the out point forward to keyframes,
so the export is a lossless stream copy; the inspector says what the snap costs.
Frame snap is exact and re-encodes (H.264 CRF 16). Playback skips what is cut.
Drag a clip's edge to trim it. Scroll pans the timeline; pinch or the slider zooms it.

## The workspace

Compare, Mask and Crop sit beside the window controls. The source rail uses colored
numbers matching the `1`–`9` shortcuts; each clip shows its filename, resolution,
frame rate, codec and duration. The inspector below shows the focused clip's details.
Scroll the rail when there are more clips than fit.

The context row switches comparison views or exposes brush and export controls.
The charcoal canvas holds the videos, with shared pan and zoom. The transport stays
visible below it, with frame stepping, play/pause, scrubbing and timecodes. A quiet
footer shows the current tool, clip, zoom and save/export status. `Enter` hides the
whole interface and goes fullscreen to give the image the screen.

Both keys are rebindable in the config's `[keys]` table (`next_source`,
`fullscreen_video`).

The rail narrows below 900px and the inspector hides below 600px high; the minimum
window is 720×480. These changes keep tools and source selection reachable without
covering the image.

## Ideas for more views

- **Loupe** — a magnifier following the pointer showing A|B split at 4–8× inside the ring
- **Flicker** — auto-alternate A/B every N frames (temporal delta your eyes compute)
- **Vertical split** / horizontal wipe
- **Heatmap delta** — false-color per-pixel error with a scale
- **Signed delta** — grey = equal, warm = A brighter, cool = B brighter

## Requirements

- macOS (first target; the shader/loop are portable, fullscreen + font paths are mac-specific)
- Filmcraft supplies in-process startup metadata, preview color validation, playback,
  recent/timeline thumbnails and waveform decoding. Those paths need no FFmpeg/FFprobe.
- Timeline packet/stream metadata also uses Filmcraft, including AVI, MXF, MPEG TS/PS,
  raw MPEG-1/2, Y4M and APV; MP4/MOV and MKV/WebM chapters retain their native paths.
- Filmcraft also supplies native cut export, selective H.264 GOP rendering, ProRes crop
  export, text subtitle extraction and decoded frame inspection for indexed MP4/MOV/MKV/WebM
  inputs. See [native media coverage](docs/NATIVE-MEDIA.md) for codec and preservation limits.
- FFmpeg/FFprobe are not runtime requirements. Tests use their CLIs for synthetic
  fixtures and independent reference comparisons.

## Build

```sh
cargo build --release
./target/release/abner --help
./packaging/build-app.sh --open      # self-contained Abner.app (ad-hoc codesigns; no dylibs to bundle any more)
./packaging/build-app.sh --install   # …and copy it to /Applications
```

The bundle links no third-party libraries and needs no Homebrew media helpers.
`--with-cli-tools` is retained for explicitly bundling diagnostic tools. The app icon is `assets/app-icon.png` —
drop a new square PNG there and both the bundle's `.icns` and the bare binary's Dock
icon follow. Double-clicking a video on the bundle doesn't load it yet (see TASKS.md).

`cargo test` runs the regression suite: master-clock draining, exact seek, two-player
sync, framestep adoption, reader-thread cleanup (including a source that cannot be read,
which fails at once instead of wedging a reader), redraw cadence. It generates tiny test clips with ffmpeg under `$TMPDIR`.

A Filmcraft startup metadata read that doesn't return within 30s (a file on a volume
that has gone away) fails without joining the blocked read; a decoder whose seek fails is marked failed in
the HUD rather than silently drifting out of sync.

## License

MIT

## Build-time composition

The library entry point `abner::run(registry)` adds built-in processors to the
supplied registry. On Apple silicon Macs, standard builds include
[RIFE Metal](plugins/defaults/rife-metal/README.md).
RIFE Metal loads its separately installed runtime when selected for
processing. The experimental API is in `crates/processor-api`, with a
pass-through [Identity example](examples/identity/) for plugin developers.
A generated launcher can register additional processor crates before calling
`run`.

Keep that generated project outside the committed tree. Committed host files must
not identify, document or depend on external processors (generic examples and
builtins excepted).

The quick way is **`cargo build-with-plugins [cargo build flags]`**, run from the
repository root. Abner owns the crates under `plugins/defaults/`. Place any
additional processor crates or links directly under `plugins/`. Git ignores
these local entries, and `ABNER_PLUGINS` can point elsewhere. The command
generates a launcher under `plugins/.launcher` that registers every one of them,
then builds it into `target/<profile>/abner-with-plugins` with the flags you
passed (`--release` and friends). A plugin crate depends on
`abner-processor-api` and exports one function:

```rust
pub fn factories() -> Vec<Box<dyn abner_processor_api::Factory>>
```

A provider can keep its processor crate in its own repository and symlink the
crate directory into `plugins/`. Relative dependencies resolve from the real
crate directory. For standalone builds, declare `abner-processor-api` as a Git
dependency using `https://github.com/monomadic/abner` and pin a compatible
revision. The generated launcher patches that SDK dependency to the host's
local SDK so the registry and plugin share the same Rust types.

The launcher is its own package with its own lockfile, so a run changes nothing
git tracks — not even the root `Cargo.lock` — and the build stamp keeps the real
commit hash. Plain `cargo build` includes the built-in processors.
`packaging/build-app.sh --with-plugins` bundles the composed launcher instead. Runtime descriptors and settings may contain full processor
information. `--list-processors` prints descriptors; `--check-processor ID
SETTINGS_JSON` processes four synthetic frames without opening a window. These
are composition probes, not a processor export UI. The initial contract supports
streaming CPU RGB16 SDR frames, built-in processor presets, cancellation and
completed-frame progress. API 0.2 allows buffered output and an explicit end-of-stream
flush. Create a fresh session for each contiguous timeline segment or preview seek.

**Progress has two levels.** The host owns the **total**: frames done out of the
job's frames, which plugins never see. A plugin reports the **frame**: how far
its current call (`process`, `process_video`, `process_with_inputs`, `finish`,
`finish_video`) is through all of that call's work, through the reporter handed
to `Session::set_frame_progress_reporter`, as
`FrameProgress { phase, completed, total }`: `total` is the call's whole step
count (> 0, fixed for the call), `completed` only rises within the call, each
call starts over, and `phase` names the step without restarting the count. The
host folds that fraction into the total (whole frames + the working frame's share,
split evenly across chain stages, capped below a delivered frame, never moving
backwards), so a 30–130 s frame moves the total bar smoothly. The older
`Progress { phase, completed, total: Option }` via `set_progress_reporter` is
unchanged and stays **phase-local detail**: shown as text ("Tiles 3/12"), never as
the total. A plugin may send both. Wrapping sessions must forward both setters.
The CLI prints `Encoded frames: 3/120 · 2% · ~41m 10s left · stage 1 frame 21/63
transformer`. The Process panel shows the total bar by default; while a stretch
(more than one frame) renders it adds a thin frame bar under it, and a
one-frame render's total bar already is that frame. The Export job card adds the
frame bar under its total whenever the working stage reports steps. A stage with
only phase reports gets an activity mark on a one-frame total, with its phase
fraction in the thin bar.


The processing workspace is intended as an ordered experiment/editing workflow,
never a user-editable node canvas. Compression and re-encoding choices stay visible;
preserve original media and avoid unnecessary encoding generations.

Relative `ABNER_PLUGINS` paths are resolved against the calling directory. An
explicit missing, invalid or unreadable plugin directory fails the build; only
a missing default `plugins/` directory is treated as an empty installation.


### Process workspace

Open a clip and select **Process** (also available in View > Process or with
`--process`). The chain starts empty. **Add Processor** opens the installed catalog
with search, categories, settings and presets. Add appends the configured processor;
Cancel leaves the chain unchanged. Spatial inputs are configured after adding.

The left column lists the chain in processing order. Toggle a row’s switch to
bypass it; select a row to edit its settings. Drag rows or use Move up / Move down
to reorder them. Each processor retains its own settings when focus moves.

The viewer shares the app’s pan and zoom. Its transport offers previous rendered
frame, frame back, play/pause, frame forward and next rendered frame. Click or drag
the timeline to seek; Space plays/pauses, ← / → step, `[` / `]` change source, and
`+` duplicates the focused source. **Source | Rendered** or Tab compares the result
at the same view. **Render frame** (R) processes through the focused processor;
preview length offers one frame, five frames, one second or five seconds.
**Render segment** starts at the kept segment’s beginning and stops at its end or
the preview cache budget. The progress panel reports decoding and processor work.

**Renders** lists only previews belonging to the focused source, newest first.
Click a row to restore its frame, chain, settings and output rate; the backtick key
cycles that source’s renders. Switching sources hides unrelated rows without
throwing away their cached pixels. Returning to a source makes its retained renders
available again. Selecting a saved preview works while another render continues,
including previews made before spatial inputs changed. New output at the same time
keeps separate render identities, and job completion does not take over another
selected preview. A running row can show frames already received.

The shared `processors.preview_cache_mb` budget (1024 MB by default) bounds the
paired before/after preview images across sources and configurations; it excludes
model/session memory. A render disappears once all its frames are evicted, unless
it is still running or queued. Previews are session-local memory, not saved media
files. Changing settings or inputs switches the active cache; it does not cancel
an in-flight job. A new render takes priority and queues the interrupted job to
restart afterward. Cancel explicitly stops the current render; leaving Process
does not stop it.

The right inspector uses processor schemas: numeric sliders support exact entry,
enums cycle, booleans toggle and text fields accept typing. Enter commits text,
Escape cancels, and Cmd+A / Cmd+V work. Presets apply built-in defaults or saved
numeric settings. Missing required masks show 􀇾 beside the processor name and on
the mask tile. Either opens the editor; Apply clears the indicators. Model-discovery
paths stay out of the settings UI. Unsupported input types are labelled.

Preview sessions run on a worker, in chain order, including temporal buffering
and end-of-stream flush. The host negotiates native input formats and output rates;
retiming processors can emit multiple output frames for each source frame. Display
previews use 8-bit RGBA, while processor frames preserve the negotiated precision.
Processing currently requires constant-rate input and supported SDR color metadata;
incompatible tagged color is rejected. A processor may provide its own source
reader. Whole-clip/generator factories are not supported by frame preview.

**Downsize input** appears for processors declaring suitable 2× or 4× models.
It uses the host’s spline resampler to shrink the input, then selects the matching
upscale model to restore source size. The caption identifies downsized input;
this option cannot be combined with spatial inputs.

Use **Export** to write media with the enabled processor chain and source/timeline
edits. Previewing itself does not write output files. See
[the native media contract](docs/NATIVE-MEDIA.md) for codec and preservation limits.

`--processor ID --processor-settings JSON` opens Process focused on a registered
processor.

## Reusable Crop and Mask inputs

Sources has **Crop** and **Mask** controls. Timeline has **Crop clip**, targeting
one selected kept clip, or the kept clip under the playhead. Process settings show
Crop/Mask controls only for inputs the processor declares. Process embeds the editor
in its canvas, retaining the chain and settings; source and timeline controls open
a temporary editor tab. Reset, Cancel and Apply act on the draft. Choosing another
workspace cancels it. Add Processor omits these controls until the processor is added.

A source crop is copied into a newly opened timeline. Later source edits do not
change that timeline. Clip crops are included in timeline saves and undo; split
clips inherit their crop, while joining differently cropped clips is disabled.
Source/timeline crops fit into a fixed source-sized output canvas with black bars
where needed, preserving one output size across clips. Export applies them before
the processor chain. Cropped source comparisons support Single and Side by side.

Processors can receive independent image, masked-image, mask, and processing-region
inputs alongside the complete primary image. A masked image carries its pixels
and coverage separately. An image crop returns reduced dimensions; a region
returns coordinates without discarding image pixels. See
[the spatial input contract](docs/SPATIAL-INPUTS.md) and the runnable
[generic example](examples/spatial-inputs.rs).

Current scope: static rectangles and masks; spatial inputs belong to the first
enabled processor when rendering. Later stages can open the source-based editor,
but rendering their bindings requires moving them first. Source crops, source masks,
and plugin bindings last for the
current application session. Only timeline crops are saved to the timeline file;
PNG mask export remains available. Spatial bindings are not included in numeric
processor presets.
