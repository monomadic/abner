# Sustained RIFE plugin throughput — 2026-11-23

Measured on an Apple M4 Pro. One sequential before/after pair per configuration,
60 seconds per trial after three warmup frames. 30 fps input → 60 fps output.
Source: the supplied `amona_re [DccFm9MDYa4_1].mp4` (720×720), looped and
bicubic-scaled to each test size. This measures full-size processing allocations
and inference, not detail from a native 1080p/4K camera source. Half the output
frames are source copies. No encoding or PNG/disk frame intermediates.

Before: pair API/plugin at Abner `7bb6a8d`, installed native runtime from
`3c1f304`. After: exact/bounded scheduler and cached-stream adapter. Same model
4.26 and HQ/balanced/fast tiers. Trials run before then after for each size/tier;
no compilation or validation tests run during the final timings. Ordinary desktop
background activity and thermal state were not controlled. Single paired trials
show measured differences, not statistical confidence intervals.

| Size | Tier | Before wall fps | After wall fps | Change | Before process fps | After process fps |
|---|---|---:|---:|---:|---:|---:|
| 1080p | hq | 20.25 | 22.49 | +11.1% | 20.79 | 23.17 |
| 1080p | balanced | 58.37 | 62.84 | +7.6% | 63.11 | 69.42 |
| 1080p | fast | 103.70 | 164.14 | +58.3% | 119.43 | 208.00 |
| 4K | hq | 5.66 | 6.31 | +11.5% | 5.83 | 6.52 |
| 4K | balanced | 16.14 | 21.08 | +30.6% | 17.58 | 23.57 |
| 4K | fast | 29.87 | 48.67 | +62.9% | 35.18 | 64.35 |

Wall fps includes FFmpeg decode/scaling/RGB16 delivery, unpacking, processing and
output consumption. Process fps covers `Session::process` only (including source
copies, allocation, CPU color conversion and Metal inference); it is not a GPU
kernel-only measure. Loading and the first three source frames are excluded.
The benchmark does not call finish inside the timed loop and does not measure
audio, hardware encoding, muxing, cold-start latency or reset performance.

Reproduce the after trial from this plugin directory:

```sh
cargo run --release --locked --example throughput -- /path/to/30fps-video.mp4 60
```

`RIFE_BENCH_CASE=3840:fast` selects one configuration. Raw JSON, source SHA-256,
machine metadata and the paired runner are retained with the session artifacts.

Validation: 15 hardware unit tests cover rational rates, bounded/terminal failures,
reset and cancellation, exact fractional tails, all tiers and textured multi-phase
stream/pair comparisons within one 8-bit sample step. The export integration test
covers seven actual native decode → RIFE → ProRes mux cases at 24/25/30/50,
23.976 and 29.97 fps, preserving source duration to within 2 µs. On-grid source
samples remain exact. No claim of 16-bit generated precision or image quality
improvement is made by these performance changes.
