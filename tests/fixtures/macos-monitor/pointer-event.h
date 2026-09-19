// Shared fixture-only event construction. No posting or application discovery.
#pragma once
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <math.h>
#include <stdint.h>
#include <unistd.h>
#include <dlfcn.h>

static void configure_pointer(CGEventSourceRef source, CGEventRef event, CGEventType type,
                              CGPoint point, uint32_t window, pid_t pid, int64_t tag) {
    CGEventSetSource(event,source); CGEventSetType(event,type); CGEventSetLocation(event,point);
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

typedef void (*SetWindowLocation)(CGEventRef, CGPoint);
typedef CGPoint (*GetWindowLocation)(CGEventRef);
static BOOL set_window_point(CGEventRef event, CGPoint point) {
    if (!event || !isfinite(point.x) || !isfinite(point.y)) return NO;
    SetWindowLocation set = (SetWindowLocation)dlsym(RTLD_DEFAULT, "CGEventSetWindowLocation");
    GetWindowLocation get = (GetWindowLocation)dlsym(RTLD_DEFAULT, "CGEventGetWindowLocation");
    if (!set || !get) return NO;
    set(event,point);
    return CGPointEqualToPoint(get(event),point);
}

// local is Quartz window-local TOP-LEFT, not Cocoa bottom-left.
// The seed supplies window addressing only; both coordinate fields are set below.
static CGEventRef make_addressed_pointer(CGEventSourceRef source, CGEventType type,
                                        CGPoint point, uint32_t window, NSPoint local, pid_t pid, int64_t tag) {
    if (!source || !window || pid <= 0 || tag <= 0 ||
        !isfinite(point.x) || !isfinite(point.y) || !isfinite(local.x) || !isfinite(local.y) ||
        fabs(point.x) > 1000000 || fabs(point.y) > 1000000 ||
        fabs(local.x) > 1000000 || fabs(local.y) > 1000000 ||
        (type != kCGEventLeftMouseDown && type != kCGEventLeftMouseUp)) return NULL;
    NSEvent *seed = [NSEvent mouseEventWithType:(type == kCGEventLeftMouseDown ? NSEventTypeLeftMouseDown : NSEventTypeLeftMouseUp)
        location:NSZeroPoint modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
        windowNumber:window context:nil eventNumber:0 clickCount:1
        pressure:(type == kCGEventLeftMouseDown ? 1.0 : 0.0)];
    if (!seed.CGEvent) return NULL;
    CGEventRef event = CGEventCreateCopy(seed.CGEvent);
    if (!event) return NULL;
    configure_pointer(source,event,type,point,window,pid,tag);
    // Private SPI signatures are declared in WebKit Tools/TestRunnerShared/spi/CoreGraphicsTestSPI.h.
    // Missing SPI or failed readback refuses before posting; never guess numeric fields.
    if (!set_window_point(event,NSPointToCGPoint(local)) ||
        [NSEvent eventWithCGEvent:event].windowNumber != window) {
        CFRelease(event); return NULL;
    }
    return event;
}
