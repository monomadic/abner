# Export and Process preview efficiency audit, 2026-11-22

An audit of the processed export (`media_io::pipeline_export_with_inputs`) and the
Process page's preview renderer (`processor_gui::render`), made the day the export
went to three threads. Three items were fixed the same day and E7, E2, P3, E4, E6 and E9 after them; the rest are open and
tracked as task 24 in [TASKS.md](../TASKS.md).

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

### E5. HEVC output is 8-bit Main only — open, needs FilmCraft

Ten-bit sources and float chains are quantised to 8 bits at the encoder. This is a
quality limit, not a speed one, and the largest loss in the pipeline. FilmCraft's
`VtEncoder` (pinned rev 45c1213) offers only `VtProfile::HevcMain` and NV12 input
buffers: Main 10 needs a profile, 10-bit (`x420`) pixel buffers and a plane fill in
FilmCraft itself. With that, the export's sink becomes `Yuv420p10` for 10-bit
sources and chains, and E4's plane path carries it unchanged.

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

### E8. Every crop goes through RGB16 — open

The export crop converts the frame to RGB16 (4.4 ms at 4K) so an odd origin cannot
split a chroma sample, and a timeline crop converts again. A crop whose origin and
size are multiples of the chroma subsampling can be cut from the planes directly,
with no conversion in either direction. `E` in the crop marquee already floors to
even sides.

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
(green) and a second rounding (RGB16 → RGBA8). Removing those changes output, so
it waits for a decision that the new rounding is the reference.

## Process preview

### P1. One thread does everything — open

`render` decodes, builds the "before" RGBA frame, crops, runs the chain and
converts for display on the worker thread. Summed from the table, that is roughly
12 to 15 ms of host work per 4K frame that the processor waits on (not measured
end to end). The export's reader / chain / writer split applies here unchanged: a
reader thread for decode, "before" and crop, the chain thread, and the display
conversion after it. Worth it for segment renders through fast processors; noise
for a processor that takes a second per frame.

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

### P4. Every press opens a decoder and cold-seeks — open

`Source::open` spawns a new native player per render and exact-seeks it (a cold
jump inside a long GOP is 285 to 470 ms at 1080p, per the player's own figures).
A one-frame render also reads two frames: the second only supplies the first
one's duration, which a targeted render could take from the frame rate. Keeping
one processing decoder per source between renders would turn most presses into a
seek inside its cache.

### P5. The registry lock was held for the whole render — fixed

With E3. The page's output-rate query (`update_rate`, a `try_lock`) no longer
fails for the length of a render, only while sessions are being created.

### P6. A 4K before/after pair is 66 MB — open, design

Both pictures are kept as full RGBA8, so the default 1024 MB budget holds 15 pairs
at 4K: about 0.6 s at 24 fps. The "before" is the source frame the display player
can deliver again, so storing only the "after" would roughly double a section's
length, at the cost of pairing against the player's cache (and of the retimed
case, where one original pairs with several outputs and is already shared).

### P7. Source metadata is re-read on every render — open, minor

`check_processable` and, for retimed chains, `require_constant_rate` read the
container again on each press. Unmeasured; cache per path with the source's
probe if a profile shows it.
