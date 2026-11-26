#import <Cocoa/Cocoa.h>
#import <ApplicationServices/ApplicationServices.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdint.h>
#include <signal.h>
#include <unistd.h>
#include <math.h>

// Runtime-checked CoreGraphics SPI, following Cindy's standalone display owner.
// Unlike its mirroring prototype, only the newly created extended screen is configured.
@interface NSObject (YiTermVirtualDisplayAPI)
- (id)initWithDescriptor:(id)descriptor;
- (id)initWithWidth:(unsigned int)width height:(unsigned int)height refreshRate:(double)rate;
- (BOOL)applySettings:(id)settings;
- (unsigned int)displayID;
@end

@interface YiTermVirtualDesktop : NSObject
@property(nonatomic, strong) id display;
@property(nonatomic, strong) NSArray<NSNumber *> *existing;
@property(nonatomic, strong) NSArray<NSDictionary *> *originalLayout;
@property(nonatomic) CGDirectDisplayID originalMain;
@property(nonatomic) BOOL headless;
@property(nonatomic) int32_t rightY;
@property(nonatomic) int32_t right;
@property(nonatomic) uint32_t width;
@property(nonatomic) uint32_t height;
@property(nonatomic) uint32_t density;
@property(nonatomic) uint32_t identifier;
@property(nonatomic) BOOL busy;
@property(nonatomic) BOOL initial;
@property(nonatomic) BOOL fallback;
@property(nonatomic) CFAbsoluteTime deadline;
@property(nonatomic) CGError lastError;
@property(nonatomic) CFAbsoluteTime lastModeAttempt;
@property(nonatomic) BOOL choseNearest;
@property(nonatomic) uint32_t selectedWidth;
@property(nonatomic) uint32_t selectedHeight;
@property(nonatomic, copy) NSString *stage;
@end
@implementation YiTermVirtualDesktop
@end

static YiTermVirtualDesktop *owner;
static dispatch_source_t termination;
static dispatch_source_t interrupt;
static uint32_t serialSeed;

static void emit(NSDictionary *value) {
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:0 error:nil];
    if (!data) return;
    fwrite(data.bytes, 1, data.length, stdout);
    fputc('\n', stdout);
    fflush(stdout);
}

static void cleanup(void) {
    owner.display = nil;
    owner = nil;
}

static void fail(NSString *stage) {
    owner.stage = stage;
    fprintf(stderr, "YiTerm virtual display failed: stage=%s error=%d id=%u\n",
            stage.UTF8String, (int)owner.lastError, owner.identifier);
    owner.busy = NO;
    if (owner.initial) owner.display = nil;
    emit(@{@"error": stage, @"code": @((int)owner.lastError)});
}

static BOOL registered(CGDirectDisplayID display) {
    CGDirectDisplayID ids[128]; uint32_t count = 0;
    if (CGGetOnlineDisplayList(128, ids, &count) != kCGErrorSuccess) return NO;
    for (uint32_t i = 0; i < count; i++) if (ids[i] == display) return YES;
    return NO;
}

static BOOL overlapsExisting(CGDirectDisplayID display) {
    CGRect bounds = CGDisplayBounds(display);
    if (CGRectIsEmpty(bounds)) return YES;
    for (NSNumber *number in owner.existing) {
        CGDirectDisplayID id = number.unsignedIntValue;
        if (id != display && registered(id) && CGRectIntersectsRect(bounds, CGDisplayBounds(id))) return YES;
    }
    return NO;
}

static BOOL applySize(void) {
    id mode = [[NSClassFromString(@"CGVirtualDisplayMode") alloc]
               initWithWidth:owner.width height:owner.height refreshRate:60];
    id settings = [NSClassFromString(@"CGVirtualDisplaySettings") new];
    if (!mode || !settings) return NO;
    [settings setValue:@(owner.density == 2) forKey:@"hiDPI"];
    [settings setValue:@0 forKey:@"rotation"];
    [settings setValue:@[mode] forKey:@"modes"];
    return [owner.display applySettings:settings];
}

// applySettings publishes modes; it does not guarantee WindowServer switches
// away from the default 800x600 mode. Activate a mode on this virtual ID only.
static BOOL selectMode(CGDirectDisplayID display, BOOL nearest) {
    if (display != [owner.display displayID] || (CGMainDisplayID() == display && !owner.headless)) return NO;
    CFArrayRef modes = CGDisplayCopyAllDisplayModes(display,
        (__bridge CFDictionaryRef)@{(__bridge NSString *)kCGDisplayShowDuplicateLowResolutionModes: @YES});
    if (!modes) return NO;
    CGDisplayModeRef selected = NULL;
    double best = HUGE_VAL;
    for (CFIndex i = 0; i < CFArrayGetCount(modes); i++) {
        CGDisplayModeRef mode = (CGDisplayModeRef)CFArrayGetValueAtIndex(modes, i);
        size_t w = CGDisplayModeGetWidth(mode), h = CGDisplayModeGetHeight(mode);
        size_t pw = CGDisplayModeGetPixelWidth(mode), ph = CGDisplayModeGetPixelHeight(mode);
        if (w < 640 || h < 480 || w > 1920 || h > 1920 || pw > 3840 || ph > 3840) continue;
        if (!nearest && (w != owner.width || h != owner.height)) continue;
        double dx = log((double)w / owner.width), dy = log((double)h / owner.height);
        double density = (double)pw / w;
        double score = dx * dx + dy * dy + fabs(density - owner.density) * 0.01;
        if (score < best) { best = score; selected = mode; }
    }
    BOOL success = NO;
    if (selected) {
        owner.selectedWidth = (uint32_t)CGDisplayModeGetWidth(selected);
        owner.selectedHeight = (uint32_t)CGDisplayModeGetHeight(selected);
        CGDisplayModeRef current = CGDisplayCopyDisplayMode(display);
        BOOL same = current && CGDisplayModeGetIODisplayModeID(current) == CGDisplayModeGetIODisplayModeID(selected);
        if (current) CGDisplayModeRelease(current);
        if (same) success = YES;
        else {
            owner.stage = @"CGDisplaySetDisplayMode(virtual only)";
            owner.lastError = CGDisplaySetDisplayMode(display, selected, NULL);
            success = owner.lastError == kCGErrorSuccess;
        }
    }
    CFRelease(modes);
    return success;
}

static void complete(CGDirectDisplayID display, BOOL approximate) {
    CGRect bounds = CGDisplayBounds(display);
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(display);
    unsigned int density = mode && CGDisplayModeGetWidth(mode)
        ? (unsigned int)MAX(1, CGDisplayModeGetPixelWidth(mode) / CGDisplayModeGetWidth(mode)) : 1;
    if (mode) CGDisplayModeRelease(mode);
    if (approximate) {
        fprintf(stderr, "YiTerm: requested virtual desktop %ux%u; using supported %.0fx%.0f at %ux\n",
                owner.width, owner.height, bounds.size.width, bounds.size.height, density);
    }
    owner.busy = NO;
    emit(@{@"id": @(display), @"width": @(bounds.size.width), @"height": @(bounds.size.height), @"density": @(density)});
}

static BOOL createDisplay(void) {
    id descriptor = [NSClassFromString(@"CGVirtualDisplayDescriptor") new];
    [descriptor setValue:@"YiTerm iPad Desktop" forKey:@"name"];
    [descriptor setValue:@3840 forKey:@"maxPixelsWide"];
    [descriptor setValue:@3840 forKey:@"maxPixelsHigh"];
    [descriptor setValue:@0x5949 forKey:@"vendorID"];
    [descriptor setValue:@1 forKey:@"productID"];
    if (serialSeed == 0) serialSeed = arc4random();
    uint32_t serial = ++serialSeed;
    if (serial == 0) serial = ++serialSeed;
    [descriptor setValue:@(serial) forKey:@"serialNum"];
    if ([descriptor respondsToSelector:NSSelectorFromString(@"setSerialNumber:")])
        [descriptor setValue:@(serial) forKey:@"serialNumber"];
    [descriptor setValue:dispatch_get_main_queue() forKey:@"queue"];
    [descriptor setValue:[NSValue valueWithSize:NSMakeSize(250, 180)] forKey:@"sizeInMillimeters"];
    // Complete color descriptors, matching the published Chromium SPI reference.
    [descriptor setValue:[NSValue valueWithPoint:NSMakePoint(0.3125, 0.3291)] forKey:@"whitePoint"];
    [descriptor setValue:[NSValue valueWithPoint:NSMakePoint(0.6797, 0.3203)] forKey:@"redPrimary"];
    [descriptor setValue:[NSValue valueWithPoint:NSMakePoint(0.2559, 0.6983)] forKey:@"greenPrimary"];
    [descriptor setValue:[NSValue valueWithPoint:NSMakePoint(0.1494, 0.0557)] forKey:@"bluePrimary"];
    owner.display = [[NSClassFromString(@"CGVirtualDisplay") alloc] initWithDescriptor:descriptor];
    return owner.display && applySize();
}

static BOOL arrange(CGDirectDisplayID display) {
    // macOS's 'virt' placeholder is not a physical screen. A headless Mac
    // naturally makes its first real virtual display primary.
    if (owner.headless && CGMainDisplayID() == display && !CGDisplayIsInMirrorSet(display)) return YES;
    BOOL restoreMain = CGMainDisplayID() != owner.originalMain;
    if (restoreMain && (!owner.originalMain || !registered(owner.originalMain))) {
        owner.stage = @"original main display is unavailable";
        return NO;
    }
    BOOL mirrored = CGDisplayIsInMirrorSet(display) != 0;
    BOOL overlap = overlapsExisting(display);
    if (!restoreMain && !mirrored && !overlap) return YES;
    CGDisplayConfigRef configuration = NULL;
    owner.stage = @"CGBeginDisplayConfiguration";
    owner.lastError = CGBeginDisplayConfiguration(&configuration);
    if (owner.lastError != kCGErrorSuccess) return NO;
    if (mirrored) {
        owner.stage = @"CGConfigureDisplayMirrorOfDisplay(virtual only)";
        owner.lastError = CGConfigureDisplayMirrorOfDisplay(configuration, display, kCGNullDirectDisplay);
        if (owner.lastError != kCGErrorSuccess) { CGCancelDisplayConfiguration(configuration); return NO; }
    }
    if (restoreMain) {
        // Re-anchor the saved main display first. This restores the layout
        // macOS translated when the new display automatically became primary.
        // Do not reposition mirror followers: they follow their original source.
        owner.stage = @"restore original main display origin";
        if (CGDisplayMirrorsDisplay(owner.originalMain) != kCGNullDirectDisplay) {
            CGCancelDisplayConfiguration(configuration);
            owner.stage = @"original main is a mirror follower; refusing to alter its mirror set";
            return NO;
        }
        owner.lastError = CGConfigureDisplayOrigin(configuration, owner.originalMain, 0, 0);
        if (owner.lastError != kCGErrorSuccess) { CGCancelDisplayConfiguration(configuration); return NO; }
        for (NSDictionary *saved in owner.originalLayout) {
            CGDirectDisplayID id = [saved[@"id"] unsignedIntValue];
            if (id == owner.originalMain || !registered(id) ||
                CGDisplayMirrorsDisplay(id) != kCGNullDirectDisplay) continue;
            owner.stage = @"restore existing display origin";
            owner.lastError = CGConfigureDisplayOrigin(configuration, id,
                [saved[@"x"] intValue], [saved[@"y"] intValue]);
            if (owner.lastError != kCGErrorSuccess) { CGCancelDisplayConfiguration(configuration); return NO; }
        }
    }
    if (overlap || restoreMain) {
        owner.stage = @"CGConfigureDisplayOrigin(virtual only)";
        owner.lastError = CGConfigureDisplayOrigin(configuration, display, owner.right, owner.rightY);
        if (owner.lastError != kCGErrorSuccess) { CGCancelDisplayConfiguration(configuration); return NO; }
    }
    owner.stage = @"CGCompleteDisplayConfiguration";
    owner.lastError = CGCompleteDisplayConfiguration(configuration, kCGConfigureForSession);
    return NO; // Observe asynchronous layout completion on a later main-queue turn.
}

static void pollDisplay(void) {
    if (!owner.busy) return;
    @try {
        owner.identifier = [owner.display displayID];
        CGDirectDisplayID display = owner.identifier;
        CGDisplayModeRef mode = CGDisplayCopyDisplayMode(display);
        CGRect bounds = CGDisplayBounds(display);
        BOOL hasGeometry = registered(display) && !CGRectIsEmpty(bounds);
        BOOL correctMode = mode && fabs(bounds.size.width - owner.width) < 1
            && fabs(bounds.size.height - owner.height) < 1;
        if (mode) CGDisplayModeRelease(mode);
        if (hasGeometry && !arrange(display)) {
            if (CFAbsoluteTimeGetCurrent() >= owner.deadline) { fail(owner.stage); return; }
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 50 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{ pollDisplay(); });
            return;
        }
        if (hasGeometry && correctMode && arrange(display)) { complete(display, NO); return; }
        if (owner.choseNearest && hasGeometry && fabs(bounds.size.width - owner.selectedWidth) < 1
            && fabs(bounds.size.height - owner.selectedHeight) < 1 && arrange(display)) {
            complete(display, YES); return;
        }
        if (!owner.choseNearest && hasGeometry && !correctMode && CFAbsoluteTimeGetCurrent() - owner.lastModeAttempt >= 0.25) {
            owner.lastModeAttempt = CFAbsoluteTimeGetCurrent();
            selectMode(display, NO);
        }
        if (CFAbsoluteTimeGetCurrent() >= owner.deadline) {
            if (!owner.choseNearest && hasGeometry && !correctMode && selectMode(display, YES)) {
                owner.choseNearest = YES;
                owner.deadline = CFAbsoluteTimeGetCurrent() + 1.0;
                dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 50 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{ pollDisplay(); });
                return;
            }
            // A mode switch can fail while this owned display still has a
            // usable desktop. Keep that mode instead of destroying the session.
            if (hasGeometry && display == [owner.display displayID] &&
                bounds.size.width >= 640 && bounds.size.height >= 480 &&
                bounds.size.width <= 1920 && bounds.size.height <= 1920 && arrange(display)) {
                complete(display, YES); return;
            }
            if (owner.initial && owner.density == 2 && !owner.fallback &&
                (!hasGeometry || bounds.size.width < 640 || bounds.size.height < 480)) {
                // A 1x screen retains readable logical dimensions on systems
                // unable to register the requested HiDPI mode.
                owner.fallback = YES;
                owner.choseNearest = NO;
                owner.lastModeAttempt = 0;
                owner.density = 1;
                owner.display = nil;
                fprintf(stderr, "YiTerm: retrying virtual display registration at 1x\n");
                if (!createDisplay()) { fail(@"1x virtual display creation failed"); return; }
                owner.stage = @"waiting for 1x virtual display registration";
                owner.deadline = CFAbsoluteTimeGetCurrent() + 4.0;
            } else {
                CGRect bounds = CGDisplayBounds(display);
                fprintf(stderr, "YiTerm virtual display registration: id=%u registered=%d bounds=%.0fx%.0f expected=%ux%u\n",
                        display, (int)registered(display), bounds.size.width, bounds.size.height, owner.width, owner.height);
                fail(owner.stage); return;
            }
        }
        // Return to dispatch_main instead of sleeping or nesting a run loop
        // inside a synchronous network-thread/main-thread call.
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 50 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{ pollDisplay(); });
    } @catch (NSException *exception) {
        fprintf(stderr, "YiTerm virtual display exception: %s: %s\n", exception.name.UTF8String, exception.reason.UTF8String);
        fail(@"CoreGraphics virtual display exception");
    }
}

static void request(NSDictionary *command) {
    @try {
        if (owner.busy) { emit(@{@"error": @"virtual display request already pending"}); return; }
        for (NSString *name in @[@"CGVirtualDisplay", @"CGVirtualDisplayDescriptor", @"CGVirtualDisplaySettings", @"CGVirtualDisplayMode"])
            if (!NSClassFromString(name)) { emit(@{@"error": [@"Unavailable macOS interface: " stringByAppendingString:name]}); return; }
        unsigned int w = [command[@"width"] unsignedIntValue], h = [command[@"height"] unsignedIntValue];
        unsigned int scale = [command[@"scale"] unsignedIntValue];
        if (w < 320 || h < 320 || w > 1920 || h > 1920 || scale < 100 || scale > 400) {
            emit(@{@"error": @"invalid virtual display dimensions"}); return;
        }
        owner.busy = YES;
        owner.initial = owner.display == nil;
        owner.fallback = NO;
        owner.lastError = kCGErrorSuccess;
        owner.stage = @"waiting for virtual display registration";
        if (owner.initial || CGMainDisplayID() != [owner.display displayID]) {
            CGDirectDisplayID currentVirtual = owner.display ? [owner.display displayID] : 0;
            CGDirectDisplayID ids[128]; uint32_t count = 0;
            CGError error = CGGetActiveDisplayList(128, ids, &count);
            if (error != kCGErrorSuccess || count == 0) { owner.lastError = error; fail(@"no active source display"); return; }
            NSMutableArray *existing = [NSMutableArray array];
            NSMutableArray *layout = [NSMutableArray array];
            double right = 0, rightY = 0;
            owner.originalMain = CGMainDisplayID();
            owner.headless = YES;
            for (uint32_t i = 0; i < count; i++) {
                if (ids[i] == currentVirtual) continue;
                CGRect bounds = CGDisplayBounds(ids[i]);
                if (CGDisplayModelNumber(ids[i]) != 0x76697274) owner.headless = NO;
                [existing addObject:@(ids[i])];
                [layout addObject:@{@"id": @(ids[i]), @"x": @(bounds.origin.x), @"y": @(bounds.origin.y)}];
                if (CGRectGetMaxX(bounds) > right) { right = CGRectGetMaxX(bounds); rightY = bounds.origin.y; }
            }
            if (!isfinite(right) || !isfinite(rightY) || right > INT32_MAX - 1920 ||
                rightY < INT32_MIN || rightY > INT32_MAX - 1920) { fail(@"invalid virtual display origin"); return; }
            owner.existing = existing; owner.originalLayout = layout;
            owner.right = (int32_t)right; owner.rightY = (int32_t)rightY;
            if (owner.initial) {
                owner.density = scale >= 150 ? 2 : 1;
                if (owner.headless) fprintf(stderr, "YiTerm: headless macOS placeholder; owned virtual display may be primary\n");
            }
        }
        // Keyboard/toolbar layout can temporarily report a 320-point viewport.
        // A macOS desktop must remain large enough for normal application UI.
        owner.width = MAX(800, w); owner.height = MAX(600, h);
        if (owner.width != w || owner.height != h)
            fprintf(stderr, "YiTerm: viewer %ux%u uses minimum virtual desktop %ux%u\n", w, h, owner.width, owner.height);
        owner.lastModeAttempt = 0;
        owner.choseNearest = NO;
        // Keep an initial 1x fallback for this screen; do not repeatedly retry
        // unsupported HiDPI on every viewport update.
        if (owner.initial ? !createDisplay() : !applySize()) { fail(@"applySettings failed"); return; }
        owner.deadline = CFAbsoluteTimeGetCurrent() + 4.0;
        dispatch_async(dispatch_get_main_queue(), ^{ pollDisplay(); });
    } @catch (NSException *exception) {
        fprintf(stderr, "YiTerm virtual display exception: %s: %s\n", exception.name.UTF8String, exception.reason.UTF8String);
        fail(@"invalid virtual display request or unsupported interface");
    }
}

int yiterm_virtual_display_run(void) {
    @autoreleasepool {
        owner = [YiTermVirtualDesktop new];
        signal(SIGPIPE, SIG_IGN);
        signal(SIGTERM, SIG_IGN); signal(SIGINT, SIG_IGN);
        termination = dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, SIGTERM, 0, dispatch_get_main_queue());
        dispatch_source_set_event_handler(termination, ^{ cleanup(); exit(0); }); dispatch_resume(termination);
        interrupt = dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, SIGINT, 0, dispatch_get_main_queue());
        dispatch_source_set_event_handler(interrupt, ^{ cleanup(); exit(0); }); dispatch_resume(interrupt);
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            char *line = NULL; size_t capacity = 0;
            while (getline(&line, &capacity, stdin) > 0) {
                @autoreleasepool {
                    if (strlen(line) > 512) { emit(@{@"error": @"virtual display request too large"}); continue; }
                    NSData *data = [[NSString stringWithUTF8String:line] dataUsingEncoding:NSUTF8StringEncoding];
                    id command = data ? [NSJSONSerialization JSONObjectWithData:data options:0 error:nil] : nil;
                    dispatch_sync(dispatch_get_main_queue(), ^{
                        if (![command isKindOfClass:NSDictionary.class]) { emit(@{@"error": @"invalid virtual display request"}); return; }
                        request(command);
                    });
                }
            }
            free(line);
            dispatch_async(dispatch_get_main_queue(), ^{ cleanup(); exit(0); });
        });
        dispatch_main();
    }
    return 0;
}
