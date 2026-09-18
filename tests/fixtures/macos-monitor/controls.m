// Opt-in disposable AppKit semantic-control target. All text is synthetic.
// Never activate, make key/main, raise, send keyboard input or open documents.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>

// Explicit, small accessibility tree: native acceptance tests the exact AX
// object lifecycle, not an incidental AppKit titlebar or a document hierarchy.
// Leaves use AppKit's own children behavior. Explicit subroles remain necessary
// for the intentionally fail-closed profile, although AppKit makes them optional.
@interface FixtureField : NSTextField
@end
@implementation FixtureField
- (id)accessibilityParent { return NSAccessibilityUnignoredAncestor(self.superview); }
- (NSString *)accessibilitySubrole { return @"AXUnknown"; }
@end
@interface FixtureButton : NSButton
@end
@implementation FixtureButton
- (id)accessibilityParent { return NSAccessibilityUnignoredAncestor(self.superview); }
- (NSString *)accessibilitySubrole { return @"AXUnknown"; }
@end
@interface FixtureSecure : NSSecureTextField
@end
@implementation FixtureSecure
- (id)accessibilityParent { return NSAccessibilityUnignoredAncestor(self.superview); }
- (NSString *)accessibilitySubrole { return @"AXSecureTextField"; }
@end
@interface FixtureGroup : NSView
@end
@implementation FixtureGroup
- (BOOL)isAccessibilityElement { return YES; }
- (NSString *)accessibilityRole { return NSAccessibilityGroupRole; }
- (NSString *)accessibilitySubrole { return @"AXUnknown"; }
// AX must expose unignored children, not NSControl wrappers around their cells.
- (NSArray *)accessibilityChildren { return NSAccessibilityUnignoredChildren(self.subviews); }
- (id)accessibilityParent { return self.window; }
@end
@interface FixturePanel : NSPanel
@end
@implementation FixturePanel
- (BOOL)canBecomeKeyWindow { return NO; }
- (BOOL)canBecomeMainWindow { return NO; }
- (NSArray *)accessibilityChildren { return NSAccessibilityUnignoredChildrenForOnlyChild(self.contentView); }
@end
@interface FixtureCounter : NSObject
@property NSUInteger count;
- (void)increment:(id)sender;
@end
@implementation FixtureCounter
- (void)increment:(id)sender { (void)sender; self.count += 1; }
@end

static FixtureField *new_field(void) {
    FixtureField *f = [[FixtureField alloc] initWithFrame:NSMakeRect(20, 220, 300, 28)];
    f.editable = YES; f.selectable = YES;
    f.stringValue = @"initial disposable text";
    f.accessibilityTitle = @"Normal fixture field";
    return f;
}

int main(int argc, const char **argv) {
    if (argc != 3 || strcmp(argv[1], "--disposable-controls-fixture") != 0) return 2;
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        NSString *path = [NSString stringWithUTF8String:argv[2]];
        FixturePanel *panel = [[FixturePanel alloc] initWithContentRect:NSMakeRect(50,50,400,300)
            styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskResizable | NSWindowStyleMaskNonactivatingPanel
            backing:NSBackingStoreBuffered defer:NO];
        panel.title = @"Intendant disposable controls fixture";
        panel.animationBehavior = NSWindowAnimationBehaviorNone;
        panel.releasedWhenClosed = NO; panel.hidesOnDeactivate = NO; panel.level = NSNormalWindowLevel;
        FixtureGroup *group = [[FixtureGroup alloc] initWithFrame:NSMakeRect(0,0,400,300)];
        panel.contentView = group;
        __block FixtureField *field = new_field(); [group addSubview:field];
        FixtureCounter *counter = [FixtureCounter new];
        FixtureButton *button = [[FixtureButton alloc] initWithFrame:NSMakeRect(20,170,220,32)];
        button.title = @"Increment fixture counter"; button.target = counter; button.action = @selector(increment:);
        [group addSubview:button];
        FixtureSecure *secure = [[FixtureSecure alloc] initWithFrame:NSMakeRect(20,110,300,28)];
        secure.stringValue = @"synthetic-secret-never-return"; secure.accessibilityTitle = @"Omit secure fixture";
        [group addSubview:secure];
        FixtureButton *disabled = [[FixtureButton alloc] initWithFrame:NSMakeRect(20,55,220,32)];
        disabled.title = @"Omit disabled fixture"; disabled.enabled = NO;
        disabled.target = counter; disabled.action = @selector(increment:); [group addSubview:disabled];
        // Display below existing windows, never order front/key/main or activate.
        [panel orderWindow:NSWindowBelow relativeTo:0];
        __block NSUInteger generation = 1, ticks = 0;
        if (fcntl(STDIN_FILENO,F_SETFL,O_NONBLOCK) == -1) return 3;
        [NSTimer scheduledTimerWithTimeInterval:0.05 repeats:YES block:^(NSTimer *timer) {
            (void)timer;
            char command = 0; ssize_t n = read(STDIN_FILENO,&command,1);
            if (n == 0 || command == 'q' || ++ticks >= 2400) { [panel close]; [app terminate:nil]; return; }
            if (command == 'r') {
                // Replace ONLY the field. Same role/title/frame must never let
                // an old token substitute the newly allocated AX object.
                [field removeFromSuperview]; field = new_field(); [group addSubview:field]; generation++;
            }
            NSArray *rows = CFBridgingRelease(CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow,(CGWindowID)panel.windowNumber));
            if (rows.count != 1) return;
            NSDictionary *rect = rows[0][(id)kCGWindowBounds]; CGRect bounds;
            if (![rect isKindOfClass:NSDictionary.class] || !CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)rect,&bounds)) return;
            // Independent fixture readback of OUR OWN normal text and counter.
            // Never read or serialize the secure field's value, even here.
            NSDictionary *status = @{@"pid":@(getpid()), @"window_id":@(panel.windowNumber), @"control_generation":@(generation),
                @"normal_text":field.stringValue, @"button_count":@(counter.count),
                @"bounds":@{@"x":@(bounds.origin.x), @"y":@(bounds.origin.y), @"width":@(bounds.size.width), @"height":@(bounds.size.height)}};
            NSData *data = [NSJSONSerialization dataWithJSONObject:status options:0 error:nil];
            if (data.length > 8192 || ![data writeToFile:path atomically:YES]) { [panel close]; [app terminate:nil]; }
        }];
        [app run];
    }
    return 0;
}
