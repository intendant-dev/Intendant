// Opt-in, fixture-only input to the exact NSRunningApplication owned by browser.m.
// One fixed-size pipe message; no caller-supplied PID and no retry/fallback.
#pragma once
#include "pointer-event.h"
#include <poll.h>
#include <sys/stat.h>

typedef struct {
    uint32_t version, window;
    CGRect bounds;
    CGPoint global, local;
    int64_t tag;
} BrowserPointerPlan;
_Static_assert(sizeof(BrowserPointerPlan) == 80, "fixture protocol layout");

static BOOL browser_pointer_read(BrowserPointerPlan *plan) {
    struct stat info;
    if (fstat(STDIN_FILENO,&info) != 0 || !S_ISFIFO(info.st_mode)) return NO;
    size_t have = 0;
    NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 1;
    while (have < sizeof(*plan) && NSProcessInfo.processInfo.systemUptime < deadline) {
        struct pollfd fd = {STDIN_FILENO,POLLIN,0};
        if (poll(&fd,1,20) < 0) return NO;
        if (!(fd.revents & (POLLIN|POLLHUP))) continue;
        ssize_t n = read(STDIN_FILENO,(char *)plan + have,sizeof(*plan)-have);
        if (n <= 0) return NO;
        have += (size_t)n;
    }
    return have == sizeof(*plan);
}

static BOOL browser_pointer_valid(BrowserPointerPlan p) {
    if (p.version != 1 || !p.window || p.tag <= 0) return NO;
    double values[] = {p.bounds.origin.x,p.bounds.origin.y,p.bounds.size.width,
        p.bounds.size.height,p.global.x,p.global.y,p.local.x,p.local.y};
    for (unsigned i=0;i<8;i++) if (!isfinite(values[i]) || fabs(values[i])>1000000) return NO;
    return p.bounds.size.width >= 200 && p.bounds.size.width <= 16384 &&
        p.bounds.size.height >= 200 && p.bounds.size.height <= 16384 &&
        CGRectContainsPoint(CGRectInset(p.bounds,24,24),p.global) &&
        p.local.x == p.global.x - p.bounds.origin.x &&
        p.local.y == p.global.y - CGRectGetMinY(p.bounds);
}

static BOOL browser_pointer_target(NSRunningApplication *browser, BrowserPointerPlan p) {
    if (!browser || browser.terminated || browser.active || !browser_pointer_valid(p) ||
        ![browser.bundleIdentifier isEqualToString:@"com.google.chrome.for.testing"]) return NO;
    NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
    if (!front || front.processIdentifier == browser.processIdentifier ||
        CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,kCGMouseButtonLeft)) return NO;
    NSArray *all = CFBridgingRelease(CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly,kCGNullWindowID));
    NSUInteger count=0; BOOL exact=NO;
    for (NSDictionary *row in all) {
        if ([row[(id)kCGWindowOwnerPID] intValue] != browser.processIdentifier ||
            [row[(id)kCGWindowLayer] intValue] != 0) continue;
        count++;
        CGRect bounds=CGRectZero;
        exact = [row[(id)kCGWindowNumber] unsignedIntValue] == p.window &&
            CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)row[(id)kCGWindowBounds],&bounds) &&
            CGRectEqualToRect(bounds,p.bounds);
    }
    return count == 1 && exact && !browser.terminated;
}

static NSDictionary *browser_pointer_send(NSRunningApplication *browser, BrowserPointerPlan p,
                                           CGEventSourceRef *retainedSource) {
    NSMutableDictionary *result=[@{@"posted_events":@0,@"dispatch_attempted":@NO,
        @"effect_verified":@NO,@"tag":@(p.tag),@"source_pid":@(getpid()),
        @"window_id":@(p.window),@"target_pid":@(browser ? browser.processIdentifier : 0)} mutableCopy];
    if (!AXIsProcessTrusted() || !CGPreflightPostEventAccess() ||
        !browser_pointer_target(browser,p)) {
        result[@"error"]=@"owned browser, geometry, permission or human-button preflight refused";
        return result;
    }
    CGEventSourceRef source=CGEventSourceCreate(kCGEventSourceStatePrivate);
    CGEventRef down=make_addressed_pointer(source,kCGEventLeftMouseDown,p.global,p.window,NSPointFromCGPoint(p.local),browser.processIdentifier,p.tag);
    CGEventRef up=make_addressed_pointer(source,kCGEventLeftMouseUp,p.global,p.window,NSPointFromCGPoint(p.local),browser.processIdentifier,p.tag);
    BOOL ready=event_matches(source,down,kCGEventLeftMouseDown,p.global,p.window,browser.processIdentifier,p.tag) &&
        event_matches(source,up,kCGEventLeftMouseUp,p.global,p.window,browser.processIdentifier,p.tag) &&
        browser_pointer_target(browser,p);
    if (ready) {
        // Both halves prepared before dispatch; keep source alive until supervisor teardown.
        // No dispatch loop, mouse warp, application activation or global event posting.
        *retainedSource=source; source=NULL;
        result[@"dispatch_attempted"]=@YES;
        CGEventPostToPid(browser.processIdentifier,down);
        CGEventPostToPid(browser.processIdentifier,up);
        result[@"posted_events"]=@2;
    } else result[@"error"]=@"event construction/readback or final target validation refused";
    if (down) CFRelease(down);
    if (up) CFRelease(up);
    if (source) CFRelease(source);
    return result;
}
