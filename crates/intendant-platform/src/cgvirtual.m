// Small private-API boundary. Original implementation; ABI facts are recorded
// in docs/src/computer-use-and-audio.md. No private class is linked statically.
// ARC owns all objects; -fobjc-arc-exceptions cleans up exception paths too.
#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

// Declarations only: all allocation uses runtime-looked-up Class values after
// Rust's full class/selector/type check. No ivars or private layout assumptions.
@interface CGVirtualDisplayDescriptor : NSObject
@property unsigned int maxPixelsWide;
@property unsigned int maxPixelsHigh;
@property unsigned int vendorID;
@property unsigned int productID;
@property unsigned int serialNum;
@property unsigned int serialNumber;
@property CGSize sizeInMillimeters;
@property CGPoint redPrimary;
@property CGPoint greenPrimary;
@property CGPoint bluePrimary;
@property CGPoint whitePoint;
@property(strong) NSString *name;
@property(strong) id queue;
@end

@interface CGVirtualDisplaySettings : NSObject
@property unsigned int hiDPI;
@property(strong) NSArray *modes;
@end

@interface CGVirtualDisplayMode : NSObject
- (id)initWithWidth:(unsigned int)width height:(unsigned int)height
       refreshRate:(double)refreshRate;
@end

@interface CGVirtualDisplay : NSObject
- (id)initWithDescriptor:(id)descriptor;
- (BOOL)applySettings:(id)settings;
@property(readonly) unsigned int displayID;
@end

@interface IntendantCGVirtualOwner : NSObject
@property(strong) CGVirtualDisplayDescriptor *descriptor;
@property(strong) CGVirtualDisplaySettings *settings;
@property(strong) CGVirtualDisplayMode *mode;
@property(strong) CGVirtualDisplay *display;
@end
@implementation IntendantCGVirtualOwner
@end

// Returns return-type|self-type|selector-type|arg... without stack offsets.
// Bounds are checked before copying. Missing class/selector, truncated metadata
// and exceptions fail closed; no private method is invoked by this probe.
int intendant_cgvirtual_method(const char *class_name, const char *selector_name,
                              int class_method, char *output, size_t capacity) {
    @try {
        @autoreleasepool {
            Class cls = objc_lookUpClass(class_name);
            if (!cls) return 1;
            // Require NSObject ancestry for ARC allocation/release semantics.
            Class ancestor = cls;
            while (ancestor && ancestor != [NSObject class]) {
                ancestor = class_getSuperclass(ancestor);
            }
            if (!ancestor) return 1;
            SEL selector = sel_registerName(selector_name);
            Method method = class_method ? class_getClassMethod(cls, selector)
                                         : class_getInstanceMethod(cls, selector);
            if (!method) return 2;
            unsigned int count = method_getNumberOfArguments(method);
            if (count > 8 || capacity == 0) return 3;
            size_t used = 0;
            for (unsigned int i = 0; i <= count; ++i) {
                char *type = i == 0 ? method_copyReturnType(method)
                                   : method_copyArgumentType(method, i - 1);
                if (!type) return 3;
                size_t length = strlen(type);
                size_t separator = i != 0;
                if (length + separator >= capacity - used) {
                    free(type);
                    return 3;
                }
                if (separator) output[used++] = '|';
                memcpy(output + used, type, length);
                used += length;
                free(type);
            }
            output[used] = 0;
            return 0;
        }
    } @catch (NSException *exception) {
        (void)exception;
        return 4;
    }
}

static double monotonic_seconds(void) {
    struct timespec now = {0};
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
    return (double)now.tv_sec + (double)now.tv_nsec / 1e9;
}

// Only observes the exact object's ID. Bounds both elapsed time and iterations;
// a private native call itself cannot be preempted by this polling deadline.
static int wait_for_display(unsigned int display_id, BOOL online,
                            unsigned int width, unsigned int height) {
    double start = monotonic_seconds();
    if (start < 0) return 1;
    for (unsigned int attempt = 0; attempt < 100; ++attempt) {
        // Refresh the online inventory: per-display cached flags can lag hotplug.
        // A full result could be truncated; fail closed instead of declaring
        // an omitted owned ID removed.
        CGDirectDisplayID ids[32];
        uint32_t count = 0;
        if (CGGetOnlineDisplayList(32, ids, &count) != kCGErrorSuccess || count >= 32) return 1;
        BOOL present = NO;
        for (uint32_t i = 0; i < count; ++i) present |= ids[i] == display_id;
        if (!online && !present) return 0;
        if (online && present && CGDisplayPixelsWide(display_id) == width &&
            CGDisplayPixelsHigh(display_id) == height) return 0;
        double now = monotonic_seconds();
        if (now < 0 || now - start >= 2.0) return 1;
        usleep(20000);
    }
    return 1;
}

int intendant_cgvirtual_create(unsigned int width, unsigned int height,
                              unsigned int serial, void **out_owner,
                              unsigned int *out_id) {
    *out_owner = NULL;
    *out_id = 0;
    @try {
        @autoreleasepool {
            IntendantCGVirtualOwner *owner = [IntendantCGVirtualOwner new];
            if (!owner) return 1;
            // Transfer before any private initialization: Rust must release even
            // failed/partially configured creations via the same teardown path.
            *out_owner = (__bridge_retained void *)owner;
            Class descriptor = objc_lookUpClass("CGVirtualDisplayDescriptor");
            Class settings = objc_lookUpClass("CGVirtualDisplaySettings");
            Class mode = objc_lookUpClass("CGVirtualDisplayMode");
            Class display = objc_lookUpClass("CGVirtualDisplay");
            owner.descriptor = [[descriptor alloc] init];
            owner.settings = [[settings alloc] init];
            owner.mode = [[mode alloc] initWithWidth:width height:height refreshRate:60.0];
            if (!owner.descriptor || !owner.settings || !owner.mode) return 1;
            owner.descriptor.name = @"Intendant experimental monitor";
            owner.descriptor.maxPixelsWide = width;
            owner.descriptor.maxPixelsHigh = height;
            owner.descriptor.sizeInMillimeters = CGSizeMake(width * 25.4 / 96.0, height * 25.4 / 96.0);
            // Fixed sRGB chromaticities; no host display/profile is inspected.
            owner.descriptor.redPrimary = CGPointMake(0.64, 0.33);
            owner.descriptor.greenPrimary = CGPointMake(0.30, 0.60);
            owner.descriptor.bluePrimary = CGPointMake(0.15, 0.06);
            owner.descriptor.whitePoint = CGPointMake(0.3127, 0.3290);
            owner.descriptor.vendorID = 0x494e;
            // Process-local serials never wrap; product distinguishes concurrent
            // harness processes. These EDID labels are not authorization or IDs.
            owner.descriptor.productID = (unsigned int)getpid();
            owner.descriptor.serialNum = serial;
            owner.descriptor.serialNumber = serial;
            owner.descriptor.queue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0);
            owner.settings.hiDPI = 0;
            owner.settings.modes = @[owner.mode];
            owner.display = [[display alloc] initWithDescriptor:owner.descriptor];
            if (!owner.display) return 2;
            if (![owner.display applySettings:owner.settings]) return 3;
            *out_id = owner.display.displayID;
            if (*out_id == 0 || wait_for_display(*out_id, YES, width, height)) return 5;
            return 0;
        }
    } @catch (NSException *exception) {
        (void)exception;
        return 4;
    }
}

int intendant_cgvirtual_destroy(void *raw_owner) {
    @try {
        unsigned int display_id = 0;
        BOOL had_display = NO;
        @autoreleasepool {
            // Consume exactly the +1 owner transferred by create, once. There
            // is no lookup/adoption/destructive CoreGraphics operation by ID.
            IntendantCGVirtualOwner *owner = (__bridge_transfer IntendantCGVirtualOwner *)raw_owner;
            display_id = owner.display.displayID;
            had_display = owner.display != nil;
            owner.display = nil;
            owner.settings = nil;
            owner.mode = nil;
            owner.descriptor = nil;
        }
        // Drain *all* temporary retains before waiting for native removal.
        if (!had_display) return 0;
        if (display_id == 0) return 1; // Cannot confirm what WindowServer removed.
        return wait_for_display(display_id, NO, 0, 0);
    } @catch (NSException *exception) {
        (void)exception;
        return 2;
    }
}

// Read-only geometry through the live retained owner. Never adopt a native ID.
int intendant_cgvirtual_bounds(void *raw_owner, unsigned int expected_id, double *bounds) {
    @try {
        @autoreleasepool {
            if (!raw_owner || !bounds || ![NSThread isMainThread]) return 1;
            IntendantCGVirtualOwner *owner = (__bridge IntendantCGVirtualOwner *)raw_owner;
            unsigned int display_id = owner.display.displayID;
            if (!owner.display || display_id == 0 || display_id != expected_id) return 1;
            CGDirectDisplayID ids[32];
            uint32_t count = 0;
            if (CGGetOnlineDisplayList(32, ids, &count) != kCGErrorSuccess || count >= 32) return 1;
            BOOL present = NO;
            for (uint32_t i = 0; i < count; ++i) present |= ids[i] == display_id;
            if (!present || CGDisplayIsInMirrorSet(display_id) || CGDisplayIsMain(display_id)) return 1;
            CGRect r = CGDisplayBounds(display_id);
            if (CGRectIsEmpty(r) || CGRectIsNull(r) || CGRectIsInfinite(r)) return 1;
            bounds[0] = r.origin.x; bounds[1] = r.origin.y;
            bounds[2] = r.size.width; bounds[3] = r.size.height;
            return 0;
        }
    } @catch (NSException *exception) {
        (void)exception;
        return 1;
    }
}
