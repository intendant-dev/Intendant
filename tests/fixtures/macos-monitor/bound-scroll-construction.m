// Opt-in native construction regression: production code, no application or posting.
#include "../../../crates/intendant-platform/src/bound_pointer.m"
#include <stdio.h>
#include <limits.h>
int main(void) { @autoreleasepool {
    BOOL ok=YES; unsigned cases=0;
    int32_t deltas[]={-600,-120,-1,1,120,600};
    for(unsigned i=0;i<6;i++) {
        IntendantScroll *s=intendant_scroll_create(getpid(),12345,-550,250,210,220,deltas[i]);
        BOOL exact=s && s->event && s->source;
        if(exact) {
            NSEvent *e=[NSEvent eventWithCGEvent:s->event];
            CGEventSourceStateID id=CGEventSourceGetSourceStateID(s->source);
            exact=e.windowNumber==12345 && e.type==NSEventTypeScrollWheel
                && e.hasPreciseScrollingDeltas && e.scrollingDeltaY==-deltas[i]
                && e.scrollingDeltaX==0 && e.phase==0 && e.momentumPhase==0
                && id!=kCGEventSourceStateHIDSystemState && id!=kCGEventSourceStateCombinedSessionState
                && CGEventGetIntegerValueField(s->event,kCGEventSourceStateID)==id
                && CGPointEqualToPoint(CGEventGetLocation(s->event),CGPointMake(-550,250));
        }
        ok &= exact; cases++; intendant_scroll_release(s);
    }
    int32_t bad[]={0,-601,601,INT32_MIN,INT32_MAX};
    for(unsigned i=0;i<5;i++) {
        void *s=intendant_scroll_create(getpid(),12345,100,100,10,10,bad[i]);
        ok &= s==NULL; cases++; intendant_scroll_release(s);
    }
    void *bad_targets[]={
        intendant_scroll_create(0,12345,100,100,10,10,1),
        intendant_scroll_create(getpid(),0,100,100,10,10,1),
        intendant_scroll_create(getpid(),12345,NAN,100,10,10,1),
        intendant_scroll_create(getpid(),12345,100,INFINITY,10,10,1),
        intendant_scroll_create(getpid(),12345,100,100,-1,10,1),
        intendant_scroll_create(getpid(),12345,100,100,10,16384,1)};
    for(unsigned i=0;i<6;i++) {ok &= bad_targets[i]==NULL;cases++;intendant_scroll_release(bad_targets[i]);}
    ok &= NSApp==nil;
    NSData *d=[NSJSONSerialization dataWithJSONObject:@{@"passed":ok?@YES:@NO,@"cases":@(cases),@"posting_calls":@0,@"application_created":NSApp?@YES:@NO} options:0 error:nil];
    if(!d || fwrite(d.bytes,1,d.length,stdout)!=d.length)return 2;
    putchar(10);return ok?0:1;
}}
