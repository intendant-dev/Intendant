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
static NSDictionary *click_result_with(NSDictionary *base, NSDictionary *entries) {
    NSMutableDictionary *copy = [base mutableCopy];
    [copy addEntriesFromDictionary:entries];
    return copy;
}
int main(int argc,const char **argv) {
    @autoreleasepool {
        (void)browser_pointer_read; (void)browser_pointer_send; (void)click_key_plan_evidence;
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
        BrowserPointerPlan click={.version=1,.window=p.window,.bounds=p.bounds,
            .global=CGPointMake(-627,177),.local=CGPointMake(173,157),.tag=1234};
        pid_t sender=getpid(), target=sender+1;
        NSDictionary *clickResult=@{@"dispatch_attempted":@YES,@"effect_verified":@NO,@"posted_events":@2,
            @"source_pid":@(sender),@"target_pid":@(target),@"window_id":@(click.window),@"tag":@(click.tag)};
        ok &= browser_click_key_mode("--disposable-chromium-click-key");++cases;
        ok &= !browser_click_key_mode("--disposable-chromium-key");++cases;
        ok &= !browser_click_key_mode("--disposable-chromium-pointer");++cases;
        ok &= !browser_click_key_mode("--disposable-chromium");++cases;
        ok &= click_key_gate(click,p,clickResult,sender,target);++cases;
        ok &= !click_key_gate(click,p,nil,sender,target);++cases;
        ok &= !click_key_gate(click,p,[@{ @"dispatch_attempted":@NO } mutableCopy],sender,target);++cases;
        BrowserKeyPlan wrongWindow=p;wrongWindow.window++;
        ok &= !click_key_gate(click,wrongWindow,clickResult,sender,target);++cases;
        BrowserKeyPlan wrongBounds=p;wrongBounds.bounds.size.width--;
        ok &= !click_key_gate(click,wrongBounds,clickResult,sender,target);++cases;
        ok &= !click_key_gate(click,p,click_result_with(clickResult,@{@"target_pid":@(target+1)}),sender,target);++cases;
        ok &= !click_key_gate(click,p,click_result_with(clickResult,@{@"source_pid":@(sender+1)}),sender,target);++cases;
        ok &= !click_key_gate(click,p,click_result_with(clickResult,@{@"tag":@(click.tag+1)}),sender,target);++cases;
        ok &= !click_key_gate(click,p,click_result_with(clickResult,@{@"posted_events":@YES}),sender,target);++cases;
        ok &= !click_key_gate(click,p,click_result_with(clickResult,@{@"window_id":@"12345"}),sender,target);++cases;
        ok &= !click_key_gate(click,p,click_result_with(clickResult,@{@"error":@"late native error"}),sender,target);++cases;
        BrowserKeyPlan sameTag=p;sameTag.tag=click.tag;
        ok &= !click_key_gate(click,sameTag,clickResult,sender,target);++cases;
        BrowserPointerPlan malformedClick=click;malformedClick.global.x=NAN;
        ok &= !click_key_gate(malformedClick,p,clickResult,sender,target);++cases;
        malformedClick=click;malformedClick.local.y++;
        ok &= !click_key_gate(malformedClick,p,clickResult,sender,target);++cases;
        (void)click_receipt_read;
        BrowserClickReceipt receipt={.click=click,.key_tag=p.tag,.challenge=777,.version=1,
            .click_count=1,.client=CGPointMake(173,70),.viewport=CGSizeMake(720,443)};
        ok &= click_receipt_valid(receipt,click,777);++cases;
        BrowserClickReceipt bad[13]; for(unsigned i=0;i<13;i++) bad[i]=receipt;
        bad[0].version=0;bad[1].click_count=0;bad[2].click_count=2;bad[3].challenge=778;
        bad[4].key_tag=click.tag;bad[5].click.window++;bad[6].click.bounds.origin.x++;
        bad[7].click.global.x++;bad[8].client.y++;bad[9].viewport.width++;
        bad[10].viewport.height=NAN;bad[11].key_tag=0;bad[12].client.x=INFINITY;
        for(unsigned i=0;i<13;i++) {ok &= !click_receipt_valid(bad[i],click,777);++cases;}
        ClickReceiptState state={0};
        ok &= !click_receipt_take(&state,p.tag);++cases;
        ok &= click_receipt_accept(&state,receipt,click,777,YES);++cases;
        ok &= click_receipt_take(&state,p.tag);++cases;
        ok &= !click_receipt_take(&state,p.tag);++cases;
        ok &= !click_receipt_accept(&state,receipt,click,777,YES);++cases;
        state=(ClickReceiptState){0};
        ok &= !click_receipt_accept(&state,receipt,click,777,NO);++cases;
        ok &= !click_receipt_accept(&state,receipt,click,777,YES);++cases;
        state=(ClickReceiptState){0};
        ok &= click_receipt_accept(&state,receipt,click,777,YES);++cases;
        ok &= !click_receipt_take(&state,p.tag+1);++cases;
        ok &= !click_receipt_take(&state,p.tag);++cases;
        state=(ClickReceiptState){0};
        ok &= click_receipt_accept(&state,receipt,click,777,YES);++cases;
        ok &= !click_receipt_accept(&state,receipt,click,777,YES);++cases;
        ok &= !click_receipt_take(&state,p.tag);++cases;
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
