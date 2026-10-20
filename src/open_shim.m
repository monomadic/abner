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

// Keep AppKit actions out of the Rust app's mutable event-loop borrow. The
// callbacks queue a request; Rust applies it on the loop and then supplies
// the actual state (checkmarks, enabled items) through ab_sync_menus.
typedef void (*AbViewCallback)(int mode);
typedef void (*AbMenuCallback)(int action);
static AbViewCallback ab_view_cb = NULL;
static AbMenuCallback ab_menu_cb = NULL;
@interface AbMenuTarget : NSObject
- (void)selectMode:(NSMenuItem *)sender;
- (void)menuAction:(NSMenuItem *)sender;
@end
@implementation AbMenuTarget
- (void)selectMode:(NSMenuItem *)sender {
    if (ab_view_cb != NULL) ab_view_cb((int)sender.tag);
}
- (void)menuAction:(NSMenuItem *)sender {
    if (ab_menu_cb != NULL) ab_menu_cb((int)sender.tag);
}
@end

// Menu action tags — mirrored by open.rs's MenuAction.
enum { AB_MENU_OPEN = 1, AB_MENU_REVEAL = 2, AB_MENU_EXPORT = 3, AB_MENU_CLOSE = 4, AB_MENU_COPY_FRAME = 5 };

static NSMenuItem *ab_item(NSMenu *menu, NSString *title, SEL action, NSString *key,
                           NSEventModifierFlags mods, NSInteger tag, id target) {
    NSMenuItem *item = [[NSMenuItem alloc] initWithTitle:title action:action keyEquivalent:key];
    item.keyEquivalentModifierMask = mods;
    item.target = target;
    item.tag = tag;
    [menu addItem:item];
    return item;
}

static void ab_add_menu(NSMenu *bar, NSMenu *menu, NSInteger at) {
    NSMenuItem *heading = [[NSMenuItem alloc] initWithTitle:menu.title action:nil keyEquivalent:@""];
    heading.submenu = menu;
    [bar insertItem:heading atIndex:MIN(at, bar.numberOfItems)];
}

// File · View · Frame, after winit's application menu. Built once; every
// call re-derives the enabled/checked state from the app, never from clicks.
void ab_sync_menus(int mode, bool has_clips, bool can_export, AbViewCallback view_cb, AbMenuCallback menu_cb) {
    static NSMenu *file = nil, *view = nil, *frame = nil;
    static AbMenuTarget *target = nil;
    ab_view_cb = view_cb;
    ab_menu_cb = menu_cb;
    if (view == nil) {
        NSMenu *bar = NSApp.mainMenu;
        if (bar == nil) return; // winit creates the standard application menu.
        target = [AbMenuTarget new];
        NSEventModifierFlags cmd = NSEventModifierFlagCommand;

        file = [[NSMenu alloc] initWithTitle:@"File"];
        file.autoenablesItems = NO;
        ab_item(file, @"Open…", @selector(menuAction:), @"o", cmd, AB_MENU_OPEN, target);
        [file addItem:[NSMenuItem separatorItem]];
        ab_item(file, @"Reveal in Finder", @selector(menuAction:), @"r", cmd | NSEventModifierFlagShift, AB_MENU_REVEAL, target);
        ab_item(file, @"Export", @selector(menuAction:), @"e", cmd, AB_MENU_EXPORT, target);
        [file addItem:[NSMenuItem separatorItem]];
        ab_item(file, @"Close", @selector(menuAction:), @"w", cmd, AB_MENU_CLOSE, target);

        view = [[NSMenu alloc] initWithTitle:@"View"];
        view.autoenablesItems = NO;
        NSArray<NSString *> *names = @[@"Sources", @"Timeline", @"Mask", @"Crop"];
        const NSInteger modes[] = {0, 3, 1, 2};
        for (NSUInteger i = 0; i < names.count; i++) {
            ab_item(view, names[i], @selector(selectMode:), @"", 0, modes[i], target);
        }

        frame = [[NSMenu alloc] initWithTitle:@"Frame"];
        frame.autoenablesItems = NO;
        ab_item(frame, @"Copy Frame as PNG", @selector(menuAction:), @"c", cmd, AB_MENU_COPY_FRAME, target);

        ab_add_menu(bar, file, 1);
        ab_add_menu(bar, view, 2);
        ab_add_menu(bar, frame, 3);
    }
    for (NSMenuItem *item in file.itemArray) {
        switch (item.tag) {
            case AB_MENU_OPEN: item.enabled = YES; break;
            case AB_MENU_EXPORT: item.enabled = can_export; break;
            case AB_MENU_CLOSE: item.enabled = YES; break; // on the empty window it quits, like ⌘W
            default: item.enabled = has_clips; break;
        }
    }
    for (NSMenuItem *item in view.itemArray) {
        item.enabled = has_clips;
        item.state = item.tag == mode ? NSControlStateValueOn : NSControlStateValueOff;
    }
    for (NSMenuItem *item in frame.itemArray) item.enabled = has_clips;
}

// Encoded PNG bytes onto the general pasteboard. Main thread.
void ab_copy_png(const uint8_t *bytes, size_t len) {
    NSPasteboard *pb = [NSPasteboard generalPasteboard];
    [pb clearContents];
    [pb setData:[NSData dataWithBytes:bytes length:len] forType:NSPasteboardTypePNG];
}

// Select the file in a Finder window.
void ab_reveal_in_finder(const char *path) {
    NSURL *url = [NSURL fileURLWithFileSystemRepresentation:path isDirectory:NO relativeToURL:nil];
    [[NSWorkspace sharedWorkspace] activateFileViewerSelectingURLs:@[url]];
}

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

// With FullSizeContentView the GPU view lies under the titlebar strip, and a
// view that can move the window hands AppKit the strip's gestures: a drag
// moves the window (fine), a double-click zooms it — even on a painted
// button, since the header input view above is not what NSThemeFrame hit
// tests there. Grafting a NO onto winit's view class (never subclassing it)
// takes both away; main.rs drags on a bare-strip press and zooms on its own
// double-click detection. 1 = grafted, 2 = the class already answers.
static BOOL ab_no_move(id self, SEL _cmd) { (void)self; (void)_cmd; return NO; }
int ab_pin_content_view(void *raw_view) {
    NSView *view = (__bridge NSView *)raw_view;
    Class cls = object_getClass(view);
    SEL sel = @selector(mouseDownCanMoveWindow);
    unsigned int count = 0;
    Method *methods = class_copyMethodList(cls, &count);
    BOOL own = NO;
    for (unsigned int i = 0; i < count; i++) {
        if (method_getName(methods[i]) == sel) { own = YES; break; }
    }
    free(methods);
    if (own) return 2;
    return class_addMethod(cls, sel, (IMP)ab_no_move, "B@:") ? 1 : 0;
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
