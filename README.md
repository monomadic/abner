# abner

**A/B testing for videos.** Point it at two (or more) video files — different encodes,
different upscalers, different grades of the same content — and flip between them while
they play in frame-locked sync.

```sh
abner original.mp4 encoded.mp4
abner --view delta original.mp4 encoded.mp4
abner a.mp4 b.mp4 c.mp4          # three-way works too
abner                            # launch window — drag clips onto it
```

`--view` takes `overlay`, `sbs`, `delta`, `split`, `checker` or `blend`, so every
mode is reachable from the command line.

## Configuration

[`abner.default.toml`](abner.default.toml) lists every setting with its default
(window size, starting view, start paused, seek step, delta gain, blend, checker
size, brush size) and is built into the binary. To change something, create a
TOML file with just those keys, in the same tables:

```toml
[playback]
view = "delta"
seek_step = 5.0
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

Born out of the [switchblade](../switchblade) project's graphics stack: in-process libav
decode (VideoToolbox for h264/hevc/prores), a wgpu renderer with mip-chained video
textures (no minification shimmer on 4K sources), idle-throttled render loop, and
mpv-style fake fullscreen.

## Sync model

One master clock drives every stream. Players decode into small bounded queues; each
frame the app pops everything due and shows the newest. Flipping the displayed video
(`Enter`) switches *textures*, not players — the other stream was already decoding the
same instant, so the flip is seamless and time never jumps. Pause stops the clock
(backpressure stalls every decoder for free); framesteps are exact seeks whose landing
frame's true pts is adopted back into the clock, so stepping can't accumulate drift.

## Keys

| Key | Action |
|---|---|
| `Enter` | flip to the next video (in overlay mode) |
| `Space` | pause / play |
| `<` `>` (or `,` `.`) | frame-step back / forward |
| `←` `→` | seek ±1s |
| `1`…`9` | show clip 1, 2, … directly |
| `V` / `Shift-V` | next / previous view, cycling through: |
| | **overlay** — videos stacked, flip with Enter (the classic A/B) |
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
| `[` `]` | slow down / speed up playback (0.25×–4×; `Backspace` resets) |
| `F` | fullscreen (borderless, same Space, instant) |
| `Tab` | toggle the workspace controls |
| `Q` | quit |
| `Esc` | leave fullscreen, else quit |

In compare modes (delta/split/checker/blend) the pair is the active video vs the next
one; `Enter` rotates which pair you're looking at. Small colored number badges match
the source list and the `1`–`9` shortcuts. With the controls hidden, `Enter` still
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

## The workspace

Compare, Mask and Crop sit beside the window controls. The source rail uses colored
numbers matching the `1`–`9` shortcuts; each clip shows its filename, resolution,
frame rate, codec and duration. The inspector below shows the focused clip's details.
Scroll the rail when there are more clips than fit.

The context row switches comparison views or exposes brush and export controls.
The charcoal canvas holds the videos, with shared pan and zoom. The transport stays
visible below it, with frame stepping, play/pause, scrubbing and timecodes. A quiet
footer shows the current tool, clip, zoom and save/export status. `Tab` hides the
whole interface to give the image the window.

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
- ffmpeg 8.x — the `ffprobe` CLI for metadata **and** the dev libraries the in-process
  decoder links against (`brew install ffmpeg` provides both)

## Build

```sh
cargo build --release
./target/release/abner --help
./packaging/build-app.sh --open      # self-contained Abner.app (bundles the ffmpeg dylibs, ad-hoc codesigns)
./packaging/build-app.sh --install   # …and copy it to /Applications
```

The bundle carries its own copies of the ffmpeg libraries, so it runs on a machine
without Homebrew; the `ffprobe` CLI it shells out to at startup is still looked up on
PATH (`--with-cli-tools` copies that in too). The app icon is `assets/app-icon.png` —
drop a new square PNG there and both the bundle's `.icns` and the bare binary's Dock
icon follow. Double-clicking a video on the bundle doesn't load it yet (see TASKS.md).

`cargo test` runs the regression suite: master-clock draining, exact seek, two-player
sync, framestep adoption, reader-thread cleanup (including a reader wedged in libav I/O
on a FIFO), redraw cadence. It generates tiny test clips with ffmpeg under `$TMPDIR`.

A startup `ffprobe` that doesn't return within 30s (a file on a volume that has gone
away) is an error rather than a hang; a decoder whose seek fails is marked failed in
the HUD rather than silently drifting out of sync.

## License

MIT
