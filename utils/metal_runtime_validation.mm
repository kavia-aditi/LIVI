#import <Cocoa/Cocoa.h>
#import <QuartzCore/QuartzCore.h>
#include <cstdio>
#include <cstdlib>
#include <algorithm>
#include <map>
#include <vector>

#include "../native/crates/livi-video-view/src/metal_presenter.mm"

// Native endpoint validation only: synthetic decoded BGRA, no GStreamer startup,
// Rust gate, AA, Bluetooth, Wi-Fi, USB, or production application.
struct Context {
  unsigned index;
  uint64_t epoch = 0;
  bool eligible = false;
  bool cancelled = false;
  bool released = false;
  std::map<uint64_t, double> submitted;
};
static Context contexts[3];
static uintptr_t handles[3] = {};
static unsigned tick = 0;
static unsigned currentStage = 0;
static uint64_t nextFrame = 1;
static unsigned counts[7] = {};
static unsigned unexpected = 0;
static unsigned cancellations = 0;
static bool cancellationObserved = false;
static bool replacementObserved = false;
static std::vector<double> latency;
static NSWindow* testWindow;
static NSView* parentView;
static NSTimer* timer;
static double began = 0;
static double gpuSeconds = 0;
static unsigned gpuSamples = 0;
static uint64_t peakMetalAllocated = 0;

// Only the standalone owner's queue is wrapped. The renderer still creates
// and presents its own real drawable/command buffer; completion is metric-only.
@interface LIVIValidationQueue : NSObject
@property(nonatomic, strong) id<MTLCommandQueue> backing;
- (id<MTLCommandBuffer>)commandBuffer;
@end

@implementation LIVIValidationQueue
- (id<MTLCommandBuffer>)commandBuffer {
  id<MTLCommandBuffer> command = [self.backing commandBuffer];
  [command addCompletedHandler:^(id<MTLCommandBuffer> completed) {
    double start = completed.GPUStartTime;
    double end = completed.GPUEndTime;
    if (completed.status != MTLCommandBufferStatusCompleted
        || !std::isfinite(start) || !std::isfinite(end)
        || start <= 0 || end < start) return;
    dispatch_async(dispatch_get_main_queue(), ^{
      gpuSeconds += end - start;
      ++gpuSamples;
    });
  }];
  return command;
}
@end

static void event(const void* raw, uint32_t kind, uint64_t frame,
                  uint64_t epoch, double time) {
  Context& context = *static_cast<Context*>(const_cast<void*>(raw));
  if (kind == 1 || kind == 2) {
    context.epoch = epoch;
    context.eligible = kind == 2;
    context.submitted.clear();
  } else if (kind == 4) {
    context.cancelled = true;
    context.eligible = false;
    context.submitted.clear();
    ++cancellations;
  } else if (kind == 3) {
    auto found = context.submitted.find(frame);
    unsigned stage = currentStage;
    bool forbidden = (context.index == 0 && stage >= 4)
        || (context.index == 1 && stage >= 6) || stage == 1 || stage == 4;
    if (forbidden || context.cancelled || !context.eligible
        || epoch != context.epoch || found == context.submitted.end()
        || !std::isfinite(time) || time <= 0) {
      std::fprintf(stderr, "UNEXPECTED player=%u stage=%u frame=%llu "
                   "epoch=%llu current_epoch=%llu cancelled=%d eligible=%d "
                   "known=%d time=%.9f\n",
                   context.index, stage, (unsigned long long)frame,
                   (unsigned long long)epoch, (unsigned long long)context.epoch,
                   context.cancelled, context.eligible,
                   found != context.submitted.end(), time);
      ++unexpected;
      return;
    }
    double milliseconds = (time - found->second) * 1000.0;
    if (!std::isfinite(milliseconds) || milliseconds < 0) {
      std::fprintf(stderr, "UNEXPECTED latency_ms=%.9f\n", milliseconds);
      ++unexpected;
      return;
    }
    latency.push_back(milliseconds);
    context.submitted.erase(found);
    ++counts[stage];
  }
}

static void releaseContext(const void* raw) {
  static_cast<Context*>(const_cast<void*>(raw))->released = true;
}

static uintptr_t create(unsigned index) {
  contexts[index].index = index;
  uintptr_t handle = livi_metal_prototype_create(
      (uintptr_t)(__bridge void*)parentView, &contexts[index], event, releaseContext);
  if (handle) {
    LIVIMetalPrototype* owner = (__bridge LIVIMetalPrototype*)(void*)handle;
    LIVIValidationQueue* measured = [LIVIValidationQueue new];
    measured.backing = owner.queue;
    owner.queue = (id<MTLCommandQueue>)measured;
  }
  return handle;
}

static void submit(unsigned index) {
  if (!handles[index]) return;
  Context& context = contexts[index];
  LIVIMetalPrototype* owner = (__bridge LIVIMetalPrototype*)(void*)handles[index];
  peakMetalAllocated = std::max(peakMetalAllocated,
                               (uint64_t)owner.device.currentAllocatedSize);
  // Deliberately submit to cancelled/hidden owners too: they must not acknowledge.
  static std::vector<uint8_t> pixels(320 * 180 * 4, 255);
  uint64_t frame = nextFrame++;
  for (size_t p = 0; p < pixels.size(); p += 4) {
    pixels[p] = static_cast<uint8_t>(frame * 3);
    pixels[p + 1] = static_cast<uint8_t>(frame * 7);
    pixels[p + 2] = static_cast<uint8_t>(frame * 11);
  }
  context.submitted[frame] = CACurrentMediaTime();
  while (context.submitted.size() > 180) context.submitted.erase(context.submitted.begin());
  livi_metal_prototype_submit(handles[index], pixels.data(), pixels.size(),
                             320, 180, frame, context.epoch);
}

static void finish(int setupFailure) {
  [timer invalidate];
  for (auto handle : handles) {
    if (handle) livi_metal_prototype_destroy(handle);
  }
  [testWindow orderOut:nil];
  [testWindow close];
  std::sort(latency.begin(), latency.end());
  double p50 = latency.empty() ? 0 : latency[(latency.size() - 1) / 2];
  double p95 = latency.empty() ? 0 : latency[(latency.size() - 1) * 95 / 100];
  bool passed = !setupFailure && unexpected == 0
      && counts[0] && counts[2] && counts[3] && counts[5] && counts[6]
      && !counts[1] && !counts[4]
      && cancellationObserved && replacementObserved;
  std::printf("REAL_NATIVE_PRESENTATION_VALIDATION %s\n", passed ? "PASS" : "FAIL");
  std::printf("cancellation_observed=%d replacement_observed=%d\n",
              cancellationObserved, replacementObserved);
  std::printf("visible=%u hidden=%u shown=%u resized=%u cancelled=%u "
              "second_player=%u replacement=%u unexpected=%u cancellations=%u\n",
              counts[0], counts[1], counts[2], counts[3], counts[4],
              counts[5], counts[6], unexpected, cancellations);
  std::printf("submit_to_present_ms samples=%zu p50=%.3f p95=%.3f max=%.3f "
              "elapsed_seconds=%.3f\n",
              latency.size(), p50, p95, latency.empty() ? 0 : latency.back(),
              CACurrentMediaTime() - began);
  std::printf("gpu_command_samples=%u gpu_seconds=%.6f "
              "gpu_command_time_over_elapsed_percent=%.3f "
              "sampled_device_allocated_peak_bytes=%llu\n",
              gpuSamples, gpuSeconds,
              100.0 * gpuSeconds / (CACurrentMediaTime() - began),
              (unsigned long long)peakMetalAllocated);
  std::printf("SYSTEM_WIDE_GPU_UTILIZATION=UNAVAILABLE no_privileged_profiler_used\n"
              "SCOPE=synthetic_BGRA_native_endpoint_only_not_decode_or_Rust_gate\n");
  std::fflush(stdout);
  std::exit(passed ? 0 : 1);
}

int main() {
  @autoreleasepool {
    // Do not attempt GUI startup without a logged-in graphical session.
    CFDictionaryRef session = CGSessionCopyCurrentDictionary();
    if (!session) {
      std::fprintf(stderr, "BLOCKER: no graphical login session; no renderer started\n");
      return 2;
    }
    CFRelease(session);
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    [NSApp finishLaunching];
    testWindow = [[NSWindow alloc]
        initWithContentRect:NSMakeRect(100, 100, 640, 360)
                  styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskResizable
                    backing:NSBackingStoreBuffered defer:NO];
    testWindow.releasedWhenClosed = NO;
    testWindow.title = @"Isolated experimental Metal validation";
    parentView = testWindow.contentView;
    [testWindow orderFrontRegardless];
    began = CACurrentMediaTime();
    handles[0] = create(0);
    if (!handles[0]) finish(1);
    timer = [NSTimer scheduledTimerWithTimeInterval:1.0 / 60.0 repeats:YES
        block:^(NSTimer* unused) {
      (void)unused;
      @autoreleasepool {
        currentStage = std::min(tick / 60, 6u);
        if (tick == 60) livi_metal_prototype_visible(handles[0], false);
        if (tick == 120) livi_metal_prototype_visible(handles[0], true);
        if (tick == 180) {
          // Queue an old-epoch frame immediately before the geometry change.
          submit(0);
          [testWindow setContentSize:NSMakeSize(800, 450)];
        }
        if (tick == 240) {
          submit(0);
          LIVIMetalPrototype* owner = (__bridge LIVIMetalPrototype*)(void*)handles[0];
          [owner cancel];
          cancellationObserved = contexts[0].cancelled;
        }
        if (tick == 300) {
          handles[1] = create(1);
          if (!handles[1]) finish(1);
        }
        if (tick == 360) {
          submit(1);
          // This must cancel the still-active second player on the same parent.
          handles[2] = create(2);
          if (!handles[2]) finish(1);
          replacementObserved = contexts[1].cancelled;
        }
        if (tick >= 420) finish(0);
        submit(0);
        if (tick >= 300) submit(1);
        if (tick >= 360) submit(2);
        ++tick;
      }
    }];
    [NSApp run];
    return 3;
  }
}
