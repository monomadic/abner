# Shared processing runtime

`abner-processing` is the headless host library used by Abner's preview/export
chain execution and by composed command-line launchers. Plugins keep their
algorithm and settings; the host keeps execution, flushing, frame validation,
conversion, timing, cancellation, progress and output publication.

`process_chain` and `finish_stage` work with sessions owned by an existing host.
`Chain` owns sessions for a complete streaming job. Errors and completion are
terminal. Sessions remain on their creating thread. The budget bounds returned
frame batches, not plugin allocations, model memory or decoder caches.

`media::run` provides native FilmCraft I/O for square-pixel SDR Rec.709,
zero-start CFR MP4/MOV sources and ProRes HQ MOV output. It processes RGB16,
converts directly to 10-bit YUV422 at the encoder, checks exact timestamps,
copies audio packets and verifies hashes and track properties before publishing
with a no-replace hard link. It rejects unsupported tracks and transformations.
It invokes no external media programs. A library call never installs signal handlers;
the CLI adds SIGINT/SIGTERM cancellation, checked between processing operations.

The first media runner does not implement VFR, HDR, FFV1, whole-clip processors,
generators, timeline edits, spatial bindings or host input-downsizing. Abner
retains its existing timeline/media orchestration and display conversion; its
chain core and ProRes output conversion now use this library. The plugin API
remains RGB16; native surface negotiation is future work, not a claimed zero-copy path.
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
