// Fixture-only exact ArrowRight event construction; no posting.
#pragma once
static const CGKeyCode ProbeKey = 124; // SDK Events.h: kVK_RightArrow
static NSString *probe_text(void) {
    unichar c = NSRightArrowFunctionKey;
    return [NSString stringWithCharacters:&c length:1];
}
static CGEventRef make_key(CGEventSourceRef source, CGEventType type,
                          uint32_t window, pid_t pid, int64_t tag) {
    if (!source || !window || pid <= 0 || tag <= 0 ||
        (type != kCGEventKeyDown && type != kCGEventKeyUp)) return NULL;
    NSEvent *seed = [NSEvent keyEventWithType:type == kCGEventKeyDown ? NSEventTypeKeyDown : NSEventTypeKeyUp
        location:NSZeroPoint modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
        windowNumber:window context:nil characters:probe_text()
        charactersIgnoringModifiers:probe_text() isARepeat:NO keyCode:ProbeKey];
    if (!seed.CGEvent) return NULL;
    CGEventRef event = CGEventCreateCopy(seed.CGEvent);
    if (!event) return NULL;
    CGEventSetSource(event,source);
    CGEventSetType(event,type);
    CGEventSetFlags(event,0);
    CGEventSetIntegerValueField(event,kCGKeyboardEventKeycode,ProbeKey);
    CGEventSetIntegerValueField(event,kCGKeyboardEventAutorepeat,0);
    CGEventSetIntegerValueField(event,kCGEventTargetUnixProcessID,pid);
    CGEventSetIntegerValueField(event,kCGEventSourceUserData,tag);
    UniChar c = NSRightArrowFunctionKey;
    CGEventKeyboardSetUnicodeString(event,1,&c);
    return event;
}
static BOOL matches(CGEventSourceRef source, CGEventRef event, CGEventType type,
                    uint32_t window, pid_t pid, int64_t tag) {
    if (!source || !event) return NO;
    CGEventSourceStateID state = CGEventSourceGetSourceStateID(source);
    NSEvent *decoded = [NSEvent eventWithCGEvent:event];
    UniChar chars[2] = {0}; UniCharCount length = 0;
    CGEventKeyboardGetUnicodeString(event,2,&length,chars);
    return decoded && decoded.windowNumber == window && decoded.keyCode == ProbeKey &&
        decoded.type == (type == kCGEventKeyDown ? NSEventTypeKeyDown : NSEventTypeKeyUp) &&
        decoded.modifierFlags == NSEventModifierFlagFunction && !decoded.isARepeat &&
        [decoded.characters isEqualToString:probe_text()] &&
        CGEventGetType(event) == type && CGEventGetFlags(event) == 0 &&
        CGEventGetIntegerValueField(event,kCGKeyboardEventKeycode) == ProbeKey &&
        CGEventGetIntegerValueField(event,kCGKeyboardEventAutorepeat) == 0 &&
        CGEventGetIntegerValueField(event,kCGEventTargetUnixProcessID) == pid &&
        CGEventGetIntegerValueField(event,kCGEventSourceUnixProcessID) == getpid() &&
        CGEventGetIntegerValueField(event,kCGEventSourceUserData) == tag &&
        state != kCGEventSourceStateHIDSystemState && state != kCGEventSourceStateCombinedSessionState &&
        CGEventGetIntegerValueField(event,kCGEventSourceStateID) == state &&
        length == 1 && chars[0] == NSRightArrowFunctionKey;
}

// Caps Lock and event-classification bits are not shortcut modifiers for this
// fixed nontext ArrowRight probe. User keyboard state is never changed.
static BOOL key_shortcut_held(CGEventFlags flags) {
    return (flags & (kCGEventFlagMaskShift|kCGEventFlagMaskControl|
        kCGEventFlagMaskAlternate|kCGEventFlagMaskCommand|kCGEventFlagMaskSecondaryFn)) != 0;
}
