// In-process port of interpolate-resolve's VideoToolbox core (native/vt_interpolate.m):
// VTFrameRateConversion, quality prioritization, revision 1, Random submission,
// 64RGBAHalf buffers filled with round-toward-zero halves. Packed RGB16 in and out.
#import <Foundation/Foundation.h>
#import <VideoToolbox/VideoToolbox.h>
#import <CoreVideo/CoreVideo.h>
#include <math.h>
#include <string.h>

typedef struct {
    void *processor;      // VTFrameProcessor, retained via __bridge_retained
    void *configuration;  // VTFrameRateConversionConfiguration
    CVPixelBufferRef a, b, out;
    int w, h;
} vtfrc;

static void report(char *err, size_t len, NSString *message) {
    if (err && len) snprintf(err, len, "%s", (message ?: @"VideoToolbox error").UTF8String);
}
static __fp16 to_half_rtz(float v) {
    __fp16 h = (__fp16)v;
    if (fabsf((float)h) > fabsf(v)) { uint16_t bits; memcpy(&bits, &h, 2); bits--; memcpy(&h, &bits, 2); }
    return h;
}
static CVPixelBufferRef make_buffer(int w, int h, NSDictionary *requirements) {
    NSMutableDictionary *a = [requirements mutableCopy] ?: [NSMutableDictionary new];
    a[(id)kCVPixelBufferPixelFormatTypeKey] = @(kCVPixelFormatType_64RGBAHalf);
    a[(id)kCVPixelBufferWidthKey] = @(w);
    a[(id)kCVPixelBufferHeightKey] = @(h);
    a[(id)kCVPixelBufferIOSurfacePropertiesKey] = @{};
    a[(id)kCVPixelBufferMetalCompatibilityKey] = @YES;
    CVPixelBufferRef b = NULL;
    if (CVPixelBufferCreate(NULL, w, h, kCVPixelFormatType_64RGBAHalf, (__bridge CFDictionaryRef)a, &b)) return NULL;
    return b;
}
static void fill(CVPixelBufferRef b, const uint16_t *rgb) {
    CVPixelBufferLockBaseAddress(b, 0);
    size_t w = CVPixelBufferGetWidth(b), h = CVPixelBufferGetHeight(b), stride = CVPixelBufferGetBytesPerRow(b);
    char *base = CVPixelBufferGetBaseAddress(b);
    for (size_t y = 0; y < h; y++) {
        __fp16 *row = (__fp16 *)(base + y * stride);
        const uint16_t *src = rgb + y * w * 3;
        for (size_t x = 0; x < w; x++) {
            for (int c = 0; c < 3; c++) row[4 * x + c] = to_half_rtz(src[3 * x + c] / 65535.0f);
            row[4 * x + 3] = 1;
        }
    }
    CVPixelBufferUnlockBaseAddress(b, 0);
}
static int drain(CVPixelBufferRef b, uint16_t *rgb) {
    CVPixelBufferLockBaseAddress(b, kCVPixelBufferLock_ReadOnly);
    size_t w = CVPixelBufferGetWidth(b), h = CVPixelBufferGetHeight(b), stride = CVPixelBufferGetBytesPerRow(b);
    const char *base = CVPixelBufferGetBaseAddress(b);
    int ok = 1;
    for (size_t y = 0; y < h && ok; y++) {
        const __fp16 *row = (const __fp16 *)(base + y * stride);
        uint16_t *dst = rgb + y * w * 3;
        for (size_t x = 0; x < w; x++) for (int c = 0; c < 3; c++) {
            float v = row[4 * x + c];
            if (!isfinite(v)) { ok = 0; break; }
            dst[3 * x + c] = (uint16_t)lrintf(fminf(1, fmaxf(0, v)) * 65535.0f);
        }
    }
    CVPixelBufferUnlockBaseAddress(b, kCVPixelBufferLock_ReadOnly);
    return ok;
}

vtfrc *vtfrc_open(int w, int h, char *err, size_t len) { @autoreleasepool {
    if (@available(macOS 15.4, *)) {
        if (![VTFrameRateConversionConfiguration isSupported]) { report(err, len, @"VideoToolbox frame rate conversion is unavailable on this Mac"); return NULL; }
        VTFrameRateConversionConfiguration *cfg = [[VTFrameRateConversionConfiguration alloc]
            initWithFrameWidth:w frameHeight:h usePrecomputedFlow:NO
            qualityPrioritization:VTFrameRateConversionConfigurationQualityPrioritizationQuality
            revision:VTFrameRateConversionConfigurationRevision1];
        if (!cfg || ![cfg.frameSupportedPixelFormats containsObject:@(kCVPixelFormatType_64RGBAHalf)]) {
            report(err, len, [NSString stringWithFormat:@"VideoToolbox rejects a %dx%d RGBAHalf configuration", w, h]);
            return NULL;
        }
        VTFrameProcessor *p = [VTFrameProcessor new];
        NSError *error = nil;
        if (![p startSessionWithConfiguration:cfg error:&error]) { report(err, len, error.localizedDescription); return NULL; }
        vtfrc *s = calloc(1, sizeof *s);
        s->w = w; s->h = h;
        s->a = make_buffer(w, h, cfg.sourcePixelBufferAttributes);
        s->b = make_buffer(w, h, cfg.sourcePixelBufferAttributes);
        s->out = make_buffer(w, h, cfg.destinationPixelBufferAttributes);
        s->processor = (__bridge_retained void *)p;
        s->configuration = (__bridge_retained void *)cfg;
        if (!s->a || !s->b || !s->out) { report(err, len, @"Pixel buffer allocation failed"); return s; }
        return s;
    }
    report(err, len, @"VideoToolbox frame rate conversion requires macOS 15.4 or later");
    return NULL;
}}

// Only called with a session created by vtfrc_open.
API_AVAILABLE(macos(15.4))
void vtfrc_close(vtfrc *s) { @autoreleasepool {
    if (!s) return;
    VTFrameProcessor *p = (__bridge_transfer VTFrameProcessor *)s->processor;
    [p endSession];
    p = nil;
    (void)(__bridge_transfer id)s->configuration;
    if (s->a) CVPixelBufferRelease(s->a);
    if (s->b) CVPixelBufferRelease(s->b);
    if (s->out) CVPixelBufferRelease(s->out);
    free(s);
}}

// Synthesize the frame at `phase` (0..1) between `a` (time ta) and `b` (time tb).
API_AVAILABLE(macos(15.4))
int vtfrc_run(vtfrc *s, const uint16_t *a, double ta, const uint16_t *b, double tb, float phase,
              uint16_t *out, char *err, size_t len) { @autoreleasepool {
    if (!s->a || !s->b || !s->out) { report(err, len, @"Session buffers are unavailable"); return 0; }
    fill(s->a, a);
    fill(s->b, b);
    VTFrameProcessor *p = (__bridge VTFrameProcessor *)s->processor;
    CMTime t0 = CMTimeMakeWithSeconds(ta, 600000), t1 = CMTimeMakeWithSeconds(tb, 600000);
    CMTime to = CMTimeMakeWithSeconds(ta + (tb - ta) * phase, 600000);
    VTFrameProcessorFrame *fa = [[VTFrameProcessorFrame alloc] initWithBuffer:s->a presentationTimeStamp:t0];
    VTFrameProcessorFrame *fb = [[VTFrameProcessorFrame alloc] initWithBuffer:s->b presentationTimeStamp:t1];
    VTFrameProcessorFrame *fo = [[VTFrameProcessorFrame alloc] initWithBuffer:s->out presentationTimeStamp:to];
    VTFrameRateConversionParameters *params = [[VTFrameRateConversionParameters alloc]
        initWithSourceFrame:fa nextFrame:fb opticalFlow:nil interpolationPhase:@[@(phase)]
        submissionMode:VTFrameRateConversionParametersSubmissionModeRandom destinationFrames:@[fo]];
    NSError *error = nil;
    if (!params || ![p processWithParameters:params error:&error]) {
        report(err, len, error.localizedDescription ?: @"Invalid frame parameters");
        return 0;
    }
    if (!drain(s->out, out)) { report(err, len, @"VideoToolbox produced a non-finite pixel"); return 0; }
    return 1;
}}
