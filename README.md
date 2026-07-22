# abner

**A/B testing for videos.** Point it at two (or more) video files — different encodes,
different upscalers, different grades of the same content — and flip between them while
they play in frame-locked sync.

```sh
abner original.mp4 encoded.mp4
abner --view delta original.mp4 encoded.mp4
abner a.mp4 b.mp4 c.mp4          # three-way works too
```

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
| `1` | **overlay** — videos stacked, flip with Enter (the classic A/B) |
| `2` | **side-by-side** — all videos in a row |
| `3` | **delta** — amplified \|A−B\| difference (`-`/`=` adjusts gain) |
| `4` | **split** — vertical wipe, divider follows the pointer |
| `5` | **checker** — checkerboard mix (`-`/`=` adjusts tile size) |
| `6` | **blend** — dissolve between A and B (`-`/`=` adjusts mix) |
| pinch | photo-style zoom on the pointer — every video pans/zooms to the same spot |
| drag / scroll | pan while zoomed (synced across videos) |
| `Z` | reset zoom |
| `[` `]` | slow down / speed up playback (0.25×–4×; `Backspace` resets) |
| `F` | fullscreen (borderless, same Space, instant) |
| `Tab` | toggle the info overlay (filename, path, res, fps, codec, bitrate, size, duration) |
| `Q` / `Esc` | quit |

In compare modes (delta/split/checker/blend) the pair is the active video vs the next
one; `Enter` rotates which pair you're looking at. Big letter badges mark what you're
seeing — A hugs the left edge, B the right. With the overlay hidden, `Enter` still
flashes the letter briefly so you know where you are.

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
```

`cargo test` runs the sync/seek/framestep regression suite (generates tiny test clips
with ffmpeg under `$TMPDIR`).

## License

MIT
