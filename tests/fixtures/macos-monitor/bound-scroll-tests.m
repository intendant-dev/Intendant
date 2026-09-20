// Nonposting regression at the production scroll loop, separate from the
// unchanged click-loop regression. No app/window, event creation or OS input.
#include "../../../crates/intendant-platform/src/bound_pointer.m"
#include <stdio.h>
static int observed;
static BOOL throwOnPost;
static void fake_post(pid_t pid, CGEventRef event) {
    (void)pid; (void)event;
    ++observed;
    if (throwOnPost) [NSException raise:@"fixture" format:@"synthetic scroll failure"];
}
int main(void) {
    @autoreleasepool {
        BOOL ok=YES; unsigned cases=0;
        for (int failure=0; failure<=1; ++failure) {
            IntendantScroll scroll={0}; scroll.pid=123;
            observed=0;throwOnPost=failure;uint8_t failed=9;
            uint8_t calls=execute_scroll(&scroll,&failed,fake_post);
            ok &= calls==1 && failed==failure && observed==1 && scroll.used==1;
            ++cases;
            ok &= execute_scroll(&scroll,&failed,fake_post)==0 && failed==1 && observed==1;
            ++cases;
        }
        uint8_t failed=9;IntendantScroll scroll={0};
        ok &= execute_scroll(NULL,&failed,fake_post)==0 && failed==1;++cases;
        ok &= execute_scroll(&scroll,&failed,NULL)==0 && failed==1;++cases;
        ok &= execute_scroll(&scroll,NULL,fake_post)==0;++cases;
        ok &= NSApp==nil;
        printf("{\"passed\":%s,\"cases\":%u,\"native_posting_calls\":0,\"application_created\":false}\n",ok?"true":"false",cases);
        return ok?0:1;
    }
}
