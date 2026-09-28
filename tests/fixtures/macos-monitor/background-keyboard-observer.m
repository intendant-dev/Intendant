// Read-only independent acceptance witness. No event taps, keyboard identities,
// text/clipboard contents, AX writes, activation or input posting. Explicit opt-in.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static uint64_t monotonic_us(void) {
    struct timespec now={0};
    if(clock_gettime(CLOCK_MONOTONIC,&now)!=0) return 0;
    return (uint64_t)now.tv_sec*1000000u+(uint64_t)now.tv_nsec/1000u;
}
static NSDictionary *sample(void) {
    NSRunningApplication *front=NSWorkspace.sharedWorkspace.frontmostApplication;
    id pid=front && !front.terminated ? (id)@(front.processIdentifier) : NSNull.null;
    id born=front.launchDate ? (id)@((int64_t)(front.launchDate.timeIntervalSince1970*1000)) : NSNull.null;
    return @{@"monotonic_us":@(monotonic_us()), @"front_pid":pid, @"front_started_ms":born,
        @"clipboard_change_count":@(NSPasteboard.generalPasteboard.changeCount),
        @"hid_down":@(CGEventSourceCounterForEventType(kCGEventSourceStateHIDSystemState,kCGEventKeyDown)),
        @"hid_up":@(CGEventSourceCounterForEventType(kCGEventSourceStateHIDSystemState,kCGEventKeyUp))};
}
int main(int argc,const char **argv) { @autoreleasepool {
    if(argc==2 && strcmp(argv[1],"--self-test")==0) {
        puts("{\"ok\":true,\"counter_samples\":0,\"input_posting_calls\":0,\"application_created\":false}");
        return NSApp?1:0;
    }
    if(argc!=2) return 2;
    uint64_t duration=strcmp(argv[1],"--observe-readonly-30s")==0?30000000u:
        (strcmp(argv[1],"--observe-readonly-120s")==0?120000000u:0);
    if(!duration) return 2;
    int flags=fcntl(STDIN_FILENO,F_GETFL);
    if(flags<0 || fcntl(STDIN_FILENO,F_SETFL,flags|O_NONBLOCK)<0) return 3;
    uint64_t started=monotonic_us(); if(!started) return 4;
    NSMutableArray *samples=[NSMutableArray array];
    BOOL stopped=NO;
    while(monotonic_us()-started<duration && samples.count<2420) {
        [samples addObject:sample()];
        if(samples.count==1) { puts("{\"ready\":true}"); fflush(stdout); }
        char c=0; ssize_t count=read(STDIN_FILENO,&c,1);
        if(count==0 || (count==1 && c=='q')) {stopped=YES;break;}
        if(count<0 && errno!=EAGAIN && errno!=EWOULDBLOCK && errno!=EINTR) break;
        uint64_t next=monotonic_us()+50000u;
        while(monotonic_us()<next) {
            [NSRunLoop.currentRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:.005]];
            if(monotonic_us()<next) usleep(1000);
        }
    }
    NSDictionary *result=@{@"samples":samples,@"stopped_by_owner_pipe":stopped?@YES:@NO,
        @"input_posting_calls":@0,@"application_created":NSApp?@YES:@NO,
        @"continuous_isolation_verified":@NO,@"human_provenance_verified":@NO};
    NSData *data=[NSJSONSerialization dataWithJSONObject:result options:0 error:nil];
    if(!data || NSApp) return 5;
    fwrite(data.bytes,1,data.length,stdout);putchar('\n');return 0;
} }
