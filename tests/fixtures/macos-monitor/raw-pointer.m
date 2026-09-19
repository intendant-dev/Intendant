// Manual raw-pointer experiment. Default/self-test creates no application or
// window and POSTS NOTHING. Live modes target only the retained fixture panel,
// directly or from a private child restricted to its live parent.
// This is fixture code, not a production raw-input capability.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ApplicationServices/ApplicationServices.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>
#include <dlfcn.h>
#include "pointer-event.h"

// Normalize both constructors identically; a source state is not a private seat.

static CGEventRef make_pointer(CGEventSourceRef source, CGEventType type,
                               CGPoint point, uint32_t window, pid_t pid, int64_t tag) {
    if (!source || (type != kCGEventLeftMouseDown && type != kCGEventLeftMouseUp) ||
        !isfinite(point.x) || !isfinite(point.y) || fabs(point.x) > 1000000 ||
        fabs(point.y) > 1000000 || !window || pid <= 0 || tag <= 0) return NULL;
    CGEventRef event = CGEventCreateMouseEvent(source, type, point, kCGMouseButtonLeft);
    if (!event) return NULL;
    configure_pointer(source,event,type,point,window,pid,tag);
    return event;
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
    BOOL spi = dlsym(RTLD_DEFAULT,"CGEventSetWindowLocation") && dlsym(RTLD_DEFAULT,"CGEventGetWindowLocation");
    NSUInteger windowCases = 0;
    if (spi) {
        for (NSUInteger i = 0; i < 3; ++i) {
            CGEventRef event = make_pointer(source,kCGEventLeftMouseDown,CGPointZero,12345,getpid(),1234567);
            CGPoint local = (CGPoint[]){CGPointZero,CGPointMake(-15.5,30.25),CGPointMake(160.25,125.75)}[i];
            ok &= set_window_point(event,local); ++cases; ++windowCases;
            if (event) CFRelease(event);
        }
    }
    ok &= !set_window_point(NULL,CGPointZero);
    CFRelease(source);
    ok &= NSApp == nil;
    return emit(@{@"ok":@(ok), @"mode":@"construction_only", @"posted_events":@0,
                  @"native_cases":@(cases), @"window_location_cases":@(windowCases),
                  @"window_location_spi_available":@(spi), @"readback_cases":observations, @"expected_pid":@(getpid()), @"application_created":(NSApp != nil ? @YES : @NO),
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
@property pid_t expectedSource;
@property CGPoint expectedGlobal;
@property NSPoint expectedLocal;
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
        @"kind":down ? @"left_down" : @"left_up", @"x":@(global.x), @"y":@(global.y),
        @"local_x":@(event.locationInWindow.x), @"local_y":@(event.locationInWindow.y)}];
    BOOL exact = window == self.expectedWindow && source == self.expectedSource &&
        fabs(global.x-self.expectedGlobal.x) <= 0.5 && fabs(global.y-self.expectedGlobal.y) <= 0.5 &&
        fabs(event.locationInWindow.x-self.expectedLocal.x) <= 0.5 &&
        fabs(event.locationInWindow.y-self.expectedLocal.y) <= 0.5 &&
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

// Public AppKit constructor carries the actual destination window, unlike
// Quartz's under-pointer metadata. The event still travels through CGEventPostToPid;
// no direct view/window delivery or global fallback is used.


// Private child protocol: fixed-size pipe message from the owning receiver.
// There is no command-line PID/window input and the only legal target is getppid().
typedef struct {
    uint32_t version, window;
    int32_t receiver;
    int64_t tag;
    CGPoint point;
    NSPoint local;
    CGRect bounds;
} SenderPlan;

static BOOL parent_window_matches(SenderPlan plan) {
    if (plan.receiver <= 1 || plan.receiver != getppid() || !plan.window || plan.tag <= 0 ||
        !isfinite(plan.point.x) || !isfinite(plan.point.y) ||
        !isfinite(plan.local.x) || !isfinite(plan.local.y) ||
        !CGRectContainsPoint(plan.bounds,plan.point)) return NO;
    NSArray *rows = CFBridgingRelease(CGWindowListCopyWindowInfo(
        kCGWindowListOptionIncludingWindow,plan.window));
    CGRect observed = CGRectZero;
    NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
    return rows.count == 1 && [rows[0][(id)kCGWindowOwnerPID] intValue] == plan.receiver &&
        [rows[0][(id)kCGWindowNumber] unsignedIntValue] == plan.window &&
        CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)rows[0][(id)kCGWindowBounds],&observed) &&
        CGRectEqualToRect(observed,plan.bounds) && front && front.processIdentifier != plan.receiver &&
        !CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,kCGMouseButtonLeft);
}

static int parent_sender(void) {
    // A stalled/dead receiver cannot leave this internal child around indefinitely.
    alarm(4);
    SenderPlan plan = {0};
    struct stat info;
    if (fstat(STDIN_FILENO,&info) != 0 || !S_ISFIFO(info.st_mode))
        return emit(@{@"ok":@NO,@"posted_events":@0,@"error":@"private pipe required"},1);
    size_t have = 0;
    while (have < sizeof(plan)) {
        ssize_t n = read(STDIN_FILENO,((char *)&plan)+have,sizeof(plan)-have);
        if (n <= 0) return emit(@{@"ok":@NO,@"posted_events":@0,@"error":@"incomplete sender plan"},1);
        have += (size_t)n;
    }
    char extra;
    if (read(STDIN_FILENO,&extra,1) != 1 || extra != 'D' || plan.version != 1 ||
        !AXIsProcessTrusted() || !parent_window_matches(plan))
        return emit(@{@"ok":@NO,@"posted_events":@0,@"error":@"parent target preflight refused"},1);
    CGEventSourceRef source = CGEventSourceCreate(kCGEventSourceStatePrivate);
    CGEventRef down = make_addressed_pointer(source,kCGEventLeftMouseDown,plan.point,
        plan.window,plan.local,plan.receiver,plan.tag);
    CGEventRef up = make_addressed_pointer(source,kCGEventLeftMouseUp,plan.point,
        plan.window,plan.local,plan.receiver,plan.tag);
    BOOL ready = event_matches(source,down,kCGEventLeftMouseDown,plan.point,plan.window,plan.receiver,plan.tag) &&
        event_matches(source,up,kCGEventLeftMouseUp,plan.point,plan.window,plan.receiver,plan.tag) &&
        parent_window_matches(plan);
    NSUInteger posted = 0;
    if (ready) {
        // The live receiver is our direct parent and owns the exact checked window.
        // No fallback, global post, key event, pointer warp or focus operation.
        CGEventPostToPid(plan.receiver,down); ++posted;
        CGEventPostToPid(plan.receiver,up); ++posted;
    }
    char acknowledgement = 0;
    if (ready) read(STDIN_FILENO,&acknowledgement,1);
    if (down) CFRelease(down);
    if (up) CFRelease(up);
    if (source) CFRelease(source);
    return emit(@{@"ok":@(ready),@"posted_events":@(posted),@"sender_pid":@(getpid()),
        @"receiver_pid":@(plan.receiver),@"acknowledged":(acknowledgement == 'A' ? @YES : @NO),@"post_access":(CGPreflightPostEventAccess() ? @YES : @NO),@"error":ready ? @"" : @"sender event/target revalidation refused"},ready ? 0 : 1);
}

// Inspect only our correlation tag; dispatch through ordinary NSApplication.
// Queue arrival is diagnostic and never substitutes for canvas receipt/effect.
static void dispatch_observed(NSApplication *app, NSEvent *event, NSWindow *panel,
                               int64_t tag, NSMutableArray *receipts, BOOL *overflow) {
    if (!event) return;
    CGEventRef cg = event.CGEvent;
    if (cg && CGEventGetIntegerValueField(cg,kCGEventSourceUserData) == tag) {
        if (receipts.count >= 8) {
            *overflow = YES;
        } else {
            [receipts addObject:@{@"event":event_fields(cg), @"appkit_window":@(event.windowNumber),
                @"appkit_type":@(event.type), @"local_x":@(event.locationInWindow.x),
                @"local_y":@(event.locationInWindow.y),
                @"resolved_own_window":(event.window == panel ? @YES : @NO)}];
        }
    }
    [app sendEvent:event];
}

static int live_probe(BOOL cross) {
    // This branch requires its own exact command-line opt-in. No arbitrary PID,
    // window or coordinates can be supplied. NOT a substitute for cross-app tests.
    if (!AXIsProcessTrusted()) return emit(@{@"ok":@NO,@"mode":cross ? @"cross_process_click" : @"self_process_click",
        @"posted_events":@0,@"error":@"existing Accessibility permission required"},1);
    NSDictionary *before = desktop_sample();
    if (before.count != 4) return emit(@{@"ok":@NO,@"mode":cross ? @"cross_process_click" : @"self_process_click",
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
    view.eventTag = tag; view.expectedSource = getpid(); panel.contentView = view;
    [panel orderWindow:NSWindowBelow relativeTo:0];
    view.expectedWindow = (uint32_t)panel.windowNumber;
    CGRect bounds = CGRectZero; NSString *error = nil;
    NSRunningApplication *foreground = NSWorkspace.sharedWorkspace.frontmostApplication;
    BOOL observationsComplete = foreground != nil;
    BOOL front = foreground && foreground.processIdentifier == getpid();
    NSTask *sender = nil; NSDictionary *senderReport = nil; BOOL senderReaped = NO;
    BOOL queueOverflow = NO;
    NSUInteger posted = 0; NSMutableArray *queueReceipts = [NSMutableArray array];
    CGEventSourceRef source = NULL; CGEventRef down = NULL, up = NULL;
    CGPoint point = CGPointZero;
    // Ordering a new AppKit window is asynchronous. Complete application startup
    // and service only this application's queue before the first bounds check.
    [app finishLaunching];
    [app updateWindows];
    BOOL boundsObserved = NO;
    NSTimeInterval readyDeadline = NSProcessInfo.processInfo.systemUptime + 1.0;
    while (!boundsObserved && NSProcessInfo.processInfo.systemUptime < readyDeadline) {
        NSEvent *event = [app nextEventMatchingMask:NSEventMaskAny
            untilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]
            inMode:NSDefaultRunLoopMode dequeue:YES];
        dispatch_observed(app,event,panel,tag,queueReceipts,&queueOverflow);
        [app updateWindows];
        foreground = NSWorkspace.sharedWorkspace.frontmostApplication;
        observationsComplete &= foreground != nil;
        front |= foreground && foreground.processIdentifier == getpid();
        boundsObserved = own_bounds(panel,&bounds);
        if (!observationsComplete || front || panel.keyWindow) break;
    }
    BOOL humanButtonHeld = CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,kCGMouseButtonLeft);
    NSDictionary *preflight = @{@"bounds_observed":@(boundsObserved), @"bounds_empty":@(CGRectIsEmpty(bounds)),
        @"key_window":@(panel.keyWindow), @"visible":@(panel.visible), @"human_left_held":@(humanButtonHeld)};
    if (!observationsComplete || !boundsObserved || CGRectIsEmpty(bounds) || front || panel.keyWindow || humanButtonHeld) {
        error = @"target unavailable, foreground, or human mouse button held; no dispatch";
    } else {
        point = CGPointMake(CGRectGetMinX(bounds)+0.37*bounds.size.width,
                            CGRectGetMinY(bounds)+0.67*bounds.size.height);
        NSPoint local = [panel convertPointFromScreen:NSMakePoint(
            NSMinX(panel.frame)+point.x-CGRectGetMinX(bounds),
            NSMaxY(panel.frame)-(point.y-CGRectGetMinY(bounds)))];
        view.expectedGlobal = point; view.expectedLocal = local;
        NSPoint quartzLocal = NSMakePoint(point.x-bounds.origin.x,point.y-bounds.origin.y);
        source = CGEventSourceCreate(kCGEventSourceStatePrivate);
        down = make_addressed_pointer(source,kCGEventLeftMouseDown,point,view.expectedWindow,quartzLocal,getpid(),tag);
        up = make_addressed_pointer(source,kCGEventLeftMouseUp,point,view.expectedWindow,quartzLocal,getpid(),tag);
        if (!event_matches(source,down,kCGEventLeftMouseDown,point,view.expectedWindow,getpid(),tag) ||
            !event_matches(source,up,kCGEventLeftMouseUp,point,view.expectedWindow,getpid(),tag)) {
            error = @"event construction/readback refused; no dispatch";
        } else {
            // Preallocate both halves. No global post, pointer warp, activation,
            // tap, keyboard event, retry, or fallback. getpid() cannot be reused
            // while this process is executing. Posting is not a delivery receipt.
            NSPipe *senderOutput = nil; NSPipe *senderInput = nil;
            if (cross) {
                sender = [NSTask new]; NSPipe *input = [NSPipe pipe]; senderInput = input; senderOutput = [NSPipe pipe];
                sender.executableURL = [NSURL fileURLWithPath:NSProcessInfo.processInfo.arguments[0]];
                sender.arguments = @[@"--private-send-to-parent"];
                sender.standardInput = input; sender.standardOutput = senderOutput;
                sender.standardError = NSFileHandle.fileHandleWithStandardError;
                NSError *launchError = nil;
                if (![sender launchAndReturnError:&launchError]) {
                    error = @"private sender launch failed"; sender = nil;
                } else {
                    view.expectedSource = sender.processIdentifier;
                    SenderPlan plan = {0}; plan.version = 1; plan.window = view.expectedWindow;
                    plan.receiver = getpid(); plan.tag = tag; plan.point = point;
                    plan.local = quartzLocal; plan.bounds = bounds;
                    // One bounded plan plus dispatch marker; acknowledgement ends source ownership.
                    NSMutableData *message = [NSMutableData dataWithBytes:&plan length:sizeof(plan)];
                    [message appendBytes:"D" length:1];
                    @try { [input.fileHandleForWriting writeData:message]; }
                    @catch (NSException *exception) { (void)exception; error = @"sender plan write failed"; }
                    // Hold sender source state through the receiver acknowledgement.
                }
            } else {
                CGEventPostToPid(getpid(),down); ++posted;
                CGEventPostToPid(getpid(),up); ++posted;
            }
            NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + (cross ? 3.0 : 1.0);
            while (NSProcessInfo.processInfo.systemUptime < deadline) {
                NSEvent *event = [app nextEventMatchingMask:NSEventMaskAny
                    untilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]
                    inMode:NSDefaultRunLoopMode dequeue:YES];
                dispatch_observed(app,event,panel,tag,queueReceipts,&queueOverflow);
                foreground = NSWorkspace.sharedWorkspace.frontmostApplication;
                observationsComplete &= foreground != nil;
                front |= foreground && foreground.processIdentifier == getpid();
                if (cross && sender && (!sender.running || view.ups > 0)) break;
            }
            if (cross && sender) {
                @try { [senderInput.fileHandleForWriting writeData:[NSData dataWithBytes:"A" length:1]]; }
                @catch (NSException *exception) { (void)exception; error = @"sender acknowledgement failed"; }
                [senderInput.fileHandleForWriting closeFile];
                NSTimeInterval stopDeadline = NSProcessInfo.processInfo.systemUptime + 0.5;
                while (sender.running && NSProcessInfo.processInfo.systemUptime < stopDeadline)
                    [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.01]];
                if (sender.running) { [sender terminate]; error = @"sender exceeded bounded lifetime"; }
                [sender waitUntilExit]; senderReaped = !sender.running;
                NSData *data = [senderOutput.fileHandleForReading readDataOfLength:4097];
                [senderOutput.fileHandleForReading closeFile];
                id decoded = data.length <= 4096 ? [NSJSONSerialization JSONObjectWithData:data options:0 error:nil] : nil;
                if ([decoded isKindOfClass:NSDictionary.class] && [decoded[@"sender_pid"] intValue] == view.expectedSource &&
                    [decoded[@"receiver_pid"] intValue] == getpid()) {
                    senderReport = decoded; posted = [decoded[@"posted_events"] unsignedIntegerValue];
                }
                if (!senderReport || sender.terminationStatus != 0 || ![senderReport[@"ok"] boolValue])
                    error = @"private sender did not confirm posting; inspect retained receipts";
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
    if (!error && posted == 2 && (view.downs != 1 || view.ups != 1 || view.clicks != 1))
        error = @"exact canvas delivery/effect not observed";
    BOOL ok = !error && observationsComplete && posted == 2 && !front && !view.overflow && !queueOverflow &&
        view.downs == 1 && view.ups == 1 && view.clicks == 1 && closed;
    return emit(@{@"ok":@(ok),@"mode":cross ? @"cross_process_click" : @"self_process_click",
        @"posted_events":(cross && sender && !senderReport ? NSNull.null : @(posted)),
        @"plan":@{@"pid":@(getpid()),@"source_pid":@(view.expectedSource),@"window_id":@(view.expectedWindow),
            @"tag":@(tag),@"x":@(point.x),@"y":@(point.y),
            @"local_x":@(view.expectedLocal.x),@"local_y":@(view.expectedLocal.y)},
        @"queue_overflow":@(queueOverflow),@"queue_receipts":queueReceipts,@"preflight":preflight,@"receipts":view.receipts,@"tagged_click_count":@(view.clicks),
        @"unrelated_event_count":@(view.unrelated),@"receipt_overflow":@(view.overflow),
        @"target_ever_front":@(front),@"observations_complete":@(observationsComplete),@"before":before,@"after":after,@"window_closed":@(closed),
        @"error":error ?: @"",@"sender_pid":@(cross ? view.expectedSource : 0),@"sender_reaped":@(senderReaped),
        @"sender_report":senderReport ?: @{},@"posting_report_available":(!cross || senderReport != nil ? @YES : @NO),
        @"cross_process_verified":@NO},ok ? 0 : 1);
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc == 1 || (argc == 2 && strcmp(argv[1],"--self-test") == 0))
            return construction_test();
        if (argc == 2 && strcmp(argv[1],"--allow-disposable-process-click") == 0)
            return live_probe(NO);
        if (argc == 2 && strcmp(argv[1],"--allow-disposable-cross-process-click") == 0) return live_probe(YES);
        if (argc == 2 && strcmp(argv[1],"--private-send-to-parent") == 0) return parent_sender();
        fprintf(stderr,"Use --self-test (no posting), --allow-disposable-process-click, or --allow-disposable-cross-process-click. No target arguments accepted.\n");
        return 2;
    }
}
