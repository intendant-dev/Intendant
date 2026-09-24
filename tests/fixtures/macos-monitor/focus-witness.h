#pragma once
// Fixture-only native focus observations. No input, activation or content reads.
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>

@interface FocusSample : NSObject
@property AXError status;
@property AXError pidStatus;
@property BOOL present;
@property BOOL valid;
@property pid_t pid;
@property(strong) id element;
@property(strong) NSDate *birth;
- (NSDictionary *)evidence;
@end
@implementation FocusSample
- (NSDictionary *)evidence {
    return @{@"status":@(_status),@"pid_status":@(_pidStatus),
             @"value_present":@(_present),@"valid":@(_valid)};
}
@end

// Equality compares retained native objects, never titles/values/coordinates.
// Missing observations deliberately yield JSON null, never unchanged=false.
static id focus_changed(FocusSample *a, FocusSample *b) {
    if (!a.valid || !b.valid || !a.element || !b.element || !a.birth || !b.birth)
        return NSNull.null;
    return (a.pid != b.pid || ![a.birth isEqual:b.birth] ||
        !CFEqual((__bridge CFTypeRef)a.element, (__bridge CFTypeRef)b.element)) ? @YES : @NO;
}

static FocusSample *focus_sample(NSRunningApplication *owner, BOOL system) {
    FocusSample *result = [FocusSample new];
    result.status = kAXErrorFailure; result.pidStatus = kAXErrorFailure;
    if (!owner || owner.terminated || owner.processIdentifier <= 0 || !owner.launchDate) return result;
    if (!AXIsProcessTrusted()) { result.status = kAXErrorAPIDisabled; return result; }
    AXUIElementRef app = system ? AXUIElementCreateSystemWide() : AXUIElementCreateApplication(owner.processIdentifier);
    if (!app) return result;
    result.status = AXUIElementSetMessagingTimeout(app, 0.05);
    if (result.status != kAXErrorSuccess) { CFRelease(app); return result; }
    CFTypeRef value = NULL;
    result.status = AXUIElementCopyAttributeValue(app, kAXFocusedUIElementAttribute, &value);
    CFRelease(app); result.present = value != NULL;
    // Own every non-null Copy-rule result, including error-plus-value replies.
    id object = value ? CFBridgingRelease(value) : nil;
    if (result.status != kAXErrorSuccess || !object ||
        CFGetTypeID((__bridge CFTypeRef)object) != AXUIElementGetTypeID()) return result;
    AXUIElementRef element = (__bridge AXUIElementRef)object;
    pid_t pid = 0; result.pidStatus = AXUIElementGetPid(element, &pid); result.pid = pid;
    if (result.pidStatus != kAXErrorSuccess || result.pid != owner.processIdentifier || owner.terminated) return result;
    result.element = object; result.birth = owner.launchDate; result.valid = YES;
    return result;
}

@interface FocusWitness : NSObject
@property NSUInteger sequence;
@property BOOL active;
@property NSTimeInterval started;
@property(strong) FocusSample *human;
@property(strong) FocusSample *receiver;
@property(strong) NSRunningApplication *front;
@property(strong) NSRunningApplication *target;
@property(strong) NSDate *frontBirth;
@property(strong) NSDictionary *receipt;
- (void)begin:(NSRunningApplication *)browser;
- (void)finish:(NSRunningApplication *)browser;
- (void)cancel;
@end
@implementation FocusWitness
- (void)begin:(NSRunningApplication *)browser {
    if (_active || _sequence >= 16 || !browser || browser.terminated) {
        _receipt = @{@"sequence":@(_sequence),@"phase":@"refused",@"error":@"witness lifecycle"}; [self cancel]; return;
    }
    ++_sequence; _active = YES; _target = browser; _started = NSProcessInfo.processInfo.systemUptime;
    _front = NSWorkspace.sharedWorkspace.frontmostApplication; _frontBirth = _front.launchDate;
    _human = focus_sample(_front, YES); _receiver = focus_sample(browser, NO);
    _receipt = @{@"sequence":@(_sequence),@"phase":@"before",
        @"human":_human.evidence,@"receiver":_receiver.evidence};
}
- (void)finish:(NSRunningApplication *)browser {
    if (!_active || !browser || browser != _target || browser.terminated) {
        _receipt = @{@"sequence":@(_sequence),@"phase":@"refused",@"error":@"witness lifecycle"}; [self cancel]; return;
    }
    NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
    FocusSample *human = focus_sample(front, YES), *receiver = focus_sample(browser, NO);
    NSTimeInterval elapsed = NSProcessInfo.processInfo.systemUptime - _started;
    BOOL complete = elapsed >= 0 && elapsed <= 30;
    id foregroundChanged = _front && front && _frontBirth && front.launchDate
        ? ((_front.processIdentifier != front.processIdentifier || ![_frontBirth isEqual:front.launchDate]) ? @YES : @NO) : NSNull.null;
    _receipt = @{@"sequence":@(_sequence),@"phase":@"after",@"complete":@(complete),
        @"human_before":_human.evidence,@"human_after":human.evidence,
        @"receiver_before":_receiver.evidence,@"receiver_after":receiver.evidence,
        @"human_changed":complete ? focus_changed(_human,human) : NSNull.null,
        @"receiver_changed":complete ? focus_changed(_receiver,receiver) : NSNull.null,
        @"foreground_changed":foregroundChanged,
        @"target_foreground_observed":((_front && _front.processIdentifier == browser.processIdentifier) ||
            (front && front.processIdentifier == browser.processIdentifier)) ? @YES : @NO,
        @"elapsed_us":@((uint64_t)(MAX(0,elapsed)*1000000)),
        @"continuous_isolation_verified":@NO};
    [self cancel];
}
- (void)cancel { _active=NO; _human=nil; _receiver=nil; _front=nil; _frontBirth=nil; _target=nil; }
@end
