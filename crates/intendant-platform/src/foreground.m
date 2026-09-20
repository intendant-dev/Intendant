// Read-only platform query. No input, activation or permission mutation.
#import <AppKit/AppKit.h>
#include <stdint.h>

// Read-only foreground candidate for exact focus observation. The AX adapter
// independently checks the retained application's AXFrontmost and focused object.
int32_t intendant_foreground_pid(void) {
    @autoreleasepool {
        @try {
            if (![NSThread isMainThread]) return 0;
            NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
            return front && !front.terminated ? front.processIdentifier : 0;
        } @catch (NSException *e) { (void)e; return 0; }
    }
}

