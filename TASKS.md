# abner — open work

Numbered for reference; order is priority. Move a task to [HISTORY.md](HISTORY.md)
when it lands (keep its number there so old references still resolve).
Effort estimates come from the 2026-09-04 comparison against switchblade.

## Now

1. **Drop-to-load on the launch window.** The 2b launch window's drop targets are
   decorative: no `DroppedFile` arm in `main.rs`, no runtime video load. winit delivers
   one event per file with no end-of-batch marker, so accumulate in `window_event` and
   flush the whole gesture from `about_to_wait` (switchblade `sb-window/src/lib.rs`,
   the `FilesDropped` path). Also needs probe + `Player::spawn` to move off the startup
   path so a drop can build the `Video` list at runtime. ~25 lines for the event
   plumbing, more for the runtime load. Until this lands, `--help` says so.
2. *(done 2026-09-04 — see HISTORY.md)*
3. **Open With / double-click a file on the .app.** LaunchServices delivers opened
   files as an Apple Event, never argv, so a bundled abner opened by double-click shows
   an empty window. Port switchblade's `open.rs` + `open_shim.m` (~130 lines, needs a
   `build.rs` + `cc`): it grafts `application:openURLs:` onto winit's delegate class,
   because winit 0.30 owns the NSApplicationDelegate and panics if it is replaced.
   Depends on 1 (the runtime-load path). Land the `CFBundleDocumentTypes` +
   `UTImportedTypeDeclarations` block from switchblade's `Info.plist.in` in the same
   change — `packaging/Info.plist.in` deliberately omits it until then.

## Next

4. **Waker latch on the decoder→loop nudge.** `main.rs` sends one proxy event per
   player per dry→ready transition with no latch; N players × transitions all queue on
   the unbounded channel. Switchblade's `Waker` swaps an `AtomicBool` in `wake()` and
   clears it at the top of `RedrawRequested`, so one event is ever in flight, and its
   `arm()` covers the startup race (a worker finishing during GPU init). ~35 lines.
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
