// Pure argument/serialization tests; never calls a native observer or posts input.
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include "activity-calibration.h"
static void check(BOOL value) { if(!value) abort(); }
int main(void) { @autoreleasepool {
    NSUInteger assertions=0;
    const char *bad[]={"0","00","01","31","-1","+1","1.0","1 ","","999","abc"};
    for(NSUInteger i=0;i<sizeof(bad)/sizeof(bad[0]);++i) {
        const char *args[]={"supervisor","--calibrate-input-observer",bad[i]};
        check(calibration_seconds(3,args)==0); ++assertions;
        check(observer_calibration_main(3,args)==2); ++assertions;
    }
    for(unsigned seconds=1;seconds<=30;seconds++) {
        char text[4];snprintf(text,sizeof(text),"%u",seconds);
        const char *args[]={"supervisor","--calibrate-input-observer",text};
        check(calibration_seconds(3,args)==seconds); ++assertions;
    }
    const char *args[]={"supervisor","--other","1"};
    check(calibration_seconds(3,args)==0); ++assertions;
    check(calibration_seconds(2,args)==0); ++assertions;
    ActivityCounters counters={{0,1,2,3,UINT32_MAX}};
    NSDictionary *record=calibration_counts(counters);
    check(record.count==5); ++assertions;
    check([record[@"scroll"] unsignedLongLongValue]==UINT32_MAX); ++assertions;
    check([record[@"key_down"] unsignedIntValue]==1); ++assertions;
    check(CFGetTypeID((__bridge CFTypeRef)record[@"any_input"])!=CFBooleanGetTypeID()); ++assertions;
    check([NSJSONSerialization isValidJSONObject:record]); ++assertions;
    NSArray *flags=@[calibration_flag(NO),calibration_flag(YES),
        calibration_flag((Boolean)0),calibration_flag((Boolean)1),
        calibration_flag(1==2),calibration_flag(2==2)];
    NSData *data=[NSJSONSerialization dataWithJSONObject:flags options:0 error:nil];
    NSArray *values=[NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    check(values.count==6); ++assertions;
    for (NSUInteger i=0;i<values.count;++i) {
        check(CFGetTypeID((__bridge CFTypeRef)values[i])==CFBooleanGetTypeID()); ++assertions;
        check([values[i] boolValue]==(i%2!=0)); ++assertions;
    }
    printf("{\"passed\":true,\"assertions\":%lu,\"counter_samples\":0,\"native_input_calls\":0}\n",(unsigned long)assertions);
    return 0;
} }
