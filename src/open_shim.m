// Open With / double-click support. LaunchServices delivers an opened
// document as an Apple Event to the NSApplicationDelegate, never as argv,
// so a plist that lists video types is only half the feature — without a
// handler the app would launch to an empty window (or, worse, AppKit's
// default NSDocumentController answers the event with "Abner cannot open
// files in the “MPEG-4 movie” format").
//
// winit 0.30 registers its OWN application delegate and panics if it finds
// a different one installed ("tried to get a delegate that was not the one
// Winit has registered", app_state.rs — replacing it was switchblade's
// first cut and aborted at launch; the platform doc's "winit registers no
// delegate" guarantee is 0.31+). So instead of swapping the delegate, this
// grafts the one method it lacks onto winit's delegate CLASS via
// class_addMethod: `application:openURLs:` is the modern (10.13+) delivery
// for the odoc event, covering both a cold launch and an open into a
// running app, and AppKit checks respondsToSelector at finishLaunching —
// so the graft must happen before the run loop starts. Each file path is
// handed to the Rust callback; batching back into one drop happens on the
// Rust side (open.rs), same as drag-in. (switchblade's open_shim.m.)

#import <AppKit/AppKit.h>
#import <objc/runtime.h>

typedef void (*AbOpenCallback)(const char *path);

static AbOpenCallback ab_open_cb = NULL;

static void ab_application_open_urls(id self, SEL _cmd, NSApplication *application,
                                     NSArray<NSURL *> *urls) {
    (void)self;
    (void)_cmd;
    (void)application;
    for (NSURL *url in urls) {
        if (![url isFileURL]) {
            continue;
        }
        const char *path = url.fileSystemRepresentation;
        if (path != NULL && ab_open_cb != NULL) {
            ab_open_cb(path);
        }
    }
}

// Main thread only, after winit's EventLoop::new (which creates the app and
// installs its delegate) and before the run loop starts. Returns 1 when the
// handler is grafted, 0 when there was no delegate to graft onto, 2 when
// the delegate already implements the selector (a future winit grew its
// own — our callback will never fire; the Rust side logs it).
int ab_install_open_handler(AbOpenCallback cb) {
    ab_open_cb = cb;
    id delegate = [[NSApplication sharedApplication] delegate];
    if (delegate == nil) {
        return 0;
    }
    SEL sel = @selector(application:openURLs:);
    Class cls = object_getClass(delegate);
    if (class_respondsToSelector(cls, sel)) {
        return 2;
    }
    return class_addMethod(cls, sel, (IMP)ab_application_open_urls, "v@:@@") ? 1 : 0;
}

// A non-modal picker feeds the same batch queue as Open With. Playback and
// redraws continue while it is open. Cancellation makes no model changes.
void ab_choose_files(void) {
    static NSOpenPanel *panel = nil;
    if (panel != nil) { [panel makeKeyAndOrderFront:nil]; return; }
    panel = [NSOpenPanel openPanel];
    panel.canChooseFiles = YES;
    panel.canChooseDirectories = NO;
    panel.allowsMultipleSelection = YES;
    panel.prompt = @"Add sources";
    [panel beginWithCompletionHandler:^(NSModalResponse response) {
        if (response == NSModalResponseOK) {
            for (NSURL *url in panel.URLs) {
                if (url.isFileURL && ab_open_cb != NULL) ab_open_cb(url.fileSystemRepresentation);
            }
        }
        panel = nil;
    }];
}

// Transparent full-size content receives the titlebar's clicks as client input.
// Use AppKit's zoom/restore action and the user's global titlebar preference.
double ab_double_click_interval(void) { return [NSEvent doubleClickInterval]; }

void ab_titlebar_double_click(void *raw_view) {
    NSWindow *window = ((__bridge NSView *)raw_view).window;
    if (window == nil || (window.styleMask & NSWindowStyleMaskFullScreen)) return;
    NSUserDefaults *defaults = [NSUserDefaults standardUserDefaults];
    NSString *action = [defaults stringForKey:@"AppleActionOnDoubleClick"];
    if ([action isEqualToString:@"None"]) return;
    if ([action isEqualToString:@"Minimize"] ||
        (action == nil && [defaults boolForKey:@"AppleMiniaturizeOnDoubleClick"])) {
        [window performMiniaturize:nil];
    } else {
        // With no content-size restriction, AppKit's standard zoom frame fills
        // the usable screen; it also owns restoring the user's previous frame.
        [window performZoom:nil];
    }
}

// The full-size GPU view paints beneath AppKit's titlebar. Native titlebar hit
// testing can otherwise turn clicks on painted buttons into zoom/drag gestures.
// Intercept only the actual controls; leave traffic lights and empty background
// to their normal handlers. Never replace or subclass winit's content view.
@interface AbHeaderInputView : NSView
@property(nonatomic, weak) NSView *target;
@property(nonatomic, copy) NSArray<NSValue *> *controlRects;
@end
@implementation AbHeaderInputView
- (BOOL)isOpaque { return NO; }
- (BOOL)acceptsFirstMouse:(NSEvent *)event { return YES; }
- (BOOL)mouseDownCanMoveWindow { return NO; }
- (NSView *)hitTest:(NSPoint)point {
    NSPoint p = [self.target convertPoint:point fromView:self.superview];
    for (NSValue *value in self.controlRects) {
        if (NSPointInRect(p, value.rectValue)) return self;
    }
    return nil;
}
- (void)mouseDown:(NSEvent *)event { [self.target mouseDown:event]; }
- (void)mouseUp:(NSEvent *)event { [self.target mouseUp:event]; }
- (void)mouseDragged:(NSEvent *)event { [self.target mouseDragged:event]; }
- (void)mouseMoved:(NSEvent *)event { [self.target mouseMoved:event]; }
@end
static char ab_header_input_key;
void ab_install_header_input(void *raw_view) {
    NSView *view = (__bridge NSView *)raw_view;
    if (objc_getAssociatedObject(view, &ab_header_input_key)) return;
    NSView *parent = view.window.contentView.superview;
    if (parent == nil) return;
    AbHeaderInputView *input = [[AbHeaderInputView alloc] initWithFrame:parent.bounds];
    input.target = view;
    input.controlRects = @[];
    input.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    [parent addSubview:input positioned:NSWindowAbove relativeTo:nil];
    objc_setAssociatedObject(view, &ab_header_input_key, input, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
}
void ab_set_header_controls(void *raw_view, const double *rects, size_t count) {
    NSView *view = (__bridge NSView *)raw_view;
    AbHeaderInputView *input = objc_getAssociatedObject(view, &ab_header_input_key);
    NSMutableArray<NSValue *> *values = [NSMutableArray arrayWithCapacity:count];
    for (size_t i = 0; i < count; i++) {
        const double *r = rects + i * 4;
        [values addObject:[NSValue valueWithRect:NSMakeRect(r[0], r[1], r[2], r[3])]];
    }
    if (![input.controlRects isEqualToArray:values]) input.controlRects = values;
}
