// Native nonposting validation of the actual fixture constructor and plan parser.
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreGraphics/CoreGraphics.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include "browser-key.h"
int main(int argc,const char **argv) {
    @autoreleasepool {
        (void)key_send; // References only. No target discovery or posting in this test.
        if(argc==2 && !strcmp(argv[1],"--read-plan")) {
            BrowserKeyPlan p={0}; BOOL ok=key_plan_read(&p) && key_plan_valid(p);
            printf("{\"accepted\":%s,\"posted_events\":0}\n",ok?"true":"false");
            return ok?0:1;
        }
        if(argc!=1) return 2;
        BOOL ok=YES; NSUInteger cases=0;
        ok &= key_cleanup_pending(YES,YES,NO);++cases;
        ok &= !key_cleanup_pending(YES,YES,YES);++cases;
        ok &= !key_cleanup_pending(YES,NO,NO);++cases;
        ok &= !key_cleanup_pending(NO,YES,NO);++cases;
        BrowserKeyPlan p={.version=1,.window=12345,.bounds=CGRectMake(-800,20,720,530),.tag=999};
        ok &= key_plan_valid(p);++cases;
        BrowserKeyPlan invalid[8];for(unsigned i=0;i<8;++i) invalid[i]=p;
        invalid[0].version=0;invalid[1].window=0;invalid[2].tag=0;invalid[3].bounds.origin.x=NAN;
        invalid[4].bounds.size.width=0;invalid[5].bounds.size.height=17000;
        invalid[6].bounds.origin.y=INFINITY;invalid[7].tag=-1;
        for(unsigned i=0;i<8;++i) { ok &= !key_plan_valid(invalid[i]);++cases; }
        CGEventFlags flags[]={kCGEventFlagMaskShift,kCGEventFlagMaskControl,kCGEventFlagMaskAlternate,
            kCGEventFlagMaskCommand,kCGEventFlagMaskSecondaryFn};
        for(unsigned i=0;i<5;++i) { ok &= key_shortcut_held(flags[i]);++cases; }
        ok &= !key_shortcut_held(0);++cases;
        ok &= !key_shortcut_held(kCGEventFlagMaskAlphaShift);++cases;
        CGEventSourceRef source=CGEventSourceCreate(kCGEventSourceStatePrivate);
        for(unsigned i=0;i<2;++i) {
            CGEventType type=i?kCGEventKeyUp:kCGEventKeyDown;
            CGEventRef e=make_key(source,type,p.window,getpid(),p.tag);
            ok &= matches(source,e,type,p.window,getpid(),p.tag);++cases;
            if(e) { CGEventSetFlags(e,kCGEventFlagMaskCommand);ok &= !matches(source,e,type,p.window,getpid(),p.tag);CFRelease(e); }
            else ok=NO;
            ++cases;
        }
        if(source) CFRelease(source);
        ok &= NSApp==nil;
        printf("{\"passed\":%s,\"cases\":%lu,\"posted_events\":0,\"application_created\":false}\n",ok?"true":"false",(unsigned long)cases);
        return ok?0:1;
    }
}
