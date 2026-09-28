// Private exact-child macOS browser owner. No input, activation, adoption or
// focus restoration. A cancelled/timed-out launch is owned through its callback.
#import <AppKit/AppKit.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>
#include "macos_browser_lifecycle.h"

static int32_t refuse_before_launch(int32_t code) {
    signal(SIGPIPE, SIG_IGN);
    fprintf(stdout, "{\"error\":\"native prelaunch refusal\",\"code\":%d}\n", code);
    fputs("{\"cleanup_verified\":true}\n", stdout); fflush(stdout);
    return code;
}

int32_t intendant_macos_browser_supervise(const char *bundle_path,
        int32_t argc, const char * const *argv) {
    @autoreleasepool {
        if (![NSThread isMainThread] || !bundle_path || argc <= 0 || argc > 64 || !argv) return refuse_before_launch(2);
        signal(SIGPIPE, SIG_IGN); // Lost receipt delivery must not orphan the app.
        if (fcntl(STDIN_FILENO, F_SETFL, O_NONBLOCK) == -1) return refuse_before_launch(7);
        char initial;
        if (read(STDIN_FILENO, &initial, 1) == 0) return refuse_before_launch(10);
        __block NSRunningApplication *browser = nil;
        __block BOOL launch_finished = NO;
        __block BOOL launch_failed = NO;
        BrowserLifecycle lifecycle = {0};
        int32_t outcome = 0;
        BOOL launch_requested = NO, timeout_reported = NO;
        NSTimeInterval started = NSProcessInfo.processInfo.systemUptime, stop_at = 0;
        @try {
            NSString *path = [NSString stringWithUTF8String:bundle_path];
            NSBundle *bundle = [NSBundle bundleWithPath:path];
            if (!path.isAbsolutePath || !bundle ||
                ![bundle.bundleIdentifier isEqualToString:@"com.google.chrome.for.testing"]) return refuse_before_launch(3);
            NSMutableArray<NSString *> *arguments = [NSMutableArray array];
            size_t bytes = 0;
            for (int32_t i = 0; i < argc; i++) {
                if (!argv[i]) return refuse_before_launch(4);
                NSString *value = [NSString stringWithUTF8String:argv[i]];
                if (!value || (bytes += [value lengthOfBytesUsingEncoding:NSUTF8StringEncoding]) > 16384) return refuse_before_launch(4);
                [arguments addObject:value];
            }
            NSWorkspaceOpenConfiguration *config = [NSWorkspaceOpenConfiguration configuration];
            config.activates = NO;
            config.createsNewApplicationInstance = YES;
            config.allowsRunningApplicationSubstitution = NO;
            config.addsToRecentItems = NO;
            config.promptsUserIfNeeded = NO;
            config.arguments = arguments;
            // The controller also clears its helper environment. Never copy its
            // provider keys, admission tokens or credential variables to Chrome.
            config.environment = @{@"HOME":NSHomeDirectory(), @"PATH":@"/usr/bin:/bin",
                @"TMPDIR":NSTemporaryDirectory(), @"LANG":@"en_US.UTF-8"};
            launch_requested = YES;
            [NSWorkspace.sharedWorkspace openApplicationAtURL:[NSURL fileURLWithPath:path]
                configuration:config completionHandler:^(NSRunningApplication *app, NSError *error) {
                    dispatch_async(dispatch_get_main_queue(), ^{
                        browser = app; launch_failed = error != nil; launch_finished = YES;
                    });
                }];
        } @catch (NSException *exception) {
            (void)exception; outcome = 9; lifecycle.stopping = true;
            if (!launch_requested) return refuse_before_launch(outcome);
        }
        for (;;) {
            @try {
                [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
                NSTimeInterval now = NSProcessInfo.processInfo.systemUptime;
                char command = 0; ssize_t n = read(STDIN_FILENO, &command, 1);
                BOOL stop = lifecycle.stopping || n == 0 || (n > 0 && command == 'q') ||
                    (n < 0 && errno != EAGAIN && errno != EINTR);
                if (!launch_finished && now - started >= 10.0) {
                    stop = YES; outcome = 5;
                    if (!timeout_reported) {
                        timeout_reported = YES;
                        fputs("{\"error\":\"launch deadline; exact cleanup retained\"}\n", stdout);
                        fflush(stdout);
                    }
                }
                if (launch_finished && (launch_failed || !browser)) { stop = YES; outcome = 5; }
                if (browser && !browser.terminated) {
                    NSRunningApplication *front = NSWorkspace.sharedWorkspace.frontmostApplication;
                    if (!front || front.processIdentifier == browser.processIdentifier) {
                        stop = YES; outcome = 8;
                    }
                }
                unsigned action = browser_lifecycle_step(&lifecycle, launch_finished,
                    browser != nil, browser && browser.terminated, stop, stop_at && now - stop_at >= 5.0);
                if (action == BrowserDone) {
                    if (!lifecycle.published && !timeout_reported) {
                        fputs("{\"error\":\"browser unavailable before publication\"}\n", stdout);
                    }
                    fputs("{\"cleanup_verified\":true}\n", stdout); fflush(stdout);
                    return outcome;
                }
                if (action == BrowserTerminate) { stop_at = now; [browser terminate]; }
                if (action == BrowserForce) [browser forceTerminate];
                if (action == BrowserPublish) {
                    if (fprintf(stdout, "{\"pid\":%d}\n", browser.processIdentifier) < 0 || fflush(stdout) != 0)
                        lifecycle.stopping = true;
                }
            } @catch (NSException *exception) {
                (void)exception; outcome = 9; lifecycle.stopping = true;
                // Keep the original callback/application owner. Do not relaunch
                // or claim cleanup while its outcome remains unknown.
            }
        }
    }
}
