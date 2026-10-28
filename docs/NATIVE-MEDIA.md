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

The remaining production media-tool calls are the FFprobe packet/chapter/stream
metadata fallback for other containers in `cut::scan`. Playback and initial probing
of those containers still use Filmcraft. The native export/inspection API reports
unsupported indexed input instead of quietly restoring an FFmpeg dependency.
Tests use FFmpeg/FFprobe only to generate synthetic fixtures or as an independent
reference; two native pipeline/subtitle tests also run with PATH=/nonexistent.
