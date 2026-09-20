// Fixture-only key delivery to the browser owned by browser.m. No arbitrary PID.
#pragma once
#include "key-event.h"
#include "browser-pointer.h"
#include <poll.h>
#include <sys/stat.h>
#include <string.h>
typedef struct { uint32_t version, window; CGRect bounds; int64_t tag; } BrowserKeyPlan;
_Static_assert(sizeof(BrowserKeyPlan) == 48, "key fixture wire layout");
static BOOL key_plan_valid(BrowserKeyPlan p) {
    double values[]={p.bounds.origin.x,p.bounds.origin.y,p.bounds.size.width,p.bounds.size.height};
    if(p.version != 1 || !p.window || p.tag <= 0) return NO;
    for(unsigned i=0;i<4;++i) if(!isfinite(values[i]) || fabs(values[i])>1000000) return NO;
    return p.bounds.size.width >= 200 && p.bounds.size.height >= 200 &&
        p.bounds.size.width <= 16384 && p.bounds.size.height <= 16384;
}
// The combined fixture is a distinct opt-in.  The existing key and pointer
// modes never acquire one another's dispatch capability.
static BOOL browser_click_key_mode(const char *mode) {
    return mode && strcmp(mode,"--disposable-chromium-click-key") == 0;
}
static NSDictionary *click_key_plan_evidence(BrowserPointerPlan p) {
    return @{@"window_id":@(p.window), @"tag":@(p.tag),
        @"bounds":@{@"X":@(p.bounds.origin.x), @"Y":@(p.bounds.origin.y),
            @"Width":@(p.bounds.size.width), @"Height":@(p.bounds.size.height)},
        @"screen_x":@(p.global.x), @"screen_y":@(p.global.y),
        @"local_x":@(p.local.x), @"local_y":@(p.local.y)};
}
static BOOL click_key_integer(id value, int64_t expected) {
    if (![value isKindOfClass:NSNumber.class] ||
        CFGetTypeID((__bridge CFTypeRef)value) == CFBooleanGetTypeID()) return NO;
    const char *type=[(NSNumber *)value objCType];
    if (!strchr("cislqCISLQ",type[0]) || type[1]) return NO;
    return [(NSNumber *)value longLongValue] == expected;
}
static BOOL click_key_boolean(id value, BOOL expected) {
    return value && CFGetTypeID((__bridge CFTypeRef)value) == CFBooleanGetTypeID() &&
        [(NSNumber *)value boolValue] == expected;
}
// A successful native click means exactly one address-validated down/up pair.
// A separate exact-plan DOM acknowledgement must also be consumed before a key.
static BOOL click_key_gate(BrowserPointerPlan click, BrowserKeyPlan key,
                           NSDictionary *clickResult, pid_t source, pid_t target) {
    return browser_pointer_valid(click) && key_plan_valid(key) && clickResult &&
        click.window == key.window && click.tag != key.tag &&
        CGRectEqualToRect(click.bounds,key.bounds) &&
        click_key_boolean(clickResult[@"dispatch_attempted"],YES) &&
        click_key_boolean(clickResult[@"effect_verified"],NO) &&
        click_key_integer(clickResult[@"posted_events"],2) &&
        click_key_integer(clickResult[@"source_pid"],source) &&
        click_key_integer(clickResult[@"target_pid"],target) &&
        click_key_integer(clickResult[@"window_id"],click.window) &&
        click_key_integer(clickResult[@"tag"],click.tag) && !clickResult[@"error"];
}
#include "click-key-receipt.h"
static BOOL key_plan_read(BrowserKeyPlan *p) {
    struct stat info;
    if(fstat(STDIN_FILENO,&info) != 0 || !S_ISFIFO(info.st_mode)) return NO;
    size_t have=0; NSTimeInterval end=NSProcessInfo.processInfo.systemUptime+1;
    while(have<sizeof(*p) && NSProcessInfo.processInfo.systemUptime<end) {
        struct pollfd fd={STDIN_FILENO,POLLIN,0};
        if(poll(&fd,1,20)<0) return NO;
        if(!(fd.revents&(POLLIN|POLLHUP))) continue;
        ssize_t n=read(STDIN_FILENO,(char *)p+have,sizeof(*p)-have);
        if(n<=0) return NO;
        have+=(size_t)n;
    }
    return have==sizeof(*p);
}
static BOOL key_target(NSRunningApplication *browser, BrowserKeyPlan p) {
    if(!browser || browser.terminated || browser.active || !key_plan_valid(p) ||
        ![browser.bundleIdentifier isEqualToString:@"com.google.chrome.for.testing"]) return NO;
    NSRunningApplication *front=NSWorkspace.sharedWorkspace.frontmostApplication;
    if(!front || front.processIdentifier==browser.processIdentifier ||
        CGEventSourceKeyState(kCGEventSourceStateHIDSystemState,ProbeKey) ||
        key_shortcut_held(CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState))) return NO;
    for(CGMouseButton b=0;b<5;++b) if(CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,b)) return NO;
    NSArray *rows=CFBridgingRelease(CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly,kCGNullWindowID));
    NSUInteger count=0; BOOL exact=NO;
    for(NSDictionary *row in rows) {
        if([row[(id)kCGWindowOwnerPID] intValue]!=browser.processIdentifier ||
            [row[(id)kCGWindowLayer] intValue]!=0) continue;
        ++count; CGRect bounds=CGRectZero;
        exact=[row[(id)kCGWindowNumber] unsignedIntValue]==p.window &&
            CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)row[(id)kCGWindowBounds],&bounds) &&
            CGRectEqualToRect(bounds,p.bounds);
    }
    return count==1 && exact && !browser.terminated;
}
static NSDictionary *key_send(NSRunningApplication *browser, BrowserKeyPlan p, CGEventSourceRef *held) {
    NSMutableDictionary *r=[@{@"posted_events":@0,@"dispatch_attempted":@NO,@"effect_verified":@NO,
        @"source_pid":@(getpid()),@"target_pid":@(browser?browser.processIdentifier:0),
        @"window_id":@(p.window),@"tag":@(p.tag),@"keycode":@(ProbeKey)} mutableCopy];
    CGEventSourceRef source=NULL; CGEventRef down=NULL,up=NULL; NSUInteger calls=0;
    @try {
        if(!AXIsProcessTrusted() || !CGPreflightPostEventAccess() || !key_target(browser,p)) {
            r[@"error"]=@"owned browser or permission/held-input preflight refused";
        } else {
            source=CGEventSourceCreate(kCGEventSourceStatePrivate);
            down=make_key(source,kCGEventKeyDown,p.window,browser.processIdentifier,p.tag);
            up=make_key(source,kCGEventKeyUp,p.window,browser.processIdentifier,p.tag);
            if(!matches(source,down,kCGEventKeyDown,p.window,browser.processIdentifier,p.tag) ||
                !matches(source,up,kCGEventKeyUp,p.window,browser.processIdentifier,p.tag) ||
                !key_target(browser,p)) r[@"error"]=@"exact key construction/final target refused";
            else {
                *held=source; source=NULL; // Retain through receiver shutdown, including partial calls.
                ++calls; CGEventPostToPid(browser.processIdentifier,down);
                ++calls; CGEventPostToPid(browser.processIdentifier,up);
            }
        }
    } @catch(NSException *e) { (void)e; r[@"error"]=@"native key exception; attempted calls retained"; }
    if(down) CFRelease(down); if(up) CFRelease(up); if(source) CFRelease(source);
    r[@"posted_events"]=@(calls); r[@"dispatch_attempted"]=calls?@YES:@NO;
    return r;
}

// A diagnostic deadline is not evidence that a separately launched app exited.
static BOOL key_cleanup_pending(BOOL keyMode, BOOL hasBrowser, BOOL terminated) {
    return keyMode && hasBrowser && !terminated;
}

// A refused key seals the click opportunity; never turn a key failure into a click.
static BOOL click_key_click_allowed(BOOL keyConsumed, BOOL stopping, BOOL front) {
    return !keyConsumed && !stopping && !front;
}
// Key diagnostics request normal termination once, then retain the owner if pending.
static BOOL key_shutdown_request(BOOL *requested, BOOL hasBrowser, BOOL terminated) {
    if(!requested || *requested || !hasBrowser || terminated) return NO;
    *requested=YES; return YES;
}
static BOOL key_shutdown_force_allowed(BOOL keyLifecycle) { return !keyLifecycle; }
