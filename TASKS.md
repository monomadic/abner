# abner — open work

Numbered for reference; order is priority. Move a task to [HISTORY.md](HISTORY.md)
when it lands (keep its number there so old references still resolve).
Effort estimates come from the 2026-09-04 comparison against switchblade.

19. *(done 2026-09-20 — mask painting; see HISTORY.md)*

## Now

1. *(done 2026-09-04 — see HISTORY.md)*
2. *(done 2026-09-04 — see HISTORY.md)*
3. *(done 2026-09-16 — see HISTORY.md)*

## Next

20. **Three-stream cold seek on FilmCraft.** `benchmark_three_4k_players` (cold exact
   seek to 3.5 s in a 4 s 4K H.264 GOP): one player lands in 690 ms against the libav
   player's 824, three players in 947 against 861 (interleaved medians, 2026-10-21). The
   contention is between streams, not per stream, and it is not rayon (capping its pool at
   2 / 4 / 6 threads is slower at every setting). Suspects: FilmCraft's VideoToolbox output
   copy on the session callback threads, the GOP cache storing every decoded picture of the
   decode-forward as planar frames (allocation churn under three sources), and our RGBA
   buffers (696 MiB resident for three players against 490: the rewind window plus
   decoding ahead to the budget). Profile with three sources before changing anything.
   2026-10-27: [profile and reproduction](docs/SEEK-PROFILING.md) added. On an M4 Pro,
   interleaved release medians are 388/618 ms (one, FilmCraft/libav) and 534/795 ms
   (three): the latency crossover did not reproduce. Three-stream peak RSS is
   3435/901 MiB; FilmCraft retains 2278 MiB of planar pictures in addition to RGBA.
   Callback copy is the leading measured cost; pool allocation/lock and GOP insertion
   time are small. No decoder policy or budget change; investigation stays open.

4. *(done 2026-10-22 — coalescing decoder waker; see HISTORY.md)*
5. **Mip floor for many-up layouts.** `MIP_LEVELS = 4` floors a 3840-wide source at
   480px on the long side — fine for 2-up, but a 6-up side-by-side on a retina window
   minifies that into a ~300px cell, which is exactly the shimmer the chain exists to
   prevent. Bump to 6 (~32 KB and two tiny blits per upload). One line; verify with a
   capture of `--view sbs` on three or more 4K clips.
6. **Re-measure `QUEUE_DEPTH` at native 4K × N.** abner keeps 4 (switchblade tuned
   down to 3 for one lane). At 4K RGBA that is ~132 MB of queue per player plus the
   recycle pool, ~400 MB per pair. Measure RSS on a 4K pair and a 4K triple before
   changing; the two-player sync test's timing margins depend on it.
7. **Comment the odd-rotation fallthrough.** `player.rs` maps a non-quarter-turn
   rotation to `-1`, which falls through to no transpose. Switchblade falls back to
   software decode (which autorotates); abner has no fallback chain, so such a clip
   renders unrotated. Rare. Either document it at the match or add the sw fallback.

## Later

8. **CoreText whole-string text stack.** Replace ab_glyph per-glyph layout with
   switchblade's `text_shim.m` + `TextRaster` trait (2026-07-29 design): real shaping,
   font fallback, ellipsis truncation, one quad per label instead of one per glyph, and
   no dependence on hardcoded `/System/Library/Fonts` paths (abner renders NO text if
   none of its five candidates parse). 1–2 days. Only worth it if the HUD grows beyond
   a fixed overlay — items 1–7 cover every actual defect without it. Bring the raster
   budget (`schedule_rasters`) across with it, not before.
9. **Modifier chords.** `main.rs` reads `logical_key` unconditionally; with ⌘/⌥/⌃ held
   macOS composes the character (⌥s → ß), so any future chord binding silently fails.
   Switchblade uses `key_without_modifiers()` plus a `ModifiersChanged` arm. ~20
   lines, on demand.
10. **objc2 0.6 / objc2-app-kit 0.3.** Five AppKit calls to migrate, but winit 0.30
    still pulls objc2 0.5 transitively, so upgrading now only duplicates the stack.
    Do it when winit 0.31 ships (0.31.0-beta.2 as of 2026-09-04).
11. **Clippy housekeeping in `app.rs`.** Four pre-existing lints: `mod tests` not last,
    `== false`, a very complex tuple type in the drop-zone table, `&PathBuf` in a test
    helper. Cosmetic.

## Ideas (unscheduled, from the README)

12. **Loupe** — a magnifier following the pointer showing A|B split at 4–8× inside the ring.
13. **Flicker** — auto-alternate A/B every N frames (temporal delta your eyes compute).
14. **Horizontal wipe** to pair with the vertical split.
15. **Heatmap delta** — false-colour per-pixel error with a scale.
16. **Signed delta** — grey = equal, warm = A brighter, cool = B brighter.
17. **Gamma-space delta?** The surface is sRGB and delta/blend/checker run in linear
    light. Switchblade moved to a non-sRGB surface (2026-08); porting that would make
    `|A−B|` a gamma-space difference, arguably more perceptually uniform for an A/B tool.
    A product decision, not a fix — decide deliberately, don't inherit it.
