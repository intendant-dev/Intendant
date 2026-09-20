// Main-thread event construction/exception boundary. No target discovery, global
// posting, keyboard, activation, cursor warp, clipboard or permission requests.
// The controller owns retained-window identity, monitor, authorization and tokens.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ApplicationServices/ApplicationServices.h>
#include <dlfcn.h>
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>

typedef void (*SetWindowLocation)(CGEventRef, CGPoint);
typedef CGPoint (*GetWindowLocation)(CGEventRef);
typedef struct {
    CGEventSourceRef source;
    CGEventRef down, up;
    pid_t pid;
    uint8_t used;
} IntendantPointerPair;

uint8_t intendant_pointer_ready(int32_t pid) {
    @autoreleasepool {
        @try {
            if (![NSThread isMainThread] || pid <= 0 || !AXIsProcessTrusted() || !CGPreflightPostEventAccess()) return 0;
            NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
            if (!front || front.processIdentifier == pid) return 0;
            for (CGMouseButton b = 0; b < 5; ++b)
                if (CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,b)) return 0;
            return 1;
        } @catch (NSException *e) { (void)e; return 0; }
    }
}
static CGEventRef construct(CGEventSourceRef source, CGEventType type,
    CGPoint global, CGPoint local, uint32_t window, pid_t pid, int64_t tag,
    SetWindowLocation set, GetWindowLocation get) {
    NSEvent *seed = [NSEvent mouseEventWithType:(type == kCGEventLeftMouseDown ? NSEventTypeLeftMouseDown : NSEventTypeLeftMouseUp)
        location:NSZeroPoint modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
        windowNumber:window context:nil eventNumber:0 clickCount:1 pressure:(type == kCGEventLeftMouseDown ? 1.0 : 0.0)];
    if (!seed.CGEvent) return NULL;
    CGEventRef event = CGEventCreateCopy(seed.CGEvent);
    if (!event) return NULL;
    @try {
        CGEventSetSource(event,source); CGEventSetType(event,type); CGEventSetLocation(event,global);
        CGEventSetFlags(event,0);
        CGEventSetIntegerValueField(event,kCGMouseEventClickState,1);
        CGEventSetIntegerValueField(event,kCGMouseEventButtonNumber,0);
        CGEventSetIntegerValueField(event,kCGMouseEventDeltaX,0);
        CGEventSetIntegerValueField(event,kCGMouseEventDeltaY,0);
        CGEventSetIntegerValueField(event,kCGMouseEventWindowUnderMousePointer,window);
        CGEventSetIntegerValueField(event,kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent,window);
        CGEventSetIntegerValueField(event,kCGEventTargetUnixProcessID,pid);
        CGEventSetIntegerValueField(event,kCGEventSourceUserData,tag);
        CGEventSetDoubleValueField(event,kCGMouseEventPressure,type == kCGEventLeftMouseDown ? 1.0 : 0.0);
        // Top-left window-local Quartz coordinates, confirmed with asymmetric
        // AppKit/Chromium receivers. Missing SPI never falls back to global input.
        set(event,local);
        CGEventSourceStateID state = CGEventSourceGetSourceStateID(source);
        BOOL exact = CGPointEqualToPoint(get(event),local) && CGPointEqualToPoint(CGEventGetLocation(event),global)
            && [NSEvent eventWithCGEvent:event].windowNumber == window
            && CGEventGetType(event) == type && CGEventGetFlags(event) == 0
            && state != kCGEventSourceStateHIDSystemState && state != kCGEventSourceStateCombinedSessionState
            && CGEventGetIntegerValueField(event,kCGEventSourceStateID) == state
            && CGEventGetIntegerValueField(event,kCGEventSourceUnixProcessID) == getpid()
            && CGEventGetIntegerValueField(event,kCGEventTargetUnixProcessID) == pid
            && CGEventGetIntegerValueField(event,kCGEventSourceUserData) == tag
            && CGEventGetIntegerValueField(event,kCGMouseEventWindowUnderMousePointer) == window
            && CGEventGetIntegerValueField(event,kCGMouseEventWindowUnderMousePointerThatCanHandleThisEvent) == window
            && CGEventGetIntegerValueField(event,kCGMouseEventClickState) == 1
            && CGEventGetIntegerValueField(event,kCGMouseEventButtonNumber) == 0
            && CGEventGetIntegerValueField(event,kCGMouseEventDeltaX) == 0
            && CGEventGetIntegerValueField(event,kCGMouseEventDeltaY) == 0
            && CGEventGetDoubleValueField(event,kCGMouseEventPressure) == (type == kCGEventLeftMouseDown ? 1.0 : 0.0);
        if (exact) return event;
    } @catch (NSException *e) { (void)e; }
    CFRelease(event); return NULL;
}
void intendant_pointer_release(void *raw) {
    IntendantPointerPair *pair = raw;
    if (!pair) return;
    if (pair->down) CFRelease(pair->down);
    if (pair->up) CFRelease(pair->up);
    if (pair->source) CFRelease(pair->source);
    free(pair);
}
void *intendant_pointer_create(int32_t pid, uint32_t window, double x, double y, double lx, double ly) {
    @autoreleasepool {
        if (![NSThread isMainThread] || pid <= 0 || !window || !isfinite(x) || !isfinite(y)
            || !isfinite(lx) || !isfinite(ly) || fabs(x) > 1000000 || fabs(y) > 1000000
            || lx < 0 || ly < 0 || lx >= 16384 || ly >= 16384) return NULL;
        // These signatures are declared by WebKit's CoreGraphicsTestSPI.h.
        SetWindowLocation set = (SetWindowLocation)dlsym(RTLD_DEFAULT,"CGEventSetWindowLocation");
        GetWindowLocation get = (GetWindowLocation)dlsym(RTLD_DEFAULT,"CGEventGetWindowLocation");
        if (!set || !get) return NULL;
        IntendantPointerPair *pair = calloc(1,sizeof(*pair));
        if (!pair) return NULL;
        @try {
            pair->pid = pid;
            pair->source = CGEventSourceCreate(kCGEventSourceStatePrivate);
            int64_t tag = 0;
            do { arc4random_buf(&tag,sizeof(tag)); tag &= INT64_MAX; } while (!tag);
            if (pair->source) {
                pair->down = construct(pair->source,kCGEventLeftMouseDown,CGPointMake(x,y),CGPointMake(lx,ly),window,pid,tag,set,get);
                pair->up = construct(pair->source,kCGEventLeftMouseUp,CGPointMake(x,y),CGPointMake(lx,ly),window,pid,tag,set,get);
                if (pair->down && pair->up) return pair;
            }
        } @catch (NSException *e) { (void)e; }
        intendant_pointer_release(pair); return NULL;
    }
}
// The posting loop is shared with the native exception regression via inclusion
// of this source and an injected nonposting callback. Production always supplies
// CGEventPostToPid; no callback or test switch is exported on the runtime surface.
static uint8_t execute_pointer_pair(IntendantPointerPair *pair, uint8_t *failed,
                                   void (*post)(pid_t, CGEventRef)) {
    if (!failed) return 0;
    *failed = 1;
    if (!pair || pair->used || !post) return 0;
    pair->used = 1;
    uint8_t calls = 0;
    @try {
        // Counts record attempts, not completion. A failure on the second call
        // MUST remain distinguishable from a successful two-call posting pair.
        ++calls; post(pair->pid,pair->down);
        ++calls; post(pair->pid,pair->up);
        *failed = 0;
    } @catch (NSException *e) { (void)e; *failed = 1; }
    return calls;
}
uint8_t intendant_pointer_post(void *raw, uint8_t *failed) {
    if (!failed) return 0;
    *failed = 1;
    IntendantPointerPair *pair = raw;
    if (!pair || pair->used || !intendant_pointer_ready(pair->pid)) return 0;
    return execute_pointer_pair(pair,failed,CGEventPostToPid);
}

// Narrow vertical-scroll extension. The addressed NSEvent seed owns window
// routing; the pixel wheel is ONLY a field template. Never copy its position.
// Nonposting and HTTP receiver acceptance are recorded in the scroll design.
typedef struct {
    CGEventSourceRef source;
    CGEventRef event;
    pid_t pid;
    uint8_t used;
} IntendantScroll;

static CGEventRef construct_scroll(CGEventSourceRef source, CGPoint global,
    CGPoint local, uint32_t window, pid_t pid, int32_t delta_y, int64_t tag,
    SetWindowLocation set, GetWindowLocation get) {
    CGEventRef wheel = NULL, event = NULL;
    @try {
        // Public convention is positive down; Quartz pixel wheel uses the
        // opposite sign. The caller bounded delta_y before this negation.
        wheel = CGEventCreateScrollWheelEvent(source,kCGScrollEventUnitPixel,1,-delta_y);
        NSEvent *seed = [NSEvent mouseEventWithType:NSEventTypeMouseMoved
            location:NSZeroPoint modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
            windowNumber:window context:nil eventNumber:0 clickCount:0 pressure:0];
        if (wheel && seed.CGEvent) event = CGEventCreateCopy(seed.CGEvent);
        if (event) {
            CGEventSetSource(event,source);
            CGEventSetType(event,kCGEventScrollWheel);
            const CGEventField integers[] = {
                kCGScrollWheelEventDeltaAxis1,kCGScrollWheelEventDeltaAxis2,kCGScrollWheelEventDeltaAxis3,
                kCGScrollWheelEventPointDeltaAxis1,kCGScrollWheelEventPointDeltaAxis2,kCGScrollWheelEventPointDeltaAxis3,
                kCGScrollWheelEventIsContinuous
            };
            const CGEventField fixed[] = {
                kCGScrollWheelEventFixedPtDeltaAxis1,kCGScrollWheelEventFixedPtDeltaAxis2,kCGScrollWheelEventFixedPtDeltaAxis3
            };
            for (size_t i=0;i<sizeof(integers)/sizeof(integers[0]);++i)
                CGEventSetIntegerValueField(event,integers[i],CGEventGetIntegerValueField(wheel,integers[i]));
            for (size_t i=0;i<sizeof(fixed)/sizeof(fixed[0]);++i)
                CGEventSetDoubleValueField(event,fixed[i],CGEventGetDoubleValueField(wheel,fixed[i]));
            CGEventSetFlags(event,0);
            CGEventSetIntegerValueField(event,kCGScrollWheelEventDeltaAxis2,0);
            CGEventSetIntegerValueField(event,kCGScrollWheelEventDeltaAxis3,0);
            CGEventSetIntegerValueField(event,kCGScrollWheelEventPointDeltaAxis2,0);
            CGEventSetIntegerValueField(event,kCGScrollWheelEventPointDeltaAxis3,0);
            CGEventSetDoubleValueField(event,kCGScrollWheelEventFixedPtDeltaAxis2,0);
            CGEventSetDoubleValueField(event,kCGScrollWheelEventFixedPtDeltaAxis3,0);
            CGEventSetIntegerValueField(event,kCGScrollWheelEventScrollPhase,0);
            CGEventSetIntegerValueField(event,kCGScrollWheelEventMomentumPhase,0);
            CGEventSetIntegerValueField(event,kCGEventTargetUnixProcessID,pid);
            CGEventSetIntegerValueField(event,kCGEventSourceUserData,tag);
            CGEventSetLocation(event,global);
            set(event,local);
            NSEvent *actual = [NSEvent eventWithCGEvent:event];
            NSEvent *template = [NSEvent eventWithCGEvent:wheel];
            BOOL exact = actual && template && actual.type == NSEventTypeScrollWheel
                // Wheel events do not expose the mouse-only under-pointer fields.
                // Retain and verify the actual addressed NSEvent window, not those hints.
                && actual.windowNumber == window && actual.modifierFlags == 0
                && actual.phase == NSEventPhaseNone && actual.momentumPhase == NSEventPhaseNone
                && actual.hasPreciseScrollingDeltas && actual.scrollingDeltaX == 0 && actual.deltaX == 0 && actual.deltaZ == 0
                && actual.scrollingDeltaY == template.scrollingDeltaY && actual.deltaY == template.deltaY
                && CGPointEqualToPoint(CGEventGetLocation(event),global) && CGPointEqualToPoint(get(event),local)
                && CGEventGetType(event) == kCGEventScrollWheel && CGEventGetFlags(event) == 0
                && CGEventSourceGetSourceStateID(source) != kCGEventSourceStateHIDSystemState
                && CGEventSourceGetSourceStateID(source) != kCGEventSourceStateCombinedSessionState
                && CGEventGetIntegerValueField(event,kCGEventSourceStateID) == CGEventSourceGetSourceStateID(source)
                && CGEventGetIntegerValueField(event,kCGEventSourceUnixProcessID) == getpid()
                && CGEventGetIntegerValueField(event,kCGEventTargetUnixProcessID) == pid
                && CGEventGetIntegerValueField(event,kCGEventSourceUserData) == tag
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventPointDeltaAxis1) == -delta_y
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventIsContinuous) == 1
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventDeltaAxis2) == 0
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventDeltaAxis3) == 0
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventPointDeltaAxis2) == 0
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventPointDeltaAxis3) == 0
                && CGEventGetDoubleValueField(event,kCGScrollWheelEventFixedPtDeltaAxis2) == 0
                && CGEventGetDoubleValueField(event,kCGScrollWheelEventFixedPtDeltaAxis3) == 0
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventScrollPhase) == 0
                && CGEventGetIntegerValueField(event,kCGScrollWheelEventMomentumPhase) == 0;
            for (size_t i=0;i<sizeof(integers)/sizeof(integers[0]);++i)
                exact &= CGEventGetIntegerValueField(event,integers[i]) == CGEventGetIntegerValueField(wheel,integers[i]);
            for (size_t i=0;i<sizeof(fixed)/sizeof(fixed[0]);++i)
                exact &= CGEventGetDoubleValueField(event,fixed[i]) == CGEventGetDoubleValueField(wheel,fixed[i]);
            if (exact) { CFRelease(wheel); return event; }
        }
    } @catch (NSException *e) { (void)e; }
    if (event) CFRelease(event);
    if (wheel) CFRelease(wheel);
    return NULL;
}
void intendant_scroll_release(void *raw) {
    IntendantScroll *scroll = raw;
    if (!scroll) return;
    if (scroll->event) CFRelease(scroll->event);
    if (scroll->source) CFRelease(scroll->source);
    free(scroll);
}
void *intendant_scroll_create(int32_t pid, uint32_t window, double x, double y,
                              double lx, double ly, int32_t delta_y) {
    @autoreleasepool {
        if (![NSThread isMainThread] || pid <= 0 || !window || !isfinite(x) || !isfinite(y)
            || !isfinite(lx) || !isfinite(ly) || fabs(x) > 1000000 || fabs(y) > 1000000
            || lx < 0 || ly < 0 || lx >= 16384 || ly >= 16384
            || delta_y == 0 || delta_y < -600 || delta_y > 600) return NULL;
        SetWindowLocation set = (SetWindowLocation)dlsym(RTLD_DEFAULT,"CGEventSetWindowLocation");
        GetWindowLocation get = (GetWindowLocation)dlsym(RTLD_DEFAULT,"CGEventGetWindowLocation");
        if (!set || !get) return NULL;
        IntendantScroll *scroll = calloc(1,sizeof(*scroll));
        if (!scroll) return NULL;
        @try {
            scroll->pid = pid;
            scroll->source = CGEventSourceCreate(kCGEventSourceStatePrivate);
            int64_t tag = 0;
            do { arc4random_buf(&tag,sizeof(tag)); tag &= INT64_MAX; } while (!tag);
            if (scroll->source) {
                scroll->event = construct_scroll(scroll->source,CGPointMake(x,y),CGPointMake(lx,ly),window,pid,delta_y,tag,set,get);
                if (scroll->event) return scroll;
            }
        } @catch (NSException *e) { (void)e; }
        intendant_scroll_release(scroll); return NULL;
    }
}
// Included directly by a hermetic native regression with a nonposting callback.
// No callback injection or test switch is exported to the controller.
static uint8_t execute_scroll(IntendantScroll *scroll, uint8_t *failed,
                              void (*post)(pid_t, CGEventRef)) {
    if (!failed) return 0;
    *failed = 1;
    if (!scroll || scroll->used || !post) return 0;
    scroll->used = 1;
    uint8_t calls = 0;
    @try {
        ++calls; post(scroll->pid,scroll->event);
        *failed = 0;
    } @catch (NSException *e) { (void)e; *failed = 1; }
    return calls;
}
uint8_t intendant_scroll_post(void *raw, uint8_t *failed) {
    if (!failed) return 0;
    *failed = 1;
    IntendantScroll *scroll = raw;
    if (!scroll || scroll->used) return 0;
    if (!intendant_pointer_ready(scroll->pid)) { scroll->used = 1; return 0; }
    return execute_scroll(scroll,failed,CGEventPostToPid);
}
