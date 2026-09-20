// Fixture-only, unmodified ArrowRight delivery. Default creates no application
// or window and posts nothing. Live mode targets only this process's own panel.
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreGraphics/CoreGraphics.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "key-event.h"
static int emit(NSDictionary *value, int code) {
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:0 error:nil];
    if (!data || data.length > 16384) return 3;
    if (fwrite(data.bytes,1,data.length,stdout) != data.length ||
        fputc('\n',stdout) == EOF || fflush(stdout) != 0) return 3;
    return code;
}
static int construction(void) {
    CGEventSourceRef source = CGEventSourceCreate(kCGEventSourceStatePrivate);
    BOOL ok = source != NULL; NSUInteger cases = 0; NSMutableArray *readbacks=[NSMutableArray array];
    for (NSUInteger i=0;i<2;++i) {
        CGEventType type = i ? kCGEventKeyUp : kCGEventKeyDown;
        CGEventRef event = make_key(source,type,12345,getpid(),98765);
        ok &= matches(source,event,type,12345,getpid(),98765); cases++;
        if(event) {
            NSEvent *e=[NSEvent eventWithCGEvent:event];
            UniChar c[2]={0}; UniCharCount n=0; CGEventKeyboardGetUnicodeString(event,2,&n,c);
            [readbacks addObject:@{@"window":@(e.windowNumber),@"type":@(e.type),@"key":@(e.keyCode),
                @"flags":@(e.modifierFlags),@"repeat":@(e.isARepeat),@"characters":e.characters ?: @"",
                @"cg_type":@(CGEventGetType(event)),@"cg_flags":@(CGEventGetFlags(event)),
                @"cg_key":@(CGEventGetIntegerValueField(event,kCGKeyboardEventKeycode)),
                @"state":@(CGEventGetIntegerValueField(event,kCGEventSourceStateID)),
                @"expected_state":@(CGEventSourceGetSourceStateID(source)),
                @"source":@(CGEventGetIntegerValueField(event,kCGEventSourceUnixProcessID)),
                @"target":@(CGEventGetIntegerValueField(event,kCGEventTargetUnixProcessID)),
                @"tag":@(CGEventGetIntegerValueField(event,kCGEventSourceUserData)),@"unicode_count":@(n),@"unicode":@(c[0])}];
        }
        if(event) CFRelease(event);
    }
    CGEventRef invalid[] = {
        make_key(NULL,kCGEventKeyDown,12345,getpid(),1),
        make_key(source,kCGEventKeyDown,0,getpid(),1),
        make_key(source,kCGEventKeyDown,12345,0,1),
        make_key(source,kCGEventKeyDown,12345,getpid(),0),
        make_key(source,kCGEventLeftMouseDown,12345,getpid(),1),
    };
    for (NSUInteger i=0;i<sizeof(invalid)/sizeof(invalid[0]);++i) {
        ok &= invalid[i] == NULL; cases++;
        if(invalid[i]) CFRelease(invalid[i]);
    }
    if(source) CFRelease(source);
    ok &= NSApp == nil;
    return emit(@{@"ok":@(ok),@"mode":@"construction_only",@"cases":@(cases),@"readbacks":readbacks,
        @"posted_events":@0,@"application_created":NSApp ? @YES : @NO},ok ? 0 : 1);
}

@interface KeyPanel : NSPanel
@end
@implementation KeyPanel
- (BOOL)canBecomeKeyWindow { return NO; }
- (BOOL)canBecomeMainWindow { return NO; }
@end
@interface KeyCanvas : NSView
@property int64_t eventTag;
@property NSUInteger effects;
@property NSUInteger unrelated;
@property BOOL overflow;
@property(strong) NSMutableArray *receipts;
@end
static NSDictionary *receipt(NSEvent *event) {
    CGEventRef cg=event.CGEvent;
    if(!cg || (event.type != NSEventTypeKeyDown && event.type != NSEventTypeKeyUp)) return @{};
    return @{@"kind":event.type == NSEventTypeKeyDown ? @"key_down" : @"key_up",
        @"pid":@(getpid()),@"source_pid":@(CGEventGetIntegerValueField(cg,kCGEventSourceUnixProcessID)),
        @"window_id":@(event.windowNumber),@"keycode":@(event.keyCode),
        @"tag":@(CGEventGetIntegerValueField(cg,kCGEventSourceUserData)),
        @"flags":@(event.modifierFlags),@"repeat":event.isARepeat ? @YES : @NO,
        @"expected_character":[event.characters isEqualToString:probe_text()] ? @YES : @NO};
}
@implementation KeyCanvas
- (BOOL)acceptsFirstResponder { return YES; }
- (void)drawRect:(NSRect)rect { (void)rect; [[NSColor darkGrayColor] setFill]; NSRectFill(self.bounds); }
- (void)record:(NSEvent *)event {
    CGEventRef cg=event.CGEvent;
    if(!cg || CGEventGetIntegerValueField(cg,kCGEventSourceUserData) != self.eventTag) {
        self.unrelated++; return; // Never inspect or retain human key contents.
    }
    if(self.receipts.count >= 8) { self.overflow=YES; return; }
    [self.receipts addObject:receipt(event)];
    if(event.type == NSEventTypeKeyDown && event.keyCode == ProbeKey &&
        event.modifierFlags == NSEventModifierFlagFunction && !event.isARepeat &&
        [event.characters isEqualToString:probe_text()]) self.effects++;
}
- (void)keyDown:(NSEvent *)event { [self record:event]; }
- (void)keyUp:(NSEvent *)event { [self record:event]; }
@end
static NSDictionary *sample(void) {
    NSRunningApplication *front=NSWorkspace.sharedWorkspace.frontmostApplication;
    CGEventRef event=CGEventCreate(NULL);
    if(!front || !event) { if(event) CFRelease(event); return @{}; }
    CGPoint p=CGEventGetLocation(event); CFRelease(event);
    return @{@"front_pid":@(front.processIdentifier),@"pointer_x":@(p.x),@"pointer_y":@(p.y),
        @"clipboard_change_count":@(NSPasteboard.generalPasteboard.changeCount)};
}
static BOOL own_window(NSWindow *panel) {
    NSArray *rows=CFBridgingRelease(CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow,(CGWindowID)panel.windowNumber));
    return panel.visible && rows.count == 1 &&
        [rows[0][(id)kCGWindowNumber] integerValue] == panel.windowNumber &&
        [rows[0][(id)kCGWindowOwnerPID] intValue] == getpid();
}
static BOOL ready(void) {
    NSRunningApplication *front=NSWorkspace.sharedWorkspace.frontmostApplication;
    if(!AXIsProcessTrusted() || !CGPreflightPostEventAccess() || !front ||
        front.processIdentifier == getpid() || CGEventSourceKeyState(kCGEventSourceStateHIDSystemState,ProbeKey) ||
        key_shortcut_held(CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState))) return NO;
    for(CGMouseButton b=0;b<5;++b) if(CGEventSourceButtonState(kCGEventSourceStateHIDSystemState,b)) return NO;
    return YES;
}
static int live(void) {
    NSDictionary *before=sample();
    if(before.count != 4 || !ready()) return emit(@{@"ok":@NO,@"mode":@"self_process_key",
        @"posted_events":@0,@"error":@"permission, foreground or held-input preflight refused",
        @"ax_trusted":AXIsProcessTrusted()?@YES:@NO,@"post_access":CGPreflightPostEventAccess()?@YES:@NO,
        @"sample_available":(before.count==4 ? @YES : @NO),@"hid_flags":@(CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState))},1);
    NSApplication *app=[NSApplication sharedApplication];
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    [app finishLaunching];
    KeyPanel *panel=[[KeyPanel alloc] initWithContentRect:NSMakeRect(50,50,320,220)
        styleMask:NSWindowStyleMaskTitled|NSWindowStyleMaskNonactivatingPanel
        backing:NSBackingStoreBuffered defer:NO];
    panel.title=@"Intendant disposable keyboard receiver";
    panel.releasedWhenClosed=NO; panel.hidesOnDeactivate=NO;
    panel.animationBehavior=NSWindowAnimationBehaviorNone;
    KeyCanvas *view=[[KeyCanvas alloc] initWithFrame:NSMakeRect(0,0,320,220)];
    view.receipts=[NSMutableArray array]; panel.contentView=view;
    do { int64_t tag; arc4random_buf(&tag,sizeof(tag)); view.eventTag=tag & INT64_MAX; } while(!view.eventTag);
    [panel orderWindow:NSWindowBelow relativeTo:0];
    [panel makeFirstResponder:view]; // Only our private window's receiver; no key/activation request.
    NSMutableArray *queue=[NSMutableArray array];
    BOOL front=NO, observed=YES, overflow=NO;
    NSTimeInterval start=NSProcessInfo.processInfo.systemUptime;
    while(!own_window(panel) && NSProcessInfo.processInfo.systemUptime-start < 0.5) {
        NSEvent *event=[app nextEventMatchingMask:NSEventMaskAny untilDate:[NSDate dateWithTimeIntervalSinceNow:0.01]
            inMode:NSDefaultRunLoopMode dequeue:YES];
        if(event) [app sendEvent:event];
    }
    CGEventSourceRef source=NULL; CGEventRef down=NULL,up=NULL; NSUInteger posted=0;
    NSString *error=nil;
    if(!own_window(panel) || panel.firstResponder != view || panel.keyWindow || !ready()) error=@"exact receiver preflight refused";
    else {
        source=CGEventSourceCreate(kCGEventSourceStatePrivate);
        down=make_key(source,kCGEventKeyDown,(uint32_t)panel.windowNumber,getpid(),view.eventTag);
        up=make_key(source,kCGEventKeyUp,(uint32_t)panel.windowNumber,getpid(),view.eventTag);
        if(!matches(source,down,kCGEventKeyDown,(uint32_t)panel.windowNumber,getpid(),view.eventTag) ||
            !matches(source,up,kCGEventKeyUp,(uint32_t)panel.windowNumber,getpid(),view.eventTag) ||
            !own_window(panel) || panel.firstResponder != view || !ready()) error=@"construction or final identity preflight refused";
        else {
            // No global post, shortcut, input retry, window dispatch or focus restoration.
            @try {
                ++posted; CGEventPostToPid(getpid(),down);
                ++posted; CGEventPostToPid(getpid(),up);
            } @catch(NSException *exception) { (void)exception; error=@"native posting exception; effects uncertain"; }
            NSTimeInterval stop=NSProcessInfo.processInfo.systemUptime+1.0;
            while(NSProcessInfo.processInfo.systemUptime < stop) {
                NSEvent *event=[app nextEventMatchingMask:NSEventMaskAny untilDate:[NSDate dateWithTimeIntervalSinceNow:0.01]
                    inMode:NSDefaultRunLoopMode dequeue:YES];
                if(event) {
                    CGEventRef cg=event.CGEvent;
                    if(cg && CGEventGetIntegerValueField(cg,kCGEventSourceUserData) == view.eventTag) {
                        if(queue.count < 8) [queue addObject:receipt(event)]; else overflow=YES;
                    }
                    [app sendEvent:event];
                }
                NSRunningApplication *f=NSWorkspace.sharedWorkspace.frontmostApplication;
                observed &= f != nil; front |= f && f.processIdentifier == getpid();
            }
        }
    }
    NSDictionary *after=sample(); observed &= after.count == 4;
    BOOL responderUnchanged=panel.firstResponder == view;
    uint32_t window=(uint32_t)panel.windowNumber;
    [panel close]; BOOL closed=!panel.visible;
    if(down) CFRelease(down); if(up) CFRelease(up); if(source) CFRelease(source);
    BOOL ok=!error && posted == 2 && view.effects == 1 && view.receipts.count == 2 &&
        !view.overflow && !overflow && !front && observed && responderUnchanged && closed;
    if(!ok && !error) error=@"receiver key delivery/effect not verified; queue receipt is insufficient";
    return emit(@{@"ok":@(ok),@"mode":@"self_process_key",@"posted_events":@(posted),
        @"plan":@{@"pid":@(getpid()),@"source_pid":@(getpid()),@"window_id":@(window),@"tag":@(view.eventTag),@"keycode":@(ProbeKey)},
        @"queue_receipts":queue,@"receipts":view.receipts,@"effect_count":@(view.effects),
        @"unrelated_events":@(view.unrelated),@"overflow":(overflow||view.overflow ? @YES : @NO),
        @"target_ever_front":@(front),@"observations_complete":@(observed),
        @"responder_unchanged":@(responderUnchanged),@"window_closed":@(closed),
        @"before":before,@"after":after,@"error":error ?: @""},ok ? 0 : 1);
}
int main(int argc,const char **argv) {
    @autoreleasepool {
        if(argc == 1 || (argc == 2 && !strcmp(argv[1],"--self-test"))) return construction();
        if(argc == 2 && !strcmp(argv[1],"--allow-disposable-key")) return live();
        fprintf(stderr,"Use --self-test or --allow-disposable-key; no target or key arguments accepted.\n");
        return 2;
    }
}
