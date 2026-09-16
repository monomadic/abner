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
