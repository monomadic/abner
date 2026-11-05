# RIFE Metal for Abner

An in-process Abner processor (`rife.metal`) using the native runtime from
`rife-metal-rs`, RIFE Metal 0.1.7 / model 4.26. It keeps one loaded model, graph
cache and two reusable input pixel buffers per session. No subprocesses, PNGs,
video encoding or intermediate files are involved in processing.

## Setup

Install the updated runtime from a sibling `rife-metal-rs` checkout:

```sh
cd ~/src/rife-metal-rs
./install.sh
cd ~/src/abner
cargo build --release
./packaging/build-app.sh --install
```

Requires Apple silicon, macOS 14+, and the complete installed runtime at
`~/.local/libexec/rife-metal-rs/`. A custom installation can set
`RIFE_METAL_LIBRARY` to its `libRifeNative.dylib`; keep both RifeMetal resource
bundles beside that library. The app loads this external runtime at session
creation, so listing plugins does not require it. A missing or older runtime
produces an actionable error. The app bundle alone does not include this runtime.

Choose **RIFE Metal** in Abner's processor strip, or launch:

```sh
target/release/abner input.mp4 --processor rife.metal
```

## Controls

- **Quality:** `hq` estimates motion at full resolution; `balanced` at half;
  `fast` at quarter. Output dimensions stay unchanged.
- **Leave-one-out** (default): reconstruct each interior frame from its two
  neighbours using their actual timestamp spacing. Compare against the original
  or use the difference view to inspect reconstruction error. First and last
  frames pass through; empty and one-frame clips are supported.
- **In-between:** synthesize between each frame and its successor, at **Phase**
  (0.05–0.95, default midpoint). The result retains the earlier frame's timestamp
  for comparison; only the last frame passes through.

- **Retime:** generates frames on a new grid at 2x, 3x, 4x, or a fixed target
  rate. Frames that land on the source grid keep the original samples.

The host contract is full-range RGB16 SDR Rec.709. RIFE consumes rounded 8-bit
BGRA and returns 8-bit samples expanded to RGB16; this does not preserve 16-bit
precision. Pass-through frames retain original samples. Geometry is limited to
16–8192 by 16–4320, with internal model padding. Dimensions/time base cannot
change mid-session and timestamps must increase. Recreate sessions at cuts,
seeks or settings changes. Cancellation is checked around inference; in-flight
GPU work completes before cancellation returns. Errors terminate the session.

## Validation

```sh
cd plugins/defaults/rife-metal
cargo test --release -- --test-threads=1
cargo clippy --all-targets -- -D warnings
cd ../../..
target/release/abner --check-processor rife.metal '{}'
```

The hardware tests cover moving-bar reconstruction, midpoint synthesis, exact
VFR phase selection, RGB channel order, non-aligned geometry, all quality tiers,
short-stream flushing, terminal errors, and cancellation.
