// Opt-in Chromium fixture supervisor. Launches ONLY an explicitly supplied
// disposable browser/profile, without activation, and owns exact-child cleanup.
// Observes foreground PID, pointer and clipboard change count, never contents.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ApplicationServices/ApplicationServices.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>

static NSDictionary *observation(void) {
    CGEventRef event = CGEventCreate(NULL);
    if (!event) return nil;
    CGPoint point = CGEventGetLocation(event); CFRelease(event);
    NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
    if (!front) return nil;
    return @{@"front_pid": @(front.processIdentifier),
             @"pointer_x": @(point.x), @"pointer_y": @(point.y),
             @"clipboard_change_count": @(NSPasteboard.generalPasteboard.changeCount)};
}

// Diagnostic only: bounded roles/counts from our own disposable browser's sole
// AX window. Never read titles, text values or descriptions, and never act.
static id ax_copy(AXUIElementRef element, CFStringRef key) {
    CFTypeRef value = NULL;
    AXError error = AXUIElementCopyAttributeValue(element, key, &value);
    if (error != kAXErrorSuccess) { if (value) CFRelease(value); return nil; }
    return value ? CFBridgingRelease(value) : nil;
}
// Describe only exact-object relationships in the disposable fixture.
static NSString *probe_role(id object) {
    if (!object || CFGetTypeID((__bridge CFTypeRef)object) != AXUIElementGetTypeID()) return @"not-element";
    AXUIElementRef element = (__bridge AXUIElementRef)object;
    AXUIElementSetMessagingTimeout(element, 0.05);
    id role = ax_copy(element, kAXRoleAttribute);
    return [role isKindOfClass:NSString.class] && [role length] <= 64 ? role : @"unavailable";
}
static NSDictionary *probe_parent_chain(id object, id root, pid_t pid, NSTimeInterval deadline) {
    NSMutableArray *nodes = [NSMutableArray array], *seen = [NSMutableArray array];
    NSString *error = nil; BOOL reached = NO;
    for (NSUInteger depth = 0; depth <= 16; depth++) {
        if (NSProcessInfo.processInfo.systemUptime >= deadline) { error = @"parent deadline"; break; }
        if (!object || CFGetTypeID((__bridge CFTypeRef)object) != AXUIElementGetTypeID() || [seen containsObject:object]) { error = @"parent invalid/cycle"; break; }
        [seen addObject:object]; AXUIElementRef element = (__bridge AXUIElementRef)object;
        AXUIElementSetMessagingTimeout(element, 0.05);
        pid_t actual = 0;
        if (AXUIElementGetPid(element, &actual) != kAXErrorSuccess || actual != pid) { error = @"foreign parent"; break; }
        if (CFEqual((__bridge CFTypeRef)object, (__bridge CFTypeRef)root)) { reached = YES; [nodes addObject:@{@"role":probe_role(object), @"root":@YES}]; break; }
        id parent = ax_copy(element, kAXParentAttribute), window = ax_copy(element, kAXWindowAttribute);
        if (!parent || CFGetTypeID((__bridge CFTypeRef)parent) != AXUIElementGetTypeID()) { error = @"parent unavailable"; break; }
        AXUIElementRef p = (__bridge AXUIElementRef)parent; AXUIElementSetMessagingTimeout(p, 0.05);
        CFIndex count = 0; AXError status = AXUIElementGetAttributeValueCount(p, kAXChildrenAttribute, &count);
        BOOL member = NO; NSString *childError = @"";
        if (status == kAXErrorSuccess && count > 0 && count <= 32) {
            CFArrayRef raw = NULL; status = AXUIElementCopyAttributeValues(p, kAXChildrenAttribute, 0, 33, &raw);
            NSArray *children = raw ? CFBridgingRelease(raw) : nil;
            if (status == kAXErrorSuccess && children && children.count <= 32) member = [children containsObject:object];
            else childError = @"children copy failed";
        } else childError = @"no bounded children";
        [nodes addObject:@{@"role":probe_role(object), @"parent_role":probe_role(parent), @"parent_exposes_child":@(member),
            @"window_matches":@(window && CFEqual((__bridge CFTypeRef)window, (__bridge CFTypeRef)root)), @"children_error":childError}];
        object = parent;
    }
    return @{@"chain":nodes, @"reached_exact_window":@(reached), @"error":error ?: @""};
}
static NSDictionary *tree_probe(pid_t pid) {
    if (!AXIsProcessTrusted()) return @{ @"error": @"diagnostic accessibility permission unavailable" };
    AXUIElementRef app = AXUIElementCreateApplication(pid);
    AXUIElementSetMessagingTimeout(app, 0.05);
    id windows = ax_copy(app, kAXWindowsAttribute); CFRelease(app);
    if (![windows isKindOfClass:NSArray.class] || [windows count] != 1)
        return @{ @"error": @"diagnostic requires one exact fixture window" };
    NSMutableArray *stack = [NSMutableArray arrayWithObject:@{ @"element": windows[0], @"path": @[], @"objects": @[] }];
    NSMutableArray *seen = [NSMutableArray array], *mismatches = [NSMutableArray array];
    NSArray *deepest = @[]; NSUInteger maxChildren = 0, controls = 0;
    NSString *error = nil;
    NSTimeInterval deadline = NSProcessInfo.processInfo.systemUptime + 4;
    while (stack.count) {
        if (seen.count >= 256 || NSProcessInfo.processInfo.systemUptime >= deadline) { error = @"diagnostic budget"; break; }
        NSDictionary *entry = stack.lastObject; [stack removeLastObject];
        id object = entry[@"element"];
        if (CFGetTypeID((__bridge CFTypeRef)object) != AXUIElementGetTypeID() || [seen containsObject:object]) {
            error = @"diagnostic invalid element or cycle"; break;
        }
        [seen addObject:object]; AXUIElementRef element = (__bridge AXUIElementRef)object;
        AXUIElementSetMessagingTimeout(element, 0.05);
        NSString *role = ax_copy(element, kAXRoleAttribute);
        NSString *subrole = ax_copy(element, kAXSubroleAttribute);
        if (![role isKindOfClass:NSString.class] || role.length > 64) { error = @"diagnostic invalid role"; break; }
        NSArray *path = [entry[@"path"] arrayByAddingObject:role];
        if (path.count > deepest.count) deepest = path;
        if (path.count > 32) { error = @"diagnostic depth budget"; break; }
        if ([role.lowercaseString containsString:@"secure"] || [role.lowercaseString containsString:@"password"] ||
            ([subrole isKindOfClass:NSString.class] && ([subrole.lowercaseString containsString:@"secure"] || [subrole.lowercaseString containsString:@"password"]))) continue;
        NSArray *objects = [entry[@"objects"] arrayByAddingObject:object];
        if ([entry[@"objects"] count] > 0) {
            id parent = ax_copy(element, kAXParentAttribute), expected = [entry[@"objects"] lastObject];
            if ((!parent || !CFEqual((__bridge CFTypeRef)parent, (__bridge CFTypeRef)expected)) && mismatches.count < 8) {
                [mismatches addObject:@{@"downward_roles":path, @"expected_parent_role":probe_role(expected),
                    @"actual_parent_role":probe_role(parent), @"parents":probe_parent_chain(object, windows[0], pid, deadline)}];
            }
        }
        if ([@[@"AXButton", @"AXTextField", @"AXTextArea", @"AXCheckBox", @"AXRadioButton"] containsObject:role]) controls++;
        CFIndex count = 0; AXError status = AXUIElementGetAttributeValueCount(element, kAXChildrenAttribute, &count);
        if (status == kAXErrorAttributeUnsupported || status == kAXErrorNoValue || (status == kAXErrorSuccess && count == 0)) continue;
        if (status != kAXErrorSuccess || count < 0 || count > 32) { error = @"diagnostic children budget or read failure"; break; }
        maxChildren = MAX(maxChildren, (NSUInteger)count);
        CFArrayRef raw = NULL; status = AXUIElementCopyAttributeValues(element, kAXChildrenAttribute, 0, 33, &raw);
        NSArray *children = raw ? CFBridgingRelease(raw) : nil;
        if (status != kAXErrorSuccess || !children || children.count > 32) { error = @"diagnostic child array failure"; break; }
        for (id child in children.reverseObjectEnumerator) [stack addObject:@{ @"element":child, @"path":path, @"objects":objects }];
    }
    return @{ @"visited": @(seen.count), @"max_depth": @(deepest.count ? deepest.count - 1 : 0),
        @"max_children": @(maxChildren), @"actionable_roles": @(controls), @"deepest_roles":deepest,
        @"complete":error ? @NO : @YES, @"error":error ?: @"", @"parent_mismatches":mismatches };
}

int main(int argc, const char **argv) {
    if (argc != 6 || strcmp(argv[1], "--disposable-chromium") != 0) return 2;
    @autoreleasepool {
        NSString *bundlePath = [NSString stringWithUTF8String:argv[2]];
        NSString *profile = [NSString stringWithUTF8String:argv[3]];
        NSString *statusPath = [NSString stringWithUTF8String:argv[4]];
        NSString *page = [NSString stringWithUTF8String:argv[5]];
        if (![page hasPrefix:@"file://"]) return 7;
        NSBundle *bundle = [NSBundle bundleWithPath:bundlePath];
        if (!bundle || ![bundle.bundleIdentifier isEqualToString:@"com.google.chrome.for.testing"])
            return 3; // Never attach to the user's normal browser.
        NSDictionary *before = observation(); if (!before) return 4;
        NSWorkspaceOpenConfiguration *config = [NSWorkspaceOpenConfiguration configuration];
        config.activates = NO; config.createsNewApplicationInstance = YES;
        config.arguments = @[[NSString stringWithFormat:@"--user-data-dir=%@", profile],
            @"--remote-debugging-port=0", @"--remote-debugging-address=127.0.0.1",
            @"--no-startup-window", @"--no-first-run", @"--no-default-browser-check",
            @"--use-mock-keychain", @"--password-store=basic", @"--disable-background-networking", @"--disable-sync", @"--disable-component-update",
            @"--disable-default-apps", @"--disable-domain-reliability", @"--disable-breakpad",
            @"--metrics-recording-only", @"--force-renderer-accessibility=complete"];
        config.environment = NSProcessInfo.processInfo.environment;
        __block NSRunningApplication *browser = nil;
        __block NSString *launchError = nil;
        __block BOOL launchFinished = NO;
        if (fcntl(STDIN_FILENO, F_SETFL, O_NONBLOCK) == -1) return 5;
        [NSWorkspace.sharedWorkspace openApplicationAtURL:[NSURL fileURLWithPath:bundlePath]
            configuration:config completionHandler:^(NSRunningApplication *app, NSError *error) {
                // The completion queue is not assumed to be main. All owner
                // state and AX diagnostics are confined to this run-loop thread.
                dispatch_async(dispatch_get_main_queue(), ^{
                    browser = app; launchError = error.localizedDescription; launchFinished = YES;
                });
            }];
        NSTimeInterval started = NSProcessInfo.processInfo.systemUptime;
        NSDictionary *diagnostic = nil; BOOL stopping = NO; NSTimeInterval stopAt = 0; NSUInteger tick = 0; BOOL browserEverFront = NO;
        while (NSProcessInfo.processInfo.systemUptime - started < 200) {
            [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
            char command = 0; ssize_t n = read(STDIN_FILENO, &command, 1);
            if (command == 'd' && browser && !browser.terminated) diagnostic = tree_probe(browser.processIdentifier);
            if (!stopping && (n == 0 || command == 'q' || NSProcessInfo.processInfo.systemUptime - started > 180)) {
                stopping = YES; stopAt = NSProcessInfo.processInfo.systemUptime;
                [browser terminate];
            }
            if (stopping && browser && !browser.terminated) {
                if (NSProcessInfo.processInfo.systemUptime - stopAt > 4) [browser forceTerminate];
            }
            if (launchFinished && !browser) break;
            if (stopping && launchFinished && (!browser || browser.terminated)) break;
            if (!launchFinished) continue;
            NSMutableDictionary *status = [@{@"supervisor_pid": @(getpid()),
                @"browser_pid": @(browser.processIdentifier), @"before": before,
                @"browser_terminated": @(browser.terminated)} mutableCopy];
            NSDictionary *current = observation();
            if (current) {
                status[@"observation"] = current;
                browserEverFront |= [current[@"front_pid"] intValue] == browser.processIdentifier;
            }
            status[@"tick"] = @(++tick);
            status[@"browser_ever_front"] = browserEverFront ? @YES : @NO;
            if (launchError) status[@"error"] = launchError;
            // Enumerate ONLY numeric metadata of our exact browser process.
            NSArray *windows = CFBridgingRelease(CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly, kCGNullWindowID));
            NSMutableArray *owned = [NSMutableArray array];
            for (NSDictionary *window in windows) {
                if ([window[(id)kCGWindowOwnerPID] intValue] == browser.processIdentifier &&
                    [window[(id)kCGWindowLayer] intValue] == 0) {
                    [owned addObject:@{@"window_id": window[(id)kCGWindowNumber], @"bounds": window[(id)kCGWindowBounds]}];
                }
            }
            status[@"windows"] = owned; if (diagnostic) status[@"tree_diagnostic"] = diagnostic;
            NSData *data = [NSJSONSerialization dataWithJSONObject:status options:0 error:nil];
            if (data.length > 16384 || ![data writeToFile:statusPath atomically:YES]) { stopping = YES; [browser terminate]; }
        }
        if (browser && !browser.terminated) [browser forceTerminate];
        NSTimeInterval end = NSProcessInfo.processInfo.systemUptime + 5;
        while (browser && !browser.terminated && NSProcessInfo.processInfo.systemUptime < end)
            [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
        NSDictionary *final = @{@"supervisor_pid": @(getpid()), @"browser_pid": @(browser ? browser.processIdentifier : 0),
            @"browser_terminated": (!browser || browser.terminated ? @YES : @NO), @"launch_finished": @(launchFinished), @"tick": @(++tick), @"browser_ever_front": browserEverFront ? @YES : @NO,
            @"error": launchError ?: @"", @"before": before, @"observation": observation() ?: @{}};
        [[NSJSONSerialization dataWithJSONObject:final options:0 error:nil] writeToFile:statusPath atomically:YES];
        return browser && browser.terminated ? 0 : 6;
    }
}
