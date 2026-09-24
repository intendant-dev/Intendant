// Pure counter/lifecycle tests. No counter sampling, AX reads or event posting.
#include "activity-witness.h"
#include <stdio.h>
static void check(BOOL ok) { if(!ok) abort(); }
int main(void) { @autoreleasepool {
    NSUInteger cases=0;
    ActivityCounters a={{10,20,30,40,50}},b=a;
    NSDictionary *d=activity_delta(a,b);
    check(d.count==5 && [d[@"changed"] isEqual:@NO]);++cases;
    check(CFGetTypeID((__bridge CFTypeRef)d[@"changed"])==CFBooleanGetTypeID());++cases;
    for(NSUInteger i=0;i<ActivityCounterCount;i++) {
        b=a;b.values[i]+=3;d=activity_delta(a,b);
        check([d[@"changed"] isEqual:@YES] && [d[@"counter_regression"] isEqual:@NO]);++cases;
        uint64_t sum=0;for(NSNumber *n in [d[@"deltas"] allValues])sum+=n.unsignedLongLongValue;
        check(sum==3);++cases;
        b=a;b.values[i]-=1;d=activity_delta(a,b);
        check(d[@"changed"]==NSNull.null && [d[@"counter_regression"] isEqual:@YES]);++cases;
        for(id value in [d[@"deltas"] allValues])check(value==NSNull.null);++cases;
    }
    a=(ActivityCounters){{0}};b=(ActivityCounters){{UINT32_MAX,UINT32_MAX,UINT32_MAX,UINT32_MAX,UINT32_MAX}};
    d=activity_delta(a,b);check([d[@"deltas"][@"key_down"] unsignedLongLongValue]==UINT32_MAX);++cases;
    check([NSJSONSerialization isValidJSONObject:d]);++cases;
    ActivityWitness *w=[ActivityWitness new];[w begin:nil];check(!w.active && !w.receipt[@"hid_activity"]);++cases;
    [w finish:nil];check(!w.active && !w.receipt[@"hid_activity"]);++cases;
    printf("{\"passed\":true,\"cases\":%lu,\"counter_samples\":0,\"native_input_calls\":0}\n",(unsigned long)cases);
    return 0;
} }
