# Empty processor chain and catalog dialog validation

2026-11-20, macOS, native winit/wgpu app, 1280×800 logical pixels at 2× scale.
References: [empty chain](https://www.figma.com/design/oDvZ81OLuzewiI2oLcqeVU?node-id=655-6349),
[dialog](https://www.figma.com/design/oDvZ81OLuzewiI2oLcqeVU?node-id=655-6576).

## Behavior

Installed descriptors and chain membership are separate. Browsing stages edits
a per-processor draft; preview/export snapshots continue to use the original
chain and saved settings. Add validates typed input and appends one enabled
stage. Cancel restores the original focus. Existing stages cannot be added
twice. Backspace removes the focused stage; A opens the dialog. Schema-driven
settings and presets are reused. Spatial inputs are chosen after adding.
Render restoration includes disabled members and omits catalog-only entries.

## Checks

- Before changes: 38 processor tests passed.
- `cargo test --lib processor_gui:: -- --test-threads=1`: 45 passed, including
  draft isolation, Add/Cancel, search, invalid values, empty registry, render
  restoration and small/large modal geometry.
- `cargo test --workspace`: 272 passed, 4 failed, 4 ignored. The titlebar,
  workspace-selection and source-edge-drag failures reproduce in an archived
  unchanged HEAD with the pre-existing local edit. The playback duplication
  test passed when rerun alone; the unchanged checkout also had a timing-related
  inspector-grip failure. These failures are outside the changed panel tests.
- `cargo build --release` and `cargo build-with-plugins --release`: passed.
- `cargo fmt --all -- --check`: blocked by pre-existing formatting, starting in
  build.rs. The new picker module passes rustfmt.
- `cargo clippy --workspace --all-targets -- -D warnings`: blocked by existing
  warnings, starting with suspicious assignment formatting in app.rs:1033.
- `git diff --check`: passed.

## Native visual check

Launched the composed release app with a synthetic 1280×720, 24 fps, six-second
clip using `--process` and `--add-processor`. Captured only each test process's
CGWindowID with `screencapture -x -l`, without global keyboard injection. Checked
the empty chain, widened canvas, modal placement, category bar, initial catalog
scroll position, settings/presets, metadata and footer. Native input dispatch is
covered by the panel tests; captures do not prove every pointer interaction or
processing plugin. No performance or processing-quality claim is made.

## Populated rail correction — 2026-11-20

Adding a stage reserved 44px above the rows for Add Processor, but the group
box still used the old rows-only height and was painted over that button.
The group now includes the actual list-top offset, and the button is painted
after its background. Card positioning and hit testing are unchanged.

Regression coverage draws 1, 3 and 12 members at 720×480 and 1280×800, checking
that the visible rows and Add Processor stay within the group and the button
is painted above the background. All 47 processor tests and the composed
release build pass. A targeted native capture with a synthetic clip confirms
the populated panel, including the visible Add Processor button.

The follow-up places Add Processor after the last visible processor row, with
a 12px gap. Long chains reserve the same space beneath their scroll viewport
so the button remains reachable. The group includes both rows and button;
the regression check now asserts this ordering at both viewport sizes.
