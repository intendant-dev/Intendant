#pragma once
// Fixture-only HID event counts: no event taps, key identities or input posting.
#include "focus-witness.h"
#import <CoreGraphics/CoreGraphics.h>

enum { ActivityCounterCount = 5 };
typedef struct { uint32_t values[ActivityCounterCount]; } ActivityCounters;
static ActivityCounters activity_sample_for_state(CGEventSourceStateID state) {
    const CGEventType types[ActivityCounterCount] = { kCGAnyInputEventType,
        kCGEventKeyDown, kCGEventKeyUp, kCGEventMouseMoved, kCGEventScrollWheel };
    ActivityCounters result = {{0}};
    for (NSUInteger i=0;i<ActivityCounterCount;i++)
        result.values[i] = CGEventSourceCounterForEventType(state, types[i]);
    return result;
}
static ActivityCounters activity_sample(void) {
    return activity_sample_for_state(kCGEventSourceStateHIDSystemState);
}
static NSDictionary *activity_delta(ActivityCounters before, ActivityCounters after) {
    NSArray *names = @[@"any_input",@"key_down",@"key_up",@"mouse_move",@"scroll"];
    BOOL regressed=NO, changed=NO;
    for (NSUInteger i=0;i<ActivityCounterCount;i++) {
        regressed |= after.values[i] < before.values[i];
        changed |= after.values[i] != before.values[i];
    }
    NSMutableDictionary *deltas=[NSMutableDictionary dictionary];
    for (NSUInteger i=0;i<ActivityCounterCount;i++)
        deltas[names[i]] = regressed ? (id)NSNull.null : @((uint64_t)after.values[i]-before.values[i]);
    return @{@"source":@"hid_system",@"counter_regression":regressed?@YES:@NO,
        @"deltas":deltas,@"changed":regressed ? (id)NSNull.null : (changed?@YES:@NO),
        @"attribution":@"not_authenticated"};
}
static NSDictionary *activity_current_evidence(NSUInteger sequence, ActivityCounters before, ActivityCounters after) {
    return @{@"sequence":@(sequence), @"hid_activity":activity_delta(before,after)};
}
@interface ActivityWitness : FocusWitness
@property ActivityCounters counts;
- (NSDictionary *)currentEvidence;
@end
@implementation ActivityWitness
- (void)begin:(NSRunningApplication *)browser {
    [super begin:browser];
    if (!self.active || ![self.receipt[@"phase"] isEqual:@"before"]) return;
    self.counts=activity_sample();
    NSMutableDictionary *d=[self.receipt mutableCopy];
    d[@"hid_activity"]=@{@"source":@"hid_system",@"sampled":@YES}; self.receipt=d;
}
- (NSDictionary *)currentEvidence {
    if (!self.active) return nil;
    return activity_current_evidence(self.sequence,self.counts,activity_sample());
}
- (void)finish:(NSRunningApplication *)browser {
    BOOL hadStart=self.active;
    [super finish:browser];
    if (!hadStart || ![self.receipt[@"phase"] isEqual:@"after"]) return;
    ActivityCounters after=activity_sample();
    NSMutableDictionary *d=[self.receipt mutableCopy];
    d[@"hid_activity"]=activity_delta(self.counts,after); self.receipt=d;
}
@end
