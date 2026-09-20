// Native regression for the PRODUCTION posting loop. The injected callback
// never calls CoreGraphics, creates an application, or touches a real display.
#include "../../../crates/intendant-platform/src/bound_pointer.m"
#include <stdio.h>
static int observed, throwAt;
static void fake_post(pid_t pid, CGEventRef event) {
    (void)pid; (void)event;
    ++observed;
    if (observed == throwAt) [NSException raise:@"fixture" format:@"synthetic posting failure"];
}
int main(void) {
    @autoreleasepool {
        BOOL ok=YES; unsigned cases=0;
        for (int failure=0; failure<=2; ++failure) {
            IntendantPointerPair pair={0}; pair.pid=123;
            observed=0;throwAt=failure;uint8_t failed=9;
            uint8_t calls=execute_pointer_pair(&pair,&failed,fake_post);
            ok &= calls == (failure==1 ? 1 : 2) && failed == (failure ? 1 : 0) && observed==calls;
            ++cases;
            int before=observed;
            ok &= execute_pointer_pair(&pair,&failed,fake_post)==0 && failed==1 && observed==before;
            ++cases;
        }
        uint8_t failed=9;IntendantPointerPair pair={0};
        ok &= execute_pointer_pair(NULL,&failed,fake_post)==0 && failed==1;++cases;
        ok &= execute_pointer_pair(&pair,&failed,NULL)==0 && failed==1;++cases;
        ok &= execute_pointer_pair(&pair,NULL,fake_post)==0;++cases;
        ok &= NSApp==nil;
        printf("{\"passed\":%s,\"cases\":%u,\"native_posting_calls\":0,\"application_created\":false}\n",ok?"true":"false",cases);
        return ok?0:1;
    }
}
