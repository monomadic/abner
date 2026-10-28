# abner

**A/B testing for videos.** Point it at two (or more) video files — different encodes,
different upscalers, different grades of the same content — and flip between them while
they play in frame-locked sync.

```sh
abner original.mp4 encoded.mp4
abner --view delta original.mp4 encoded.mp4
abner a.mp4 b.mp4 c.mp4          # three-way works too
abner                            # launch window — drag clips onto it
abner --no-video-splash           # just the centered logo — drag clips onto it
```

`--view` takes `overlay`, `sbs`, `delta`, `split`, `checker` or `blend`, so every
mode is reachable from the command line.

The animated splash appears only when launched with no arguments. Video paths
start playback directly without probing or decoding the splash video. Any arguments
also make the empty window logo-only, including after closing the last clip.

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
| `M` | toggle mask painting on the focused video (pauses on entry) |
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
The brush outline follows the same transform as the image. **Enter** (or a clip's number key) switches the
focused video and its separate mask. **M** or **Esc** hides the layer and restores
the comparison view; masks persist in memory, and playback stays paused until Space.

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

The library entry point `abner::run(registry)` accepts an external processor
registry; the ordinary executable supplies an empty one. The experimental API
is in `crates/processor-api`. A separately generated launcher can depend on
independent processor crates and register their factories before calling `run`.

Keep that generated project outside the committed tree. Committed host files must
not identify, document or depend on external processors (generic examples and
builtins excepted).

The quick way is **`cargo build-with-plugins [cargo build flags]`**, run from the
repository root: drop processor crates into the gitignored `plugins/` directory
(one crate per folder; `ABNER_PLUGINS` points elsewhere) and the command
generates a launcher under `plugins/.launcher` that registers every one of them,
then builds it into `target/<profile>/abner-with-plugins` with the flags you
passed (`--release` and friends). A plugin crate depends on
`abner-processor-api` by path and exports one function:

```rust
pub fn factories() -> Vec<Box<dyn abner_processor_api::Factory>>
```

The launcher is its own package with its own lockfile, so a run changes nothing
git tracks — not even the root `Cargo.lock` — and the build stamp keeps the real
commit hash. Plain `cargo build` still produces the empty-registry binary.
`packaging/build-app.sh --with-plugins` bundles the composed launcher instead. Runtime descriptors and settings may contain full processor
information. `--list-processors` prints descriptors; `--check-processor ID
SETTINGS_JSON` processes four synthetic frames without opening a window. These
are composition probes, not a processor export UI. The initial contract supports
streaming CPU RGB16 SDR frames, built-in processor presets, cancellation and
completed-frame progress. API 0.2 allows buffered output and an explicit end-of-stream
flush. Create a fresh session for each contiguous timeline segment or preview seek.


The processing workspace is intended as an ordered experiment/editing workflow,
never a user-editable node canvas. Compression and re-encoding choices stay visible;
preserve original media and avoid unnecessary encoding generations.

Relative `ABNER_PLUGINS` paths are resolved against the calling directory. An
explicit missing, invalid or unreadable plugin directory fails the build; only
a missing default `plugins/` directory is treated as an empty installation.


### Process workspace

Open a clip and select **Process**, immediately after Timeline (also available in
View > Process or with `--process`). The old floating preview dialog and P toggle
are gone. Installed processors appear on the left in processing order. Toggle a
row’s checkbox to bypass it; select a row to focus its settings, and use Move up /
Move down to reorder it. Each processor retains its own settings when focus moves.

The large viewer uses the normal clip transport: five icon buttons under it
(previous rendered frame, a frame back, play / pause, a frame on, next rendered
frame; each names itself on hover). Click the clip timeline to seek or drag its
playhead; Space plays/pauses, ← / → step a source frame, `[` / `]` walk the
sources, `+` duplicates the focused source. The page has the same footer bar as
the others, at the Timeline's height: **Render frame** (R), drawn like the
Timeline's Split button, renders the current frame through enabled processors
up to and including the focused processor; the status line and key hints sit
beside it. The returned frame replaces source display at that position and gets
a blue timeline mark; while a render runs, the frame asked for pulses amber on
the timeline and the temporal context being decoded sweeps in behind it.
**Prev rendered** / **Next rendered** (Shift-Left / Shift-Right) jump between
marked frames. ⌃⇧[ / ⌃⇧] move one header tab back or forward in every
workspace (Sources · Timeline · Process · Crop · Mask, no wrap). Seeking
preserves marks; settings, order, bypass, source or timeline edits invalidate them.
Preview frames survive switching workspaces, within the `processors.preview_cache_mb`
budget (1024 MB by default); evicted frames lose their marks. Changing processor focus conservatively clears the cache.

The right inspector uses runtime schemas: numeric sliders support exact entry by
clicking their labels, enums cycle, booleans toggle, and text fields accept paths.
Enter commits text, Escape cancels; Cmd+A / Cmd+V work. Scroll to reach additional
settings. Cycle processor presets applies built-in presets over schema defaults.
Unsupported nested/auxiliary types are labelled. Factories may opt out of frame
previews with `supports_preview()`; the host has no processor-specific branches.

Preview sessions run on a worker, in chain order, including each stage’s buffered
output and end-of-stream flush. To preserve temporal history, decoding begins at
the current kept segment’s start, continues through the requested frame and up to
24 following frames, stopping at the segment boundary or EOF. Only the requested
frame is cached. This can take time for distant frames; context preparation shows
frame progress. Reconfiguration cancels old work and rejects late results. Device
calls already executing may finish before cancellation takes effect.

Plugins are fed the source at its **native size** (display dimensions, rotation
applied), decoded as 8-bit SDR RGBA then expanded to RGB16, and their output is
rounded back to 8 bits for display — so a preview is what the plugin makes of the
real pixels, but not a 16-bit export. `processors.preview_cache_mb` bounds the paired
before/after frames a section keeps (a 1080p pair with a 2× output is ~41 MB a
frame, 4K ~166 MB; the number of frames decoded past the requested one shrinks to
fit) and excludes model/session memory; a pair that does not fit at all is refused
with the key's name. Current previews require constant-rate,
timestamp-preserving processors and SDR Rec.709; incompatible tagged color is
rejected. Persistent stack presets, auxiliary inputs, interpolation, full color
negotiation and processor export remain future work. Process does not write media;
Timeline’s existing export still exports its source edits, without processing.

`--processor ID --processor-settings JSON` opens Process focused on a registered
processor. Ordinary builds with an empty registry show an empty processor list.
