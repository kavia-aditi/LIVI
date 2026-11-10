// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

// Experimental only: no production route creates this object.
#import <Cocoa/Cocoa.h>
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#include <cmath>
#include <cstdint>
#include <limits>

extern "C" uintptr_t livi_attach_view(uintptr_t parent, int rank, void** clip);
extern "C" void livi_remove_view(void* clip);
extern "C" void livi_set_view_hidden(void* clip, bool hidden);

typedef void (*Event)(const void*, uint32_t, uint64_t, uint64_t, double);
typedef void (*Release)(const void*);

@class LIVIMetalPrototype;
@interface LIVIPrototypeView : NSView
@property(nonatomic, weak) LIVIMetalPrototype* owner;
@end

@interface LIVIPrototypeFrame : NSObject
@property(nonatomic, strong) NSData* bytes;
@property(nonatomic) uint32_t width;
@property(nonatomic) uint32_t height;
@property(nonatomic) uint64_t frame;
@property(nonatomic) uint64_t epoch;
@end
@implementation LIVIPrototypeFrame
@end

@interface LIVIMetalPrototype : NSObject {
@public
  const void* _context;
  Event _event;
  Release _release;
  void* _clip;
  uint64_t _epoch;
  BOOL _cancelled;
  BOOL _lastEligible;
  BOOL _resizing;
  NSUInteger _inflight;
  NSLock* _lock;
  LIVIPrototypeFrame* _latest;
  BOOL _scheduled;
  float _region[4];
}
@property(nonatomic, strong) LIVIPrototypeView* view;
@property(nonatomic, strong) CAMetalLayer* metal;
@property(nonatomic, strong) id<MTLDevice> device;
@property(nonatomic, strong) id<MTLCommandQueue> queue;
@property(nonatomic, strong) id<MTLRenderPipelineState> pipeline;
- (BOOL)eligible;
- (void)invalidate;
- (void)cancel;
- (void)enqueue:(LIVIPrototypeFrame*)frame;
- (void)pump;
@end

@interface LIVIPresentationReceipt : NSObject
@property(nonatomic, weak) LIVIMetalPrototype* owner;
@property(nonatomic) uint64_t frame;
@property(nonatomic) uint64_t epoch;
@property(nonatomic) double time;
@property(nonatomic) BOOL rendered;
@property(nonatomic) BOOL delivered;
- (void)report;
@end

@implementation LIVIPresentationReceipt
- (void)report {
  LIVIMetalPrototype* owner = self.owner;
  if (!owner || self.delivered || !self.rendered
      || !std::isfinite(self.time) || self.time <= 0) return;
  if ([owner eligible] != owner->_lastEligible) [owner invalidate];
  if (owner->_cancelled || ![owner eligible] || owner->_epoch != self.epoch) return;
  self.delivered = YES;
  owner->_event(owner->_context, 3, self.frame, self.epoch, self.time);
}
@end

@implementation LIVIPrototypeView
- (void)viewDidHide { [super viewDidHide]; [self.owner invalidate]; }
- (void)viewDidUnhide { [super viewDidUnhide]; [self.owner invalidate]; }
- (void)viewDidMoveToWindow { [super viewDidMoveToWindow]; [self.owner invalidate]; }
- (void)viewDidMoveToSuperview { [super viewDidMoveToSuperview]; [self.owner invalidate]; }
- (void)viewDidChangeBackingProperties {
  [super viewDidChangeBackingProperties];
  [self.owner invalidate];
}
- (void)setFrameSize:(NSSize)size {
  [super setFrameSize:size];
  [self.owner invalidate];
}
- (void)setBoundsSize:(NSSize)size {
  [super setBoundsSize:size];
  [self.owner invalidate];
}
- (NSView*)hitTest:(NSPoint)point { (void)point; return nil; }
@end

static NSMapTable<NSView*, LIVIMetalPrototype*>* owners() {
  static NSMapTable* table;
  static dispatch_once_t once;
  dispatch_once(&once, ^{ table = [NSMapTable weakToWeakObjectsMapTable]; });
  return table;
}

@implementation LIVIMetalPrototype
- (BOOL)eligible {
  NSWindow* window = self.view.window;
  return !_cancelled && !_resizing && self.view && window
      && !self.view.hiddenOrHasHiddenAncestor && !self.view.inLiveResize
      && !window.miniaturized && window.visible && !NSApp.hidden
      && (window.occlusionState & NSWindowOcclusionStateVisible)
      && self.view.bounds.size.width > 0 && self.view.bounds.size.height > 0;
}

- (void)invalidate {
  NSCAssert([NSThread isMainThread], @"prototype lifecycle must run on main");
  if (_cancelled) return;
  if (_epoch == std::numeric_limits<uint64_t>::max()) { [self cancel]; return; }
  ++_epoch;
  _lastEligible = [self eligible];
  if (_event) _event(_context, _lastEligible ? 2 : 1, 0, _epoch, 0);
}

- (void)changed:(NSNotification*)note {
  if ([note.name isEqualToString:NSWindowWillCloseNotification]) {
    [self cancel];
    return;
  }
  if ([note.name isEqualToString:NSWindowWillStartLiveResizeNotification])
    _resizing = YES;
  if ([note.name isEqualToString:NSWindowDidEndLiveResizeNotification])
    _resizing = NO;
  [self invalidate];
}

- (void)cancel {
  NSCAssert([NSThread isMainThread], @"prototype cancellation must run on main");
  if (_cancelled) return;
  _cancelled = YES;
  if (_event) _event(_context, 4, 0, _epoch, 0);
  [_lock lock]; _latest = nil; [_lock unlock];
  [[NSNotificationCenter defaultCenter] removeObserver:self];
  self.view.owner = nil;
  if (_clip) {
    // livi_attach_view allocates this retained view in the existing MRC bridge.
    NSView* clip = (__bridge_transfer NSView*)_clip;
    _clip = nullptr;
    livi_remove_view((__bridge void*)clip);
  }
}

- (void)dealloc {
  // Submitted blocks retain this owner, and therefore its callback context.
  if (_release) _release(_context);
}

- (void)enqueue:(LIVIPrototypeFrame*)frame {
  [_lock lock];
  _latest = frame; // latest-only: never queue an unbounded list of video copies
  BOOL schedule = !_scheduled;
  _scheduled = YES;
  [_lock unlock];
  if (schedule) dispatch_async(dispatch_get_main_queue(), ^{ [self pump]; });
}

- (void)pump {
  [_lock lock];
  LIVIPrototypeFrame* frame = _latest;
  _latest = nil;
  _scheduled = NO;
  [_lock unlock];
  if ([self eligible] != _lastEligible) [self invalidate];
  if (!frame || ![self eligible] || frame.epoch != _epoch || _inflight >= 3) return;

  CGFloat scale = self.view.window.backingScaleFactor;
  self.metal.contentsScale = scale;
  CGSize size = CGSizeMake(std::ceil(self.view.bounds.size.width * scale),
                           std::ceil(self.view.bounds.size.height * scale));
  if (!CGSizeEqualToSize(self.metal.drawableSize, size)) self.metal.drawableSize = size;
  id<CAMetalDrawable> drawable = [self.metal nextDrawable];
  if (!drawable) return;

  MTLTextureDescriptor* td = [MTLTextureDescriptor
      texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
      width:frame.width height:frame.height mipmapped:NO];
  td.usage = MTLTextureUsageShaderRead;
  td.storageMode = MTLStorageModeShared;
  id<MTLTexture> source = [self.device newTextureWithDescriptor:td];
  if (!source) return;
  [source replaceRegion:MTLRegionMake2D(0, 0, frame.width, frame.height)
           mipmapLevel:0 withBytes:frame.bytes.bytes bytesPerRow:frame.width * 4];

  MTLRenderPassDescriptor* pass = [MTLRenderPassDescriptor renderPassDescriptor];
  pass.colorAttachments[0].texture = drawable.texture;
  pass.colorAttachments[0].loadAction = MTLLoadActionClear;
  pass.colorAttachments[0].storeAction = MTLStoreActionStore;
  pass.colorAttachments[0].clearColor = MTLClearColorMake(0, 0, 0, 1);
  id<MTLCommandBuffer> command = [self.queue commandBuffer];
  id<MTLRenderCommandEncoder> encoder = [command renderCommandEncoderWithDescriptor:pass];
  if (!command || !encoder) return;
  [encoder setRenderPipelineState:self.pipeline];
  [encoder setFragmentTexture:source atIndex:0];
  [encoder setFragmentBytes:_region length:sizeof(_region) atIndex:0];
  [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
  [encoder endEncoding];

  LIVIPresentationReceipt* receipt = [LIVIPresentationReceipt new];
  receipt.owner = self;
  receipt.frame = frame.frame;
  receipt.epoch = frame.epoch;
  [drawable addPresentedHandler:^(id<MTLDrawable> presented) {
    double time = presented.presentedTime;
    if (!std::isfinite(time) || time <= 0) return;
    dispatch_async(dispatch_get_main_queue(), ^{
      receipt.time = time;
      [receipt report];
    });
  }];
  ++_inflight;
  __weak LIVIMetalPrototype* weakOwner = self;
  [command addCompletedHandler:^(id<MTLCommandBuffer> completed) {
    // GPU completion alone never succeeds: report also requires presentedTime.
    BOOL rendered = completed.status == MTLCommandBufferStatusCompleted;
    (void)source; // keep the upload alive until the command completes
    dispatch_async(dispatch_get_main_queue(), ^{
      LIVIMetalPrototype* owner = weakOwner;
      if (owner && owner->_inflight) --owner->_inflight;
      receipt.rendered = rendered;
      [receipt report];
    });
  }];
  [command presentDrawable:drawable];
  [command commit];
}
@end

static const char* shader = R"METAL(
#include <metal_stdlib>
using namespace metal;
struct Vertex { float4 position [[position]]; float2 uv; };
vertex Vertex livi_vertex(uint id [[vertex_id]]) {
  float2 p[3] = {float2(-1,-1), float2(3,-1), float2(-1,3)};
  Vertex v;
  v.position = float4(p[id],0,1);
  v.uv = float2((p[id].x+1)*0.5, 1-(p[id].y+1)*0.5);
  return v;
}
fragment float4 livi_fragment(Vertex v [[stage_in]], texture2d<float> image [[texture(0)]],
                             constant float4& region [[buffer(0)]]) {
  constexpr sampler s(filter::linear, address::clamp_to_edge);
  return image.sample(s,region.xy + v.uv * region.zw);
}
)METAL";

extern "C" uintptr_t livi_metal_prototype_create(
    uintptr_t parent, const void* context, Event event, Release release) {
  @autoreleasepool {
    if (![NSThread isMainThread] || !parent || !context || !event || !release) {
      if (release) release(context);
      return 0;
    }
    LIVIMetalPrototype* owner = [LIVIMetalPrototype new];
    owner->_context = context; owner->_event = event; owner->_release = release;
    owner->_lock = [NSLock new];
    owner->_region[0] = 0; owner->_region[1] = 0;
    owner->_region[2] = 1; owner->_region[3] = 1;
    owner.device = MTLCreateSystemDefaultDevice();
    owner.queue = [owner.device newCommandQueue];
    NSError* error = nil;
    id<MTLLibrary> library = [owner.device newLibraryWithSource:
        [NSString stringWithUTF8String:shader] options:nil error:&error];
    MTLRenderPipelineDescriptor* pd = [MTLRenderPipelineDescriptor new];
    pd.vertexFunction = [library newFunctionWithName:@"livi_vertex"];
    pd.fragmentFunction = [library newFunctionWithName:@"livi_fragment"];
    pd.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
    owner.pipeline = [owner.device newRenderPipelineStateWithDescriptor:pd error:&error];
    if (!owner.device || !owner.queue || !owner.pipeline) return 0;

    NSView* parentView = (__bridge NSView*)(void*)parent;
    [[owners() objectForKey:parentView] cancel];
    uintptr_t child = livi_attach_view(parent, 0, &owner->_clip);
    if (!child || !owner->_clip) { [owner cancel]; return 0; }
    // Consume the original child allocation retain; the clip owns its subview.
    NSView* host = (__bridge_transfer NSView*)(void*)child;
    LIVIPrototypeView* view = [[LIVIPrototypeView alloc] initWithFrame:host.bounds];
    view.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    view.wantsLayer = YES;
    CAMetalLayer* layer = [CAMetalLayer layer];
    layer.device = owner.device;
    layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    layer.framebufferOnly = YES;
    layer.maximumDrawableCount = 3;
    layer.allowsNextDrawableTimeout = YES;
    view.layer = layer;
    owner.view = view; owner.metal = layer;
    [host addSubview:view];
    view.owner = owner;
    [owners() setObject:owner forKey:parentView];

    NSNotificationCenter* nc = [NSNotificationCenter defaultCenter];
    for (NSNotificationName name in @[
        NSWindowWillStartLiveResizeNotification, NSWindowDidEndLiveResizeNotification,
        NSWindowDidResizeNotification, NSWindowDidChangeOcclusionStateNotification,
        NSWindowDidMiniaturizeNotification, NSWindowDidDeminiaturizeNotification,
        NSWindowDidChangeScreenNotification, NSWindowWillCloseNotification]) {
      [nc addObserver:owner selector:@selector(changed:) name:name object:view.window];
    }
    for (NSNotificationName name in @[NSApplicationDidHideNotification,
                                      NSApplicationDidUnhideNotification]) {
      [nc addObserver:owner selector:@selector(changed:) name:name object:NSApp];
    }
    [owner invalidate];
    return (uintptr_t)(__bridge_retained void*)owner;
  }
}

extern "C" void livi_metal_prototype_submit(uintptr_t handle, const uint8_t* bytes,
    size_t len, uint32_t width, uint32_t height, uint64_t frame, uint64_t epoch) {
  @autoreleasepool {
    if (!handle || !bytes || !width || !height || !frame || !epoch
        || width > 4096 || height > 4096
        || len != (uint64_t)width * height * 4 || len > 64 * 1024 * 1024) return;
    LIVIMetalPrototype* owner = (__bridge LIVIMetalPrototype*)(void*)handle;
    LIVIPrototypeFrame* input = [LIVIPrototypeFrame new];
    input.bytes = [NSData dataWithBytes:bytes length:len];
    input.width = width; input.height = height; input.frame = frame; input.epoch = epoch;
    [owner enqueue:input];
  }
}

extern "C" void livi_metal_prototype_visible(uintptr_t handle, bool visible) {
  if (!handle) return;
  NSCAssert([NSThread isMainThread], @"prototype visibility must run on main");
  LIVIMetalPrototype* owner = (__bridge LIVIMetalPrototype*)(void*)handle;
  if (owner->_cancelled) return;
  if (owner->_clip
      && ((__bridge NSView*)owner->_clip).hidden == !visible) return;
  if (owner->_clip) livi_set_view_hidden(owner->_clip, !visible);
  [owner invalidate];
}

extern "C" void livi_metal_prototype_region(uintptr_t handle,
    double left, double top, double width, double height) {
  if (!handle || !std::isfinite(left) || !std::isfinite(top)
      || !std::isfinite(width) || !std::isfinite(height)
      || left < 0 || top < 0 || width <= 0 || height <= 0
      || left + width > 1 || top + height > 1) return;
  NSCAssert([NSThread isMainThread], @"prototype region must run on main");
  LIVIMetalPrototype* owner = (__bridge LIVIMetalPrototype*)(void*)handle;
  if (owner->_cancelled) return;
  float next[4] = {(float)left, (float)top, (float)width, (float)height};
  bool changed = false;
  for (int i = 0; i < 4; ++i) {
    changed = changed || owner->_region[i] != next[i];
    owner->_region[i] = next[i];
  }
  if (changed) [owner invalidate];
}

extern "C" void livi_metal_prototype_destroy(uintptr_t handle) {
  if (!handle) return;
  LIVIMetalPrototype* owner = (__bridge_transfer LIVIMetalPrototype*)(void*)handle;
  if ([NSThread isMainThread]) [owner cancel];
  else dispatch_async(dispatch_get_main_queue(), ^{ [owner cancel]; });
}
