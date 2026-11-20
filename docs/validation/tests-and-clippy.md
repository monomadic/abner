# Tests and Clippy — 2026-11-20

Validated on the development Mac after the processor mask and render-history fixes,
Sources scrolling changes, and crate-wide lint cleanup.

| Command | Result |
| --- | --- |
| `cargo test --all-targets` | 284 passed, 0 failed, 4 ignored; binary and example targets also pass |
| `cargo test --doc` | Pass; no executable doc-tests |
| `cargo clippy --all-targets -- -D warnings` | Pass, no warnings |
| `git diff --check` | Pass |

The full suite ran outside the filesystem sandbox so the native HEVC encoder could
open. Its export regression passed with native access; the restricted run reported
“HEVC encoder unavailable on this machine.” No encoder test was skipped or weakened.

The four existing ignored tests remain opt-in: `register_cost`, `fit_timing`,
`benchmark_three_4k_players`, and `inspect_detection_clip`. The last requires the
`ABNER_DETECT_*` fixture/output environment variables; the others are timing probes.

The UI test corrections cover the current embedded Process editor, temporary-tab
geometry, portrait-card minimum label width, and video/audio rubber-band selection.
The band test now waits for the waveform worker and supplies deterministic audio
peaks, avoiding a timing-dependent choice between video-only and video/audio picks.

Processor coverage includes required-mask editor clicks, source-scoped render rows,
and saved/running preview selection while the registry is locked by a render. These
are automated checks; this pass did not add a native-window visual inspection.
