# VideoToolbox frame interpolation for Abner

An Abner processor (`videotoolbox.interpolate`) built on the VideoToolbox core of
`~/src/interpolate-resolve`: `VTFrameRateConversion` at quality prioritization,
revision 1, Random submission, `64RGBAHalf` buffers filled with
round-toward-zero halves. `native/vt_frc.m` is that core reduced to an
in-process open / run / close over packed RGB16; the CLI's decode, ProRes/EXR
writers, Resolve phase rounding and audio handling are not part of it.

The processor supports two timestamp-preserving comparison modes and one
frame-rate conversion mode:

- **leave-one-out** (default): each interior frame is replaced by the frame
  synthesized from its two neighbours, at the phase their timestamps put it.
  A/B or difference against the source is the interpolation error on real
  motion. This is the quality test for frame-rate conversion.
- **in-between**: each frame is replaced by the frame `phase` (0.05–0.95) of the
  way to the next one: what a 2x retime would insert there.
- **retime:** generates frames on a new grid at 2x, 3x, 4x, or a fixed target
  rate. Frames on the original grid preserve the source pixels.

First and last frames have no neighbour on one side and pass through unchanged.
Sessions need ordered frames with one time base; recreate them at cuts.

Requires Apple Silicon and macOS 15.4+. Frames 16–8192 wide, 16–4320 high.

```sh
cargo test --release          # needs VideoToolbox frame rate conversion
cd ../../.. && cargo build --release
target/release/abner --check-processor videotoolbox.interpolate '{}'
```
