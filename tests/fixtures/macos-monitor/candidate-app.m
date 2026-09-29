// Separate, user-operated acceptance app. No daemon launch, capture, or input
// on startup. Permission requests and a single test run require button actions.
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Security/Security.h>
#include <sys/stat.h>

static NSString *const BundleID = @"dev.intendant.cu-candidate";
static BOOL canRun(BOOL accessibility, BOOL screen, BOOL running, BOOL intact) {
    return accessibility && screen && !running && intact;
}
static NSDictionary *permissionState(void) {
    return @{@"accessibility": @(AXIsProcessTrusted()),
             @"screen_recording": @(CGPreflightScreenCaptureAccess())};
}
static BOOL intactBundle(void) {
    NSBundle *bundle = NSBundle.mainBundle;
    if (![bundle.bundleIdentifier isEqualToString:BundleID]) return NO;
    SecStaticCodeRef code = NULL;
    OSStatus status = SecStaticCodeCreateWithPath((__bridge CFURLRef)bundle.bundleURL,
                                                 kSecCSDefaultFlags, &code);
    if (status != errSecSuccess || !code) return NO;
    status = SecStaticCodeCheckValidity(code, kSecCSStrictValidate, NULL);
    CFRelease(code);
    return status == errSecSuccess;
}

@interface CandidateApp : NSObject <NSApplicationDelegate>
@property(strong) NSWindow *window;
@property(strong) NSTextField *statusLabel;
@property(strong) NSTextField *message;
@property(strong) NSButton *runButton;
@property(strong) NSTask *runner;
@property(strong) NSTimer *timer;
@property(strong) NSURL *stateRoot;
@property(strong) NSURL *runRoot;
@property(strong) NSDictionary *manifest;
@property(assign) BOOL bundleVerified;
@property(assign) BOOL quitPending;
@property(assign) BOOL stopRequested;
@end

@implementation CandidateApp
- (NSTextField *)label:(NSString *)text y:(CGFloat)y height:(CGFloat)height {
    NSTextField *label = [NSTextField wrappingLabelWithString:text];
    label.frame = NSMakeRect(24, y, 642, height);
    label.selectable = YES;
    [self.window.contentView addSubview:label];
    return label;
}
- (NSButton *)button:(NSString *)title x:(CGFloat)x y:(CGFloat)y action:(SEL)action {
    NSButton *button = [NSButton buttonWithTitle:title target:self action:action];
    button.frame = NSMakeRect(x, y, 208, 34);
    [self.window.contentView addSubview:button];
    return button;
}
- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    (void)notification;
    self.stateRoot = [[NSURL fileURLWithPath:NSHomeDirectory() isDirectory:YES]
        URLByAppendingPathComponent:@"Library/Application Support/Intendant CU Candidate" isDirectory:YES];
    NSError *error = nil;
    BOOL made = [NSFileManager.defaultManager createDirectoryAtURL:self.stateRoot
        withIntermediateDirectories:YES attributes:@{NSFilePosixPermissions:@0700} error:&error];
    NSURL *manifestURL = [NSBundle.mainBundle.resourceURL URLByAppendingPathComponent:@"candidate-manifest.json"];
    NSData *data = [NSData dataWithContentsOfURL:manifestURL];
    if (data) self.manifest = [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
    self.window = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, 690, 530)
        styleMask:(NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskMiniaturizable)
        backing:NSBackingStoreBuffered defer:NO];
    self.window.title = @"Intendant CU Candidate — separate test app";
    self.window.releasedWhenClosed = NO;
    NSTextField *title = [self label:@"Test the agent's private browser" y:466 height:38];
    title.font = [NSFont systemFontOfSize:23 weight:NSFontWeightSemibold];
    NSString *head = self.manifest[@"source_head"];
    if (![head isKindOfClass:NSString.class] || head.length != 40) head = @"unverified";
    [self label:[NSString stringWithFormat:@"Candidate: %@\nNot your installed Intendant. No update, login item, or provider account is used.", head]
        y:414 height:48];
    [self label:@"This app asks macOS for its own Accessibility and Screen Recording permissions.\nThose OS permissions are broad; the test uses only its disposable virtual browser.\nOpening this window does not capture your screen or send input."
        y:332 height:70];
    self.statusLabel = [self label:@"Checking this app's permission status…" y:280 height:46];
    [self button:@"1. Accessibility…" x:24 y:236 action:@selector(requestAccessibility:)];
    [self button:@"2. Screen Recording…" x:242 y:236 action:@selector(requestScreen:)];
    [self button:@"Recheck permissions" x:460 y:236 action:@selector(recheck:)];
    [self label:@"Approve “Intendant CU Candidate” in Privacy & Security, not Terminal or Desktop Commander.\nIf macOS asks you to quit and reopen this app, do so. Permissions alone do not start a test."
        y:170 height:55];
    self.runButton = [self button:@"3. Run one isolated test" x:24 y:121 action:@selector(runTest:)];
    [self button:@"Stop test" x:242 y:121 action:@selector(stopTest:)];
    [self button:@"Show reports" x:460 y:121 action:@selector(showReports:)];
    self.message = [self label:made ? @"Waiting for your approval. Installed Intendant is unchanged." : @"Cannot create the private test report directory. No test will run."
        y:22 height:84];
    self.bundleVerified = intactBundle();
    [self.window center];
    [self.window makeKeyAndOrderFront:nil];
    [self recheck:nil];
    self.timer = [NSTimer scheduledTimerWithTimeInterval:2.0 target:self selector:@selector(recheck:)
        userInfo:nil repeats:YES];
}
- (void)recheck:(id)sender {
    (void)sender;
    NSDictionary *state = permissionState();
    BOOL intact = self.bundleVerified;
    BOOL active = self.runner.running;
    self.statusLabel.stringValue = [NSString stringWithFormat:@"Accessibility: %@      Screen Recording: %@\nBundle integrity: %@",
        [state[@"accessibility"] boolValue] ? @"approved" : @"not approved",
        [state[@"screen_recording"] boolValue] ? @"approved" : @"not approved",
        intact ? @"verified" : @"not verified — rebuild this candidate"];
    self.runButton.enabled = canRun([state[@"accessibility"] boolValue],
        [state[@"screen_recording"] boolValue], active, intact);
    NSMutableDictionary *snapshot = [state mutableCopy];
    snapshot[@"bundle_id"] = BundleID;
    snapshot[@"source_head"] = self.manifest[@"source_head"] ?: @"unknown";
    snapshot[@"test_running"] = @(active);
    snapshot[@"bundle_intact"] = @(intact);
    snapshot[@"automatic_test_start"] = @NO;
    NSData *data = [NSJSONSerialization dataWithJSONObject:snapshot options:NSJSONWritingPrettyPrinted error:NULL];
    [data writeToURL:[self.stateRoot URLByAppendingPathComponent:@"status.json"]
        options:NSDataWritingAtomic error:NULL];
}
- (void)requestAccessibility:(id)sender {
    (void)sender;
    if (self.runner.running || !intactBundle()) return;
    if (!AXIsProcessTrusted()) {
        NSDictionary *options = @{(__bridge NSString *)kAXTrustedCheckOptionPrompt:@YES};
        (void)AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)options);
        self.message.stringValue = @"In System Settings → Privacy & Security → Accessibility, enable Intendant CU Candidate. This app cannot approve its own permission.";
    }
    [self recheck:nil];
}
- (void)requestScreen:(id)sender {
    (void)sender;
    if (self.runner.running || !intactBundle()) return;
    if (!CGPreflightScreenCaptureAccess()) {
        (void)CGRequestScreenCaptureAccess();
        self.message.stringValue = @"Approve Intendant CU Candidate for Screen Recording. Reopen it if macOS requests a restart, then recheck permissions.";
    }
    [self recheck:nil];
}
- (void)runTest:(id)sender {
    (void)sender;
    self.bundleVerified = intactBundle();
    [self recheck:nil];
    if (!self.runButton.enabled) return;
    NSAlert *alert = [[NSAlert alloc] init];
    alert.messageText = @"Run one disposable browser test?";
    alert.informativeText = @"This creates a virtual monitor and a fresh Chrome for Testing profile. A supervised test session will navigate, type, click and scroll only in a local fixture, then stop and clean up. Monitor hotplug can rearrange windows. No human-typing overlap is claimed. There is no automatic retry.";
    [alert addButtonWithTitle:@"Run test"];
    [alert addButtonWithTitle:@"Cancel"];
    if ([alert runModal] != NSAlertFirstButtonReturn) return;
    self.bundleVerified = intactBundle();
    [self recheck:nil];
    if (!self.runButton.enabled) return;
    NSURL *runs = [self.stateRoot URLByAppendingPathComponent:@"runs" isDirectory:YES];
    self.runRoot = [runs URLByAppendingPathComponent:NSUUID.UUID.UUIDString isDirectory:YES];
    NSError *error = nil;
    if (![NSFileManager.defaultManager createDirectoryAtURL:self.runRoot withIntermediateDirectories:YES
            attributes:@{NSFilePosixPermissions:@0700} error:&error]) {
        self.message.stringValue = @"Could not create a fresh report directory. No test started.";
        return;
    }
    NSURL *log = [self.runRoot URLByAppendingPathComponent:@"runner.log"];
    if (![NSFileManager.defaultManager createFileAtPath:log.path contents:NSData.data
            attributes:@{NSFilePosixPermissions:@0600}]) return;
    NSFileHandle *output = [NSFileHandle fileHandleForWritingToURL:log error:&error];
    if (!output) return;
    NSTask *task = [[NSTask alloc] init];
    task.executableURL = [NSURL fileURLWithPath:@"/usr/bin/python3"];
    task.arguments = @[@"-I", @"-B", [NSBundle.mainBundle.resourceURL URLByAppendingPathComponent:@"candidate_runner.py"].path,
                       @"--output-dir", self.runRoot.path];
    task.currentDirectoryURL = self.runRoot;
    task.environment = @{@"PATH":@"/usr/bin:/bin:/usr/sbin:/sbin", @"HOME":NSHomeDirectory(),
                         @"TMPDIR":NSTemporaryDirectory(), @"LANG":@"en_US.UTF-8"};
    task.standardOutput = output;
    task.standardError = output;
    task.standardInput = NSFileHandle.fileHandleWithNullDevice;
    self.stopRequested = NO;
    __weak CandidateApp *weakSelf = self;
    task.terminationHandler = ^(NSTask *finished) {
        [output closeFile];
        dispatch_async(dispatch_get_main_queue(), ^{
            CandidateApp *owner = weakSelf;
            if (!owner) return;
            owner.runner = nil;
            NSURL *resultURL = [owner.runRoot URLByAppendingPathComponent:@"result.json"];
            NSData *resultData = [NSData dataWithContentsOfURL:resultURL];
            NSDictionary *result = resultData ? [NSJSONSerialization JSONObjectWithData:resultData options:0 error:NULL] : nil;
            BOOL passed = finished.terminationStatus == 0 && [result[@"ok"] boolValue]
                && [result[@"full_workflow_verified"] boolValue]
                && [result[@"scope"] isEqualToString:@"full_task_browser"];
            owner.message.stringValue = passed ? @"The fixed supervised browser test passed. This does not establish arbitrary native-app control or simultaneous human typing. See the saved report."
                : @"The test did not pass or was stopped. Evidence is saved; no automatic retry will occur. Use Show reports for details.";
            [owner recheck:nil];
            if (owner.quitPending) [NSApp replyToApplicationShouldTerminate:YES];
        });
    };
    if (![task launchAndReturnError:&error]) {
        [output closeFile];
        self.message.stringValue = @"The isolated test runner could not start. No test is running.";
        return;
    }
    self.runner = task;
    self.message.stringValue = @"Running one isolated test. The separate test daemon checks its own OS permission context before operating. Stop requests cleanup; uncertain actions are not retried.";
    [self recheck:nil];
}
- (void)stopTest:(id)sender {
    (void)sender;
    if (self.runner.running && !self.stopRequested) {
        self.stopRequested = YES;
        [self.runner interrupt];
        self.message.stringValue = @"Stopping the test and allowing its cleanup to finish. No second signal will interrupt cleanup.";
    }
}
- (void)showReports:(id)sender {
    (void)sender;
    [NSWorkspace.sharedWorkspace openURL:self.runRoot ?: self.stateRoot];
}
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
    (void)sender;
    if (!self.runner.running) return NSTerminateNow;
    self.quitPending = YES;
    [self stopTest:nil];
    return NSTerminateLater;
}
- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)sender {
    (void)sender;
    return YES;
}
@end

int main(int argc, const char *argv[]) {
    umask(0077);
    @autoreleasepool {
        if (argc == 2 && strcmp(argv[1], "--self-test") == 0) {
            for (int i = 0; i < 16; ++i) {
                BOOL a = (i & 1) != 0, s = (i & 2) != 0;
                BOOL r = (i & 4) != 0, v = (i & 8) != 0;
                if (canRun(a, s, r, v) != (a && s && !r && v)) return 1;
            }
            puts("16 permission/run-state combinations passed; no GUI or OS permission calls");
            return 0;
        }
        if (argc == 2 && strcmp(argv[1], "--permission-check") == 0) {
            if (!intactBundle()) return 2;
            NSData *data = [NSJSONSerialization dataWithJSONObject:permissionState() options:0 error:NULL];
            fwrite(data.bytes, 1, data.length, stdout);
            puts("");
            return 0;
        }
        // LaunchServices may supply a process serial number; no other CLI lane
        // can request permissions or trigger a test.
        if (argc > 2 || (argc == 2 && strncmp(argv[1], "-psn_", 5) != 0)) return 2;
        NSApplication *app = NSApplication.sharedApplication;
        app.activationPolicy = NSApplicationActivationPolicyRegular;
        CandidateApp *delegate = [[CandidateApp alloc] init];
        app.delegate = delegate;
        [app run];
    }
    return 0;
}
