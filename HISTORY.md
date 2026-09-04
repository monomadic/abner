# abner — history

Completed work, newest first. Task numbers refer to [TASKS.md](TASKS.md) where a task
existed there before it landed; earlier entries predate the task list.

## 2026-09-04 — .app bundling (TASKS.md 2)

- `packaging/build-app.sh` + `packaging/Info.plist.in`, switchblade's recipe minus
  document icons and asset folders: bundled ffmpeg dylibs rewritten to `@rpath`,
  ad-hoc codesign, PATH-fixing launcher, LaunchServices refresh, `--install`/`--open`/
  `--with-cli-tools`/`--sign`/`--debug`. Verified: the bundle runs with an empty PATH
  (dylibs + ffprobe lookup), and `open Abner.app` shows the launch window. The
  `cargo-bundle` metadata went — it can't rewrite dylib paths, so its bundle only ran
  on the machine that built it.
- `assets/app-icon.png` is now the icon slot for both the `.icns` and the bare
  binary's Dock icon. `set_app_icon()` returns early inside a bundle (the
  `setApplicationIconImage`-overrides-the-plist trap).
- `scripts/window-id.swift` matches the bundled app's owner name too.

## 2026-09-04 — switchblade catch-up

Assessment of abner against switchblade (forked 2026-07-23; switchblade gained ~166
commits since, of which only a handful touched shared code). Landed the fixes that were
real or latent defects; the rest became TASKS.md items 1–11.

- `0069bc8` docs: README, CLAUDE.md and `--help` brought in line with the code (no more
  drag-and-drop promise, Esc semantics, `--view` names, test list, bundle-icon trap);
  `scripts/window-id.swift` added for targeted window captures.
- `5fe8f57` `cargo update` (~60 transitive bumps, naga 30.0.1; no manifest changes —
  every direct dependency was already on its latest stable) and CLAUDE.md notes.
- `69a8a0c` `src/schedule.rs` lifted verbatim from sb-window. The redraw-cadence rules
  in `about_to_wait` were already a byte-for-byte inline of it; the point is the five
  tests pinning the occlusion / `MIN_FRAME` / idle-deadline invariants. Zero behaviour
  change.
- `1020d0b` text atlas: no mid-frame reset (earlier text in the frame had already baked
  UVs — switchblade's garbled-badge bug), a full atlas refuses + memoizes and wipes at
  `begin_frame()`; per-rect glyph uploads instead of two whole-atlas uploads per dirty
  frame (one of them dead code); 1024² → 2048².
- `380e9dd` decoder robustness from switchblade's post-fork fixes: `Drop` serialises
  with the reader's park loop (lost-wakeup thread leak); AVIO interrupt callback
  installed before `avformat_open_input` so a drop reaches a reader wedged in libav
  I/O on a dead mount (new FIFO-dribble test); a failed seek fails the player instead
  of silently desyncing; the startup `ffprobe` runs under a 30s deadline.

Deliberately NOT ported: per-player pacing anchors (abner's master clock makes the
whole class of bug impossible), the VT hardware scale chain (abner never scales), the
gamma-space surface flip (see TASKS.md 17), and the split text/tile pipeline (abner's
single pipeline cannot express the layering bugs it caused). abner is not joining
switchblade's workspace: the shared code is a few hundred lines that diverged on
purpose, and sb-window's `App` trait is shaped around a tile grid.

## 2026-07-27

- `797ef8f` feat(macos): embed and set Dock icon at startup — PNG `include_bytes!`'d
  and handed to `NSApp.setApplicationIconImage`, since a bare Mach-O has no
  `CFBundleIconFile` and running from a shell is the common case.

## 2026-07-23 — initial build

- `9097359` Window 2a, the Instrument HUD, from the Claude Design mockups: corner
  brackets, A|B toggle, per-clip info block, hover-revealed transport with seek bar,
  keycap legend.
- `fc93185` Launch window for the no-arguments / .app case (2b; drop targets
  decorative — TASKS.md 1).
- `741dcb5` Initial commit: N-player frame-locked sync on one master clock (no
  per-player pacing), exact-seek framestep with pts adoption, overlay / side-by-side /
  delta / split / checker / blend views, photo-style synced zoom, speed control, fake
  fullscreen, mip-chained video textures, ab_glyph text. Adapted from switchblade's
  media and render stack as of that date.
