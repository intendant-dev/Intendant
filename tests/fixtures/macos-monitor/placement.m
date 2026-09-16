// Disposable native acceptance target only. Never opens user documents, raises
// or activates an application, changes focus, or injects input. Run explicitly.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>

@interface IntendantPlacementView : NSView
@end
@implementation IntendantPlacementView
- (void)drawRect:(NSRect)rect {
    (void)rect;
    [NSColor.systemBlueColor setFill]; NSRectFill(self.bounds);
}
@end

@interface IntendantPlacementPanel : NSPanel
@end
@implementation IntendantPlacementPanel
- (BOOL)canBecomeKeyWindow { return NO; }
- (BOOL)canBecomeMainWindow { return NO; }
@end

static IntendantPlacementPanel *new_panel(void) {
    IntendantPlacementPanel *p = [[IntendantPlacementPanel alloc]
        initWithContentRect:NSMakeRect(50, 50, 320, 240)
        styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskResizable |
                  NSWindowStyleMaskNonactivatingPanel
        backing:NSBackingStoreBuffered defer:NO];
    p.title = @"Intendant disposable placement fixture";
    // Keep fixture geometry stable: launch animation changes CG bounds without AX writes.
    p.animationBehavior = NSWindowAnimationBehaviorNone;
    p.releasedWhenClosed = NO;
    p.hidesOnDeactivate = NO;
    p.level = NSNormalWindowLevel;
    p.contentView = [[IntendantPlacementView alloc] initWithFrame:NSMakeRect(0, 0, 320, 240)];
    // Show this newly-created fixture BELOW all windows, without key/main or
    // order-front APIs. Its placement will come only from the explicit tool.
    [p orderWindow:NSWindowBelow relativeTo:0];
    return p;
}

int main(int argc, const char **argv) {
    if (argc != 3 || strcmp(argv[1], "--disposable-placement-fixture") != 0) return 2;
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        NSString *path = [NSString stringWithUTF8String:argv[2]];
        __block IntendantPlacementPanel *panel = new_panel();
        if (!panel) return 3;
        __block NSUInteger generation = 1;
        __block NSInteger replacedWindowID = 0;
        __block NSUInteger ticks = 0;
        // Fixed command buffer, one-byte commands; no terminal/keyboard APIs.
        if (fcntl(STDIN_FILENO, F_SETFL, O_NONBLOCK) == -1) return 4;
        [NSTimer scheduledTimerWithTimeInterval:0.05 repeats:YES block:^(NSTimer *timer) {
            char command = 0;
            ssize_t n = read(STDIN_FILENO, &command, 1);
            if (n == 0 || command == 'q' || ++ticks >= 1800) {
                [panel close]; [app terminate:nil]; return;
            }
            if (command == 'r') {
                // Report which fixture was replaced for list-to-bind acceptance.
                replacedWindowID = panel.windowNumber;
                [panel close]; panel = new_panel(); generation++;
            }
            NSArray *rows = CFBridgingRelease(CGWindowListCopyWindowInfo(
                kCGWindowListOptionIncludingWindow, (CGWindowID)panel.windowNumber));
            if (rows.count != 1) return;
            NSDictionary *window = rows[0];
            NSDictionary *rect = window[(id)kCGWindowBounds];
            if (![rect isKindOfClass:NSDictionary.class]) return;
            CGRect bounds;
            if (!CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)rect, &bounds)) return;
            NSDictionary *status = @{
                @"pid": @(getpid()), @"window_id": @(panel.windowNumber),
                @"fixture_generation": @(generation), @"replaced_window_id": @(replacedWindowID),
                @"bounds": @{@"x": @(bounds.origin.x), @"y": @(bounds.origin.y),
                    @"width": @(bounds.size.width), @"height": @(bounds.size.height)}
            };
            NSData *data = [NSJSONSerialization dataWithJSONObject:status options:0 error:nil];
            if (data.length > 2048 || ![data writeToFile:path atomically:YES]) {
                [panel close]; [app terminate:nil];
            }
        }];
        [app run];
    }
    return 0;
}
