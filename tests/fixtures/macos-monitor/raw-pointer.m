// Manual raw-pointer experiment. Default/self-test creates no application or
// window and POSTS NOTHING. Live mode can post only to this process's own panel.
// This is fixture code, not a production raw-input capability.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ApplicationServices/ApplicationServices.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static CGEventRef make_pointer(CGEventSourceRef source, CGEventType type,
                               CGPoint point, uint32_t window, pid_t pid, int64_t tag) {
    if (!source || (type != kCGEventLeftMouseDown && type != kCGEventLeftMouseUp) ||
        !isfinite(point.x) || !isfinite(point.y) || fabs(point.x) > 1000000 ||
        fabs(point.y) > 1000000 || !window || pid <= 0 || tag <= 0) return NULL;
    CGEventRef event = CGEventCreateMouseEvent(source, type, point, kCGMouseButtonLeft);
    if (!event) return NULL;
    // Never inherit the human's held modifier flags or pointer deltas.
    CGEventSetFlags(event, 0);
    CGEventSetIntegerValueField(event, kCGMouseEventClickState, 1);
    CGEventSetIntegerValueField(event, kCGMouseEventButtonNumber, 0);
    CGEventSetIntegerValueField(event, kCGMouseEventDeltaX, 0);
    CGEventSetIntegerValueField(event, kCGMouseEventDeltaY, 0);
    CGEventSetIntegerValueField(event, kCGMouseEventWindowUnderMousePointer, window);
    CGEventSetIntegerValueField(event, kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent, window);
    CGEventSetIntegerValueField(event, kCGEventTargetUnixProcessID, pid);
    CGEventSetIntegerValueField(event, kCGEventSourceUserData, tag);
    CGEventSetDoubleValueField(event, kCGMouseEventPressure,
                              type == kCGEventLeftMouseDown ? 1.0 : 0.0);
    return event;
}

static BOOL event_matches(CGEventSourceRef source, CGEventRef event, CGEventType type, CGPoint point,
                          uint32_t window, pid_t pid, int64_t tag) {
    return source && event && CGEventGetType(event) == type && CGEventGetFlags(event) == 0 &&
        CGPointEqualToPoint(CGEventGetLocation(event), point) &&
        // Private creates a unique table; -1 is the creation selector, not its ID.
        CGEventSourceGetSourceStateID(source) != kCGEventSourceStateCombinedSessionState &&
        CGEventSourceGetSourceStateID(source) != kCGEventSourceStateHIDSystemState &&
        CGEventGetIntegerValueField(event, kCGEventSourceStateID) == CGEventSourceGetSourceStateID(source) &&
        CGEventGetIntegerValueField(event, kCGMouseEventClickState) == 1 &&
        CGEventGetIntegerValueField(event, kCGMouseEventButtonNumber) == 0 &&
        CGEventGetIntegerValueField(event, kCGMouseEventDeltaX) == 0 &&
        CGEventGetIntegerValueField(event, kCGMouseEventDeltaY) == 0 &&
        CGEventGetIntegerValueField(event, kCGMouseEventWindowUnderMousePointer) == window &&
        CGEventGetIntegerValueField(event, kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent) == window &&
        CGEventGetIntegerValueField(event, kCGEventTargetUnixProcessID) == pid &&
        CGEventGetIntegerValueField(event, kCGEventSourceUnixProcessID) == getpid() &&
        CGEventGetIntegerValueField(event, kCGEventSourceUserData) == tag &&
        CGEventGetDoubleValueField(event, kCGMouseEventPressure) ==
            (type == kCGEventLeftMouseDown ? 1.0 : 0.0);
}

static int emit(NSDictionary *value, int code) {
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:0 error:&error];
    if (!data || data.length > 16384) return 3;
    if (fwrite(data.bytes, 1, data.length, stdout) != data.length ||
        fputc('\n', stdout) == EOF || fflush(stdout) != 0) return 3;
    return code;
}

static NSDictionary *event_fields(CGEventRef event) {
    if (!event) return @{@"error":@"allocation failed"};
    CGPoint p = CGEventGetLocation(event);
    return @{@"type":@(CGEventGetType(event)), @"flags":@(CGEventGetFlags(event)),
        @"x":@(p.x), @"y":@(p.y),
        @"state":@(CGEventGetIntegerValueField(event,kCGEventSourceStateID)),
        @"source_pid":@(CGEventGetIntegerValueField(event,kCGEventSourceUnixProcessID)),
        @"target_pid":@(CGEventGetIntegerValueField(event,kCGEventTargetUnixProcessID)),
        @"window":@(CGEventGetIntegerValueField(event,kCGMouseEventWindowUnderMousePointer)),
        @"handle_window":@(CGEventGetIntegerValueField(event,kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent)),
        @"tag":@(CGEventGetIntegerValueField(event,kCGEventSourceUserData)),
        @"pressure":@(CGEventGetDoubleValueField(event,kCGMouseEventPressure)),
        @"click_count":@(CGEventGetIntegerValueField(event,kCGMouseEventClickState)),
        @"button":@(CGEventGetIntegerValueField(event,kCGMouseEventButtonNumber)),
        @"dx":@(CGEventGetIntegerValueField(event,kCGMouseEventDeltaX)),
        @"dy":@(CGEventGetIntegerValueField(event,kCGMouseEventDeltaY))};
}

static int construction_test(void) {
    CGEventSourceRef source = CGEventSourceCreate(kCGEventSourceStatePrivate);
    if (!source) return emit(@{@"ok":@NO, @"mode":@"construction_only",
                              @"posted_events":@0, @"error":@"private source unavailable"}, 1);
    BOOL ok = YES; NSUInteger cases = 0; NSMutableArray *observations = [NSMutableArray array];
    // All target identities and positions here are synthetic. No OS enumeration.
    for (NSUInteger i = 0; i < 3; ++i) {
        CGPoint point = (CGPoint[]){CGPointMake(-770.5,30.25), CGPointZero,
                                   CGPointMake(11999.5,-999.25)}[i];
        for (NSUInteger j = 0; j < 2; ++j) {
            CGEventType type = j ? kCGEventLeftMouseUp : kCGEventLeftMouseDown;
            CGEventRef event = make_pointer(source, type, point, 12345, getpid(), 1234567);
            ok &= event_matches(source, event, type, point, 12345, getpid(), 1234567); ++cases;
            [observations addObject:event_fields(event)];
            if (event) CFRelease(event);
        }
    }
    // Invalid identities, unsupported events and nonfinite coordinates fail
    // before allocating a usable event. Never normalize/clamp a bad target.
    CGEventRef invalid[] = {
        make_pointer(source, kCGEventLeftMouseDown, CGPointZero, 0, getpid(), 1),
        make_pointer(source, kCGEventLeftMouseDown, CGPointZero, 1, 0, 1),
        make_pointer(source, kCGEventLeftMouseDown, CGPointZero, 1, getpid(), 0),
        make_pointer(source, kCGEventMouseMoved, CGPointZero, 1, getpid(), 1),
        make_pointer(source, kCGEventLeftMouseDown, CGPointMake(NAN,0), 1, getpid(), 1),
        make_pointer(source, kCGEventLeftMouseDown, CGPointMake(0,INFINITY), 1, getpid(), 1),
        make_pointer(source, kCGEventLeftMouseDown, CGPointMake(1000001,0), 1, getpid(), 1),
    };
    for (NSUInteger i = 0; i < sizeof(invalid)/sizeof(invalid[0]); ++i) {
        ok &= invalid[i] == NULL; ++cases;
        if (invalid[i]) CFRelease(invalid[i]);
    }
    CFRelease(source);
    ok &= NSApp == nil;
    return emit(@{@"ok":@(ok), @"mode":@"construction_only", @"posted_events":@0,
                  @"native_cases":@(cases), @"readback_cases":observations, @"expected_pid":@(getpid()), @"application_created":(NSApp != nil ? @YES : @NO),
                  @"delivery_verified":@NO, @"effect_verified":@NO}, ok ? 0 : 1);
}

@interface ProbePanel : NSPanel
@end
@implementation ProbePanel
- (BOOL)canBecomeKeyWindow { return NO; }
- (BOOL)canBecomeMainWindow { return NO; }
@end

@interface ProbeCanvas : NSView
@property int64_t eventTag;
@property uint32_t expectedWindow;
@property NSUInteger downs;
@property NSUInteger ups;
@property NSUInteger clicks;
@property NSUInteger unrelated;
@property BOOL downAccepted;
@property BOOL overflow;
@property(strong) NSMutableArray *receipts;
@end
@implementation ProbeCanvas
- (BOOL)acceptsFirstMouse:(NSEvent *)event { (void)event; return YES; }
- (BOOL)acceptsFirstResponder { return NO; }
- (void)drawRect:(NSRect)rect {
    (void)rect;
    [[NSColor colorWithCalibratedRed:0.1 green:0.25 blue:0.4 alpha:1] setFill];
    NSRectFill(self.bounds);
}
- (void)record:(NSEvent *)event down:(BOOL)down {
    CGEventRef cg = event.CGEvent;
    if (!cg || CGEventGetIntegerValueField(cg,kCGEventSourceUserData) != self.eventTag) {
        self.unrelated += 1; return; // No human event contents or coordinates retained.
    }
    if (self.receipts.count >= 8) { self.overflow = YES; return; }
    CGPoint global = CGEventGetLocation(cg);
    NSInteger window = event.window.windowNumber;
    int64_t source = CGEventGetIntegerValueField(cg,kCGEventSourceUnixProcessID);
    [self.receipts addObject:@{@"tag":@(self.eventTag), @"pid":@(getpid()),
        @"source_pid":@(source), @"window_id":@(window),
        @"kind":down ? @"left_down" : @"left_up", @"x":@(global.x), @"y":@(global.y)}];
    BOOL exact = window == self.expectedWindow && source == getpid() &&
        NSPointInRect([self convertPoint:event.locationInWindow fromView:nil],self.bounds);
    if (down) {
        self.downs += 1; self.downAccepted = exact && self.downs == 1;
    } else {
        self.ups += 1;
        if (exact && self.downAccepted && self.ups == 1) self.clicks += 1;
        self.downAccepted = NO;
    }
}
- (void)mouseDown:(NSEvent *)event { [self record:event down:YES]; }
- (void)mouseUp:(NSEvent *)event { [self record:event down:NO]; }
@end

static NSDictionary *desktop_sample(void) {
    NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
    CGEventRef sample = CGEventCreate(NULL);
    if (!front || !sample) { if (sample) CFRelease(sample); return @{}; }
    CGPoint point = CGEventGetLocation(sample); CFRelease(sample);
    return @{@"front_pid":@(front.processIdentifier), @"pointer_x":@(point.x),
             @"pointer_y":@(point.y), @"clipboard_change_count":@(NSPasteboard.generalPasteboard.changeCount)};
}

static BOOL own_bounds(NSWindow *window, CGRect *bounds) {
    NSArray *rows = CFBridgingRelease(CGWindowListCopyWindowInfo(
        kCGWindowListOptionIncludingWindow, (CGWindowID)window.windowNumber));
    if (rows.count != 1 || [rows[0][(id)kCGWindowOwnerPID] intValue] != getpid() ||
        [rows[0][(id)kCGWindowNumber] integerValue] != window.windowNumber) return NO;
    return CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)
        rows[0][(id)kCGWindowBounds], bounds);
}

static int live_probe(void) {
    // This branch requires its own exact command-line opt-in. No arbitrary PID,
    // window or coordinates can be supplied. NOT a substitute for cross-app tests.
    if (!AXIsProcessTrusted()) return emit(@{@"ok":@NO,@"mode":@"self_process_click",
        @"posted_events":@0,@"error":@"existing Accessibility permission required"},1);
    NSDictionary *before = desktop_sample();
    if (before.count != 4) return emit(@{@"ok":@NO,@"mode":@"self_process_click",
        @"posted_events":@0,@"error":@"desktop observation unavailable; no dispatch"},1);
    NSApplication *app = [NSApplication sharedApplication];
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    ProbePanel *panel = [[ProbePanel alloc] initWithContentRect:NSMakeRect(50,50,320,220)
        styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskNonactivatingPanel
        backing:NSBackingStoreBuffered defer:NO];
    panel.title = @"Intendant disposable raw-pointer probe";
    panel.releasedWhenClosed = NO; panel.hidesOnDeactivate = NO;
    panel.animationBehavior = NSWindowAnimationBehaviorNone;
    ProbeCanvas *view = [[ProbeCanvas alloc] initWithFrame:NSMakeRect(0,0,320,220)];
    view.receipts = [NSMutableArray array];
    int64_t tag = 0;
    do { arc4random_buf(&tag,sizeof(tag)); tag &= INT64_MAX; } while (tag == 0);
    view.eventTag = tag; panel.contentView = view;
    [panel orderWindow:NSWindowBelow relativeTo:0];
    view.expectedWindow = (uint32_t)panel.windowNumber;
    CGRect bounds = CGRectZero; NSString *error = nil;
    NSRunningApplication *foreground = NSWorkspace.sharedWorkspace.frontmostApplication;
    BOOL observationsComplete = foreground != nil;
    BOOL front = foreground && foreground.processIdentifier == getpid();
    NSUInteger posted = 0;
    CGEventSourceRef source = NULL; CGEventRef down = NULL, up = NULL;
    CGPoint point = CGPointZero;
    if (!observationsComplete || !own_bounds(panel,&bounds) || CGRectIsEmpty(bounds) || front ||
        panel.keyWindow || CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,kCGMouseButtonLeft)) {
        error = @"target unavailable, foreground, or human mouse button held; no dispatch";
    } else {
        point = CGPointMake(CGRectGetMidX(bounds),CGRectGetMidY(bounds));
        source = CGEventSourceCreate(kCGEventSourceStatePrivate);
        down = make_pointer(source,kCGEventLeftMouseDown,point,view.expectedWindow,getpid(),tag);
        up = make_pointer(source,kCGEventLeftMouseUp,point,view.expectedWindow,getpid(),tag);
        if (!event_matches(source,down,kCGEventLeftMouseDown,point,view.expectedWindow,getpid(),tag) ||
            !event_matches(source,up,kCGEventLeftMouseUp,point,view.expectedWindow,getpid(),tag)) {
            error = @"event construction/readback refused; no dispatch";
        } else {
            // Preallocate both halves. No global post, pointer warp, activation,
            // tap, keyboard event, retry, or fallback. getpid() cannot be reused
            // while this process is executing. Posting is not a delivery receipt.
            CGEventPostToPid(getpid(),down); ++posted;
            CGEventPostToPid(getpid(),up); ++posted;
            NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 1.0;
            while (NSProcessInfo.processInfo.systemUptime < deadline) {
                NSEvent *event = [app nextEventMatchingMask:NSEventMaskAny
                    untilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]
                    inMode:NSDefaultRunLoopMode dequeue:YES];
                if (event) [app sendEvent:event];
                foreground = NSWorkspace.sharedWorkspace.frontmostApplication;
                observationsComplete &= foreground != nil;
                front |= foreground && foreground.processIdentifier == getpid();
            }
        }
    }
    if (down) CFRelease(down);
    if (up) CFRelease(up);
    if (source) CFRelease(source);
    NSDictionary *after = desktop_sample();
    observationsComplete &= after.count == 4;
    [panel close];
    BOOL closed = !panel.visible;
    BOOL ok = !error && observationsComplete && posted == 2 && !front && !view.overflow &&
        view.downs == 1 && view.ups == 1 && view.clicks == 1 && closed;
    return emit(@{@"ok":@(ok),@"mode":@"self_process_click",@"posted_events":@(posted),
        @"plan":@{@"pid":@(getpid()),@"source_pid":@(getpid()),@"window_id":@(view.expectedWindow),
            @"tag":@(tag),@"x":@(point.x),@"y":@(point.y)},
        @"receipts":view.receipts,@"tagged_click_count":@(view.clicks),
        @"unrelated_event_count":@(view.unrelated),@"receipt_overflow":@(view.overflow),
        @"target_ever_front":@(front),@"observations_complete":@(observationsComplete),@"before":before,@"after":after,@"window_closed":@(closed),
        @"error":error ?: @"",@"cross_process_verified":@NO},ok ? 0 : 1);
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc == 1 || (argc == 2 && strcmp(argv[1],"--self-test") == 0))
            return construction_test();
        if (argc == 2 && strcmp(argv[1],"--allow-disposable-process-click") == 0)
            return live_probe();
        fprintf(stderr,"Use --self-test (no posting), or explicit --allow-disposable-process-click. No target arguments accepted.\n");
        return 2;
    }
}
