#import <Cocoa/Cocoa.h>
#import <ApplicationServices/ApplicationServices.h>
#include <stdbool.h>
#include <unistd.h>
#include <stdint.h>
#include <math.h>

// Runtime-checked CoreGraphics SPI. All configuration targets the display
// created by this object; no physical display mode or origin is changed.
@interface NSObject (YiTermVirtualDisplayAPI)
- (id)initWithDescriptor:(id)descriptor;
- (id)initWithWidth:(unsigned int)width height:(unsigned int)height refreshRate:(double)rate;
- (BOOL)applySettings:(id)settings;
- (unsigned int)displayID;
@end

@interface YiTermVirtualDesktop : NSObject
@property(nonatomic, strong) id display;
@property(nonatomic) uint32_t identifier;
@property(nonatomic) uint32_t width;
@property(nonatomic) uint32_t height;
@property(nonatomic) uint32_t scale;
@end
@implementation YiTermVirtualDesktop
@end

static bool validSize(uint32_t w, uint32_t h, uint32_t scale) {
    return w >= 320 && h >= 320 && w <= 1920 && h <= 1920 && scale >= 100 && scale <= 400;
}

static bool applySize(YiTermVirtualDesktop *desktop, uint32_t w, uint32_t h, uint32_t scale) {
    if (!validSize(w, h, scale)) return false;
    // Retina 2x stays within the existing transport/capture memory budgets.
    uint32_t density = scale >= 150 ? 2 : 1;
    if (desktop.width == w && desktop.height == h && desktop.scale == density) return true;
    id mode = [[NSClassFromString(@"CGVirtualDisplayMode") alloc] initWithWidth:w height:h refreshRate:60];
    id settings = [NSClassFromString(@"CGVirtualDisplaySettings") new];
    if (!mode || !settings) return false;
    [settings setValue:@(density == 2) forKey:@"hiDPI"];
    [settings setValue:@[mode] forKey:@"modes"];
    if (![desktop.display applySettings:settings]) return false;
    desktop.width = w; desktop.height = h; desktop.scale = density;
    return true;
}

static void *createOnMain(uint32_t w, uint32_t h, uint32_t scale) {
    @autoreleasepool {
        @try {
            if (!validSize(w, h, scale)) return NULL;
            for (NSString *name in @[@"CGVirtualDisplay", @"CGVirtualDisplayDescriptor", @"CGVirtualDisplaySettings", @"CGVirtualDisplayMode"])
                if (!NSClassFromString(name)) return NULL;
            CGDirectDisplayID displays[64]; uint32_t count = 0;
            if (CGGetActiveDisplayList(64, displays, &count) != kCGErrorSuccess || !count) return NULL;
            double right = 0;
            for (uint32_t i = 0; i < count; i++) right = MAX(right, CGRectGetMaxX(CGDisplayBounds(displays[i])));
            if (!isfinite(right) || right > INT32_MAX - 1920) return NULL;
            id descriptor = [NSClassFromString(@"CGVirtualDisplayDescriptor") new];
            [descriptor setValue:@"YiTerm iPad Desktop" forKey:@"name"];
            [descriptor setValue:@3840 forKey:@"maxPixelsWide"];
            [descriptor setValue:@3840 forKey:@"maxPixelsHigh"];
            [descriptor setValue:@0x594954 forKey:@"vendorID"];
            [descriptor setValue:@1 forKey:@"productID"];
            [descriptor setValue:@(getpid()) forKey:@"serialNum"];
            if ([descriptor respondsToSelector:NSSelectorFromString(@"setSerialNumber:")])
                [descriptor setValue:@(getpid()) forKey:@"serialNumber"];
            [descriptor setValue:dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0) forKey:@"queue"];
            [descriptor setValue:[NSValue valueWithSize:NSMakeSize(250, 180)] forKey:@"sizeInMillimeters"];
            YiTermVirtualDesktop *desktop = [YiTermVirtualDesktop new];
            desktop.display = [[NSClassFromString(@"CGVirtualDisplay") alloc] initWithDescriptor:descriptor];
            if (!desktop.display || !applySize(desktop, w, h, scale)) return NULL;
            CGDirectDisplayID display = [desktop.display displayID];
            desktop.identifier = display;
            for (int i = 0; i < 40 && !CGDisplayIsOnline(display); i++) usleep(50000);
            if (!CGDisplayIsOnline(display)) return NULL;
            CGDisplayConfigRef configuration;
            if (CGBeginDisplayConfiguration(&configuration) != kCGErrorSuccess) return NULL;
            if (CGConfigureDisplayMirrorOfDisplay(configuration, display, kCGNullDirectDisplay) != kCGErrorSuccess ||
                CGConfigureDisplayOrigin(configuration, display, (int32_t)right, 0) != kCGErrorSuccess) {
                CGCancelDisplayConfiguration(configuration); return NULL;
            }
            if (CGCompleteDisplayConfiguration(configuration, kCGConfigureForSession) != kCGErrorSuccess) return NULL;
            return (__bridge_retained void *)desktop;
        } @catch (NSException *exception) { return NULL; }
    }
}

uint32_t yiterm_virtual_display_id(void *handle) {
    @autoreleasepool {
        @try {
            YiTermVirtualDesktop *desktop = (__bridge YiTermVirtualDesktop *)handle;
            return desktop ? desktop.identifier : 0;
        } @catch (NSException *exception) { return 0; }
    }
}

static bool resizeOnMain(void *handle, uint32_t w, uint32_t h, uint32_t scale) {
    @autoreleasepool {
        @try {
            YiTermVirtualDesktop *desktop = (__bridge YiTermVirtualDesktop *)handle;
            return desktop && applySize(desktop, w, h, scale);
        } @catch (NSException *exception) { return false; }
    }
}

void *yiterm_virtual_display_create(uint32_t w, uint32_t h, uint32_t scale) {
    if ([NSThread isMainThread]) return createOnMain(w, h, scale);
    __block void *result = NULL;
    dispatch_sync(dispatch_get_main_queue(), ^{ result = createOnMain(w, h, scale); });
    return result;
}

bool yiterm_virtual_display_resize(void *handle, uint32_t w, uint32_t h, uint32_t scale) {
    if ([NSThread isMainThread]) return resizeOnMain(handle, w, h, scale);
    __block bool result = false;
    dispatch_sync(dispatch_get_main_queue(), ^{ result = resizeOnMain(handle, w, h, scale); });
    return result;
}

void yiterm_virtual_display_destroy(void *handle) {
    if (!handle) return;
    void (^releaseDisplay)(void) = ^{
        @autoreleasepool {
            @try { __unused id desktop = (__bridge_transfer id)handle; }
            @catch (NSException *exception) { }
        }
    };
    // A GUI shutdown may be joining the listener. Never wait for its main
    // thread here; the display is released on the next main-queue turn.
    if ([NSThread isMainThread]) releaseDisplay();
    else dispatch_async(dispatch_get_main_queue(), releaseDisplay);
}
