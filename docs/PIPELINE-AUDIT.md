# Export and Process preview efficiency audit, 2026-11-22

An audit of the processed export (`media_io::pipeline_export_with_inputs`) and the
Process page's preview renderer (`processor_gui::render`), made the day the export
went to three threads. Three items were fixed the same day and E7, E2, P3, E4, E6, E9, E8, P1, P4
and P6 after them, E5 with a FilmCraft change, and P7 was measured and closed. Nothing is
open; task 24 is in [HISTORY.md](../HISTORY.md).

**Summary.** With no plugin time, the export already runs at the hardware
encoder's ceiling, so its remaining waste is in particular paths: cropped exports,
cut timelines, colour round trips and locking. The preview still does everything on
one thread, as the export did before, but the host work around the plugin is now a
few milliseconds per conversion, so it matters for fast processors and is noise for
slow ones. The largest unmeasured costs in both are sessions created more often
than needed.

## Method and measurements

Apple M4 Pro, 14 cores. Release builds. Source: 3840×2160 24 fps H.264
(`testsrc2`, x264 ultrafast, yuv420p, GOP 24). Synthetic footage with no B
pictures; do not generalize the absolute figures to other codecs or machines.

Whole export, the ignored `export_throughput` test (HEVC 40 Mbps, 2 s of source):

| Run | Result |
|---|---|
| Abner, no stages, 48 frames | 0.97 s, 49.7 fps |
| Abner, frame-doubling float stage, 96 frames | 1.73 s, 55.6 fps |
| ffmpeg `-hwaccel videotoolbox … -c:v hevc_videotoolbox -b:v 40M`, 48 frames | 1.05 s |
| the same ffmpeg command, 240 frames | 4.28 s, 56 fps |
| ffmpeg decode only, 240 frames | 1.78 s, 135 fps |

Abner matches ffmpeg's hardware path. The VideoToolbox HEVC encoder is the limit,
about 55 fps at 4K on this machine. The 2 s runs include set-up and finishing, so
they understate steady state; a longer Abner run was not made.

Host conversions, one 4K frame, mean of 8 (a temporary benchmark, not committed):

| Operation | ms |
|---|---|
| validate yuv420p8 / yuv420p10 / rgba8 / rgbf32 | 0.00 / 0.27 / 0.67 / 1.35 |
| yuv420p8 → rgba8 (as built, through RGB16) | 5.07 |
| yuv420p8 → rgba8 (direct fixed-point, sketch) | 1.39 |
| yuv420p8 → rgb16 | 4.43 |
| yuv420p10 → rgba8 | 5.69 |
| yuv420p8 → rgbf32 | 2.38 |
| rgbf32 → rgba8 | 1.73 |
| rgbf32 → yuv420p8 | 2.97 |
| rgbf32 → yuv422p10 | 2.62 |
| yuv420p8 → yuv422p10 (plane resample) | 3.84 |
| rgb16 → yuv422p10 | 3.88 |
| export crop copy, 3600×2000, per-sample iterator (before) | 12.65 |
| export crop copy, whole rows (after) | 0.79 |

Not measured: any real processor, ProRes export throughput, audio encode and
container finishing on long files, the preview renderer end to end.

## Export

### E1. Crop copy was per sample on one thread — fixed

`Prep::next` copied the cropped RGB16 picture through a per-sample iterator:
12.65 ms per 4K frame. With the RGB16 conversions around it the reader thread took
roughly 22 ms per frame, which is below the encoder's ceiling, so a cropped export
was reader-bound. It now copies whole rows (0.79 ms).

### E2. Sessions were rebuilt for every kept range — fixed for processors that opt in

The loop over `keeps` created every stage's session again for each range. The
processor API now has `Session::reset` (discard buffered input and temporal state,
keep what is expensive, return a fresh cancellation token; `false` by default), and
the export keeps each stage's session in an `abner_processing::reuse::Pool` across
its ranges, resetting it instead of creating it. A processor that does not
implement `reset` is created per range as before, so the gain arrives as
processors adopt it. Not measured: no in-repo processor is expensive to create.

### E3. The registry lock was held for the whole export — fixed

The export worker locked the shared `Mutex<Registry>` before the export and kept
it to the end. The preview worker takes the same lock, so a Render press during an
export waited, silently, until the export finished. Both jobs now reach the
registry through `processing_source::RegistryAccess` and hold the lock only while
sessions and the source are created and the chain's rate is asked
(`jobs_release_the_registry_while_frames_are_processed`).

A consequence to know about: an export and a preview render can now run their
sessions at the same time. Before, the lock made them take turns. Two heavy
processors running together share the accelerator and its memory.

### E4. Untouched frames took a colour round trip — fixed

The HEVC encoder was given RGBA8: with no stages a frame went decoder planes →
RGB16 → RGBA8 in the reader, then FilmCraft's `VideoEncoder` wrapper converted
RGBA8 back to 4:2:0 on the CPU. The export now drives
`filmcraft_platform::videotoolbox_encode::VtEncoder` itself and gives it the
chain's `Yuv420p8` planes: host planar frames are always BT.709 limited range
(`color::from_decoded` keeps planes only for those), which is what the session
declares in its VUI. With no stages the decoder's planes go to the encoder as
decoded (`hevc_export_takes_planes_and_pictures_are_optional`); a processor's
output is converted once, at 16 bits or more, instead of through 8-bit RGBA.
Wall-clock is unchanged while the encoder is the limit (50.0 and 54.8 fps on the
4K benchmark); the CPU saving is below, with E6.

### E5. HEVC output was 8-bit Main only — fixed

Ten-bit sources and float chains were quantised to 8 bits at the encoder, the
largest loss in the pipeline. FilmCraft's `VtEncoder` now has
`VtProfile::HevcMain10` (Main 10 profile, P010-style `x420` buffers,
`VtEncoder::encode_10` for 10-bit planes; fork branch `abner-hevc-main10`). The
export picks Main 10 when there is more than 8 bits to keep: a source whose pixel
format is deeper than 8 bits (`deep_pixel_format`), any processor in the chain, or
a timeline crop (both work at 16 bits or float). An untouched 8-bit source stays
Main, the more widely decoded profile. The sink is then `Yuv420p10`, which E4's
plane path hands to the encoder unchanged. A machine whose encoder refuses Main 10
logs a warning and exports Main, converting the planes down.

`ten_bit_sources_export_as_hevc_main10`: a 10-bit source comes out as
`Main 10, yuv420p10le` with its low bits in use; an 8-bit one as `Main, yuv420p`.
4K benchmark with a stage (now Main 10): 55.6 fps, as before.

### E6. Every frame was converted to RGBA for the page — fixed

`write_frames` converted every chain output to RGBA8, which the HEVC encoder used
as input but ProRes did not, and the page keeps only the newest frame between
polls. The encoders now take the chain's own format (E4), and the RGBA picture is
made only when the page has taken the previous one
(`pipeline_export_with_inputs`'s `want_picture`; a frame without one carries
empty pixels and only moves the page's counters). Callers that pass no check get
a picture for every frame, as before.

User CPU for the whole 4K benchmark run (`export_throughput`, both chains, its
ffmpeg fixture included), two runs each:

| Build | User CPU |
|---|---|
| Before E4/E6 | 12.26 s, 13.53 s |
| After, a picture for every frame | 11.62 s, 11.54 s |
| After, no picture taken (`ABNER_BENCH_NO_PICTURE=1`) | 6.55 s, 6.67 s |

### E7. Audio waited for video, and MKV was written twice — fixed

Audio was encoded after the last video frame, on the calling thread, and an MKV
export was muxed as MP4 and then remuxed whole into Matroska. Measured on a
10-minute 1080p 24 fps source with stereo AAC (1.5 GB): the audio pass took 8.7 s
and the remux 2.35 s.

Encoding exports now encode audio beside the video (`AudioFeed`: one thread per
stream, a bounded queue, each packet written once the video has reached its time,
so the file is interleaved rather than all audio at the end). A processed export
to MKV is written directly by `MkvSink`, which takes the MP4 writer's packets and
timing (AAC priming becomes the codec delay) and appends chapters, attachments and
tags through the same `mkv_append` the remux uses
(`pipeline_matroska_is_written_directly_and_matches_mov` checks the packets and
times against the MOV of the same job). The same 10-minute export, HEVC 20 Mbps,
no stages:

| Output | Before | After |
|---|---|---|
| MP4 | 75.97 s | 67.32 s |
| MKV | 77.90 s | 67.28 s |

Still remuxed: packet-copy and crop exports to MKV (`export_with_cancel`), whose
packets come from any source container through the MP4 writer.

### E8. Every export crop went through RGB16 — fixed for crops on the chroma grid

The export crop converted the frame to RGB16 (4.4 ms at 4K) so an odd origin
could not split a chroma sample. `media_io::crop_planes` now cuts the planes
directly when the crop's origin and size are multiples of the chroma subsampling
(`E` in the crop marquee floors to even sides already); each pixel keeps its own
chroma sample, so the planes convert to exactly the RGB16 cut
(`aligned_crops_cut_planes_like_rgb16`). Off the grid the RGB16 cut remains.
Cropped 4K benchmark (`ABNER_BENCH_CROP=120,80,3600,2000`, no pictures): user CPU
10.1 s → 6.2 s, 54.8 → 56.7 fps with no stages.

The timeline crop (`spatial::apply_crop`) is not a cut: it scales the cropped
region back to the frame with a bilinear resample, so it stays in RGB16.

### E9. Y'CbCr → RGBA8 went through an RGB16 intermediate — fixed, bit-identical

`color::convert` had no direct planar → RGBA8 path: it built a 48-bit-per-pixel
RGB16 frame (50 MB at 4K) through `media::rgb16`, which reads each sample through a
`&dyn Fn`, and then narrowed it. `color::planar_rgb` now does planar → RGBA8 and
planar → RGB16 in one pass with the same per-pixel arithmetic: the normalised code
values come from a table built by the same `normalize_y` / `normalize_c` (the same
`f32`s without a division and a conversion per sample), `ycbcr_to_rgb` and the
RGB16 rounding are unchanged, and RGBA8 is narrowed per sample as before.
`print_conversion_digests` is unchanged (TOTAL `dc5c45cd6fd143d8` before and after).

| 4K, mean of 8 | Before | After |
|---|---|---|
| yuv420p8 → rgba8 | 6.61 ms | 3.76 ms |
| yuv420p10 → rgba8 | 5.48 ms | 3.33 ms |
| yuv420p8 → rgb16 | 4.44 ms | 2.95 ms |

The fixed-point sketch's 1.39 ms needs what remains: a float division per pixel
(green) and a second rounding (RGB16 → RGBA8). Removing those changes output, and
the 2.4 ms left at 4K was judged not worth a new reference: closed as is.

## Process preview

### P1. One thread did everything — split for segment renders

`render` decoded, built the "before" RGBA frame, cropped, ran the chain and
converted for display on the worker thread. A segment render now decodes, builds
the "before" picture and prepares the input (the timeline's crops, conversion to
the first stage's format: the export's own `media_io::Prep`) on a reader thread
while the worker runs the chain; a one-frame render, with nothing to overlap,
stays on one thread. The display conversion of the "after" picture is still on
the chain's thread.

`preview_throughput` (4K, 24-frame segment through a one-frame-delay RGB16
stage, two runs each):

| Stage time per frame | Before | After |
|---|---|---|
| 0 ms | 297 ms | 309 ms |
| 20 ms | 890, 874 ms | 693, 667 ms |

With an instant stage the reader is the slower side and nothing changes; the
gain grows with the processor's own time, up to the reader's ~14 ms per 4K frame.
The chain now plans from the first stage's format, as the export does, instead
of the decoder's.

### P2. Output frames were validated four times — fixed

The plan validated each frame at the plugin's output, `to_sink` validated it again
inside `convert`, once more after it, and `emit` a fourth time; the "before" branch
re-validated the decoder's frame. `FormatPlan::to_sink` now uses
`convert_validated` and keeps only the batch budget, `emit` checks geometry only,
and `color::rgba8_validated` serves frames a decoder or plan already checked (the
export writer uses it too). Each scan was at most 1.35 ms at 4K.

### P3. Every Render press created the sessions from scratch — fixed for processors that opt in

The preview worker keeps the last render's sessions in its own `Pool`, keyed by
processor, settings (after the Downsize overlay) and input size, and resets them
for the next press. Spatial inputs are bound per render (`abner_processor_api::bind`),
so a mask edit reuses the model too. The pool is settled after every render (kept
after success or cancellation, dropped after any other failure or a panic) and
cleared when an export starts (`Panel::release_sessions`, so a model is not held
twice) and when the clips go. Same opt-in caveat as E2
(`resettable_sessions_are_created_once_per_settings`).

### P4. Every press opened a decoder and cold-seeked — fixed

`Source::open` spawned a new native player per render. The preview worker now
keeps its decoder between renders (`Kept::decoder`, `Source::open_kept`: reused
for the same file at the same size, dropped on a release like the sessions), so a
press near the last one seeks inside what it has decoded. Six one-frame 4K
renders stepping forward one frame: 792 ms with a fresh decoder each, 49 ms with
the kept one (`preview_throughput`).

Reusing a reader exposed a player bug that a fresh one never hit:
`take_next_native` read on to the first cached frame past the position, which
could be a frame from an older span beyond frames not decoded yet. It now takes
only the frame that follows in the same decoded span
(`kept_decoder_renders_what_a_fresh_one_does` fails without it). The app's own
`take_next` calls follow a seek and were not affected.

A one-frame render still reads one frame ahead, for the first frame's duration.

### P5. The registry lock was held for the whole render — fixed

With E3. The page's output-rate query (`update_rate`, a `try_lock`) no longer
fails for the length of a render, only while sessions are being created.

### P6. A 4K before/after pair was 66 MB — fixed

Both pictures were kept as full RGBA8, so the default 1024 MB budget held 15 pairs
at 4K: about 0.6 s at 24 fps. The stored "before" was never drawn: the Source view
shows the display player's own frame. Its only use was the "N frames with changed
pixels" count. A `Preview` now carries the "after" and a `changed` flag, which the
worker sets while it still holds the original, and the budget, the frame count and
the stash price the "after" alone: 30 frames at 4K, ~1.25 s. When a stage changes
the size, the original is not converted at all (3.8 ms per 4K frame saved), since
the frame has changed by definition. The originals waiting for a buffering stage's
output are still held in flight, as before.

### P7. Source metadata is re-read on every render — measured, left as is

`check_processable` reads the container again on each press. Measured with
`preview_throughput`: 0.07 ms per check on the 4K MP4 (a header-only read),
against about 8 ms for a one-frame render with the kept decoder. Not worth a
cache. `require_constant_rate` runs only for retimed chains and was not measured.
