# Mask autodetect: optimisation notes

Written 2026-11-23 from a read of `src/watermark.rs`, `src/watermark/static_overlay.rs`,
`src/watermark/stroke_lift.rs` and the callers in `src/app.rs`. The original cost
estimates are kept below. The first measurements and changes are in
[the benchmark results](#2026-11-23-benchmark-results).

The original pipeline, in one paragraph: `Task::spawn` picks evenly spaced sample times in
the chosen range (8 to 512 frames, or every frame), a worker thread reads each one with an
exact `video_frame` request, converts it to RGBA8 at full size and keeps the RGB of the
search area plus a 96 px context border. `static_overlay::detect_checked` then computes a
per-pixel temporal median and 1/8 to 7/8 spread, compares each pixel's spread with its
surroundings (a fixed overlay damps it by `1 − alpha`), requires persistent edges and a
moving band around each connected component, and thresholds to a binary mask
(`opacity ≥ 0.35`, `confidence ≥ 0.25`). `stroke_lift` is a fallback used only inside a
drawn box when the temporal pass finds nothing.

## 0. Measure first

- The ignored test `watermark::tests::inspect_detection_clip` already runs `analyze` on a
  real file (`ABNER_DETECT_INPUT`, `_OUTPUT`, `_COUNT`, `_BOX`). Extend it to print two
  timings, reading frames and analysing, and peak RSS.
- Baseline matrix: 1080p and 4K; whole frame and a drawn 400×200 box; 64 frames and every
  frame; a 4 s GOP H.264 clip and an all-intra ProRes clip.
- Keep one or two reference masks (PNG) per clip so quality changes can be diffed pixel
  for pixel, not judged by eye.

## Performance

### P1. "Every frame" decodes the range once per batch

Implemented with a bounded frame set. See [the one-pass benchmarks](#one-pass-every-frame).

`watermark.rs:163-179`. With `samples == None` and more than 64 frames, the range is split
into `groups = total.div_ceil(64)` interleaved batches (`skip(group).step_by(groups)`) and
`analyze` runs once per batch. Each call reopens the source and walks the whole range with
targets `groups` frames apart, so the decoder passes over the range `groups` times.

- 10 minutes at 30 fps: 18,000 frames, 282 batches, about 282 passes over the clip
  (roughly 5 million frame decodes for 18,000 used).
- The timeout is `total × 3 s` (minimum 180 s), which this will reach on long clips.

Fix, in order of effort:

1. **One pass, exact detector unchanged.** Decode the range once, sequentially. Keep a
   bounded set of K frames (64, or what the memory rule allows) chosen for diversity:
   admit a frame when the set is not full, otherwise replace the member closest to its
   neighbour when the new frame is farther from the set than that pair is from each other
   (distance on a 64×36 luma thumbnail of the search area, so the test is cheap). Run
   `detect_checked` once on the result. This also delivers Q3 and Q4.
2. **Streaming statistics** if more than K frames of evidence is wanted: per pixel and
   channel keep a running mean and mean absolute deviation (a spread proxy), and for the
   two edge axes a running signed difference and magnitude. Constant memory in the frame
   count. This is a different estimator from the 1/8 to 7/8 quantile spread, so it needs
   the reference masks from step 0 before it can replace the exact one.

Check first that sequential `video_frame` requests continue FilmCraft's running decoder
(CLAUDE.md says a forward request in reach does); if not, read through
`Player::spawn_native` / `take_next_native` as the Process preview does.

The pinned FilmCraft `GopCache` continues through close forward requests, including
the next sync sample. Its `running_decoder_continues_through_a_nearby_sync_sample`
test covers that transition. This is the decoder path to use for P1 on long-GOP input.

### P2. Sparse samples decode half a GOP each

`watermark.rs:254-258`. Each sample is `FrameRequest::full(t + 0.25 frame)`, an exact
request, so FilmCraft decodes from the previous sync sample to the target. On a 4 s GOP at
30 fps that averages about 60 decodes per frame kept. Detection has no use for the exact
frame: any frame near `t` is the same evidence.

Fix: snap each sample time to a sync sample. `fc.rs` already has the table
(`before`, `at_or_after`, around line 124; `nearest` is test-only and can lose its
`#[cfg(test)]`). After snapping, dedupe the times and, when fewer than the requested count
remain (long GOPs, short range), fill the gaps with exact requests as today. All-intra
sources are unaffected. Expected: up to GOP/2 fewer decodes for whole-clip runs.

Keyframes are also the encoder's cleanest pictures and tend to sit on scene cuts, which
helps Q3.

### P3. 4K whole-frame detection is refused above about 27 frames

`watermark.rs:150`: `n × (batch × 3 + 112) ≤ 1536 MiB`, where `n` is the context area.
At 3840×2160, `n = 8,294,400`; 64 frames needs 2.5 GB, so the default setting fails with
"Detection needs more memory". Only the 8 and 16 frame stops work at 4K whole frame.

Fix: coarse to fine.

1. For a whole-frame (or large) search, box-downsample each sample by 2 (1080p-class) or
   4 (4K) as it is read and run the detector on that, with `radius` and the component
   size floors scaled by the same factor.
2. Take the bounding boxes of the surviving components, pad them by the detector's halo,
   and re-run at full resolution on those boxes only, from the same decoded frames (keep
   the full-size crop per box, or decode a second time now that P2 makes that cheap).

Memory drops by 4 to 16 times and so does the analysis. Risk: thin strokes (1 to 2 px)
vanish at quarter size; the box average of a damped stroke still damps, but verify on a
fine-text watermark before trusting it. A drawn box under some area threshold skips the
coarse step.

### P4. The analysis is one thread and allocates per pixel

`static_overlay.rs:13-23` (`temporal`): for every pixel and channel it collects a new
`Vec<f32>` through `Frame::pixel` (a u8 → f32 divide per read) and fully sorts it. At 4K
whole frame with 64 frames that is 25 million allocations and sorts.

- Work on `u8`. For N ≤ 256 frames, sort a reused `[u8; N]` scratch (or use
  `select_nth_unstable` three times: the median, the 1/8 and the 7/8). For large N a
  256-bin counting pass gives all three in one sweep.
- Hoist the scratch out of the loop; read rows, not pixels: for row `y`, slice each
  frame's row once and walk the slices together.
- `par_chunks_mut` over output rows with rayon (already a dependency, used by the host
  conversions). The context grid loop (`local_grid` / `outer_grid`) and the per-pixel
  opacity and edge loop parallelise the same way; the flood fills stay serial.
- `checkpoint` is a `FnMut` captured mutably: under rayon, check the cancel flag from an
  `AtomicBool` per row instead.
- The same pattern is in `stroke_lift.rs:147-160`.

Results must stay bit-identical to the current maps; the existing unit tests plus the
reference masks cover that.

### P5. Smaller costs

- `watermark.rs:262-268`: the whole frame is converted to RGBA and the search area copied
  out per pixel. Convert only the context rows, or copy rows with `chunks_exact(4)`; for a
  small drawn box on 4K the conversion is nearly all waste.
- `watermark.rs:271`: the duplicate test compares the new frame against every kept frame.
  It exits on the first differing byte, so it is cheap unless frames really repeat; a
  64-bit hash per frame makes it O(1) either way.
- `stroke_lift.rs:16-34` (`pass`): min/max over a `2r+1` window computed naively,
  O(w·h·r) per pass, four passes per opening, two openings per polarity. A van Herk /
  Gil-Werman running min/max is O(w·h). Only matters for large drawn boxes (`coarse` is
  up to 40).
- `watermark.rs:145`: after P1 and P2 the timeout can be a flat stall detector (no
  progress for N seconds) instead of a budget proportional to the frame count.

## Quality

### Q1. Keep the maps; add a sensitivity control to the review step

The Process review bar now has a Sensitivity slider from 0% to 100%, starting at
50%. Each step thresholds the saved opacity and confidence values, removes
small islands, and applies the current grow/shrink offset. Add to mask uses the
same pixels as the orange preview. A run with only weaker candidates starts in
review with a prompt to raise sensitivity. The worker keeps the detector's
structural checks, and coarse searches scout for weak candidates before native
refinement. Drawn-box stroke proposals retain their accepted stroke scores.

`ABNER_DETECT_REVIEW=1` in the release diagnostic checks the saved 50% mask against
the worker's default selection and reports the three settings below. FilmCraft's
decode counters stay fixed while recalculating the masks.

| Fixture | 0% pixels | 50% pixels | 100% pixels |
| --- | ---: | ---: | ---: |
| 1080p H.264, Every frame | 38,600 | 39,070 | 39,914 |
| 4K thin-stroke fixture, whole frame, 64 samples | 1,483 | 1,680 | 1,831 |
| 4K H.264, drawn box, 64 samples | 119 | 7,514 | 7,514 |

`watermark.rs:213-221` thresholds `opacity` and `confidence` at fixed values and the
`Proposal` carries only the binary result. In the in-place editor the result is already
held for review (`Detection::Proposal`, `app.rs:1423`).

- Carry `opacity` and `confidence` (region-sized `Vec<f32>`, or quantised to u8) in
  `Proposal`, plus the region.
- A slider on the review bar re-runs `region_mask` with a scaled threshold pair and
  rebuilds the preview texture; no decode, no detector run.
- `detect_checked` zeroes everything under 0.25 confidence at its end
  (`static_overlay.rs`, last loop), so a "more sensitive" setting needs that final cut to
  move to the caller, with the small-island filter re-run after it.
- "Nothing static found" can then say whether there were sub-threshold candidates, which
  is a better prompt than More frames / Whole clip alone.

### Q2. Grow the proposal

The Process review bar now offsets proposals from −2 to +8 image pixels, starting
at +1. Each step filters the original selection inside the detection area. The
orange preview shows the exact pixels Add to mask will commit in one undo step.

The mask is a hard cut, so the antialiased rim of a logo and the compression ringing
around it are left outside, and a processor that inpaints the mask leaves an outline.

- A grow / shrink stepper on the review bar (−2 to +8 px, default +1 or +2), applied to
  the proposal before Add to mask; a separable max filter on the u8 mask, clipped to the
  search region.
- Better default than a fixed number: hysteresis. Pixels adjacent to an accepted component
  join it at a lower opacity threshold (say 0.15) for up to a few rings, the way
  `stroke_lift` already reclaims soft edges (`stroke_lift.rs`, the `for _ in 0..fine`
  loop).

### Q3. Choose frames that differ

The detector can say nothing where the surroundings barely change
(`local_reference < 0.06` is skipped). Evenly spaced times over a locked-off shot, or the
±30 s "Near playhead" range inside one scene, give little evidence however many frames
are read.

- Candidate pool: the sync samples in the range (P2), or the single pass of P1.
- Greedy farthest-point selection on small luma thumbnails of the search area until the
  requested count is reached.
- Report the diversity: when the chosen set's median pairwise distance is under a floor,
  say so in the result ("the picture barely changes in this range") and offer Whole clip.

### Q4. Pool evidence instead of voting

In every-frame mode each batch yields its own binary mask and a pixel survives with 60 %
of the votes (`watermark.rs:178`). A pixel that is marginal in every batch fails in all of
them, where one estimate from all frames would pass it; and each batch re-applies the
component filters to a weaker map. Falls out of P1: one detector run over one pooled set.

### Q5. Reject static scenery with the blend model

HISTORY 2026-11-22 records the false positive: the still interior of a test pattern was
proposed along with the overlay. The island test (`surrounded`, 70 % of the band moving)
cannot tell a static object in moving scenery from a logo.

For a translucent overlay, `pixel = (1 − a) · background + a · colour` in every frame. So
across frames the pixel regresses linearly on the local background (mean of the moving
band just outside the component, per channel) with slope `1 − a` and a high R²; static
scenery has slope near 0 and no correlation.

- Per component, fit that line; require the slope to agree with the opacity the spread
  ratio already estimated (`1 − opacity`, within a tolerance) when opacity is under about
  0.9.
- Limit, stated plainly: an opaque logo (slope 0) and a static object are the same data.
  This removes false positives only among translucent candidates; opaque ones still need
  the user's box or a look at the proposal.
- The fit is in gamma-encoded values; the relation is exact only in linear light. Decode
  with the sRGB curve before fitting, or accept a looser tolerance.

### Q6. Native planes for deep sources

Samples go through `fc::fit_rgba_into`, the 8-bit display conversion. The flicker floor in
`stroke_lift` is 0.008 (two 8-bit levels) and the letterbox test 0.012; on clean 10-bit
footage grain sits below both and the attenuation cue has nothing to measure.

- Read through the native path (`take_next_native`) and work on luma at the source depth,
  plus chroma only for the edge-direction test.
- Luma alone also cuts the temporal pass to one channel in three (a P4 win), at the cost
  of the coloured-logo case the RGB edge test exists for; keep chroma for edges.

### Q7. Out of reach of the current model (noted, not planned)

- An overlay present for part of the range: the 1/8 to 7/8 spread needs it on screen in
  roughly 7 of 8 samples. A windowed run that reports where it appears would be a new
  feature, and masks are static today (TASKS 23).
- Moving or animated overlays: the method is screen-space by definition.
- Soft-edged or blurred overlays fail the edge seed rule (`seeds < 3`, `edge > 0.85`).
  Q1's sensitivity control is the cheap relief; a real fix needs evidence that is not
  edge based.

## Suggested order

1. Step 0, then P2 (small, self-contained, large win for the default settings).
2. P4 (mechanical, bit-identical, verifiable with existing tests).
3. P3, which unblocks 4K whole-frame runs.
4. P1 with frame selection, bringing Q3 and Q4.
5. Q2, then Q1 (both live in the review step that already exists).
6. Q5, Q6 after reference masks exist to judge them against.

## 2026-11-23 benchmark results

Step 0 is complete. P2 is implemented for MP4/MOV family files with a video sync
table. Each eligible keyframe replaces the closest target that chose it. Other
targets retain their exact requests. Matroska Cues use segment timestamps, so
MKV/WebM keep exact requests until the decoder's playable-track origin is available.
The first part of P4 is also in place. Both detectors compute temporal quantiles
on byte samples with reused scratch and parallel rows. The context grid and edge
pass still run serially.

One release test process per case on an Apple M4 Pro with 48 GB RAM, macOS 26.5.
Reading includes conversion and sampling. Times for Every frame are summed across
the three batches. Peak RSS is for the process and includes the decoder's cache.
The fixtures are 8 s at 24 fps, with a translucent static mark on `testsrc2`.
H.264 uses 96-frame GOPs. ProRes Proxy is transcoded from that H.264. The extra
sparse run uses the same H.264 fixture recipe at 36 s.

| Clip | Area | Samples | Read s before → after | Analyse s before → after | Peak MiB before → after |
| --- | --- | --- | ---: | ---: | ---: |
| 1080p H.264 | Box | 64 | 0.363 → 0.363 | 0.293 → 0.059 | 506 → 505 |
| 1080p H.264 | Box | All | 1.054 → 1.048 | 0.882 → 0.181 | 552 → 544 |
| 1080p H.264 | Whole | 64 | 0.410 → 0.358 | 2.733 → 0.523 | 933 → 933 |
| 1080p H.264 | Whole | All | 1.049 → 1.052 | 7.983 → 1.601 | 965 → 964 |
| 1080p ProRes | Box | 64 | 0.245 → 0.232 | 0.333 → 0.066 | 870 → 870 |
| 1080p ProRes | Box | All | 0.738 → 0.662 | 0.999 → 0.199 | 916 → 895 |
| 1080p ProRes | Whole | 64 | 0.336 → 0.329 | 2.858 → 0.516 | 1135 → 1136 |
| 1080p ProRes | Whole | All | 0.991 → 0.944 | 8.436 → 1.610 | 1284 → 1233 |
| 4K H.264 | Box | 64 | 1.334 → 1.346 | 0.228 → 0.058 | 1036 → 1035 |
| 4K H.264 | Box | All | 4.001 → 4.089 | 0.689 → 0.171 | 1104 → 1070 |
| 4K ProRes | Box | 64 | 0.922 → 0.852 | 0.271 → 0.062 | 3280 → 3280 |
| 4K ProRes | Box | All | 2.551 → 2.465 | 0.814 → 0.179 | 3313 → 3315 |
| 4K, both codecs | Whole | 64 and All | Memory guard | — | — |
| 1080p H.264, 36 s | Box | 8 | 0.833 → 0.237 | 0.071 → 0.047 | 485 → 387 |

P2 alone reduced the sparse read from 0.833 s to 0.233 s. The 8 s clips have only
two H.264 keyframes, and nearby exact requests can share FilmCraft's running
decoder, so the 64-frame runs saw little read-time change. P2 changed 4,016 mask
pixels in the sparse run, 314 in the 1080p H.264 whole-frame 64-sample run and five
in the 4K H.264 box 64-sample run. All other successful masks matched the baseline.
P4 matched the P2 masks byte-for-byte across all 13 successful cases. The
[reference PNGs](docs/evidence/autodetect-baseline/) contain the pre-change
proposals for these synthetic clips.

To recreate a test clip with FFmpeg 9.0.2, set `size` to `1920x1080` or `3840x2160`,
and `seconds` to `8` or `36`.

```sh
ffmpeg -f lavfi -i "testsrc2=size=${size}:rate=24:duration=${seconds}" \
  -vf 'drawbox=x=88:y=88:w=312:h=112:color=white@0.6:t=5,drawbox=x=102:y=105:w=284:h=8:color=white@0.6:t=fill,drawbox=x=102:y=125:w=180:h=8:color=white@0.6:t=fill,drawbox=x=102:y=145:w=244:h=8:color=white@0.6:t=fill' \
  -an -c:v libx264 -preset ultrafast -g 96 -keyint_min 96 -sc_threshold 0 input.mp4
ffmpeg -i input.mp4 -an -c:v prores_ks -profile:v 0 -threads 4 input.mov
ABNER_DETECT_INPUT=input.mp4 ABNER_DETECT_OUTPUT=mask.png \
  ABNER_DETECT_COUNT=64 ABNER_DETECT_BOX=56,56,400,200 \
  cargo test --release --lib watermark::tests::inspect_detection_clip \
  -- --exact --ignored --nocapture
```

Unset `ABNER_DETECT_BOX` for Whole frame and use `ABNER_DETECT_COUNT=all` for
Every frame. The diagnostic prints per-batch timings and peak RSS on macOS.

## Parallel maps and 4K whole-frame scans

P4 now runs the context grid and opacity and edge calculations in parallel.
The edge quartile reuses scratch and selects its rank directly. Temporal
quantiles use a byte histogram above 256 distinct frames. The 13 successful
cases above and a 512-sample box case produced byte-identical masks before and
after this change. At 64 samples, 1080p whole-frame analysis went from about
0.52 s to 0.22 s for H.264 and 0.25 s for ProRes on the same machine.

P3 keeps a native-resolution search whenever it fits the detector memory estimate.
Larger searches box-average at the finest power-of-two scale that fits. The coarse
pass scales its radius and component thresholds to match. For large sample sets,
it can use an evenly spaced subset to retain finer spatial detail. Weaker coarse
edge evidence can also nominate a window for refinement. Refined windows bound
the coarse hits within spatial tiles and add native-resolution padding and context.
Windows share a read of the full requested sample set when they fit in one batch.
The proposal contains pixels accepted by the native-resolution detector.

| 4K whole-frame case | Coarse scale | Read s | Analyse s | Peak MiB |
| --- | ---: | ---: | ---: | ---: |
| H.264, 64 | 2× | 2.74 | 0.51 | 1768 |
| H.264, All | 2× | 8.22 | 1.17 | 1879 |
| ProRes, 64 | 2× | 1.99 | 0.49 | 3998 |
| ProRes, All | 2× | 5.64 | 1.19 | 3931 |

The memory estimate covers detector working data. Peak process RSS also includes
the full-size decoder and its cache. These P3 measurements preceded P1, when
Every frame used three interleaved batches on the 192-frame fixtures.

A temporary full-resolution diagnostic with a 4 GiB detector budget gave a
comparison at 64 samples. Peak RSS was 2800 MiB for H.264 and 4242 MiB for
ProRes. The coarse-to-fine mask differed from the full-resolution diagnostic by
47,415 H.264 pixels and 55,571 ProRes pixels elsewhere in the test pattern.
Both whole-frame paths missed the drawn mark on those fixtures, where
the small drawn-box path could use the stroke fallback.

For thin-stroke coverage, a 24 s 4K H.264 fixture used a temporally varying gray
background and three adjacent horizontal marks. At 64 samples, whole-frame
search matched the native box proposal byte-for-byte, including 243 of 244 pixels
in the one-pixel stroke. At 512 samples the native box included 241 of those
pixels. The native box PNGs for both runs are in
[the reference masks](docs/evidence/autodetect-baseline/). An initial 4× pass
matched the adjacent strokes but missed a separate fixture containing only the
one-pixel stroke. The implementation uses a 2× scout with 221 of the 512 samples
at 4K, followed by native refinement on all 512. With the weaker edge seeds it
matched the native box for the isolated stroke, covering 242 of 244 pixels.

```sh
ffmpeg -f lavfi -i 'color=gray:s=192x108:r=24:d=24' \
  -vf 'noise=alls=75:allf=t+u,scale=3840:2160:flags=bilinear,drawbox=x=102:y=104:w=284:h=2:color=white@0.9:t=fill,drawbox=x=102:y=121:w=180:h=2:color=white@0.9:t=fill,drawbox=x=102:y=145:w=244:h=1:color=white@0.9:t=fill' \
  -an -c:v libx264 -preset ultrafast -g 96 -keyint_min 96 -sc_threshold 0 fine-moving.mp4
```

For the isolated-stroke fixture, remove the first two `drawbox` filters.

## One-pass Every frame

Every frame opens FilmCraft once and advances through the existing detection
frame grid. It keeps at most 64 distinct pictures in up to 768 MiB of native
frame storage, choosing them by distance between 64×36 luma thumbnails of the
search area. A new picture can replace a member of the closest pair if it is
farther from the set. The temporal detector pools the retained frames in one
estimate. Coarse and native refinement use the same retained pictures, so
refinement avoids reopening or decoding the source. Small search areas keep a
chroma-aligned crop with the detector halo. Fixed sample counts keep the P2/P3 path.

Before is the `32b0238` implementation with the same diagnostic counters added.
After is the one-pass implementation. One release process per case on the same
M4 Pro, with `ABNER_DETECT_COUNT=all`. The timing is wall time through proposal
delivery. Decoder input is FilmCraft's count of samples fed to decoders. RSS is
peak process memory, including FilmCraft's cache and frame pool.

| Fixture | Source frames | Retained after | Time s before → after | Decoder input before → after | Peak MiB before → after |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1080p H.264, whole | 192 | 64 | 1.74 → 0.72 | 576 → 192 | 988 → 941 |
| 1080p ProRes, whole | 192 | 64 | 1.63 → 0.60 | 192 → 192 | 1222 → 1474 |
| 4K H.264, whole | 192 | 64 | 9.18 → 2.09 | 1152 → 192 | 1707 → 1704 |
| 4K ProRes, whole | 192 | 24 | 6.68 → 1.04 | 384 → 192 | 3915 → 2819 |
| 1080p H.264, 36 s box | 864 | 64 | 26.38 → 1.55 | 12,051 → 864 | 891 → 482 |
| 4K H.264, 24 s thin-stroke box | 576 | 64 | 66.88 → 7.11 | 5,174 → 576 | 1244 → 991 |
| 4K H.264, 24 s thin-stroke whole | 576 | 64 | 135.83 → 7.70 | 10,348 → 576 | 1490 → 1775 |

Each H.264 after-run recorded one decoder start. The 1080p ProRes and whole
thin-stroke cases had higher peak RSS while the native set was held. The frame
grid still uses the clip's nominal rate, so on variable-rate sources a grid
position can repeat a picture or skip one.

The 10–20 s subrange of the 36 s clip completed in 0.58 s with one decoder
start. FilmCraft consumed 293 samples for the 241 grid positions, including
reference frames needed to decode the selected range.

Pooling changes the proposal. Across both thin-stroke searches, all 928 drawn
pixels belonging to the two-pixel strokes survived before and after. The box
still covered 243 of 244 pixels of the one-pixel stroke. Whole-frame coverage
went from 243 to 241. The 36 s H.264 box proposal changed from 5,837 to 6,656
selected pixels. The 64-sample whole-frame checks on the two 4K H.264 fixtures
matched their pre-change PNGs byte-for-byte.

Use the earlier fixture commands to repeat the matrix. The two box rows use
`ABNER_DETECT_BOX=56,56,400,200` and the other rows leave it unset. Set
`ABNER_DETECT_RANGE=10,20` to run only that portion of a clip, in seconds.
