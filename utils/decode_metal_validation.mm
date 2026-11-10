#import <Cocoa/Cocoa.h>
#import <QuartzCore/QuartzCore.h>
#import <Metal/Metal.h>
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdint>

static NSWindow* window;
static NSTimer* timer;
static unsigned ticks;
static int outcome = 2;
static double began;
static double gpuSeconds;
static unsigned gpuSamples;
static uint64_t allocatedPeak;

// Test-only queue decorator; completion records metrics, never presentation ACKs.
@interface LIVIDecodeMeasuredQueue : NSObject
@property(nonatomic, strong) id<MTLCommandQueue> backing;
- (id<MTLCommandBuffer>)commandBuffer;
@end
@implementation LIVIDecodeMeasuredQueue
- (id<MTLCommandBuffer>)commandBuffer {
  id<MTLCommandBuffer> command = [self.backing commandBuffer];
  [command addCompletedHandler:^(id<MTLCommandBuffer> completed) {
    double start = completed.GPUStartTime;
    double end = completed.GPUEndTime;
    if (completed.status == MTLCommandBufferStatusCompleted
        && std::isfinite(start) && std::isfinite(end) && start > 0 && end >= start) {
      dispatch_async(dispatch_get_main_queue(), ^{
        gpuSeconds += end - start;
        ++gpuSamples;
      });
    }
  }];
  return command;
}
@end

static void measure(NSView* view, bool wrap, bool initial = true) {
  if (initial) view = view.superview ?: view;
  if ([NSStringFromClass(view.class) isEqualToString:@"LIVIPrototypeView"]) {
    id owner = [view valueForKey:@"owner"];
    if (owner) {
      id<MTLDevice> device = [owner valueForKey:@"device"];
      allocatedPeak = std::max(allocatedPeak, (uint64_t)device.currentAllocatedSize);
      id queue = [owner valueForKey:@"queue"];
      if (wrap && ![queue isKindOfClass:[LIVIDecodeMeasuredQueue class]]) {
        LIVIDecodeMeasuredQueue* measured = [LIVIDecodeMeasuredQueue new];
        measured.backing = queue;
        [owner setValue:measured forKey:@"queue"];
      }
    }
  }
  for (NSView* child in view.subviews) measure(child, wrap, false);
}

extern "C" void livi_decode_validation_measure() {
  measure(window.contentView, true);
}
extern "C" void livi_decode_validation_resize() {
  [window setContentSize:NSMakeSize(800, 450)];
}

extern "C" int livi_decode_validation_run(
    void* context, int (*callback)(void*, uintptr_t, uint32_t)) {
  @autoreleasepool {
    CFDictionaryRef session = CGSessionCopyCurrentDictionary();
    if (!session) {
      std::fprintf(stderr, "BLOCKER: no graphical login session\n");
      return 2;
    }
    CFRelease(session);
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    [NSApp finishLaunching];
    window = [[NSWindow alloc] initWithContentRect:NSMakeRect(100, 100, 640, 360)
        styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskResizable
        backing:NSBackingStoreBuffered defer:NO];
    window.releasedWhenClosed = NO;
    window.title = @"Isolated offline GStreamer to Metal validation";
    [window orderFrontRegardless];
    began = CACurrentMediaTime();
    timer = [NSTimer scheduledTimerWithTimeInterval:1.0 / 30.0 repeats:YES
        block:^(NSTimer* unused) {
      (void)unused;
      @autoreleasepool {
        measure(window.contentView, false);
        int result = callback(context, (uintptr_t)(__bridge void*)window.contentView, ticks++);
        if (result != 0) {
          outcome = result > 0 ? 0 : 1;
          [timer invalidate];
          [window orderOut:nil];
          [NSApp stop:nil];
          [NSApp postEvent:[NSEvent otherEventWithType:NSEventTypeApplicationDefined
              location:NSZeroPoint modifierFlags:0 timestamp:0
              windowNumber:0 context:nil subtype:0 data1:0 data2:0]
              atStart:YES];
        }
      }
    }];
    [NSApp run];
    [window close];
    double elapsed = CACurrentMediaTime() - began;
    std::printf("elapsed_seconds=%.3f gpu_command_samples=%u gpu_seconds=%.6f "
                "gpu_command_time_over_elapsed_percent=%.3f "
                "sampled_device_allocated_peak_bytes=%llu\n",
                elapsed, gpuSamples, gpuSeconds, 100 * gpuSeconds / elapsed,
                (unsigned long long)allocatedPeak);
    std::printf("SYSTEM_WIDE_GPU_UTILIZATION=UNAVAILABLE\n");
    if (outcome == 0 && gpuSamples == 0) {
      std::fprintf(stderr, "VALIDATION FAILED: missing GPU command timing samples\n");
      outcome = 1;
    }
    std::printf("GPU_MEASUREMENT_CHECK %s samples=%u\n",
                gpuSamples ? "PASS" : "FAIL", gpuSamples);
    return outcome;
  }
}
