// Nonposting tests include the production constructor and posting loop.
#include "../../../crates/intendant-platform/src/bound_arrow.m"
#include <stdio.h>
static unsigned callbacks,throwAt;
static CGKeyCode expectedKey;
static void fake_post(pid_t pid,CGEventRef event) {
    if(pid!=getpid() || !event || CGEventGetIntegerValueField(event,kCGKeyboardEventKeycode)!=expectedKey) @throw [NSException exceptionWithName:@"fixture" reason:nil userInfo:nil];
    callbacks++;
    if(callbacks==throwAt) @throw [NSException exceptionWithName:@"fixture" reason:nil userInfo:nil];
}
int main(void) { @autoreleasepool {
    unsigned cases=0; BOOL ok=YES;
    for (uint8_t left=0;left<=1;left++) {
    expectedKey=left?kVK_LeftArrow:kVK_RightArrow;
    CGEventSourceRef source=CGEventSourceCreate(kCGEventSourceStatePrivate);
    for(unsigned i=0;i<2;i++) {
        CGEventType type=i?kCGEventKeyUp:kCGEventKeyDown;
        CGEventRef event=make_key(source,type,12345,getpid(),1234,left);
        ok &= matches(source,event,type,12345,getpid(),1234,left);cases++;
        ok &= !matches(source,event,type,12346,getpid(),1234,left);cases++;
        ok &= !matches(source,event,type,12345,getpid(),4321,left);cases++;
        ok &= !matches(source,event,type,12345,getpid(),1234,1-left);cases++;
        ok &= !matches(source,event,type,12345,getpid(),1234,2);cases++;
        if(event) {
            CGEventSetFlags(event,kCGEventFlagMaskCommand);
            ok &= !matches(source,event,type,12345,getpid(),1234,left); cases++;
            CGEventSetFlags(event,0); CGEventSetIntegerValueField(event,kCGKeyboardEventAutorepeat,1);
            ok &= !matches(source,event,type,12345,getpid(),1234,left); cases++;
            CFRelease(event);
        } else ok=NO;
    }
    if(source) CFRelease(source);
    for(unsigned fault=0;fault<=2;fault++) {
        ArrowPair *p=intendant_arrow_create(getpid(),12345,left); if(!p) return 2;
        callbacks=0;throwAt=fault;uint8_t failed=1;
        unsigned calls=execute_arrow_pair(p,&failed,YES,fake_post);
        ok &= calls==(fault?fault:2) && callbacks==calls && failed==(fault?1:0); cases++;
        ok &= execute_arrow_pair(p,&failed,YES,fake_post)==0 && failed==1 && callbacks==calls;cases++;
        intendant_arrow_release(p);
    }
    ArrowPair *p=intendant_arrow_create(getpid(),12345,left);if(!p) return 2;
    callbacks=0;throwAt=0;uint8_t failed=0;
    ok &= execute_arrow_pair(p,&failed,NO,fake_post)==0 && failed==1;cases++;
    ok &= execute_arrow_pair(p,&failed,YES,fake_post)==0 && callbacks==0;cases++;
    intendant_arrow_release(p);
    ok &= intendant_arrow_create(0,12345,left)==NULL;cases++;
    ok &= intendant_arrow_create(getpid(),0,left)==NULL;cases++;
    ok &= !key_shortcut_held(kCGEventFlagMaskAlphaShift);cases++;
    ok &= key_shortcut_held(kCGEventFlagMaskCommand);cases++;
    ok &= key_shortcut_held(kCGEventFlagMaskShift);cases++;
    ok &= key_shortcut_held(kCGEventFlagMaskSecondaryFn);cases++;
    }
    ok &= intendant_arrow_create(getpid(),12345,2)==NULL;cases++;
    ok &= intendant_arrow_create(getpid(),12345,255)==NULL;cases++;
    NSData *data=[NSJSONSerialization dataWithJSONObject:@{@"passed":@(ok),@"cases":@(cases),
        @"native_posting_calls":@0,@"application_created":NSApp?@YES:@NO} options:0 error:nil];
    if(!data || NSApp) return 3; fwrite(data.bytes,1,data.length,stdout);putchar(10);return ok?0:1;
} }
