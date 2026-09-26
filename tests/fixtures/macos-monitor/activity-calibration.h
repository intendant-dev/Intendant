#pragma once
// Read-only diagnostic mode of the actual supervisor, never a second input path.
#include "activity-witness.h"
#include <dlfcn.h>
#include <sys/stat.h>
#include <errno.h>
#include <math.h>
#include <stdio.h>

static NSDictionary *calibration_counts(ActivityCounters value) {
    return @{@"any_input":@(value.values[0]), @"key_down":@(value.values[1]),
        @"key_up":@(value.values[2]), @"mouse_move":@(value.values[3]),
        @"scroll":@(value.values[4])};
}
static unsigned calibration_seconds(int argc, const char **argv) {
    if (argc != 3 || strcmp(argv[1], "--calibrate-input-observer") != 0) return 0;
    const char *text=argv[2];
    if (!text || !*text || *text=='0' || strlen(text)>2) return 0;
    unsigned seconds=0;
    for (; *text; ++text) {
        if (*text<'0' || *text>'9') return 0;
        seconds=seconds*10+(unsigned)(*text-'0');
    }
    return seconds>=1 && seconds<=30 ? seconds : 0;
}
// C/Carbon flags must serialize as JSON booleans, not integer 0/1.
static NSNumber *calibration_flag(BOOL value) { return value ? @YES : @NO; }
static NSDictionary *calibration_access(void) {
    // Optional documented Carbon query: absence stays unknown, not disabled.
    typedef unsigned char (*SecureInputQuery)(void);
    SecureInputQuery secure=(SecureInputQuery)dlsym(RTLD_DEFAULT,"IsSecureEventInputEnabled");
    struct stat console={0};
    id sameConsole=stat("/dev/console", &console)==0 ? (id)calibration_flag(console.st_uid==geteuid()) : NSNull.null;
    return @{@"listen_event_access":calibration_flag(CGPreflightListenEventAccess()),
        @"post_event_access":calibration_flag(CGPreflightPostEventAccess()),
        @"accessibility_trusted":calibration_flag(AXIsProcessTrusted()),
        @"secure_event_input":secure ? (id)calibration_flag(secure()!=0) : NSNull.null,
        @"console_user_matches":sameConsole};
}
static BOOL calibration_emit(NSDictionary *record) {
    NSData *data=[NSJSONSerialization dataWithJSONObject:record options:0 error:nil];
    if (!data || data.length>16384) return NO;
    return fwrite(data.bytes,1,data.length,stdout)==data.length && fputc('\n',stdout)!=EOF && fflush(stdout)==0;
}
static NSDictionary *calibration_snapshot(NSUInteger sequence) {
    double began=NSProcessInfo.processInfo.systemUptime;
    ActivityCounters hid=activity_sample(); // Identical function used by ActivityWitness.
    ActivityCounters session=activity_sample_for_state(kCGEventSourceStateCombinedSessionState);
    NSDictionary *access=calibration_access();
    double ended=NSProcessInfo.processInfo.systemUptime;
    return @{@"kind":@"sample", @"sequence":@(sequence),
        @"started_uptime":@(began), @"finished_uptime":@(ended),
        @"hid_system":calibration_counts(hid), @"combined_session":calibration_counts(session),
        @"access":access};
}
static void calibration_wait(double until) {
    while (NSProcessInfo.processInfo.systemUptime<until) {
        [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.01]];
        // A run loop with no registered source may return immediately.
        if (NSProcessInfo.processInfo.systemUptime<until) usleep(1000);
    }
}
static int observer_calibration_main(int argc, const char **argv) {
    unsigned seconds=calibration_seconds(argc,argv);
    if (!seconds) return 2; // Validate before any permission or counter query.
    @autoreleasepool {
        int flags=fcntl(STDIN_FILENO,F_GETFL);
        if (flags<0 || fcntl(STDIN_FILENO,F_SETFL,flags|O_NONBLOCK)<0) return 3;
        // stdout contains only bounded JSON records. No files, windows or browser.
        NSISO8601DateFormatter *date=[NSISO8601DateFormatter new];
        NSDictionary *ready=@{@"kind":@"ready", @"schema":@1,
            @"profile":@"input_observer_calibration", @"pid":@(getpid()),
            @"seconds":@(seconds), @"period_ms":@100,
            @"utc":[date stringFromDate:NSDate.date],
            @"os":NSProcessInfo.processInfo.operatingSystemVersionString,
            @"access":calibration_access(), @"native_input_calls":@0,
            @"windows_created":@0, @"browsers_launched":@0, @"event_taps_installed":@0};
        if (!calibration_emit(ready)) return 4;
        double armed=NSProcessInfo.processInfo.systemUptime;
        BOOL started=NO;
        NSString *reason=@"start_deadline";
        while (NSProcessInfo.processInfo.systemUptime-armed<30) {
            char command=0; ssize_t n=read(STDIN_FILENO,&command,1);
            if (n==1 && command=='s') { started=YES; break; }
            if (n==0) { reason=@"stdin_closed_before_start"; break; }
            if (n==1 && command!='\n' && command!='\r') { reason=@"cancelled_before_start"; break; }
            if (n<0 && errno!=EAGAIN && errno!=EWOULDBLOCK && errno!=EINTR) { reason=@"stdin_error"; break; }
            calibration_wait(NSProcessInfo.processInfo.systemUptime+0.02);
        }
        NSUInteger count=0; BOOL completed=NO;
        double start=NSProcessInfo.processInfo.systemUptime;
        if (started) {
            if (!calibration_emit(@{@"kind":@"started", @"utc":[date stringFromDate:NSDate.date],
                    @"started_uptime":@(start)})) return 4;
            reason=@"sampling_deadline";
            while (count<=seconds*10 && NSProcessInfo.processInfo.systemUptime-start<=seconds+2) {
                char command=0; ssize_t n=read(STDIN_FILENO,&command,1);
                if (n==0 || (n==1 && command=='q')) { reason=@"cancelled"; break; }
                if (n<0 && errno!=EAGAIN && errno!=EWOULDBLOCK && errno!=EINTR) { reason=@"stdin_error"; break; }
                if (!calibration_emit(calibration_snapshot(count))) return 4;
                ++count;
                if (count==seconds*10+1) { completed=YES; reason=@"finished"; break; }
                calibration_wait(start+count*0.1);
            }
        }
        BOOL emitted=calibration_emit(@{@"kind":@"finished", @"completed":calibration_flag(completed),
            @"samples":@(count), @"reason":reason, @"utc":[date stringFromDate:NSDate.date],
            @"finished_uptime":@(NSProcessInfo.processInfo.systemUptime),
            @"native_input_calls":@0, @"windows_created":@0, @"browsers_launched":@0,
            @"event_taps_installed":@0});
        (void)fcntl(STDIN_FILENO,F_SETFL,flags);
        return !emitted ? 4 : completed ? 0 : 5;
    }
}
