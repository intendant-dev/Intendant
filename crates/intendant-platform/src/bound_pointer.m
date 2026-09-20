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
