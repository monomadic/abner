// Does VideoToolbox decode this codec in HARDWARE on this machine?
//
// h264/hevc/prores are a given on Apple silicon; VP9 (M1+) and AV1 (M3+)
// depend on the chip, and for AV1 the answer decides which libav decoder
// to open: the VideoToolbox hwaccel hangs off the native `av1` decoder,
// which has no software fallback, while the default `libdav1d` is
// software only. Asking up front keeps a clip from failing outright on a
// chip without the block. VP9 decoding is a supplemental decoder that
// must be registered before VideoToolbox will admit to supporting it
// (libav does the same inside its vp9 hwaccel).

#include <VideoToolbox/VideoToolbox.h>
#include <stdbool.h>

bool ab_vt_hw_decodes(uint32_t codec_type) {
    if (codec_type == kCMVideoCodecType_VP9) {
        VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9);
    }
    return VTIsHardwareDecodeSupported(codec_type);
}
