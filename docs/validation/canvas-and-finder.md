# Canvas fit, source tooltips and Finder routing

2026-11-20, native macOS release build, synthetic 1280×720 clip.

The shared canvas fit now uses a fixed 24-point inset instead of proportional
side margins. Workspace changes reset the shared zoom and centre; loading
clips already does so. Existing 1:1 and pointer-anchored zoom behavior remains.
Sources controls are not painted behind Process or Export: tooltip items are
drawn last by the renderer, so merely covering those controls was insufficient.

Finder's initial document is tagged before NSApplicationDidFinishLaunching.
Later events launch the current executable with absolute paths and the selected
config; no shell is used. Spawn and child reaping happen off the UI thread.
Spawn failures notify the existing window. An entire Finder batch stays together
in one new process. The original source list stays untouched. In-app opens and
drops keep adding locally. `--reuse-window` / `--new-window` override
`[window] reuse_window` (default false) for the receiving process.

## Verification

- `cargo test --lib` and a clean `cargo test --workspace`: 278 passed,
  4 ignored, 3 known baseline failures:
  native_titlebar_excludes_controls_content_and_fullscreen,
  native_workspace_selection_matches_header_for_every_transition, and
  source_edge_drag_resizes_cards_without_selecting_or_seeking. These same
  failures were previously reproduced in the unchanged checkout.
- New regressions cover fit margins and aspect ratio, hidden source tooltips,
  workspace fit reset, config override parsing, and literal child arguments.
- `packaging/build-app.sh --with-plugins`: release bundle built and signed.
- Native Timeline and Process captures inspected at 1280×800 logical pixels.
- An isolated test bundle was opened through LaunchServices with synthetic
  documents. Cold open produced one process; the next open produced a second
  process and preserved the original window's document. `--reuse-window` and
  the config option each retained one process with both documents.
  `--new-window` overrode a reuse-enabled config and produced two processes.
- The processor dialog remains hybrid: boolean controls use Figleaf shapes;
  panels, catalog rows, text inputs and most text use the native item renderer.

No inference or performance benchmark was run.
