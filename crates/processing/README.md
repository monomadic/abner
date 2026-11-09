# Shared processing runtime

`abner-processing` is the headless host library used by Abner's preview/export
chain execution and by composed command-line launchers. Plugins keep their
algorithm and settings; the host keeps execution, flushing, frame validation,
conversion, timing, cancellation, progress and output publication.

`process_chain` and `finish_stage` retain the legacy RGB16 host entry points.
Use `FormatPlan` for negotiated sessions owned by an existing host.
`Chain` owns sessions for a complete streaming job. Errors and completion are
terminal. Sessions remain on their creating thread. The budget bounds returned
frame batches, not plugin allocations, model memory or decoder caches.

`media::run` provides native FilmCraft I/O for square-pixel SDR Rec.709,
zero-start CFR MP4/MOV sources and ProRes HQ MOV output. It retains supported
decoder planes, negotiates each processor boundary and the encoder format, checks exact timestamps,
copies audio packets and verifies hashes and track properties before publishing
with a no-replace hard link. It rejects unsupported tracks and transformations.
It invokes no external media programs. A library call never installs signal handlers;
the CLI adds SIGINT/SIGTERM cancellation, checked between processing operations.

The first media runner does not implement VFR, HDR, FFV1, whole-clip processors,
generators, timeline edits, spatial bindings or host input-downsizing. Abner
retains its existing timeline/media orchestration and display conversion; its
chain core and output conversion use this library. CPU format negotiation is
shared; GPU surface negotiation remains future work.
Linux media codec availability depends on the FilmCraft build, and each plugin
must supply its own supported inference backend. This release is validated on macOS.

## Compose a CLI

```sh
ABNER_PLUGINS=/absolute/path/to/plugin-directory cargo build-with-plugins --cli --release
./target/release/abner-process list
./target/release/abner-process describe processor.id
./target/release/abner-process run --input source.mov --output result.mov \
  --processor processor.id --settings settings.json
```

Or pass `--chain chain.json`, containing an array of
`{"processor":"processor.id","settings":{}}`. An empty array transcodes without
a processor. Plugin defaults apply when settings are omitted. The generated
launcher depends on this crate and the processor API, not the GUI. The same
composition command without `--cli` builds the GUI.

Run `cargo test --manifest-path crates/processing/Cargo.toml` for buffered-chain,
budget, cancellation, precision, timing, audio and publication checks.

## CPU format negotiation

`VideoFrame` carries one declared `PixelFormat`, owned/shared pixel storage and
integer timing. The supported layouts are full-range RGB16 and opaque RGBA8,
plus limited-range BT.709 YUV420p8, YUV420p10, YUV422p10 and YUV444p10. Every format
uses encoded SDR Rec.709; this is layout/depth/chroma negotiation, not HDR or
gamut conversion. Planes have no row padding and use block-aligned chroma.
Other decoded SDR layouts take an explicit RGB16 fallback using their color
metadata. Unsupported alpha/HDR is rejected. Mid-segment format changes fail.

A session advertises executable input/output **pairs** with `format_pairs()`.
The host selects one pair with `configure_format()` before any frame is fed.
Opt-in sessions implement `process_video()` and `finish_video()` with their
native buffers. No plugin needs a converter, decoder, encoder or planner.
The default pair and methods bridge the existing RGB16 `process`/`finish`
contract without changing existing plugins. Native-only sessions can omit the
legacy methods, which return an explicit error if called.

`FormatPlan` selects across the complete chain and sink with a deterministic
precision-first cost: avoid losing source depth/chroma before processing, then
minimize format transitions. Mandatory display/encoder quantization stays at
the sink; plugin pair order breaks equal-cost ties. Promotion from 10-bit YUV
to RGB16 does not pretend six new source bits were created. This is a format
cost model, not measured execution time or knowledge of an algorithm's internals.

Use `Chain::negotiate(source, sink)` followed by `process_video` and
`finish_video`, or `FormatPlan::new`/`process`/`finish_stage` with host-owned
sessions. Plans are fixed for a segment. Drop the sessions after an error or
format change; recreate at cuts/seeks. Output layout, geometry, timing/storage
validity and frame batch budgets are checked, including buffered tails. The
budget excludes decoder/model memory and simultaneous conversion scratch space.

Matching formats move/share their original buffers. YUV layout/depth changes
resample directly in YUV and preserve headroom/footroom; RGB conversion uses the
shared host converters only when needed. Matching ProRes planes are moved into
the encoder if uniquely owned, otherwise copied without a color transform.
This is not a general GPU zero-copy claim.

Abner's preview and export negotiate from the player's existing RGBA8 boundary;
preview and HEVC end in RGBA8, ProRes in YUV422p10. The player still prepares
RGBA8 for display, so high-bit-depth decode preservation currently applies to
the standalone native media runner. Timeline crops/downsize are host RGB16
operations, and spatial bindings require an RGB16-capable plugin contract.
`Rgb16Boundary` adapts a native-only plugin behind an RGB16 host operation.
These explicit boundaries preserve existing behavior while ordinary native
chains avoid unnecessary RGBA8↔RGB16 expansion/reduction.

For hosts with both an encoder and a preview, `process_native` and
`finish_stage_native` leave the last processor's output in its selected format.
Abner uses this branch point: display conversion borrows RGB16 or shares RGBA8,
while the encoder consumes its own conversion. The preview never passes through
the encoder's chroma subsampling first.
