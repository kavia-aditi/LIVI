// SPDX-License-Identifier: GPL-3.0-or-later
#import <AppKit/AppKit.h>
#import <IOBluetooth/IOBluetooth.h>
#import <CoreBluetooth/CoreBluetooth.h>
#include <sys/socket.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdbool.h>
#include <errno.h>

extern void livi_native_arrival(int, const char *, const char *);
extern bool livi_native_should_page(bool, bool, bool);
extern void livi_native_page_finished(bool);

static IOBluetoothSDPUUID *AAUUID(void) {
    const unsigned char bytes[] = {
        0x4d,0xe1,0x7a,0x00,0x52,0xcb,0x11,0xe6,0xbd,0xf4,0x08,0x00,0x20,0x0c,0x9a,0x66
    };
    return [IOBluetoothSDPUUID uuidWithBytes:bytes length:16];
}
static NSString *Address(IOBluetoothDevice *d) {
    return [[[d addressString] stringByReplacingOccurrencesOfString:@"-" withString:@":"] uppercaseString];
}
static NSDictionary *UIntElement(unsigned value, unsigned size) {
    return @{@"DataElementType":@1, @"DataElementSize":@(size), @"DataElementValue":@(value)};
}

@interface LIVINativeAdapter : NSObject <NSApplicationDelegate, NSWindowDelegate,
    CBCentralManagerDelegate, IOBluetoothDeviceInquiryDelegate,
    IOBluetoothDevicePairDelegate, IOBluetoothRFCOMMChannelDelegate>
@property NSWindow *window;
@property NSTextView *log;
@property CBCentralManager *privacy;
@property IOBluetoothDeviceInquiry *inquiry;
@property IOBluetoothDevicePair *pair;
@property IOBluetoothDevice *target;
@property NSString *targetAddress;
@property IOBluetoothSDPServiceRecord *record;
@property IOBluetoothUserNotification *notification;
@property IOBluetoothRFCOMMChannel *channel;
@property NSMutableData *toRust;
@property NSData *inFlight;
@property NSTimer *timer;
@property NSTimer *deadline;
@property BOOL available;
@property BOOL enabled;
@property BOOL querying;
@property BOOL opening;
@property BOOL quitting;
@property BOOL pairCancelled;
@property int bridgeFD;
@property unsigned scanSeconds;
@property double nextHealth;
@property double queryDeadline;
@property double openingDeadline;
@property double pairDeadline;
@end

@implementation LIVINativeAdapter
- (void)note:(NSString *)text {
    NSLog(@"[native-bt-dev] %@", text);
    if (!self.log) return;
    NSString *line = [text stringByAppendingString:@"\n"];
    [[self.log textStorage] appendAttributedString:[[NSAttributedString alloc] initWithString:line]];
    [self.log scrollRangeToVisible:NSMakeRange(self.log.string.length, 0)];
}
- (void)button:(NSString *)title x:(CGFloat)x action:(SEL)action {
    NSButton *b = [NSButton buttonWithTitle:title target:self action:action];
    b.frame = NSMakeRect(x, 24, 165, 34);
    [self.window.contentView addSubview:b];
}
- (void)applicationDidFinishLaunching:(NSNotification *)unused {
    (void)unused;
    self.bridgeFD = -1;
    self.toRust = [NSMutableData data];
    self.window = [[NSWindow alloc] initWithContentRect:NSMakeRect(0,0,900,560)
        styleMask:NSWindowStyleMaskTitled|NSWindowStyleMaskClosable|NSWindowStyleMaskMiniaturizable|NSWindowStyleMaskResizable
        backing:NSBackingStoreBuffered defer:NO];
    self.window.title = @"LIVI — Native Bluetooth Development (real hardware)";
    self.window.delegate = self;
    self.window.releasedWhenClosed = NO;
    NSScrollView *scroll = [[NSScrollView alloc] initWithFrame:NSMakeRect(20,80,860,460)];
    scroll.hasVerticalScroller = YES;
    self.log = [[NSTextView alloc] initWithFrame:scroll.bounds];
    self.log.editable = NO;
    self.log.autoresizingMask = NSViewWidthSizable|NSViewHeightSizable;
    scroll.documentView = self.log;
    [self.window.contentView addSubview:scroll];
    [self button:@"Discover phones" x:20 action:@selector(scan:)];
    if (!self.scanSeconds) {
        [self button:@"Pair / connect target" x:195 action:@selector(connect:)];
        [self button:@"Disconnect owned" x:370 action:@selector(disconnect:)];
        [self button:@"Reconnect target" x:545 action:@selector(connect:)];
    }
    [self.window center];
    [self.window makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    [self note:@"Real hardware backend. OS pairing, RFCOMM, Wi-Fi and visible projection are separate results."];
    [self note:@"No USB watcher, HFP registration, radio reset or production helper is started."];
    // CoreBluetooth is used ONLY for the public privacy/state gate. AA data uses classic RFCOMM.
    self.privacy = [[CBCentralManager alloc] initWithDelegate:self queue:dispatch_get_main_queue()
        options:@{CBCentralManagerOptionShowPowerAlertKey:@NO}];
    self.timer = [NSTimer scheduledTimerWithTimeInterval:0.05 target:self selector:@selector(tick:) userInfo:nil repeats:YES];
    if (self.scanSeconds) {
        self.deadline = [NSTimer scheduledTimerWithTimeInterval:self.scanSeconds target:self
            selector:@selector(scanDeadline:) userInfo:nil repeats:NO];
    }
}
- (void)scanDeadline:(NSTimer *)unused {
    (void)unused;
    [self note:@"Bounded discovery window ended; this is not Android Auto validation."];
    [NSApp terminate:nil];
}
- (void)centralManagerDidUpdateState:(CBCentralManager *)unused {
    (void)unused;
    self.nextHealth = 0;
    if (self.scanSeconds && self.privacy.state == CBManagerStatePoweredOn) [self scan:nil];
}
- (BOOL)controllerReady {
    if (self.privacy.state != CBManagerStatePoweredOn) return NO;
    IOBluetoothHostController *host = [IOBluetoothHostController defaultController];
    return host && host.powerState == kBluetoothHCIPowerStateON && host.addressAsString.length > 0;
}
- (void)withdraw {
    self.available = NO;  // Withdraw BEFORE releasing any registration or owned stream.
    if (self.inquiry) {
        self.inquiry.delegate = nil;
        [self.inquiry stop];
        self.inquiry = nil;
    }
    [self.notification unregister];
    self.notification = nil;
    [self.record removeServiceRecord];
    self.record = nil;
    [self closeOwned];
    // IOBluetooth exposes no public per-query cancellation token. Keep it busy until
    // its callback, rather than allowing an old callback to impersonate a new query.
    if (self.querying) self.enabled = NO;
    // Keep the outstanding attempt serialized until its callback; stop may disconnect ACL.
    if (self.pair) self.pairCancelled = YES;
}
- (void)publish {
    if (self.scanSeconds || self.record) return;
    NSDictionary *record = @{
        @"0001 - ServiceClassIDList": @[AAUUID()],
        @"0004 - ProtocolDescriptorList": @[
            @[[IOBluetoothSDPUUID uuid16:0x0100]],
            @[[IOBluetoothSDPUUID uuid16:0x0003], UIntElement(8,1)]
        ],
        @"0005 - BrowseGroupList": @[[IOBluetoothSDPUUID uuid16:0x1002]],
        @"0009 - BluetoothProfileDescriptorList": @[
            @[[IOBluetoothSDPUUID uuid16:0x1101], UIntElement(0x0102,2)]
        ],
        @"0100 - ServiceName": @"LIVI Native Android Auto Wireless (experimental)",
        @"LocalAttributes": @{@"Persistent":@NO}
    };
    self.record = [IOBluetoothSDPServiceRecord publishedServiceRecordWithDictionary:record];
    BluetoothRFCOMMChannelID assigned = 0;
    if (!self.record || [self.record getRFCOMMChannelID:&assigned] != kIOReturnSuccess || !assigned) {
        [self withdraw];
        [self note:@"SDP publication unavailable; no private HCI or permission workaround will be attempted."];
        return;
    }
    self.notification = [IOBluetoothRFCOMMChannel registerForChannelOpenNotifications:self
        selector:@selector(incoming:channel:) withChannelID:assigned
        direction:kIOBluetoothUserNotificationChannelDirectionIncoming];
    if (!self.notification) {
        [self withdraw];
        [self note:@"RFCOMM registration failed."];
        return;
    }
    [self note:[NSString stringWithFormat:@"Transient AA SDP service published on RFCOMM channel %u.", assigned]];
}
- (void)tick:(NSTimer *)unused {
    (void)unused;
    double now = [NSDate timeIntervalSinceReferenceDate];
    if (now >= self.nextHealth) {
        self.nextHealth = now + 1;
        BOOL ready = [self controllerReady];
        if (!ready) {
            if (self.available) { [self withdraw]; [self note:@"Adapter/consent unavailable; owned Bluetooth resources withdrawn."]; }
            if (self.privacy.state == CBManagerStateUnauthorized && !self.quitting) {
                [self note:@"Bluetooth access denied. Grant consent in System Settings; no automated TCC reset."];
                self.quitting = YES; // Suppress repeated denied messages, not an app shutdown.
            }
        } else {
            self.quitting = NO;
            if (!self.available) {
                self.available = YES;
                [self note:@"Built-in Bluetooth adapter available."];
            }
            [self publish];
            if (self.querying && now > self.queryDeadline) {
                // Do not start overlapping queries with indistinguishable late callbacks.
                self.enabled = NO;
                [self note:@"SDP query timed out; automatic retries disabled until it completes."];
            }
            if (self.opening && now > self.openingDeadline) {
                [self closeOwned];
                livi_native_page_finished(false);
                [self note:@"Owned RFCOMM open timed out."];
            }
            if (self.pair && !self.pairCancelled && now > self.pairDeadline) {
                self.pairCancelled = YES;
                self.enabled = NO;
                [self note:@"Pairing timed out; waiting for the OS attempt to finish before any retry."];
            }
            if (livi_native_should_page(self.enabled, self.available,
                self.querying || self.opening || self.channel != nil || self.pair != nil)) [self page];
        }
    }
    [self pump];
}
- (void)scan:(id)unused {
    (void)unused;
    if (![self controllerReady]) { [self note:@"Discovery unavailable: check Bluetooth consent and adapter power."]; return; }
    if (self.inquiry) return;
    // Also show OS-paired phones; an already-connected phone may not answer inquiry.
    for (IOBluetoothDevice *device in [IOBluetoothDevice pairedDevices]) {
        if ([[device name] localizedCaseInsensitiveContainsString:@"OnePlus"])
            [self note:[NSString stringWithFormat:@"OS-paired phone: %@ (paired=%d, ACL=%d; not LIVI projection).",
                device.name, device.isPaired, device.isConnected]];
    }
    self.inquiry = [IOBluetoothDeviceInquiry inquiryWithDelegate:self];
    self.inquiry.inquiryLength = 8;
    IOReturn result = [self.inquiry start];
    [self note:result == kIOReturnSuccess ? @"Classic Bluetooth inquiry started; keep phone Bluetooth settings open." :
        [NSString stringWithFormat:@"Inquiry failed: 0x%x", result]];
    if (result != kIOReturnSuccess) self.inquiry = nil;
}
- (void)deviceInquiryDeviceFound:(IOBluetoothDeviceInquiry *)sender device:(IOBluetoothDevice *)device {
    if (sender != self.inquiry || !self.available) return;
    // Names only in logs. Do not persist personal addresses.
    [self note:[NSString stringWithFormat:@"Discovered: %@", device.name ?: @"Unnamed Bluetooth device"]];
}
- (void)deviceInquiryComplete:(IOBluetoothDeviceInquiry *)sender error:(IOReturn)error aborted:(BOOL)aborted {
    if (sender != self.inquiry) return;
    self.inquiry.delegate = nil; self.inquiry = nil;
    [self note:[NSString stringWithFormat:@"Inquiry ended (error=0x%x, cancelled=%d).", error, aborted]];
}
- (void)connect:(id)unused {
    (void)unused;
    if (![self controllerReady] || self.scanSeconds || !self.targetAddress.length) return;
    if (self.querying || self.opening || self.channel || self.pair) {
        [self note:@"An owned connection/attempt already exists."]; return;
    }
    self.target = [IOBluetoothDevice deviceWithAddressString:self.targetAddress];
    if (!self.target) { [self note:@"Configured phone address is unavailable."]; return; }
    if (self.target.isPaired) {
        self.enabled = YES;
        [self page];
        return;
    }
    if (self.target.isConnected) {
        [self note:@"Unpaired phone already has a system link. Pair in macOS Settings first; LIVI will not take over that link."];
        return;
    }
    self.pair = [IOBluetoothDevicePair pairWithDevice:self.target];
    self.pair.delegate = self;
    self.pairCancelled = NO;
    self.pairDeadline = [NSDate timeIntervalSinceReferenceDate] + 60;
    IOReturn result = [self.pair start];
    if (result != kIOReturnSuccess) {
        self.pair.delegate = nil; self.pair = nil;
        [self note:[NSString stringWithFormat:@"Pairing start failed: 0x%x", result]];
    } else [self note:@"Pairing started. Check both Mac and phone; consent is required."];
}
- (void)devicePairingUserConfirmationRequest:(id)sender numericValue:(BluetoothNumericValue)value {
    if (sender != self.pair || !self.available || self.pairCancelled) {
        if (sender == self.pair) [self.pair replyUserConfirmation:NO];
        return;
    }
    NSAlert *alert = [NSAlert new];
    alert.messageText = [NSString stringWithFormat:@"Does %06u match your OnePlus?", (unsigned)value];
    alert.informativeText = @"Compare the code on the physical phone. Accept only if it matches.";
    [alert addButtonWithTitle:@"Codes match"];
    [alert addButtonWithTitle:@"Reject"];
    BOOL accept = [alert runModal] == NSAlertFirstButtonReturn;
    if (sender == self.pair)
        [self.pair replyUserConfirmation:accept && self.available && !self.pairCancelled];
}
- (void)devicePairingPINCodeRequest:(id)sender {
    (void)sender;
    self.enabled = NO;
    [self note:@"Legacy PIN pairing is unsupported; use macOS Bluetooth Settings. No PIN is guessed."];
}
- (void)devicePairingUserPasskeyNotification:(id)sender passkey:(BluetoothPasskey)key {
    if (sender == self.pair)
        [self note:[NSString stringWithFormat:@"Enter displayed passkey %06u on the phone.", (unsigned)key]];
}
- (void)devicePairingFinished:(id)sender error:(IOReturn)error {
    if (sender != self.pair) return;
    self.pair.delegate = nil; self.pair = nil;
    self.enabled = error == kIOReturnSuccess && self.available && !self.pairCancelled;
    [self note:error == kIOReturnSuccess ? @"OS pairing completed; Android Auto is not yet connected." :
        [NSString stringWithFormat:@"Pairing failed/cancelled: 0x%x", error]];
    if (self.enabled) [self page];
}
- (void)page {
    if (!self.enabled || !self.available || self.querying || self.opening || self.channel || self.pair) return;
    if (!self.target.isPaired) { self.enabled = NO; [self note:@"Pairing was removed; automatic connection stopped."]; return; }
    self.querying = YES;
    self.queryDeadline = [NSDate timeIntervalSinceReferenceDate] + 15;
    // Query also wakes phones which initiate their own connection to our advertised service.
    IOReturn result = [self.target performSDPQuery:self uuids:@[AAUUID()]];
    if (result != kIOReturnSuccess) {
        self.querying = NO;
        livi_native_page_finished(false);
        [self note:[NSString stringWithFormat:@"Target SDP request failed: 0x%x (bounded retry).", result]];
    }
}
- (void)sdpQueryComplete:(IOBluetoothDevice *)device status:(IOReturn)status {
    if (device != self.target || !self.querying) return;
    self.querying = NO;
    if (!self.available || !self.enabled || self.channel) return;
    IOBluetoothSDPServiceRecord *remote = [device getServiceRecordForUUID:AAUUID()];
    BluetoothRFCOMMChannelID channelID = 0;
    if (status != kIOReturnSuccess || !remote ||
        [remote getRFCOMMChannelID:&channelID] != kIOReturnSuccess || !channelID) {
        livi_native_page_finished(false);
        [self note:@"Remote AA RFCOMM service not offered yet; listening for phone-initiated bootstrap."];
        return;
    }
    self.opening = YES;
    self.openingDeadline = [NSDate timeIntervalSinceReferenceDate] + 10;
    IOBluetoothRFCOMMChannel *channel = nil;
    IOReturn result = [device openRFCOMMChannelAsync:&channel withChannelID:channelID delegate:self];
    // An immediate callback may already have adopted this exact channel.
    if (result == kIOReturnSuccess && self.opening && !self.channel) self.channel = channel;
    else if (result == kIOReturnSuccess && channel != self.channel) {
        [channel setDelegate:nil]; [channel closeChannel];
    }
    if (result != kIOReturnSuccess) {
        self.opening = NO;
        livi_native_page_finished(false);
        [self note:[NSString stringWithFormat:@"RFCOMM open rejected: 0x%x", result]];
    }
}
- (void)incoming:(IOBluetoothUserNotification *)notification channel:(IOBluetoothRFCOMMChannel *)channel {
    if (notification != self.notification || !self.available || !self.enabled ||
        ![Address([channel getDevice]) isEqualToString:self.targetAddress] || self.channel) {
        // Close only a channel delivered to our service, never the device's baseband link.
        [channel closeChannel]; return;
    }
    self.channel = channel;
    [channel setDelegate:self];
    [self adopt:channel];
}
- (void)rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:(IOReturn)error {
    if (!self.available || !self.enabled || !self.opening ||
        (self.channel && channel != self.channel)) {
        [channel setDelegate:nil]; [channel closeChannel]; return;
    }
    self.opening = NO;
    if (error != kIOReturnSuccess) {
        [self closeOwned];
        livi_native_page_finished(false);
        [self note:[NSString stringWithFormat:@"RFCOMM connection failed: 0x%x", error]];
        return;
    }
    self.channel = channel;
    [self adopt:channel];
}
- (void)adopt:(IOBluetoothRFCOMMChannel *)channel {
    if (self.bridgeFD >= 0) return;
    int sockets[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) { [self closeOwned]; return; }
    for (int i=0; i<2; i++) {
        fcntl(sockets[i], F_SETFD, FD_CLOEXEC);
        fcntl(sockets[i], F_SETFL, O_NONBLOCK);
        int yes = 1; setsockopt(sockets[i], SOL_SOCKET, SO_NOSIGPIPE, &yes, sizeof(yes));
    }
    self.bridgeFD = sockets[0];
    self.opening = NO;
    NSString *local = [[[IOBluetoothHostController defaultController] addressAsString]
        stringByReplacingOccurrencesOfString:@"-" withString:@":"];
    livi_native_arrival(sockets[1], Address([channel getDevice]).UTF8String, local.UTF8String);
    livi_native_page_finished(true);
    [self note:@"Real owned RFCOMM channel connected; beginning WPP. This is NOT projection success."];
}
- (void)pump {
    if (self.bridgeFD < 0 || !self.channel || self.opening) return;
    if (self.toRust.length) {
        ssize_t n = send(self.bridgeFD, self.toRust.bytes, self.toRust.length, 0);
        if (n > 0) [self.toRust replaceBytesInRange:NSMakeRange(0,(NSUInteger)n) withBytes:NULL length:0];
        else if (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) { [self closeOwned]; return; }
    }
    if (self.inFlight) return;
    unsigned char buffer[4096];
    size_t mtu = MIN((size_t)self.channel.getMTU, sizeof(buffer));
    if (!mtu) { [self closeOwned]; return; }
    ssize_t n = recv(self.bridgeFD, buffer, mtu, 0);
    if (n == 0) { [self closeOwned]; return; }
    if (n < 0) {
        if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) [self closeOwned];
        return;
    }
    self.inFlight = [NSData dataWithBytes:buffer length:(NSUInteger)n];
    IOReturn result = [self.channel writeAsync:(void *)self.inFlight.bytes length:(UInt16)n refcon:NULL];
    if (result != kIOReturnSuccess) { [self closeOwned]; livi_native_page_finished(false); }
}
- (void)rfcommChannelData:(IOBluetoothRFCOMMChannel *)channel data:(void *)data length:(size_t)length {
    if (channel != self.channel || self.bridgeFD < 0) return;
    if (length > 262144 || self.toRust.length > 262144-length) {
        [self note:@"RFCOMM receive buffer limit reached; closing only owned stream."];
        [self closeOwned]; return;
    }
    [self.toRust appendBytes:data length:length];
    [self pump];
}
- (void)rfcommChannelWriteComplete:(IOBluetoothRFCOMMChannel *)channel refcon:(void *)refcon status:(IOReturn)error {
    (void)refcon;
    if (channel != self.channel) return;
    self.inFlight = nil;
    if (error != kIOReturnSuccess) { [self closeOwned]; livi_native_page_finished(false); }
}
- (void)rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)channel {
    if (channel != self.channel) return;
    [self closeOwned];
    livi_native_page_finished(false);
    [self note:@"Owned RFCOMM connection lost; recovery remains bounded and does not stop Wi-Fi sessions."];
}
- (void)closeOwned {
    IOBluetoothRFCOMMChannel *old = self.channel;
    self.channel = nil; self.opening = NO;
    [old setDelegate:nil];
    [old closeChannel];
    if (self.bridgeFD >= 0) { close(self.bridgeFD); self.bridgeFD = -1; }
    self.toRust.length = 0;
    // writeAsync reports success only after buffering; this reference also spans normal completion.
    self.inFlight = nil;
}
- (void)disconnect:(id)unused {
    (void)unused;
    NSAlert *alert = [NSAlert new];
    alert.messageText = @"Disconnect LIVI's RFCOMM connection?";
    alert.informativeText = @"Only the channel owned by this development app will close. Existing Wi-Fi projection and unrelated OS Bluetooth links are not reset.";
    [alert addButtonWithTitle:@"Disconnect owned"];
    [alert addButtonWithTitle:@"Cancel"];
    if ([alert runModal] != NSAlertFirstButtonReturn) return;
    self.enabled = NO;
    if (self.pair) self.pairCancelled = YES;
    [self closeOwned];
    [self note:@"Owned connection disconnected; automatic reconnect disabled until explicitly requested."];
}
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)unused {
    (void)unused;
    [self.timer invalidate]; [self.deadline invalidate];
    self.enabled = NO;
    [self withdraw];
    self.pair.delegate = nil;
    // NSApplication's normal termination exits the process before Rust RAII runs.
    [NSApp stop:nil];
    [NSApp postEvent:[NSEvent otherEventWithType:NSEventTypeApplicationDefined
        location:NSZeroPoint modifierFlags:0 timestamp:0 windowNumber:0
        context:nil subtype:0 data1:0 data2:0] atStart:NO];
    return NSTerminateCancel;
}
- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)unused { (void)unused; return YES; }
@end

// Deliberately separate from the hardware path: these checks do not obtain a controller.
@interface LIVITestChannel : NSObject
@property unsigned closes;
@property (weak) LIVINativeAdapter *owner;
@property BOOL closedWhileAvailable;
@end
@implementation LIVITestChannel
- (IOReturn)setDelegate:(id)delegate { (void)delegate; return kIOReturnSuccess; }
- (IOReturn)closeChannel {
    self.closes++;
    self.closedWhileAvailable |= self.owner.available;
    return kIOReturnSuccess;
}
@end

int livi_native_ownership_test(void) {
    @autoreleasepool {
        LIVINativeAdapter *owner = [LIVINativeAdapter new];
        owner.bridgeFD = -1;
        owner.toRust = [NSMutableData data];
        LIVITestChannel *old = [LIVITestChannel new];
        owner.available = YES;
        owner.enabled = YES;
        // A late open after cancellation must be rejected, not turned into an arrival.
        [owner rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)old status:kIOReturnSuccess];
        if (old.closes != 1 || owner.channel != nil) return 1;
        int sockets[2];
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) return 2;
        owner.bridgeFD = sockets[0];
        owner.channel = (IOBluetoothRFCOMMChannel *)old;
        owner.available = YES;
        owner.querying = YES;
        old.owner = owner;
        [owner withdraw];
        char byte;
        ssize_t end = recv(sockets[1], &byte, 1, 0);
        close(sockets[1]);
        if (end != 0 || owner.available || owner.channel || owner.bridgeFD != -1 ||
            old.closedWhileAvailable || !owner.querying || owner.enabled) return 3;
        unsigned closes = old.closes;
        [owner rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)old];
        if (old.closes != closes) return 4; // Stale close is ignored.
        [owner withdraw]; // Repeated teardown is safe.
        return 0;
    }
}

void livi_native_run(const char *target, unsigned scanSeconds) {
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        LIVINativeAdapter *owner = [LIVINativeAdapter new];
        owner.targetAddress = [[NSString stringWithUTF8String:target] uppercaseString];
        owner.scanSeconds = scanSeconds;
        app.delegate = owner;
        [app run];
        app.delegate = nil;
    }
}
