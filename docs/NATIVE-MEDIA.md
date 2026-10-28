# Native media I/O

Abner pins Filmcraft to `3be5ba26da4578cc973eb8f0ab070df1bda9a1f6`.
`src/media_io.rs` implements indexed media I/O without spawning FFmpeg or FFprobe.

| Operation | Native coverage |
| --- | --- |
| Indexed inputs | MP4/MOV/M4V/3GP and Matroska/WebM |
| Keyframe export | Original coded video/audio packets with supported MP4 sample entries; PCM is sliced by audio sample |
| Frame export | SDR Rec.709/untagged input → H.264 CRF 16, no B-frames, AAC-LC 192 kb/s per audio stream at 48 kHz |
| Crop export | Upright display pixels → ProRes 422 Proxy + PCM 16-bit at 48 kHz, retaining source picture durations |
| Selective GOP render | 8-bit H.264 4:2:0; untouched coded slices and auxiliary streams copied, edited intervals independently encoded |
| Subtitle lane | First subtitle track: MP4 tx3g/text/wvtt; Matroska SubRip, ASS/SSA, WebVTT; plain text, Unicode and timings |
| Frame inspection | Actual decoder output timestamps; H.264/HEVC/VP9 picture types, intra-only codec types; independent container key flags |

Frame cut export retains the previous CFR behavior. Crop and selective rendering
retain VFR picture timing; copies retain original MP4 video timebases, composition
offsets and display matrices. Compressed audio cuts remain packet-aligned, so a cut
can differ by one audio packet. Frame export and crop export retain every audio
stream; Filmcraft supplies channel order and resampling, with no implicit downmix.
Encoder CRF values and pixel scaling are not an x264/FFmpeg quality-parity promise.
Rendering currently uses the same SDR Rec.709/untagged color boundary as previews;
stream copies preserve coded color without converting it.

Native output containers are MP4/M4V, MOV and MKV for mapped codecs. The current
Matroska muxer cannot carry an MP4 display transform: rotated copies require MP4/MOV.
WebM output requires WebM-compatible codecs; H.264/AAC output must use MP4/MOV/MKV.
Unsupported container/codec mappings fail without launching a helper or publishing
an incomplete output. Multi-description MP4 tracks currently fail explicitly.
Bitmap subtitles are not converted into text. Picture types without a parser remain
`?`; a partial I/P-copy decision requires complete, decoded, known I/P metadata.

Source text metadata and MP4 sample-entry descriptions are carried when copying;
chapter titles use native Nero MP4 chapter boxes or Matroska Chapters. Chapter titles
in MP4 are limited to 255 UTF-8 bytes and 255 entries. These are not archival remux
preservation guarantees: arbitrary attachments, unknown boxes, packet side data,
container-specific track metadata and multiple edit-list programs are not promised.

Files are read on demand, packets are size-bounded, audio encoding uses one-second
blocks, and temporary files sit beside the destination and are removed on failure.
Selective rendering checks cancellation between packets/frames and rechecks source
frame dependencies, output PTS, forced key boundaries and complete decoder delivery
before publishing a new working file. Export does not overwrite its source.

Timeline packet metadata for AVI, MXF, MPEG transport/program streams, raw MPEG-1/2,
Y4M, standalone APV and still images uses `src/media_packets.rs`. AVI uses indexed
chunk sizes and timing slots; H.264 picture order is recovered when B-frames require
it. Unknown AVI index key flags remain false unless the codec headers or an intra-only
format establish a random-access picture. MXF uses edit units, material-package origin,
index offsets and Filmcraft AVC picture-order recovery; MPEG picture headers repair
index key flags when available. MPEG transport/program timestamps are interpolated by
Filmcraft and normalized to the first presented picture, matching the playback clock.
Raw MPEG picture order comes from Filmcraft access-unit/GOP/temporal-reference parsing.
Y4M/APV frame tables use bounded byte-reader adapters around Filmcraft source metadata.
Raw APV uses Filmcraft's default 24 fps clock; that elementary stream carries no rate.
Packet granularity and codec coverage follow the pinned Filmcraft readers.

All packet paths retain the 30-second caller deadline and cooperative read cancellation.
The elementary MPEG index buffer is capped at 128 MiB; AVI/MXF header probes are bounded
at 8/16 KiB. AVI H.264 checks the first 300 nonempty chunks for reordering, matching
Filmcraft playback, then reads all bounded headers when reordering is found.
Unknown color declarations remain unknown in stream facts; decoder defaults
are not presented as declared tags. Formats without a native packet adapter fail explicitly.
Additional containers do not gain export or subtitle support from this metadata migration.
Only MP4/MOV and Matroska currently expose chapter tables through these adapters.

There are no production FFmpeg/FFprobe media-tool calls. Tests use them to generate
synthetic fixtures and obtain independent packet/decode references. The AVI, MXF,
MPEG TS/PS, raw MPEG and Y4M fixtures are also read in child test processes with
PATH=/nonexistent; native-generated MXF/Y4M tests need no fixture helper at all.
The optional packaging flag that bundles diagnostic tools remains compatible.
