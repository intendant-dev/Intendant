// Fixed unmodified horizontal arrows. Main-thread ARC/exception island.
// No target discovery, focus changes, text/chords, clicks or global input.
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Carbon/Carbon.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>
static CGKeyCode arrow_code(uint8_t left) { return left ? kVK_LeftArrow : kVK_RightArrow; }
static unichar arrow_character(uint8_t left) { return left ? NSLeftArrowFunctionKey : NSRightArrowFunctionKey; }
static NSString *arrow_text(uint8_t left) {
    unichar c = arrow_character(left);
    return [NSString stringWithCharacters:&c length:1];
}
static CGEventRef make_key(CGEventSourceRef source, CGEventType type,
                          uint32_t window, pid_t pid, int64_t tag, uint8_t left) {
    if (left > 1 || !source || !window || pid <= 0 || tag <= 0 ||
        (type != kCGEventKeyDown && type != kCGEventKeyUp)) return NULL;
    NSEvent *seed = [NSEvent keyEventWithType:type == kCGEventKeyDown ? NSEventTypeKeyDown : NSEventTypeKeyUp
        location:NSZeroPoint modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
        windowNumber:window context:nil characters:arrow_text(left)
        charactersIgnoringModifiers:arrow_text(left) isARepeat:NO keyCode:arrow_code(left)];
    if (!seed.CGEvent) return NULL;
    CGEventRef event = CGEventCreateCopy(seed.CGEvent);
    if (!event) return NULL;
    @try {
    CGEventSetSource(event,source);
    CGEventSetType(event,type);
    CGEventSetFlags(event,0);
    CGEventSetIntegerValueField(event,kCGKeyboardEventKeycode,arrow_code(left));
    CGEventSetIntegerValueField(event,kCGKeyboardEventAutorepeat,0);
    CGEventSetIntegerValueField(event,kCGEventTargetUnixProcessID,pid);
    CGEventSetIntegerValueField(event,kCGEventSourceUserData,tag);
    UniChar c = arrow_character(left);
    CGEventKeyboardSetUnicodeString(event,1,&c);
    } @catch(NSException *e) { (void)e; CFRelease(event); return NULL; }
    return event;
}
static BOOL matches(CGEventSourceRef source, CGEventRef event, CGEventType type,
                    uint32_t window, pid_t pid, int64_t tag, uint8_t left) {
    if (left > 1 || !source || !event) return NO;
    CGEventSourceStateID state = CGEventSourceGetSourceStateID(source);
    NSEvent *decoded = [NSEvent eventWithCGEvent:event];
    UniChar chars[2] = {0}; UniCharCount length = 0;
    CGEventKeyboardGetUnicodeString(event,2,&length,chars);
    return decoded && decoded.windowNumber == window && decoded.keyCode == arrow_code(left) &&
        decoded.type == (type == kCGEventKeyDown ? NSEventTypeKeyDown : NSEventTypeKeyUp) &&
        decoded.modifierFlags == NSEventModifierFlagFunction && !decoded.isARepeat &&
        [decoded.characters isEqualToString:arrow_text(left)] &&
        CGEventGetType(event) == type && CGEventGetFlags(event) == 0 &&
        CGEventGetIntegerValueField(event,kCGKeyboardEventKeycode) == arrow_code(left) &&
        CGEventGetIntegerValueField(event,kCGKeyboardEventAutorepeat) == 0 &&
        CGEventGetIntegerValueField(event,kCGEventTargetUnixProcessID) == pid &&
        CGEventGetIntegerValueField(event,kCGEventSourceUnixProcessID) == getpid() &&
        CGEventGetIntegerValueField(event,kCGEventSourceUserData) == tag &&
        state != kCGEventSourceStateHIDSystemState && state != kCGEventSourceStateCombinedSessionState &&
        CGEventGetIntegerValueField(event,kCGEventSourceStateID) == state &&
        length == 1 && chars[0] == arrow_character(left);
}

// Caps Lock and event-classification bits are not shortcut modifiers for this
// fixed nontext horizontal-arrow probe. User keyboard state is never changed.
static BOOL key_shortcut_held(CGEventFlags flags) {
    return (flags & (kCGEventFlagMaskShift|kCGEventFlagMaskControl|
        kCGEventFlagMaskAlternate|kCGEventFlagMaskCommand|kCGEventFlagMaskSecondaryFn)) != 0;
}

typedef struct { CGEventSourceRef source; CGEventRef down,up; pid_t pid; uint8_t used; } ArrowPair;
uint8_t intendant_arrow_ready(int32_t pid) {
    @autoreleasepool { @try {
        if (![NSThread isMainThread] || pid<=0 || !AXIsProcessTrusted() || !CGPreflightPostEventAccess()) return 0;
        NSRunningApplication *front=NSWorkspace.sharedWorkspace.frontmostApplication;
        if (!front || front.terminated || front.processIdentifier==pid) return 0;
        if (key_shortcut_held(CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState))) return 0;
        for (CGMouseButton b=0;b<5;b++) if(CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,b)) return 0;
        for (CGKeyCode k=0;k<128;k++) {
            // Caps Lock is a latched nontext state, not a held shortcut.
            if(k!=57 && CGEventSourceKeyState(kCGEventSourceStateHIDSystemState,k)) return 0;
        }
        return 1;
    } @catch(NSException *e) { (void)e; return 0; } }
}
void intendant_arrow_release(void *raw) {
    ArrowPair *p=raw; if(!p) return;
    if(p->down) CFRelease(p->down); if(p->up) CFRelease(p->up);
    if(p->source) CFRelease(p->source); free(p);
}
void *intendant_arrow_create(int32_t pid,uint32_t window,uint8_t left) {
    @autoreleasepool {
        if(left > 1 || ![NSThread isMainThread] || pid<=0 || !window) return NULL;
        ArrowPair *p=calloc(1,sizeof(*p)); if(!p) return NULL;
        @try {
            p->pid=pid; p->source=CGEventSourceCreate(kCGEventSourceStatePrivate);
            int64_t tag=0; do { arc4random_buf(&tag,sizeof(tag)); tag &= INT64_MAX; } while(!tag);
            p->down=make_key(p->source,kCGEventKeyDown,window,pid,tag,left);
            p->up=make_key(p->source,kCGEventKeyUp,window,pid,tag,left);
            if(matches(p->source,p->down,kCGEventKeyDown,window,pid,tag,left) &&
               matches(p->source,p->up,kCGEventKeyUp,window,pid,tag,left)) return p;
        } @catch(NSException *e) { (void)e; }
        intendant_arrow_release(p); return NULL;
    }
}
// Regression tests include this exact loop with a nonposting callback. There
// is no runtime test switch, allocation, await, cancellation or retargeting
// between the two native posting calls. Attempts and failures remain separate.
static uint8_t execute_arrow_pair(ArrowPair *p,uint8_t *failed,BOOL ready,void (*post)(pid_t,CGEventRef)) {
    if(!failed) return 0; *failed=1;
    if(!p || p->used) return 0; p->used=1;
    if(!ready || !post) return 0;
    uint8_t calls=0;
    @try { ++calls; post(p->pid,p->down); ++calls; post(p->pid,p->up); *failed=0; }
    @catch(NSException *e) { (void)e; *failed=1; }
    return calls;
}
uint8_t intendant_arrow_post(void *raw,uint8_t *failed) {
    ArrowPair *p=raw;
    return execute_arrow_pair(p,failed,p && intendant_arrow_ready(p->pid),CGEventPostToPid);
}
