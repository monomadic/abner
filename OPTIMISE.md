# Mask autodetect: optimisation notes

Written 2026-11-23 from a read of `src/watermark.rs`, `src/watermark/static_overlay.rs`,
`src/watermark/stroke_lift.rs` and the callers in `src/app.rs`. **Nothing here is
measured.** Every cost below is derived from the code; step 0 is to get numbers before
changing anything.

How it works today, in one paragraph: `Task::spawn` picks evenly spaced sample times in
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
