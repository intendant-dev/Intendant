// Pure native object/evidence tests. No NSApplication, window, input or AX reads.
#include "focus-witness.h"
#include <unistd.h>
#include <stdio.h>
static void check(BOOL yes) { if (!yes) abort(); }
static FocusSample *fixture(pid_t pid) {
    FocusSample *s=[FocusSample new]; s.status=0; s.pidStatus=0; s.present=YES; s.valid=YES; s.pid=pid;
    s.birth=[NSDate dateWithTimeIntervalSince1970:1];
    s.element=CFBridgingRelease(AXUIElementCreateApplication(pid)); return s;
}
int main(void) { @autoreleasepool {
    NSUInteger cases=0;
    FocusSample *a=fixture(getpid()), *b=fixture(getpid());
    check([focus_changed(a,b) isEqual:@NO]); ++cases;
    check(CFGetTypeID((__bridge CFTypeRef)focus_changed(a,b))==CFBooleanGetTypeID()); ++cases;
    b.birth=[NSDate dateWithTimeIntervalSince1970:2]; check([focus_changed(a,b) isEqual:@YES]); ++cases;
    b=fixture(getpid()+1); check([focus_changed(a,b) isEqual:@YES]); ++cases;
    check(CFGetTypeID((__bridge CFTypeRef)focus_changed(a,b))==CFBooleanGetTypeID()); ++cases;
    b=fixture(getpid()); b.valid=NO; check(focus_changed(a,b)==NSNull.null); ++cases;
    check(focus_changed(b,a)==NSNull.null); ++cases;
    check(focus_changed(nil,a)==NSNull.null); ++cases;
    b=fixture(getpid()); b.element=nil; check(focus_changed(a,b)==NSNull.null); ++cases;
    b=fixture(getpid()); b.birth=nil; check(focus_changed(a,b)==NSNull.null); ++cases;
    NSDictionary *d=a.evidence;
    check(d.count==4 && CFGetTypeID((__bridge CFTypeRef)d[@"valid"])==CFBooleanGetTypeID()); ++cases;
    check(CFGetTypeID((__bridge CFTypeRef)d[@"value_present"])==CFBooleanGetTypeID()); ++cases;
    check(CFGetTypeID((__bridge CFTypeRef)d[@"status"])==CFNumberGetTypeID()); ++cases;
    FocusSample *absent=focus_sample(nil,YES);
    check(!absent.valid && !absent.present && absent.status==kAXErrorFailure); ++cases;
    check([NSJSONSerialization isValidJSONObject:d]); ++cases;
    FocusWitness *w=[FocusWitness new]; [w finish:nil];
    check([w.receipt[@"phase"] isEqual:@"refused"] && !w.active); ++cases;
    [w begin:nil]; check(w.sequence==0 && !w.active); ++cases;
    w.active=YES; w.human=a; w.receiver=b; [w begin:nil];
    check(!w.active && !w.human && !w.receiver); ++cases;
    w.sequence=16; [w begin:nil]; check(w.sequence==16 && !w.active); ++cases;
    printf("{\"passed\":true,\"cases\":%lu,\"native_input_calls\":0,\"native_focus_reads\":0}\n",(unsigned long)cases);
    return 0;
} }
