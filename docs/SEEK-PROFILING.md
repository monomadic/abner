# Task 20 — exact-seek investigation, 2026-10-27

The October 21 three-stream latency regression did **not reproduce** on this
machine. FilmCraft's substantially larger memory retention did reproduce.
No decoder policy, Rayon settings, rewind window, or memory budget was changed.
Task 20 remains open for reproducing the original crossover and testing an
upstream callback-copy improvement; these results do not justify an Abner fix.

## Method and results

Apple M4 Pro, Mac16,7, 14 cores (10 performance / 4 efficiency), 48 GB RAM.
Abner starting revision `70f2404`, FilmCraft pin `3be5ba26`. Release builds.
The existing generated fixture is four seconds of 3840×2160 24 fps H.264,
`testsrc2`, x264 ultrafast, four encoder threads, yuv420p, GOP 96. Each invocation
creates a fresh fixture and fresh players; the seek timer excludes fixture
encoding but includes asynchronous player opening. It is a cold **player** seek,
not a cold filesystem-cache measurement. This synthetic clip has no B pictures;
results must not be generalized to HEVC, reordered H.264, HDR, or other hardware.

The standalone libav comparison uses the player at
`a24f4dfe00bf2eef8a9c84af188f490bf57d8906` (parent of the FilmCraft migration),
with its existing ignored benchmark parameterized for one/three players. Its
unrelated codec test's `crate::probe::vt_accel(codec)` was replaced by `true` so
the module can compile standalone; the benchmark already passes `true`.
Dependencies: `rsmpeg 0.18`, `link_system_ffmpeg`, `ffmpeg8`, and `log 0.4`.
The default installed FFmpeg 9 headers do not compile with that rsmpeg version;
`PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg@8/lib/pkgconfig` selects the compatible
installed FFmpeg 8. Build and benchmark execution were separate.

Three samples per backend/count, run sequentially in FC/AV, AV/FC, FC/AV order.
No instrumented dependency or other test/build ran during this comparison.
FilmCraft counters confirm 87 hardware pictures / one session for one player,
and 261 pictures / three sessions for three players, without declines/fallbacks.

| Players | FilmCraft samples (ms) | Median | libav samples (ms) | Median |
| --- | --- | --- | --- | --- |
| 1 | 389, 383, 388 | 388 | 620, 618, 617 | 618 |
| 3 | 542, 532, 534 | 534 | 802, 795, 793 | 795 |

All FilmCraft landings were asserted to be exactly 3.5 seconds and synchronized;
the resident rewind seek returned 3.458333 seconds immediately. The historical
690/824 and 947/861 ms medians remain historical evidence, not current results.
Three samples are a local diagnostic, not a confidence interval or universal
performance claim.

Restricted runs silently declined VideoToolbox and fell back to software. They
are excluded from the table. Successful hardware runs required access outside
the restricted execution environment. Always inspect the printed hardware
counters before interpreting a run.

## Where time and memory go

`hardware-profile.txt` is a separate instrumented **development-profile** run,
with optimized dependency builds. Its 779 ms latency is not comparable to the
release table. Timings below sum across threads and the complete trace (including
a small amount of decode-ahead), not a common, frozen pre-landing interval.
Callback, decoder, and request intervals overlap and must not be added together.
Synchronous diagnostic output perturbs timing and is included in enclosing scopes.

| Scope | Aggregate time |
| --- | ---: |
| Frame requests, including demux/decode/cache/callback waiting | 2066.1 ms |
| VideoToolbox asynchronous waits (overlap callback work) | 1214.3 ms |
| Callback pixel-buffer copy, deinterleave, pool acquisition | 749.9 ms / 260 pictures |
| Fresh planar buffer allocation calls | 1.09 ms / 592 calls |
| Planar pool mutex acquisition | 0.035 ms / 780 calls |
| GOP output insertion/eviction | 0.252 ms / 261 calls |
| Abner RGBA prepare (pool, allocation and zero fill) | 55.4 ms |
| Abner RGBA pool mutex acquisition (subset of prepare) | 0.005 ms |
| RGBA conversion | 210.6 ms |
| Abner cache push, including any backpressure wait | 0.058 ms |

The callback locks the CoreVideo image, copies luma, deinterleaves UV into owned
planar buffers, unlocks, then publishes output through a mutex. There are at most
two in-flight access units per session. It copies **all** decoded pictures needed
for references, even when Abner requests only the final seven frames of the GOP.
The measured cost points toward copy/deinterleave/memory traffic, not time inside
allocation calls or the measured pool mutexes. Allocation measurements exclude
later first-touch page faults, which can be charged to copying. This is not proof
of memory-bandwidth saturation, nor does it measure every FilmCraft lock.

FilmCraft's per-source GOP budget is nominally 384 MiB but has a **64-picture
minimum**. A native 4K 8-bit 4:2:0 picture is 12,441,600 bytes. Consequently the
measured planar cache is 796,262,400 bytes per source (759.375 MiB), or
2,388,787,200 bytes (2278.125 MiB) for three. Its process-wide 1 GiB budget does
not forcibly evict recently active caches (500 ms recency protection).
This retention is additional to Abner's byte-budgeted RGBA cache, recycled RGBA
buffers, an in-flight converted picture per reader, and decoder/CoreVideo storage.
Abner's `cached_bytes()` describes only its cache, not total process memory.

Separate `/usr/bin/time -l` hardware runs measured peak RSS of **3434.7 MiB** for
FilmCraft and **900.7 MiB** for libav. Those runs reported RGBA cache occupancy of
727.7 and 537.9 MiB respectively. Peak RSS covers the whole benchmark lifetime;
it is not a snapshot at landing or directly comparable to October's quoted
696/490 MiB without the old measurement methodology. Retained-byte accounting,
allocator capacity, and RSS are distinct quantities.

## Controlled hypothesis test

`ABNER_BENCH_STAGE=planar|rgba` opens three identical sources before a barrier,
then requests frame indices 78 through 84 on each. Both modes retain the planar
sources. `rgba` additionally converts and retains the same seven native-size
RGBA pictures, with no decode-ahead. Opening is excluded in **both** modes.

| Stage | Samples (ms) | Median |
| --- | --- | --- |
| planar only | 506, 506, 508 | 506 |
| planar + retained RGBA | 531, 529, 534 | 531 |

Every run decoded exactly 261 samples/pictures, created three hardware sessions,
and retained the same planar bytes. Thus allocation/conversion of the RGBA
window adds about 25 ms in this controlled workload. It does not explain the
historical three-stream crossover by itself. This stage benchmark deliberately
does not test the production cache, seeks, or playback synchronization; the
production benchmark and correctness tests do that.

A useful next experiment is optimizing callback copy/deinterleave in FilmCraft,
then repeating these measurements with pixel-parity checks, reordered content,
and the original hardware. Reducing the 64-picture minimum may save memory but
could force redundant decode; no such change is justified or shipped here.

## Running the diagnostics

Normal release benchmark, one process at a time:

```sh
ABNER_BENCH_PLAYERS=3 cargo test --release --lib benchmark_three_4k_players -- --ignored --nocapture
ABNER_BENCH_PLAYERS=1 cargo test --release --lib benchmark_three_4k_players -- --ignored --nocapture
ABNER_BENCH_PLAYERS=3 ABNER_BENCH_STAGE=planar cargo test --release --lib benchmark_three_4k_players -- --ignored --nocapture
ABNER_BENCH_PLAYERS=3 ABNER_BENCH_STAGE=rgba cargo test --release --lib benchmark_three_4k_players -- --ignored --nocapture
```

Set `ABNER_PROFILE_SEEK=1` for per-reader request, RGBA preparation, conversion,
cache-push and retained-byte diagnostics. Output carries a thread ID to distinguish
readers of the same source. Prepare includes pool lookup, allocation and zeroing;
pool-lock time is also reported. Push includes intentional parking on a full
cache, so a long push is not by itself contention. Disable tracing for timings.
No clock reads or trace output occur on the ordinary playback path.

For deeper FilmCraft diagnostics, use Python 3.11+ and the pinned checkout:

```sh
python3 scripts/profile-filmcraft.py /path/to/pinned/filmcraft /private/tmp/filmcraft-seek-profile
```

The destination must not exist. The script copies the dependency and adds timing
only to callback copying, asynchronous waits, planar allocation/pool acquisition,
and GOP storage. It prints a Cargo override file to pass using
`cargo --config /private/tmp/filmcraft-seek-profile/abner-profile.toml test ...`.
It checks each instrumentation site and fails on source drift. It never edits
Cargo's cached checkout or the repository's dependency pin. Cargo resolves path
overrides into `Cargo.lock`: save the current lockfile before using the override
and restore that saved file afterward. Do not commit diagnostic path resolutions.
Use an isolated build directory when comparing variants, and run only one
benchmark at a time. Printed per-call timing is diagnostic, not a low-overhead
profiler or an instruction-level CPU trace.

## Validation

Player/conversion tests: **12 passed, 1 ignored**. This includes exact forward/back
seeks, resident rewind, budget enforcement, synchronization, drop wakeup,
unreadable-source handling, VP9/AV1 and RGBA conversion parity against FFmpeg.
Both generated dependency instrumentation variants compiled and the final one
ran with three successful VideoToolbox sessions.

The full library suite was also attempted serially: **135 passed, 1 failed,
3 ignored**. The failure is
`processor_gui::tests::preview_drains_buffered_tail_and_matches_fractional_rate_source`
with `Preview decoder failed`. It also failed in the initial full-suite run;
no processor code was changed. Full-suite success is not claimed.

Raw runs, trace, timing summary, RSS and test logs are in
[evidence/seek-2026-10-27](evidence/seek-2026-10-27/).
