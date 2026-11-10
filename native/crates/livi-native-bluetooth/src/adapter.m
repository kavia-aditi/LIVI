// SPDX-License-Identifier: GPL-3.0-or-later
#import <Foundation/Foundation.h>
#import <CoreBluetooth/CoreBluetooth.h>
#import <IOBluetooth/IOBluetooth.h>
#include <sys/socket.h>
#include <fcntl.h>
#include <unistd.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <errno.h>

typedef struct {
    uint32_t kind, value;
    uint64_t generation;
    uint8_t peer[6], local[6];
    int fd;
} LBTWire;
_Static_assert(sizeof(LBTWire) == 32, "Rust/C event layout");

@interface LBTEvent : NSObject {
@public LBTWire wire;
}
@end
@implementation LBTEvent
- (instancetype)init {
    if ((self = [super init])) wire.fd = -1;
    return self;
}
- (void)dealloc {
    if (wire.fd >= 0) close(wire.fd);
}
@end

static IOBluetoothSDPUUID *AAUUID(void) {
    const unsigned char bytes[] = {
        0x4d,0xe1,0x7a,0x00,0x52,0xcb,0x11,0xe6,0xbd,0xf4,0x08,0x00,0x20,0x0c,0x9a,0x66
    };
    return [IOBluetoothSDPUUID uuidWithBytes:bytes length:16];
}
static NSDictionary *UIntElement(unsigned value, unsigned size) {
    return @{@"DataElementType":@1, @"DataElementSize":@(size), @"DataElementValue":@(value)};
}
static NSDictionary *AAServiceDictionary(void) {
    return @{
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
}
static NSString *Address(IOBluetoothDevice *device) {
    return [[[device addressString] stringByReplacingOccurrencesOfString:@"-" withString:@":"] uppercaseString];
}
static BOOL ParseAddress(NSString *address, uint8_t out[6]) {
    if (address.length != 17) return NO;
    unsigned v[6];
    if (sscanf(address.UTF8String, "%2x:%2x:%2x:%2x:%2x:%2x",
               &v[0], &v[1], &v[2], &v[3], &v[4], &v[5]) != 6) return NO;
    unsigned sum = 0;
    for (unsigned i = 0; i < 6; i++) { out[i] = (uint8_t)v[i]; sum |= v[i]; }
    return sum != 0 && !(out[0] & 1);
}

@interface LBTAdapter : NSObject <CBCentralManagerDelegate,
    IOBluetoothDeviceInquiryDelegate, IOBluetoothDevicePairDelegate,
    IOBluetoothRFCOMMChannelDelegate>
@property CBCentralManager *privacy;
@property IOBluetoothDeviceInquiry *inquiry;
@property IOBluetoothDevicePair *pair;
@property IOBluetoothDevice *target;
@property NSString *targetAddress;
@property IOBluetoothSDPServiceRecord *record;
@property IOBluetoothUserNotification *notification;
@property IOBluetoothRFCOMMChannel *channel;
@property NSMutableArray<LBTEvent *> *events;
@property NSMutableData *toRust;
@property NSData *inFlight;
@property BOOL enabled, available, cancelled, querying, opening, closing, awaitingConsent;
@property BOOL pairedOnly;
@property BOOL diagnosticReporting;
@property uint32_t diagnosticCategory, diagnosticOperation, diagnosticRaw;
@property BOOL diagnosticHasRaw, diagnosticProvisional;
@property NSMutableArray<NSData *> *diagnosticEvents;
@property BOOL diagnosticOverflow, diagnosticIncoming, diagnosticStopped;
@property uint64_t generation;
@property int bridgeFD;
@property BluetoothRFCOMMChannelID listenerChannel;
@property double now, pairDeadline, queryDeadline, openDeadline, inquiryDeadline;
- (BOOL)ready;
- (IOBluetoothSDPServiceRecord *)makeRecord;
- (IOBluetoothUserNotification *)registerChannel:(BluetoothRFCOMMChannelID)channel;
- (BOOL)emit:(uint32_t)kind value:(uint32_t)value generation:(uint64_t)generation;
- (void)withdraw;
- (void)closeOwned;
- (void)publish;
- (void)tick:(double)now;
- (uint32_t)start;
- (uint32_t)discover;
- (uint32_t)connectAddress:(NSString *)address generation:(uint64_t)generation;
- (uint32_t)connectAddress:(NSString *)address generation:(uint64_t)generation pairedOnly:(BOOL)pairedOnly;
- (uint32_t)selectAddress:(NSString *)address generation:(uint64_t)generation
    pairedOnly:(BOOL)pairedOnly queryRemote:(BOOL)queryRemote;
- (IOBluetoothDevice *)deviceForAddress:(NSString *)address;
- (NSString *)localAddress;
- (uint32_t)confirm:(uint64_t)generation accept:(BOOL)accept;
- (void)cancel:(uint64_t)generation;
- (void)fail:(uint32_t)error;
- (void)page;
- (void)adopt:(IOBluetoothRFCOMMChannel *)channel;
- (void)pump;
- (void)sdpQueryComplete:(IOBluetoothDevice *)device status:(IOReturn)status;
- (void)diagnose:(uint32_t)category operation:(uint32_t)operation raw:(IOReturn)raw present:(BOOL)present;
- (void)trace:(uint32_t)kind raw:(IOReturn)raw present:(BOOL)present;
@end

// Public SDP queries have no cancellation handle. Keep the callback target alive,
// and serialize new queries, until the outstanding callback actually returns.
// A destroyed owner is inert and retains no registration, fd, or Rust pointer.
static LBTAdapter *PendingQueryOwner;
static LBTAdapter *PendingOpenOwner;

@implementation LBTAdapter
- (instancetype)init {
    if ((self = [super init])) {
        _bridgeFD = -1;
        _events = [NSMutableArray array];
        _toRust = [NSMutableData data];
    }
    return self;
}
- (BOOL)emit:(uint32_t)kind value:(uint32_t)value generation:(uint64_t)generation {
    if (self.closing) return NO;
    if (self.events.count >= 64) {
        [self.events removeAllObjects];
        self.enabled = NO;
        [self withdraw];
        LBTEvent *failure = [LBTEvent new];
        failure->wire.kind = 6; failure->wire.value = 10;
        [self.events addObject:failure];
        return NO;
    }
    LBTEvent *event = [LBTEvent new];
    event->wire.kind = kind; event->wire.value = value; event->wire.generation = generation;
    [self.events addObject:event];
    return YES;
}
- (uint32_t)start {
    if (self.closing) { [self diagnose:1 operation:1 raw:0 present:NO]; return 1; }
    if (self.enabled) return 0;
    if (![[[NSBundle mainBundle] objectForInfoDictionaryKey:@"NSBluetoothAlwaysUsageDescription"] length]) {
        [self diagnose:8 operation:1 raw:0 present:NO];
        return 2;
    }
    self.enabled = YES;
    self.cancelled = YES; // No selected phone until explicit connect.
    self.privacy = [[CBCentralManager alloc] initWithDelegate:self queue:dispatch_get_main_queue()
        options:@{CBCentralManagerOptionShowPowerAlertKey:@NO}];
    if (!self.privacy) { [self diagnose:1 operation:1 raw:0 present:NO]; self.enabled = NO; return 1; }
    return 0;
}
- (void)diagnose:(uint32_t)category operation:(uint32_t)operation raw:(IOReturn)raw present:(BOOL)present {
    if (!self.diagnosticReporting || (self.diagnosticCategory && !self.diagnosticProvisional)) return;
    self.diagnosticCategory = category; self.diagnosticOperation = operation;
    self.diagnosticRaw = (uint32_t)raw; self.diagnosticHasRaw = present;
    // A failed SDP completion still permits an incoming connection.
    self.diagnosticProvisional = category == 5 && operation >= 6 && operation <= 8;
}
- (void)trace:(uint32_t)kind raw:(IOReturn)raw present:(BOOL)present {
    if (!self.diagnosticReporting || self.diagnosticOverflow) return;
    // Reserve the final slot for a sticky loss marker. No device data or owners.
    if (self.diagnosticEvents.count >= 63) {
        kind = 18; raw = 0; present = NO; self.diagnosticOverflow = YES;
    }
    uint32_t wire[3] = {kind, (uint32_t)raw, present ? 1u : 0u};
    [self.diagnosticEvents addObject:[NSData dataWithBytes:wire length:sizeof(wire)]];
}
- (BOOL)ready {
    if (self.privacy.state != CBManagerStatePoweredOn) return NO;
    IOBluetoothHostController *host = [IOBluetoothHostController defaultController];
    return host && host.powerState == kBluetoothHCIPowerStateON && host.addressAsString.length > 0;
}
- (void)centralManagerDidUpdateState:(CBCentralManager *)manager {
    if (!self.enabled || self.closing || manager != self.privacy) return;
    if (manager.state == CBManagerStateUnauthorized || manager.state == CBManagerStateUnsupported) {
        [self diagnose:manager.state == CBManagerStateUnauthorized ? 8 : 9
            operation:13 raw:0 present:NO];
        [self withdraw];
        [self emit:1 value:0 generation:0];
        [self emit:6 value:1 generation:0];
    }
    // Registration and pumping are driven by the host's existing main-thread timer.
}
- (IOBluetoothSDPServiceRecord *)makeRecord {
    return [IOBluetoothSDPServiceRecord publishedServiceRecordWithDictionary:AAServiceDictionary()];
}
- (IOBluetoothUserNotification *)registerChannel:(BluetoothRFCOMMChannelID)channel {
    return [IOBluetoothRFCOMMChannel registerForChannelOpenNotifications:self
        selector:@selector(incoming:channel:) withChannelID:channel
        direction:kIOBluetoothUserNotificationChannelDirectionIncoming];
}
- (void)publish {
    if (!self.enabled || self.record || self.closing) return;
    self.record = [self makeRecord];
    BluetoothRFCOMMChannelID assigned = 0;
    IOReturn assignedResult = self.record ? [self.record getRFCOMMChannelID:&assigned] : kIOReturnSuccess;
    if (!self.record || assignedResult != kIOReturnSuccess
        || assigned < 1 || assigned > 30) {
        [self diagnose:11 operation:16 raw:assignedResult present:assignedResult != kIOReturnSuccess];
        self.enabled = NO; [self withdraw]; [self emit:6 value:3 generation:0]; return;
    }
    // Subscribe to the channel actually assigned by the public SDP API.
    self.notification = [self registerChannel:assigned];
    if (!self.notification) {
        [self diagnose:11 operation:16 raw:0 present:NO];
        self.enabled = NO; [self withdraw]; [self emit:6 value:3 generation:0]; return;
    }
    self.listenerChannel = assigned;
    self.available = YES;
    [self trace:20 raw:0 present:NO];
    [self emit:1 value:1 generation:0];
}
- (void)withdraw {
    self.available = NO; // Always withdraw BEFORE releasing owned resources.
    self.cancelled = YES;
    if (self.awaitingConsent) [self.pair replyUserConfirmation:NO];
    self.awaitingConsent = NO;
    self.inquiry.delegate = nil;
    [self.inquiry stop]; self.inquiry = nil;
    [self.notification unregister]; self.notification = nil;
    self.listenerChannel = 0;
    [self.record removeServiceRecord]; self.record = nil;
    [self closeOwned];
    // Never stop pairing by disconnecting the device's ACL. Await its completion.
    // A pending SDP query remains busy until its callback; do not impersonate it.
}
- (void)closeOwned {
    IOBluetoothRFCOMMChannel *old = self.channel;
    BOOL pendingOpen = self.opening && PendingOpenOwner == self;
    self.channel = nil; self.opening = NO;
    // Preserve the callback target for a pending open, even after cancellation.
    // The global owner is released only when that original callback returns.
    if (!pendingOpen) [old setDelegate:nil];
    [old closeChannel];
    if (self.bridgeFD >= 0) { close(self.bridgeFD); self.bridgeFD = -1; }
    self.toRust.length = 0; self.inFlight = nil;
}
- (void)tick:(double)now {
    self.now = now;
    if (!self.enabled || self.closing) return;
    if (![self ready]) {
        if (self.available) {
            [self diagnose:9 operation:14 raw:0 present:NO];
            [self withdraw]; [self emit:1 value:0 generation:0];
        }
        return;
    }
    [self publish];
    if (!self.enabled) return;
    if (self.pairedOnly && !self.cancelled && !self.target.isPaired) {
        [self diagnose:4 operation:4 raw:0 present:NO];
        [self fail:6]; return;
    }
    if (self.inquiry && now >= self.inquiryDeadline) {
        self.inquiry.delegate = nil; [self.inquiry stop]; self.inquiry = nil;
    }
    if (!self.cancelled &&
        ((self.pair && now >= self.pairDeadline) ||
         // An incoming channel may win while the outbound query is pending.
         // Keep callback ownership, but its deadline cannot cancel that transport.
         (self.querying && self.bridgeFD < 0 && now >= self.queryDeadline) ||
         (self.opening && now >= self.openDeadline))) {
        [self trace:self.querying ? 15 : (self.opening ? 16 : 17) raw:0 present:NO];
        [self diagnose:7 operation:self.querying ? 5 : (self.opening ? 9 : 15) raw:0 present:NO];
        [self fail:8];
    }
    [self pump];
}
- (uint32_t)discover {
    if (!self.enabled || !self.available || self.closing) return 1;
    if (self.inquiry) return 4;
    self.inquiry = [IOBluetoothDeviceInquiry inquiryWithDelegate:self];
    self.inquiry.inquiryLength = 8;
    self.inquiryDeadline = self.now + 10;
    IOReturn result = [self.inquiry start];
    if (result != kIOReturnSuccess) {
        self.inquiry.delegate = nil; self.inquiry = nil; return 9;
    }
    return 0;
}
- (void)deviceInquiryDeviceFound:(IOBluetoothDeviceInquiry *)sender device:(IOBluetoothDevice *)device {
    if (sender != self.inquiry || !self.available || self.closing) return;
    uint8_t peer[6];
    if (ParseAddress(Address(device), peer) && [self emit:2 value:0 generation:0]) {
        memcpy(self.events.lastObject->wire.peer, peer, 6);
    }
}
- (void)deviceInquiryComplete:(IOBluetoothDeviceInquiry *)sender error:(IOReturn)error aborted:(BOOL)aborted {
    (void)aborted;
    if (sender != self.inquiry) return;
    self.inquiry.delegate = nil; self.inquiry = nil;
    if (error != kIOReturnSuccess && self.enabled) [self emit:6 value:9 generation:0];
}
- (uint32_t)connectAddress:(NSString *)address generation:(uint64_t)generation {
    return [self connectAddress:address generation:generation pairedOnly:NO];
}
- (IOBluetoothDevice *)deviceForAddress:(NSString *)address {
    return [IOBluetoothDevice deviceWithAddressString:address];
}
- (NSString *)localAddress {
    return [[[IOBluetoothHostController defaultController] addressAsString]
        stringByReplacingOccurrencesOfString:@"-" withString:@":"];
}
- (uint32_t)connectAddress:(NSString *)address generation:(uint64_t)generation pairedOnly:(BOOL)pairedOnly {
    return [self selectAddress:address generation:generation pairedOnly:pairedOnly queryRemote:YES];
}
- (uint32_t)selectAddress:(NSString *)address generation:(uint64_t)generation
    pairedOnly:(BOOL)pairedOnly queryRemote:(BOOL)queryRemote {
    // Passive selection is paired-only and can never reach the pairing path.
    if (!queryRemote && !pairedOnly) return 6;
    if (!self.enabled || !self.available || self.closing) {
        [self diagnose:1 operation:2 raw:0 present:NO]; return 1;
    }
    if (self.pair || self.querying || self.opening || self.channel ||
        PendingQueryOwner || PendingOpenOwner) {
        [self diagnose:2 operation:2 raw:0 present:NO]; return 4;
    }
    self.target = [self deviceForAddress:address];
    if (!self.target) { [self diagnose:3 operation:3 raw:0 present:NO]; return 5; }
    if (pairedOnly && !self.target.isPaired) {
        [self diagnose:4 operation:4 raw:0 present:NO]; return 6;
    }
    if (!self.target.isPaired && self.target.isConnected) return 4;
    self.pairedOnly = pairedOnly;
    self.targetAddress = address; self.generation = generation; self.cancelled = NO;
    if (self.target.isPaired) {
        if (queryRemote) [self page];
        else [self trace:21 raw:0 present:NO];
        return 0;
    }
    // A bond may disappear between checks. Diagnostic mode must never fall
    // through to the ordinary pairing path, even during that race.
    if (pairedOnly) {
        [self diagnose:4 operation:4 raw:0 present:NO];
        self.cancelled = YES; return 6;
    }
    self.pair = [IOBluetoothDevicePair pairWithDevice:self.target];
    if (!self.pair) return 9;
    self.pair.delegate = self; self.pairDeadline = self.now + 60;
    IOReturn result = [self.pair start];
    if (result != kIOReturnSuccess) {
        self.pair.delegate = nil; self.pair = nil; self.cancelled = YES; return 9;
    }
    return 0;
}
- (void)devicePairingUserConfirmationRequest:(id)sender numericValue:(BluetoothNumericValue)value {
    if (sender != self.pair) return;
    if (self.pairedOnly) { [self.pair replyUserConfirmation:NO]; [self fail:6]; return; }
    if (!self.enabled || !self.available || self.cancelled || self.closing || value > 999999) {
        [self.pair replyUserConfirmation:NO]; return;
    }
    self.awaitingConsent = YES;
    [self emit:3 value:(uint32_t)value generation:self.generation];
}
- (uint32_t)confirm:(uint64_t)generation accept:(BOOL)accept {
    if (self.pairedOnly) return 6;
    if (!self.awaitingConsent || generation != self.generation || !self.pair
        || self.cancelled || !self.enabled || !self.available || self.closing) return 6;
    self.awaitingConsent = NO;
    // A response can only come from an explicit host comparison-code decision.
    [self.pair replyUserConfirmation:accept];
    if (!accept) [self fail:6];
    return 0;
}
- (void)devicePairingPINCodeRequest:(id)sender {
    if (sender == self.pair) [self fail:7]; // Never guess or persist a legacy PIN.
}
- (void)devicePairingUserPasskeyNotification:(id)sender passkey:(BluetoothPasskey)key {
    if (sender == self.pair && !self.cancelled && self.enabled && !self.closing && key <= 999999)
        [self emit:4 value:key generation:self.generation];
}
- (void)devicePairingFinished:(id)sender error:(IOReturn)error {
    if (sender != self.pair) return;
    self.pair.delegate = nil; self.pair = nil; self.awaitingConsent = NO;
    if (self.cancelled || self.closing || !self.enabled || !self.available) return;
    if (error != kIOReturnSuccess) { [self fail:6]; return; }
    [self page];
}
- (void)page {
    if (!self.enabled || !self.available || self.cancelled || self.closing
        || self.querying || self.opening || self.channel ||
        PendingQueryOwner || PendingOpenOwner) return;
    if (!self.target.isPaired) { [self diagnose:4 operation:4 raw:0 present:NO]; [self fail:6]; return; }
    self.querying = YES; self.queryDeadline = self.now + 15;
    PendingQueryOwner = self;
    [self trace:1 raw:0 present:NO];
    IOReturn result = [self.target performSDPQuery:self uuids:@[AAUUID()]];
    [self trace:result == kIOReturnSuccess ? 2 : 3 raw:result present:YES];
    if (result != kIOReturnSuccess && self.querying) {
        [self diagnose:5 operation:5 raw:result present:YES];
        self.querying = NO;
        if (PendingQueryOwner == self) PendingQueryOwner = nil;
        [self fail:9];
    }
}
- (void)sdpQueryComplete:(IOBluetoothDevice *)device status:(IOReturn)status {
    __attribute__((objc_precise_lifetime)) LBTAdapter *keepAlive = self;
    [self trace:4 raw:status present:YES];
    if (device != self.target || !self.querying) {
        [self trace:6 raw:status present:YES]; return;
    }
    self.querying = NO;
    if (PendingQueryOwner == self) PendingQueryOwner = nil;
    if (self.cancelled || self.closing || !self.enabled || !self.available) {
        [self trace:7 raw:status present:YES]; return;
    }
    if (self.channel) { [self trace:6 raw:status present:YES]; return; }
    [self trace:5 raw:status present:YES];
    if (self.pairedOnly && !self.target.isPaired) {
        [self diagnose:4 operation:4 raw:0 present:NO]; [self fail:6]; return;
    }
    IOBluetoothSDPServiceRecord *remote = [device getServiceRecordForUUID:AAUUID()];
    [self trace:remote ? 8 : 9 raw:0 present:NO];
    BluetoothRFCOMMChannelID channelID = 0;
    // Preserve the original short-circuit evaluation and wait behavior.
    IOReturn channelResult = kIOReturnSuccess;
    if (status == kIOReturnSuccess && remote) channelResult = [remote getRFCOMMChannelID:&channelID];
    if (status == kIOReturnSuccess && remote) {
        [self trace:channelResult == kIOReturnSuccess && channelID ? 10 : 11
            raw:channelResult present:YES];
    } else [self trace:12 raw:0 present:NO];
    if (status != kIOReturnSuccess || !remote || channelResult != kIOReturnSuccess || !channelID) {
        if (status != kIOReturnSuccess) [self diagnose:5 operation:6 raw:status present:YES];
        else if (!remote) [self diagnose:5 operation:7 raw:0 present:NO];
        else [self diagnose:5 operation:8 raw:channelResult present:channelResult != kIOReturnSuccess];
        // Remain available for a phone-initiated connection; no unbounded retry.
        return;
    }
    self.opening = YES; self.openDeadline = self.now + 10;
    PendingOpenOwner = self;
    IOBluetoothRFCOMMChannel *channel = nil;
    IOReturn result = [device openRFCOMMChannelAsync:&channel withChannelID:channelID delegate:self];
    if (result == kIOReturnSuccess) {
        if (PendingOpenOwner == self && self.opening && !self.channel) self.channel = channel;
        else if (channel != self.channel) {
            [channel setDelegate:nil]; [channel closeChannel];
        }
    } else {
        if (PendingOpenOwner == self) PendingOpenOwner = nil;
        if (channel != self.channel) { [channel setDelegate:nil]; [channel closeChannel]; }
        [self diagnose:6 operation:9 raw:result present:YES];
        [self fail:9];
    }
    (void)keepAlive;
}
- (void)incoming:(IOBluetoothUserNotification *)notification channel:(IOBluetoothRFCOMMChannel *)channel {
    if (self.diagnosticReporting) self.diagnosticIncoming = YES;
    [self trace:13 raw:0 present:NO];
    if (notification != self.notification || !self.enabled || !self.available ||
        self.cancelled || self.closing || self.channel || PendingOpenOwner ||
        ![channel isIncoming] || [channel getChannelID] != self.listenerChannel ||
        ![Address([channel getDevice]) isEqualToString:self.targetAddress]) {
        [self trace:26 raw:0 present:NO];
        [channel closeChannel]; return;
    }
    self.channel = channel;
    BOOL open = [channel isOpen];
    [self trace:open ? 22 : 23 raw:0 present:NO];
    if (!open) {
        [self diagnose:6 operation:10 raw:0 present:NO]; [self fail:9]; return;
    }
    IOReturn attached = [channel setDelegate:self];
    [self trace:attached == kIOReturnSuccess ? 24 : 25 raw:attached present:YES];
    if (attached != kIOReturnSuccess) {
        [self diagnose:6 operation:10 raw:attached present:YES]; [self fail:9]; return;
    }
    [self adopt:channel];
}
- (void)rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:(IOReturn)error {
    __attribute__((objc_precise_lifetime)) LBTAdapter *keepAlive = self;
    [self trace:27 raw:error present:YES];
    if (PendingOpenOwner != self) {
        // An incoming channel was already open when adopted. A successful
        // completion for that exact live stream must not tear down its bridge.
        if (channel == self.channel && self.bridgeFD >= 0 && !self.opening &&
            !self.cancelled && self.enabled && self.available && !self.closing) {
            if (error != kIOReturnSuccess) {
                [self diagnose:6 operation:10 raw:error present:YES]; [self fail:9];
            }
            return;
        }
        [channel setDelegate:nil]; [channel closeChannel]; return;
    }
    PendingOpenOwner = nil;
    // The completion may run before openRFCOMMChannelAsync fills its out-param.
    if (self.opening && !self.channel && !self.cancelled) self.channel = channel;
    if (channel != self.channel || !self.opening || self.cancelled ||
        !self.enabled || !self.available || self.closing) {
        [channel setDelegate:nil]; [channel closeChannel]; return;
    }
    self.opening = NO;
    if (error != kIOReturnSuccess) {
        [self diagnose:6 operation:10 raw:error present:YES]; [self fail:9]; return;
    }
    [self adopt:channel];
    (void)keepAlive;
}
- (void)adopt:(IOBluetoothRFCOMMChannel *)channel {
    if (self.bridgeFD >= 0 || channel != self.channel) return;
    if (self.pairedOnly && !self.target.isPaired) {
        [self diagnose:4 operation:4 raw:0 present:NO]; [self fail:6]; return;
    }
    if (![channel isOpen] || ![channel getMTU] ||
        ![Address([channel getDevice]) isEqualToString:self.targetAddress]) {
        [self diagnose:6 operation:10 raw:0 present:NO]; [self fail:9]; return;
    }
    int sockets[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) { [self fail:9]; return; }
    for (unsigned i = 0; i < 2; i++) {
        int flags = fcntl(sockets[i], F_GETFL);
        int yes = 1;
        if (flags < 0 || fcntl(sockets[i], F_SETFD, FD_CLOEXEC) < 0 ||
            fcntl(sockets[i], F_SETFL, flags | O_NONBLOCK) < 0 ||
            setsockopt(sockets[i], SOL_SOCKET, SO_NOSIGPIPE, &yes, sizeof(yes)) < 0) {
            close(sockets[0]); close(sockets[1]); [self fail:9]; return;
        }
    }
    uint8_t peer[6], local[6];
    NSString *host = [self localAddress];
    if (!ParseAddress(Address([channel getDevice]), peer) || !ParseAddress(host, local)) {
        close(sockets[0]); close(sockets[1]); [self fail:5]; return;
    }
    if (self.diagnosticProvisional) {
        self.diagnosticCategory = 0; self.diagnosticProvisional = NO;
    }
    self.bridgeFD = sockets[0]; self.opening = NO;
    if (![self emit:5 value:0 generation:self.generation]) { close(sockets[1]); return; }
    LBTEvent *event = self.events.lastObject;
    event->wire.fd = sockets[1];
    memcpy(event->wire.peer, peer, 6); memcpy(event->wire.local, local, 6);
    [self trace:19 raw:0 present:NO]; // Open selected-peer channel and bridge fd now owned.
}
- (void)pump {
    if (self.bridgeFD < 0 || !self.channel || self.opening) return;
    if (self.toRust.length) {
        ssize_t count = send(self.bridgeFD, self.toRust.bytes, self.toRust.length, 0);
        if (count > 0)
            [self.toRust replaceBytesInRange:NSMakeRange(0, (NSUInteger)count) withBytes:NULL length:0];
        else if (count < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) {
            [self fail:9]; return;
        }
    }
    if (self.inFlight) return;
    unsigned char buffer[4096];
    size_t mtu = MIN((size_t)self.channel.getMTU, sizeof(buffer));
    if (!mtu) { [self fail:9]; return; }
    ssize_t count = recv(self.bridgeFD, buffer, mtu, 0);
    if (count == 0) {
        [self closeOwned]; self.cancelled = YES;
        [self emit:7 value:0 generation:self.generation]; return;
    }
    if (count < 0) {
        if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) [self fail:9];
        return;
    }
    self.inFlight = [NSData dataWithBytes:buffer length:(NSUInteger)count];
    IOReturn writeResult = [self.channel writeAsync:(void *)self.inFlight.bytes length:(UInt16)count refcon:NULL];
    if (writeResult != kIOReturnSuccess) {
        [self diagnose:6 operation:11 raw:writeResult present:YES]; [self fail:9];
    }
}
- (void)rfcommChannelData:(IOBluetoothRFCOMMChannel *)channel data:(void *)data length:(size_t)length {
    if (channel != self.channel || self.bridgeFD < 0 || self.cancelled || self.closing) return;
    if (length > 262144 || self.toRust.length > 262144 - length) { [self fail:10]; return; }
    [self.toRust appendBytes:data length:length]; [self pump];
}
- (void)rfcommChannelWriteComplete:(IOBluetoothRFCOMMChannel *)channel refcon:(void *)refcon status:(IOReturn)error {
    (void)refcon;
    if (channel != self.channel) return;
    self.inFlight = nil;
    if (error != kIOReturnSuccess) {
        [self diagnose:6 operation:12 raw:error present:YES]; [self fail:9];
    }
}
- (void)rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)channel {
    if (channel != self.channel) return;
    [self diagnose:6 operation:20 raw:0 present:NO];
    [self closeOwned]; self.cancelled = YES;
    [self emit:7 value:0 generation:self.generation];
}
- (void)cancel:(uint64_t)generation {
    if (generation != self.generation) return;
    self.cancelled = YES;
    if (self.awaitingConsent) [self.pair replyUserConfirmation:NO];
    self.awaitingConsent = NO;
    [self closeOwned];
    // Remove queued callbacks and undelivered descriptors for this attempt.
    NSIndexSet *stale = [self.events indexesOfObjectsPassingTest:
        ^BOOL(LBTEvent *event, NSUInteger index, BOOL *stop) {
            (void)index; (void)stop; return event->wire.generation == generation;
        }];
    [self.events removeObjectsAtIndexes:stale];
}
- (void)fail:(uint32_t)error {
    uint64_t generation = self.generation;
    // Operation-specific detail already recorded above takes precedence.
    if (!self.diagnosticCategory || self.diagnosticProvisional) {
        if (error == 6) [self diagnose:4 operation:4 raw:0 present:NO];
        else if (error == 8) [self diagnose:7 operation:15 raw:0 present:NO];
        else [self diagnose:10 operation:21 raw:0 present:NO];
    }
    [self cancel:generation];
    [self emit:6 value:error generation:generation];
}
@end

bool livi_bt_is_main_thread(void) { return pthread_main_np() != 0; }
void *livi_bt_create(void) {
    if (!livi_bt_is_main_thread()) return NULL;
    return (__bridge_retained void *)[LBTAdapter new];
}
void livi_bt_stop(void *pointer) {
    LBTAdapter *owner = (__bridge LBTAdapter *)pointer;
    if (owner.diagnosticReporting && !owner.diagnosticStopped) {
        if (!owner.diagnosticIncoming) [owner trace:14 raw:0 present:NO];
        owner.diagnosticStopped = YES;
    }
    owner.enabled = NO;
    [owner withdraw];
    owner.privacy.delegate = nil; owner.privacy = nil;
    [owner.events removeAllObjects];
}
void livi_bt_destroy(void *pointer) {
    if (!pointer) return;
    LBTAdapter *owner = (__bridge_transfer LBTAdapter *)pointer;
    livi_bt_stop((__bridge void *)owner);
    owner.closing = YES;
    owner.pair.delegate = nil; owner.pair = nil;
    // PendingQueryOwner keeps only an inert callback target alive until completion.
}
uint32_t livi_bt_start(void *pointer) { return [(__bridge LBTAdapter *)pointer start]; }
void livi_bt_tick(void *pointer, double now) { [(__bridge LBTAdapter *)pointer tick:now]; }
bool livi_bt_event(void *pointer, LBTWire *out) {
    LBTAdapter *owner = (__bridge LBTAdapter *)pointer;
    LBTEvent *event = owner.events.firstObject;
    if (!event) return false;
    *out = event->wire; event->wire.fd = -1; // Exact transfer, including descriptor 0.
    [owner.events removeObjectAtIndex:0];
    return true;
}
uint32_t livi_bt_discover(void *pointer) { return [(__bridge LBTAdapter *)pointer discover]; }
uint32_t livi_bt_connect(void *pointer, uint64_t generation, const uint8_t *peer) {
    NSString *address = [NSString stringWithFormat:@"%02X:%02X:%02X:%02X:%02X:%02X",
        peer[0], peer[1], peer[2], peer[3], peer[4], peer[5]];
    return [(__bridge LBTAdapter *)pointer connectAddress:address generation:generation];
}
uint32_t livi_bt_confirm(void *pointer, uint64_t generation, bool accept) {
    return [(__bridge LBTAdapter *)pointer confirm:generation accept:accept];
}
void livi_bt_cancel(void *pointer, uint64_t generation) {
    [(__bridge LBTAdapter *)pointer cancel:generation];
}

uint32_t livi_bt_connect_paired(void *pointer, uint64_t generation, const uint8_t *peer) {
    NSString *address = [NSString stringWithFormat:@"%02X:%02X:%02X:%02X:%02X:%02X",
        peer[0], peer[1], peer[2], peer[3], peer[4], peer[5]];
    return [(__bridge LBTAdapter *)pointer connectAddress:address generation:generation pairedOnly:YES];
}
uint32_t livi_bt_listen_paired(void *pointer, uint64_t generation, const uint8_t *peer) {
    NSString *address = [NSString stringWithFormat:@"%02X:%02X:%02X:%02X:%02X:%02X",
        peer[0], peer[1], peer[2], peer[3], peer[4], peer[5]];
    return [(__bridge LBTAdapter *)pointer selectAddress:address generation:generation
        pairedOnly:YES queryRemote:NO];
}
bool livi_bt_diagnostic_idle(void *pointer) {
    LBTAdapter *owner = (__bridge LBTAdapter *)pointer;
    return !owner.enabled && !owner.available && !owner.channel && owner.bridgeFD < 0 &&
        !owner.record && !owner.notification && !owner.privacy && !owner.inquiry && !owner.pair &&
        PendingQueryOwner != owner && PendingOpenOwner != owner && !owner.events.count;
}
bool livi_bt_diagnostic_cleanup_snapshot(void *pointer, uint32_t *out) {
    LBTAdapter *owner = (__bridge LBTAdapter *)pointer;
    if (!owner.diagnosticReporting) return false;
    // Local ownership only. Never query Bluetooth or mutate/drain resources.
    uint32_t bits = 0;
    if (PendingQueryOwner == owner) bits |= 1u << 0;
    if (PendingOpenOwner == owner) bits |= 1u << 1;
    if (owner.channel) bits |= 1u << 2;
    if (owner.bridgeFD >= 0) bits |= 1u << 3;
    if (owner.record) bits |= 1u << 4;
    if (owner.notification) bits |= 1u << 5;
    if (owner.privacy) bits |= 1u << 6;
    if (owner.inquiry) bits |= 1u << 7;
    if (owner.pair) bits |= 1u << 8;
    if (owner.events.count) bits |= 1u << 9;
    if (owner.enabled) bits |= 1u << 10;
    if (owner.available) bits |= 1u << 11;
    // Queued descriptors are context in addition to the queued-event blocker.
    for (LBTEvent *event in owner.events) {
        if (event->wire.fd >= 0) { bits |= 1u << 12; break; }
    }
    *out = bits;
    return true;
}
void livi_bt_diagnostic_reporting(void *pointer) {
    LBTAdapter *owner = (__bridge LBTAdapter *)pointer;
    if (!owner.diagnosticReporting) owner.diagnosticEvents = [NSMutableArray array];
    owner.diagnosticReporting = YES;
}
bool livi_bt_diagnostic_event(void *pointer, uint32_t *out) {
    LBTAdapter *owner = (__bridge LBTAdapter *)pointer;
    NSData *event = owner.diagnosticEvents.firstObject;
    if (!event) return false;
    memcpy(out, event.bytes, 3 * sizeof(uint32_t));
    [owner.diagnosticEvents removeObjectAtIndex:0];
    return true;
}
bool livi_bt_diagnostic_failure(void *pointer, uint32_t *out) {
    LBTAdapter *owner = (__bridge LBTAdapter *)pointer;
    if (!owner.diagnosticReporting) return false;
    // Read existing manager state only; never query a device/controller here.
    if (!owner.diagnosticCategory && owner.enabled && !owner.available) {
        if (owner.privacy.state == CBManagerStateUnauthorized)
            [owner diagnose:8 operation:13 raw:0 present:NO];
        else [owner diagnose:9 operation:14 raw:0 present:NO];
    }
    if (!owner.diagnosticCategory) return false;
    out[0] = owner.diagnosticCategory; out[1] = owner.diagnosticOperation;
    out[2] = owner.diagnosticRaw; out[3] = owner.diagnosticHasRaw ? 1 : 0;
    return true;
}

// These fakes never construct a Bluetooth manager, query a controller, publish
// a service or start pairing. Older lifecycle scenarios stub successful adoption;
// bridge scenarios use real adoption with a fake local-address provider instead.
@interface LBTFakeChannel : NSObject
@property unsigned closes;
@property BOOL closedWhileAvailable;
@property (weak) LBTAdapter *owner;
@property IOReturn writeResult;
@property id device;
@property BOOL reportsClosed, zeroMTU;
@property NSData *written;
@property BOOL reportsOutgoing;
@property BluetoothRFCOMMChannelID testChannelID;
@property IOReturn delegateResult;
@end
@implementation LBTFakeChannel
- (IOBluetoothDevice *)getDevice { return self.device; }
- (IOReturn)setDelegate:(id)delegate { return delegate ? self.delegateResult : kIOReturnSuccess; }
- (BOOL)isIncoming { return !self.reportsOutgoing; }
- (BluetoothRFCOMMChannelID)getChannelID { return self.testChannelID ?: 8; }
- (BOOL)isOpen { return !self.reportsClosed; }
- (BluetoothRFCOMMMTU)getMTU { return self.zeroMTU ? 0 : 64; }
- (IOReturn)writeAsync:(void *)data length:(UInt16)length refcon:(void *)refcon {
    (void)refcon;
    self.written = [NSData dataWithBytes:data length:length];
    return self.writeResult;
}
- (IOReturn)closeChannel {
    self.closes++; self.closedWhileAvailable |= self.owner.available;
    return kIOReturnSuccess;
}
@end
@interface LBTFakePair : NSObject
@property unsigned replies;
@property BOOL accepted;
@end
@implementation LBTFakePair
- (void)setDelegate:(id)delegate { (void)delegate; }
- (void)replyUserConfirmation:(BOOL)accept { self.replies++; self.accepted = accept; }
@end
@interface LBTFakeRecord : NSObject
@property unsigned removes;
@property BOOL conflict;
@property IOReturn channelResult;
@property BluetoothRFCOMMChannelID assigned;
@end
@implementation LBTFakeRecord
- (IOReturn)getRFCOMMChannelID:(BluetoothRFCOMMChannelID *)channel {
    *channel = self.conflict ? 0 : (self.assigned ?: 8); return self.channelResult;
}
- (IOReturn)removeServiceRecord { self.removes++; return kIOReturnSuccess; }
@end
@interface LBTFakeToken : NSObject
@property unsigned unregisters;
@end
@implementation LBTFakeToken
- (void)unregister { self.unregisters++; }
@end
@interface LBTFakeDevice : NSObject
@property BOOL paired;
@property IOReturn queryResult, openResult;
@property LBTFakeRecord *remoteRecord;
@property BOOL loseBondAfterFirstCheck;
@property unsigned pairedChecks;
@property BOOL completeDuringQuery;
@property BOOL deferCompletion;
@property (weak) id callbackTarget;
@property unsigned completions;
@property unsigned queries, opens;
@property NSString *testAddress;
@end
@implementation LBTFakeDevice
- (NSString *)addressString { return self.testAddress ?: @"02:01:02:03:04:05"; }
- (BOOL)isPaired {
    self.pairedChecks++;
    return self.paired && (!self.loseBondAfterFirstCheck || self.pairedChecks == 1);
}
- (BOOL)isConnected { return NO; }
- (IOReturn)performSDPQuery:(id)target uuids:(NSArray *)uuids {
    (void)uuids;
    self.queries++;
    if (self.completeDuringQuery)
        [target sdpQueryComplete:(IOBluetoothDevice *)self status:kIOReturnSuccess];
    if (self.deferCompletion && self.queryResult == kIOReturnSuccess) {
        self.callbackTarget = target; // The fake must not supply callback-owner retention.
        [NSTimer scheduledTimerWithTimeInterval:0.001 target:self
            selector:@selector(completeQuery:) userInfo:nil repeats:NO];
    }
    return self.queryResult;
}
- (void)completeQuery:(NSTimer *)timer {
    (void)timer;
    self.completions++;
    [self.callbackTarget sdpQueryComplete:(IOBluetoothDevice *)self status:kIOReturnSuccess];
}
- (IOBluetoothSDPServiceRecord *)getServiceRecordForUUID:(IOBluetoothSDPUUID *)uuid {
    (void)uuid; return (IOBluetoothSDPServiceRecord *)self.remoteRecord;
}
- (IOReturn)openRFCOMMChannelAsync:(IOBluetoothRFCOMMChannel **)channel
    withChannelID:(BluetoothRFCOMMChannelID)channelID delegate:(id)delegate {
    self.opens++;
    (void)channelID; (void)delegate; *channel = nil; return self.openResult;
}
@end
@interface LBTFakePrivacy : NSObject
@property CBManagerState state;
@property (weak) id delegate;
@end
@implementation LBTFakePrivacy
@end
#if defined(LIVI_OFFLINE_INQUIRY_TESTS)
@interface LBTFakeInquiry : NSObject
@property (weak) id delegate;
@property unsigned stops;
@end
@implementation LBTFakeInquiry
- (IOReturn)stop { self.stops++; return kIOReturnSuccess; }
@end
#endif
@interface LBTFakeAdapter : LBTAdapter
@property LBTFakeRecord *fakeRecord;
@property LBTFakeToken *fakeToken;
@property BOOL failNotification;
@property LBTFakeDevice *fakeDevice;
@property unsigned pages;
@property BOOL useRealPage, notReady;
@property BOOL fakeAdopt;
@property unsigned adoptions;
@end
@implementation LBTFakeAdapter
- (NSString *)localAddress { return @"02:06:07:08:09:0A"; }
- (void)adopt:(IOBluetoothRFCOMMChannel *)channel {
    if (self.fakeAdopt) { self.adoptions++; return; }
    [super adopt:channel];
}
- (BOOL)ready { return !self.notReady; }
- (IOBluetoothDevice *)deviceForAddress:(NSString *)address {
    (void)address; return (IOBluetoothDevice *)self.fakeDevice;
}
- (void)page {
    if (self.fakeDevice && !self.useRealPage) { self.pages++; return; }
    [super page];
}
- (IOBluetoothSDPServiceRecord *)makeRecord {
    return (IOBluetoothSDPServiceRecord *)self.fakeRecord;
}
- (IOBluetoothUserNotification *)registerChannel:(BluetoothRFCOMMChannelID)channel {
    if (channel != (self.fakeRecord.assigned ?: 8) || self.failNotification) return nil;
    return (IOBluetoothUserNotification *)self.fakeToken;
}
@end

static BOOL DiagnosticMatches(LBTAdapter *owner, uint32_t category, uint32_t operation,
    IOReturn raw, BOOL present) {
    uint32_t out[4] = {0};
    return livi_bt_diagnostic_failure((__bridge void *)owner, out) &&
        out[0] == category && out[1] == operation &&
        out[2] == (uint32_t)raw && out[3] == (present ? 1u : 0u);
}

static BOOL TraceMatches(LBTAdapter *owner, NSArray<NSNumber *> *kinds) {
    if (owner.diagnosticEvents.count != kinds.count) return NO;
    for (NSNumber *kind in kinds) {
        uint32_t wire[3] = {0};
        if (!livi_bt_diagnostic_event((__bridge void *)owner, wire) ||
            wire[0] != kind.unsignedIntValue) return NO;
    }
    return YES;
}

static uint32_t LifecycleTest(LBTFakeAdapter *owner, LBTFakeChannel *channel, unsigned scenario) {
    livi_bt_diagnostic_reporting((__bridge void *)owner);
    owner.fakeDevice = [LBTFakeDevice new]; owner.fakeDevice.paired = YES;
    owner.target = (IOBluetoothDevice *)owner.fakeDevice;
    owner.record = (IOBluetoothSDPServiceRecord *)owner.fakeRecord;
    owner.targetAddress = @"02:01:02:03:04:05";
    owner.pairedOnly = YES; owner.useRealPage = YES;
    owner.querying = YES; PendingQueryOwner = owner;
    switch (scenario) {
        case 41:
        case 42:
            owner.querying = NO; PendingQueryOwner = nil;
            owner.fakeDevice.queryResult = scenario == 42 ? kIOReturnError : kIOReturnSuccess;
            [owner page];
            const uint32_t *queryRaw = owner.diagnosticEvents.lastObject.bytes;
            if (queryRaw[1] != (uint32_t)owner.fakeDevice.queryResult || queryRaw[2] != 1) return 126;
            if (!TraceMatches(owner, scenario == 41 ? @[@1,@2] : @[@1,@3])) return 101;
            if ((PendingQueryOwner == owner) != (scenario == 41)) return 102;
            break;
        case 43: {
            LBTFakeDevice *other = [LBTFakeDevice new];
            [owner sdpQueryComplete:(IOBluetoothDevice *)other status:kIOReturnError];
            if (!TraceMatches(owner, @[@4,@6]) || !owner.querying || PendingQueryOwner != owner) return 103;
            break;
        }
        case 44:
            owner.querying = NO; PendingQueryOwner = nil;
            [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
            if (!TraceMatches(owner, @[@4,@6])) return 104;
            break;
        case 45:
            owner.channel = (IOBluetoothRFCOMMChannel *)channel;
            [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
            if (!TraceMatches(owner, @[@4,@6]) || owner.querying || PendingQueryOwner) return 105;
            break;
        case 46:
        case 47:
        case 48:
        case 49:
        case 50:
            if (scenario == 46) [owner cancel:7];
            if (scenario == 47) livi_bt_stop((__bridge void *)owner);
            if (scenario == 48) owner.closing = YES;
            if (scenario == 49) owner.enabled = NO;
            if (scenario == 50) owner.available = NO;
            [owner sdpQueryComplete:owner.target status:kIOReturnError];
            if (!TraceMatches(owner, scenario == 47 ? @[@14,@4,@7] : @[@4,@7]) ||
                owner.querying || PendingQueryOwner || owner.opening) return 106;
            break;
        case 51:
            owner.fakeDevice.paired = NO;
            [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
            if (!TraceMatches(owner, @[@4,@5]) || !owner.cancelled || owner.opening) return 107;
            break;
        case 52:
        case 53:
        case 54:
        case 55:
        case 56:
        case 57: {
            if (scenario >= 54) owner.fakeDevice.remoteRecord = owner.fakeRecord;
            if (scenario == 54) owner.fakeRecord.conflict = YES;
            if (scenario == 55) owner.fakeRecord.channelResult = kIOReturnBadArgument;
            IOReturn status = scenario == 52 || scenario == 57 ? kIOReturnError : kIOReturnSuccess;
            [owner sdpQueryComplete:owner.target status:status];
            uint32_t wire[3] = {0};
            if (!livi_bt_diagnostic_event((__bridge void *)owner, wire) ||
                wire[0] != 4 || wire[1] != (uint32_t)status || wire[2] != 1) return 108;
            NSArray *expected = scenario == 52 ? @[@5,@9,@12] :
                scenario == 53 ? @[@5,@9,@12] :
                scenario == 57 ? @[@5,@8,@12] :
                scenario == 56 ? @[@5,@8,@10] : @[@5,@8,@11];
            if (scenario == 55) {
                NSData *last = owner.diagnosticEvents.lastObject;
                const uint32_t *raw = last.bytes;
                if (raw[1] != (uint32_t)kIOReturnBadArgument || raw[2] != 1) return 109;
            }
            if (!TraceMatches(owner, expected) || owner.querying || PendingQueryOwner ||
                owner.opening != (scenario == 56)) return 110;
            break;
        }
        case 58:
        case 59:
        case 60:
            owner.queryDeadline = 1;
            [owner tick:1];
            if (!owner.cancelled || !owner.querying || PendingQueryOwner != owner ||
                !TraceMatches(owner, @[@15])) return 111;
            if (scenario == 59) {
                [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
                if (!TraceMatches(owner, @[@4,@7]) || PendingQueryOwner) return 112;
            }
            if (scenario == 60) {
                livi_bt_stop((__bridge void *)owner);
                if (livi_bt_diagnostic_idle((__bridge void *)owner)) return 113;
                [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
                if (!TraceMatches(owner, @[@14,@4,@7]) ||
                    !livi_bt_diagnostic_idle((__bridge void *)owner)) return 114;
            }
            break;
        case 61:
        case 62:
            owner.querying = NO; PendingQueryOwner = nil;
            if (scenario == 61) { owner.opening = YES; owner.openDeadline = 1; PendingOpenOwner = owner; }
            else { owner.pair = (IOBluetoothDevicePair *)[LBTFakePair new]; owner.pairDeadline = 1; }
            [owner tick:1];
            if (!owner.cancelled || !TraceMatches(owner, scenario == 61 ? @[@16] : @[@17])) return 115;
            break;
        case 63:
        case 64:
            channel.device = owner.fakeDevice; owner.fakeAdopt = YES;
            owner.notification = (IOBluetoothUserNotification *)owner.fakeToken;
            [owner incoming:scenario == 63 ? nil : owner.notification channel:(IOBluetoothRFCOMMChannel *)channel];
            livi_bt_stop((__bridge void *)owner);
            if (!TraceMatches(owner, scenario == 63 ? @[@13,@26] : @[@13,@22,@24]) ||
                owner.adoptions != (scenario == 64 ? 1u : 0u) ||
                channel.closes != 1 || PendingQueryOwner != owner) return 116;
            break;
        case 65:
            livi_bt_stop((__bridge void *)owner); livi_bt_stop((__bridge void *)owner);
            if (!TraceMatches(owner, @[@14])) return 117;
            break;
        case 66:
            for (unsigned i = 0; i < 100; i++) [owner trace:4 raw:kIOReturnError present:YES];
            if (owner.diagnosticEvents.count != 64 || !owner.diagnosticOverflow) return 118;
            for (unsigned i = 0; i < 64; i++) {
                uint32_t wire[3] = {0};
                if (!livi_bt_diagnostic_event((__bridge void *)owner, wire) ||
                    wire[0] != (i == 63 ? 18u : 4u) ||
                    wire[2] != (i == 63 ? 0u : 1u)) return 119;
            }
            [owner trace:4 raw:0 present:NO];
            if (owner.diagnosticEvents.count) return 120;
            break;
        case 67:
            owner.diagnosticReporting = NO; owner.diagnosticEvents = nil;
            [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
            livi_bt_stop((__bridge void *)owner);
            if (owner.diagnosticEvents || owner.diagnosticIncoming || owner.diagnosticStopped) return 121;
            break;
        case 68:
            [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
            [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
            if (!TraceMatches(owner, @[@4,@5,@9,@12,@4,@6])) return 122;
            break;
        case 69: {
            __weak LBTAdapter *weakOwner = owner;
            void *retained = (__bridge_retained void *)owner;
            livi_bt_destroy(retained);
            if (!weakOwner.closing || PendingQueryOwner != owner) return 123;
            [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
            if (!TraceMatches(owner, @[@14,@4,@7]) || PendingQueryOwner) return 124;
            break;
        }
        case 70:
        case 71:
            owner.querying = NO; PendingQueryOwner = nil;
            owner.fakeDevice.completeDuringQuery = YES;
            owner.fakeDevice.queryResult = scenario == 70 ? kIOReturnSuccess : kIOReturnError;
            [owner page];
            if (!TraceMatches(owner, scenario == 70 ?
                @[@1,@4,@5,@9,@12,@2] : @[@1,@4,@5,@9,@12,@3]) ||
                owner.querying || PendingQueryOwner || owner.cancelled) return 127;
            break;
        case 72:
            livi_bt_stop((__bridge void *)owner);
            [owner incoming:nil channel:(IOBluetoothRFCOMMChannel *)channel];
            if (!TraceMatches(owner, @[@14,@13,@26]) || channel.closes != 1 ||
                PendingQueryOwner != owner) return 128;
            break;
        default: return 125;
    }
    return 0;
}

static uint32_t CleanupSnapshotTest(LBTFakeAdapter *owner, LBTFakeChannel *channel, unsigned scenario) {
    livi_bt_stop((__bridge void *)owner);
    livi_bt_diagnostic_reporting((__bridge void *)owner);
    unsigned index = scenario - 74;
    uint32_t expected = scenario == 73 ? 0 : (scenario <= 85 ? 1u << index : 0);
    int sockets[2] = {-1, -1};
    LBTEvent *event = nil;
    if (scenario == 74) PendingQueryOwner = owner;
    if (scenario == 75) PendingOpenOwner = owner;
    if (scenario == 76) owner.channel = (IOBluetoothRFCOMMChannel *)channel;
    if (scenario == 77 || scenario == 86) {
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) return 130;
        if (scenario == 77) { owner.bridgeFD = sockets[0]; sockets[0] = -1; }
        else {
            event = [LBTEvent new]; event->wire.fd = sockets[0]; sockets[0] = -1;
            [owner.events addObject:event]; expected = (1u << 9) | (1u << 12);
        }
    }
    if (scenario == 78) owner.record = (IOBluetoothSDPServiceRecord *)owner.fakeRecord;
    if (scenario == 79) owner.notification = (IOBluetoothUserNotification *)owner.fakeToken;
    if (scenario == 80) owner.privacy = (CBCentralManager *)[LBTFakePrivacy new];
    if (scenario == 81) owner.inquiry = (IOBluetoothDeviceInquiry *)[NSObject new];
    if (scenario == 82) owner.pair = (IOBluetoothDevicePair *)[LBTFakePair new];
    if (scenario == 83) [owner.events addObject:[LBTEvent new]];
    if (scenario == 84) owner.enabled = YES;
    if (scenario == 85) owner.available = YES;
    if (scenario == 87) owner.diagnosticReporting = NO;
    if (scenario == 88) {
        PendingQueryOwner = owner; PendingOpenOwner = owner;
        owner.channel = (IOBluetoothRFCOMMChannel *)channel;
        [owner.events addObject:[LBTEvent new]];
        expected = 7u | (1u << 9);
    }
    uint32_t first = 0xFFFFFFFF, second = 0xFFFFFFFF;
    BOOL ok = livi_bt_diagnostic_cleanup_snapshot((__bridge void *)owner, &first);
    BOOL again = livi_bt_diagnostic_cleanup_snapshot((__bridge void *)owner, &second);
    BOOL idle = livi_bt_diagnostic_idle((__bridge void *)owner);
    // Clear the inert inquiry placeholder before ordinary teardown messages.
    owner.inquiry = nil;
    if (sockets[1] >= 0) close(sockets[1]);
    if (scenario == 87)
        return !ok && !again && first == 0xFFFFFFFF && second == first ? 0 : 131;
    if (!ok || !again || first != expected || second != expected ||
        idle != ((expected & 0xFFF) == 0)) return 132;
    if ((scenario == 74 || scenario == 88) && PendingQueryOwner != owner) return 133;
    if ((scenario == 75 || scenario == 88) && PendingOpenOwner != owner) return 134;
    if ((scenario == 76 || scenario == 88) && owner.channel != (IOBluetoothRFCOMMChannel *)channel) return 135;
    if (scenario == 86 && (!owner.events.count || event->wire.fd < 0)) return 136;
    return 0;
}

// Independent owner: the outer test harness must not keep this adapter alive.
// Timers exercise local default-mode dispatch, not IOBluetooth's OS delivery.
static uint32_t SDPCallbackLifetimeTest(unsigned scenario) {
    LBTFakeAdapter *owner = [LBTFakeAdapter new];
    owner.enabled = YES; owner.available = YES; owner.generation = 7;
    owner.fakeRecord = [LBTFakeRecord new]; owner.fakeToken = [LBTFakeToken new];
    owner.record = (IOBluetoothSDPServiceRecord *)owner.fakeRecord;
    LBTFakeDevice *device = [LBTFakeDevice new];
    device.paired = YES;
    owner.fakeDevice = device; owner.target = (IOBluetoothDevice *)device;
    owner.pairedOnly = YES; owner.useRealPage = YES;
    livi_bt_diagnostic_reporting((__bridge void *)owner);
    __weak LBTAdapter *weakOwner = owner;
    uint32_t result = 0;
    @autoreleasepool {
        device.queryResult = scenario == 89 ? kIOReturnError : kIOReturnSuccess;
        device.deferCompletion = scenario >= 91;
        [owner page];
        if (scenario == 89) {
            if (owner.querying || PendingQueryOwner) result = 140;
        } else if (!owner.querying || PendingQueryOwner != owner ||
            ![owner respondsToSelector:@selector(sdpQueryComplete:status:)]) result = 141;

        if (scenario == 92) {
            [owner tick:15]; // Original query deadline; no callback has been pumped.
            if (!owner.cancelled || !owner.querying || PendingQueryOwner != owner ||
                !DiagnosticMatches(owner, 7, 5, 0, NO)) result = 142;
        }
        if (scenario == 93) {
            [owner cancel:7]; livi_bt_stop((__bridge void *)owner);
            if (livi_bt_diagnostic_idle((__bridge void *)owner)) result = 143;
        }
        if (scenario <= 90 || scenario == 92) {
            void *retained = (__bridge_retained void *)owner;
            livi_bt_destroy(retained);
            owner = nil; // PendingQueryOwner is now the only strong adapter reference.
        }
    }
    if (scenario == 89) {
        if (weakOwner || PendingQueryOwner) result = 144;
    } else if (scenario == 90) {
        if (!weakOwner || !weakOwner.closing || PendingQueryOwner != weakOwner) result = 145;
        // An unrelated callback must neither free nor replace the pending target.
        [PendingQueryOwner sdpQueryComplete:nil status:kIOReturnError];
        if (!weakOwner || PendingQueryOwner != weakOwner) result = 146;
        @autoreleasepool {
            [PendingQueryOwner sdpQueryComplete:(IOBluetoothDevice *)device status:kIOReturnSuccess];
        }
        if (weakOwner || PendingQueryOwner) result = 147;
    } else {
        // No delivery occurs until this test pumps the current thread's default mode.
        if (device.completions || !weakOwner || PendingQueryOwner != weakOwner) result = 148;
        NSDate *limit = [NSDate dateWithTimeIntervalSinceNow:1.0];
        @autoreleasepool {
            while (!device.completions && limit.timeIntervalSinceNow > 0) {
                [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode beforeDate:limit];
            }
        }
        if (device.completions != 1 || PendingQueryOwner) result = 149;
        if (scenario == 92) {
            if (weakOwner) result = 150;
        } else {
            if (owner.querying || owner.opening || owner.channel) result = 151;
            if (!TraceMatches(owner, scenario == 91 ?
                @[@1,@2,@4,@5,@9,@12] : @[@1,@2,@14,@4,@7])) result = 152;
            livi_bt_stop((__bridge void *)owner);
            if (!livi_bt_diagnostic_idle((__bridge void *)owner)) result = 153;
        }
    }
    // Test-only cleanup also on failed assertions; never leave a global fake owner.
    LBTAdapter *remaining = PendingQueryOwner;
    if (remaining) { livi_bt_stop((__bridge void *)remaining); PendingQueryOwner = nil; }
    if (owner) livi_bt_stop((__bridge void *)owner);
    return result;
}

// Exercise real adoption, fd transfer and pumping with local NSObject channels.
// No controller/privacy manager/SDP service is constructed; not radio evidence.
static uint32_t IncomingTransportTest(LBTFakeAdapter *owner, LBTFakeChannel *channel,
    unsigned scenario) {
    owner.fakeDevice = [LBTFakeDevice new]; owner.fakeDevice.paired = YES;
    owner.target = (IOBluetoothDevice *)owner.fakeDevice;
    owner.targetAddress = @"02:01:02:03:04:05"; owner.pairedOnly = YES;
    owner.record = (IOBluetoothSDPServiceRecord *)owner.fakeRecord;
    owner.notification = (IOBluetoothUserNotification *)owner.fakeToken;
    owner.querying = YES; owner.queryDeadline = 15; PendingQueryOwner = owner;
    channel.device = owner.fakeDevice;
    channel.reportsClosed = scenario == 97;
    channel.zeroMTU = scenario == 98;
    livi_bt_diagnostic_reporting((__bridge void *)owner);
    [owner incoming:owner.notification channel:(IOBluetoothRFCOMMChannel *)channel];
    if (scenario == 97 || scenario == 98) {
        if (owner.bridgeFD >= 0 || owner.channel || !owner.cancelled ||
            channel.closes != 1 || !TraceMatches(owner,
                scenario == 97 ? @[@13,@23] : @[@13,@22,@24]) ||
            owner.events.count != 1 || owner.events.lastObject->wire.kind != 6) return 160;
        return 0;
    }
    LBTWire event = {0}; event.fd = -1;
    if (!livi_bt_event((__bridge void *)owner, &event) || event.kind != 5 ||
        event.generation != 7 || event.fd < 0 || owner.bridgeFD < 0 ||
        owner.channel != (IOBluetoothRFCOMMChannel *)channel) {
        if (event.fd >= 0) close(event.fd);
        return 161;
    }
    uint32_t result = 0;
    int flags = fcntl(event.fd, F_GETFL);
    if (!(flags & O_NONBLOCK) || !(fcntl(event.fd, F_GETFD) & FD_CLOEXEC)) result = 162;
    if (scenario == 94 || scenario == 95) {
        [owner tick:16];
        if (owner.cancelled || owner.bridgeFD < 0 || !owner.querying ||
            PendingQueryOwner != owner || channel.closes || owner.events.count ||
            !TraceMatches(owner, @[@13,@22,@24,@19])) result = 163;
        if (scenario == 95) {
            [owner sdpQueryComplete:owner.target status:kIOReturnError];
            if (PendingQueryOwner || owner.querying || owner.cancelled ||
                owner.bridgeFD < 0 || channel.closes ||
                !TraceMatches(owner, @[@4,@6])) result = 164;
        }
    } else if (scenario == 96) {
        [owner cancel:7];
        char byte;
        if (recv(event.fd, &byte, 1, MSG_DONTWAIT) != 0 ||
            PendingQueryOwner != owner || channel.closes != 1) result = 165;
        [owner sdpQueryComplete:owner.target status:kIOReturnSuccess];
        if (PendingQueryOwner || owner.channel || owner.bridgeFD >= 0 ||
            !owner.cancelled || owner.events.count) result = 166;
    } else if (scenario == 99) {
        char bytes[4] = {0};
        [owner rfcommChannelData:(IOBluetoothRFCOMMChannel *)channel data:"abc" length:3];
        if (recv(event.fd, bytes, sizeof(bytes), MSG_DONTWAIT) != 3 ||
            memcmp(bytes, "abc", 3)) result = 167;
        if (send(event.fd, "xyz", 3, 0) != 3) result = 168;
        [owner pump];
        if (![channel.written isEqualToData:[NSData dataWithBytes:"xyz" length:3]] ||
            !owner.inFlight) result = 169;
        [owner rfcommChannelWriteComplete:(IOBluetoothRFCOMMChannel *)channel
            refcon:NULL status:kIOReturnSuccess];
        if (owner.inFlight || owner.cancelled) result = 170;
    } else if (scenario == 100) {
        [owner rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)channel];
        char byte;
        if (recv(event.fd, &byte, 1, MSG_DONTWAIT) != 0 ||
            owner.channel || owner.bridgeFD >= 0 || !owner.cancelled ||
            owner.events.lastObject->wire.kind != 7) result = 171;
        [owner tick:16];
        if (owner.events.count != 1 || channel.closes != 1) result = 172;
    } else if (scenario == 101) {
        // An allocated/opening channel alone is not an adopted transport.
        close(owner.bridgeFD); owner.bridgeFD = -1;
        [owner tick:16];
        if (!owner.cancelled || owner.channel ||
            owner.events.lastObject->wire.value != 8) result = 173;
    }
    close(event.fd);
    return result;
}

static uint32_t PassiveListenerTest(LBTFakeAdapter *owner, LBTFakeChannel *channel,
    unsigned scenario) {
    owner.available = NO;
    owner.fakeDevice = [LBTFakeDevice new];
    owner.fakeDevice.paired = scenario != 103;
    owner.fakeDevice.loseBondAfterFirstCheck = scenario == 104;
    owner.useRealPage = YES;
    if (scenario == 111) owner.fakeRecord.assigned = 12;
    livi_bt_diagnostic_reporting((__bridge void *)owner);
    [owner publish];
    LBTWire event = {0}; event.fd = -1;
    if (!owner.available || !owner.notification ||
        owner.listenerChannel != (scenario == 111 ? 12 : 8) ||
        !livi_bt_event((__bridge void *)owner, &event) || event.kind != 1) return 180;
    const uint8_t peer[6] = {2,1,2,3,4,5};
    uint32_t selected = livi_bt_listen_paired((__bridge void *)owner, 7, peer);
    if (scenario == 103 || scenario == 104) {
        if (selected != 6 || owner.pair || owner.querying || owner.opening ||
            owner.fakeDevice.queries || owner.fakeDevice.opens ||
            PendingQueryOwner || PendingOpenOwner || owner.bridgeFD >= 0) return 181;
        livi_bt_stop((__bridge void *)owner);
        return livi_bt_diagnostic_idle((__bridge void *)owner) ? 0 : 182;
    }
    if (selected || owner.cancelled || !owner.pairedOnly || owner.pair ||
        owner.querying || owner.opening || PendingQueryOwner || PendingOpenOwner) return 183;
    for (NSNumber *time in @[@1,@16,@45,@59]) {
        [owner tick:time.doubleValue];
        if (owner.cancelled || !owner.available || owner.channel ||
            owner.bridgeFD >= 0 || owner.events.count ||
            owner.fakeDevice.queries || owner.fakeDevice.opens) return 184;
    }
    if (scenario == 102) {
        if (!TraceMatches(owner, @[@20,@21])) return 185;
        livi_bt_stop((__bridge void *)owner);
        if (!TraceMatches(owner, @[@14]) ||
            !livi_bt_diagnostic_idle((__bridge void *)owner)) return 186;
        return 0;
    }
    channel.device = owner.fakeDevice;
    if (scenario == 106) channel.delegateResult = kIOReturnNotPermitted;
    if (scenario == 107) {
        LBTFakeDevice *other = [LBTFakeDevice new];
        other.testAddress = @"02:09:08:07:06:05"; channel.device = other;
    }
    channel.reportsOutgoing = scenario == 108;
    if (scenario == 109) channel.testChannelID = 9;
    if (scenario == 111) channel.testChannelID = 12;
    channel.reportsClosed = scenario == 110;
    if (scenario == 112) [owner cancel:7];
    if (scenario == 113) { owner.fakeDevice.paired = NO; [owner tick:59.5]; }
    if (scenario == 115) { owner.notReady = YES; [owner tick:59.5]; }
    [owner incoming:scenario == 114 ? nil : owner.notification
        channel:(IOBluetoothRFCOMMChannel *)channel];
    if (scenario == 105 || scenario == 111 || scenario == 116 || scenario == 117) {
        if (!livi_bt_event((__bridge void *)owner, &event) || event.kind != 5 ||
            event.generation != 7 || event.fd < 0 || owner.bridgeFD < 0 ||
            owner.channel != (IOBluetoothRFCOMMChannel *)channel) {
            if (event.fd >= 0) close(event.fd);
            return 187;
        }
        // Native metadata/bridge checks are real code, but the channel is a fake.
        BOOL valid = memcmp(event.peer, peer, sizeof(peer)) == 0;
        if (!TraceMatches(owner, @[@20,@21,@13,@22,@24,@19])) valid = NO;
        if (scenario == 116 || scenario == 117) {
            [owner rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel
                status:scenario == 116 ? kIOReturnSuccess : kIOReturnError];
            if (scenario == 116) {
                char byte;
                if (channel.closes || owner.bridgeFD < 0 || owner.events.count ||
                    recv(event.fd, &byte, 1, MSG_DONTWAIT) != -1 ||
                    errno != EAGAIN || !TraceMatches(owner, @[@27])) valid = NO;
            } else {
                char byte;
                if (channel.closes != 1 || owner.channel || owner.bridgeFD >= 0 ||
                    !owner.cancelled || recv(event.fd, &byte, 1, MSG_DONTWAIT) != 0 ||
                    !DiagnosticMatches(owner, 6, 10, kIOReturnError, YES)) valid = NO;
                livi_bt_stop((__bridge void *)owner);
                close(event.fd);
                return valid && livi_bt_diagnostic_idle((__bridge void *)owner) ? 0 : 194;
            }
        }
        LBTFakeChannel *duplicate = [LBTFakeChannel new];
        duplicate.device = owner.fakeDevice;
        duplicate.testChannelID = channel.testChannelID;
        [owner incoming:owner.notification channel:(IOBluetoothRFCOMMChannel *)duplicate];
        if (duplicate.closes != 1 || channel.closes || owner.events.count) valid = NO;
        livi_bt_stop((__bridge void *)owner);
        char byte;
        if (recv(event.fd, &byte, 1, MSG_DONTWAIT) != 0 || channel.closes != 1 ||
            !livi_bt_diagnostic_idle((__bridge void *)owner)) valid = NO;
        close(event.fd);
        return valid ? 0 : 188;
    }
    if (owner.channel || owner.bridgeFD >= 0 || channel.closes != 1) return 189;
    for (LBTEvent *queued in owner.events) if (queued->wire.kind == 5) return 190;
    if (scenario == 106) {
        const uint32_t *wire = owner.diagnosticEvents.lastObject.bytes;
        if (wire[0] != 25 || wire[1] != (uint32_t)kIOReturnNotPermitted ||
            wire[2] != 1 || !DiagnosticMatches(owner, 6, 10, kIOReturnNotPermitted, YES)) return 191;
    }
    if (owner.fakeDevice.queries || owner.fakeDevice.opens ||
        PendingQueryOwner || PendingOpenOwner || owner.pair) return 192;
    livi_bt_stop((__bridge void *)owner);
    return livi_bt_diagnostic_idle((__bridge void *)owner) ? 0 : 193;
}

static uint32_t ServiceDictionaryTest(void) {
    // Public value conversion only: never publish a record or touch the radio.
    NSDictionary *record = AAServiceDictionary();
    NSArray *classes = [[IOBluetoothSDPDataElement
        withElementValue:record[@"0001 - ServiceClassIDList"]] getArrayValue];
    if (classes.count != 1 || [classes[0] getSize] != 16 ||
        ![[classes[0] getUUIDValue] isEqual:AAUUID()]) return 195;
    NSArray *protocols = [[IOBluetoothSDPDataElement
        withElementValue:record[@"0004 - ProtocolDescriptorList"]] getArrayValue];
    if (protocols.count != 2) return 196;
    NSArray *l2cap = [protocols[0] getArrayValue];
    NSArray *rfcomm = [protocols[1] getArrayValue];
    if (l2cap.count != 1 || rfcomm.count != 2 ||
        ![[l2cap[0] getUUIDValue] isEqual:[IOBluetoothSDPUUID uuid16:0x0100]] ||
        ![[rfcomm[0] getUUIDValue] isEqual:[IOBluetoothSDPUUID uuid16:0x0003]] ||
        [rfcomm[1] getTypeDescriptor] != 1 || [rfcomm[1] getSize] != 1 ||
        [[rfcomm[1] getNumberValue] unsignedIntValue] != 8) return 197;
    NSArray *profiles = [[IOBluetoothSDPDataElement
        withElementValue:record[@"0009 - BluetoothProfileDescriptorList"]] getArrayValue];
    if (profiles.count != 1) return 198;
    NSArray *spp = [profiles[0] getArrayValue];
    if (spp.count != 2 ||
        ![[spp[0] getUUIDValue] isEqual:[IOBluetoothSDPUUID uuid16:0x1101]] ||
        [spp[1] getTypeDescriptor] != 1 || [spp[1] getSize] != 2 ||
        [[spp[1] getNumberValue] unsignedIntValue] != 0x0102) return 199;
    NSArray *browse = [[IOBluetoothSDPDataElement
        withElementValue:record[@"0005 - BrowseGroupList"]] getArrayValue];
    return browse.count == 1 &&
        [[browse[0] getUUIDValue] isEqual:[IOBluetoothSDPUUID uuid16:0x1002]] &&
        [record[@"LocalAttributes"][@"Persistent"] isEqual:@NO] ? 0 : 200;
}

#if defined(LIVI_OFFLINE_INQUIRY_TESTS)
static uint32_t DiscoveryIsolationTest(LBTFakeAdapter *owner, unsigned scenario) {
    // Retain old senders explicitly: no real inquiry, controller or manager.
    // Direct callback-handler isolation only: not OS delivery/lifetime evidence.
    LBTFakeInquiry *old = [LBTFakeInquiry new];
    LBTFakeInquiry *current = [LBTFakeInquiry new];
    LBTFakeDevice *device = [LBTFakeDevice new];
    old.delegate = owner;
    owner.inquiry = (IOBluetoothDeviceInquiry *)old;
    [owner deviceInquiryComplete:(IOBluetoothDeviceInquiry *)old
        error:kIOReturnSuccess aborted:NO];
    if (owner.inquiry || old.delegate || owner.events.count) return 209;
    current.delegate = owner;
    owner.inquiry = (IOBluetoothDeviceInquiry *)current;
    owner.cancelled = YES;
    owner.record = (IOBluetoothSDPServiceRecord *)owner.fakeRecord;
    if (scenario == 121) {
        owner.inquiryDeadline = 10;
        [owner tick:10];
        if (owner.inquiry || current.delegate || current.stops != 1) return 201;
        [owner deviceInquiryDeviceFound:(IOBluetoothDeviceInquiry *)current
            device:(IOBluetoothDevice *)device];
        [owner deviceInquiryComplete:(IOBluetoothDeviceInquiry *)current
            error:kIOReturnError aborted:YES];
        if (owner.events.count) return 202;
        // A fresh fake inquiry can still deliver before withdrawal.
        owner.inquiry = (IOBluetoothDeviceInquiry *)old; old.delegate = owner;
        [owner withdraw];
        [owner deviceInquiryDeviceFound:(IOBluetoothDeviceInquiry *)old
            device:(IOBluetoothDevice *)device];
        [owner deviceInquiryComplete:(IOBluetoothDeviceInquiry *)old
            error:kIOReturnError aborted:YES];
        return owner.inquiry || old.delegate || old.stops != 1 ||
            owner.events.count || owner.available ? 203 : 0;
    }
    [owner deviceInquiryDeviceFound:(IOBluetoothDeviceInquiry *)old
        device:(IOBluetoothDevice *)device];
    [owner deviceInquiryComplete:(IOBluetoothDeviceInquiry *)old
        error:scenario == 120 ? kIOReturnError : kIOReturnSuccess aborted:YES];
    if (owner.events.count || owner.inquiry != (IOBluetoothDeviceInquiry *)current ||
        current.delegate != owner || current.stops) return 204;
    [owner deviceInquiryDeviceFound:(IOBluetoothDeviceInquiry *)current
        device:(IOBluetoothDevice *)device];
    if (owner.events.count != 1) return 205;
    LBTEvent *event = owner.events.firstObject;
    const uint8_t expected[6] = {2, 1, 2, 3, 4, 5};
    if (event->wire.kind != 2 || event->wire.generation != 0 ||
        event->wire.fd != -1 || memcmp(event->wire.peer, expected, 6)) return 206;
    // Late errors after valid delivery cannot clear the new inquiry or add failure.
    [owner deviceInquiryComplete:(IOBluetoothDeviceInquiry *)old
        error:kIOReturnError aborted:NO];
    if (owner.inquiry != (IOBluetoothDeviceInquiry *)current || owner.events.count != 1)
        return 207;
    [owner deviceInquiryComplete:(IOBluetoothDeviceInquiry *)current
        error:kIOReturnSuccess aborted:NO];
    return owner.inquiry || current.delegate || owner.events.count != 1 ? 208 : 0;
}
#endif

uint32_t livi_bt_offline_test(uint32_t scenario) {
    // Native fake tests may run on Rust test threads; no real backend is created.
    static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
    pthread_mutex_lock(&lock);
    uint32_t result = 0;
    @autoreleasepool {
        LBTFakeAdapter *owner = [LBTFakeAdapter new];
        owner.enabled = YES; owner.available = YES; owner.generation = 7;
        owner.listenerChannel = 8;
        owner.fakeRecord = [LBTFakeRecord new]; owner.fakeToken = [LBTFakeToken new];
        LBTFakeChannel *channel = [LBTFakeChannel new]; channel.owner = owner;
        LBTFakePair *pair = [LBTFakePair new];
        int sockets[2] = {-1, -1};
#if defined(LIVI_OFFLINE_INQUIRY_TESTS)
        if (scenario >= 119 && scenario <= 121) result = DiscoveryIsolationTest(owner, scenario);
        else
#endif
        if (scenario == 118) result = ServiceDictionaryTest();
        else if (scenario >= 102 && scenario <= 117) result = PassiveListenerTest(owner, channel, scenario);
        else if (scenario >= 94 && scenario <= 101) result = IncomingTransportTest(owner, channel, scenario);
        else if (scenario >= 89 && scenario <= 93) result = SDPCallbackLifetimeTest(scenario);
        else if (scenario >= 73 && scenario <= 88) result = CleanupSnapshotTest(owner, channel, scenario);
        else if (scenario >= 41 && scenario <= 72) result = LifecycleTest(owner, channel, scenario);
        else switch (scenario) {
            case 0:
                owner.available = NO; owner.failNotification = YES;
                [owner publish];
                if (owner.record || owner.notification || owner.available ||
                    owner.fakeRecord.removes != 1 || owner.enabled) result = 1;
                break;
            case 1:
                owner.available = NO; owner.fakeRecord.conflict = YES;
                [owner publish];
                if (owner.available || owner.notification || owner.record ||
                    owner.fakeRecord.removes != 1 ||
                    owner.events.lastObject->wire.value != 3) result = 2;
                break;
            case 2:
                owner.pair = (IOBluetoothDevicePair *)pair;
                [owner devicePairingUserConfirmationRequest:pair numericValue:123456];
                if (pair.replies || !owner.awaitingConsent || owner.events.lastObject->wire.kind != 3)
                    result = 3;
                if ([owner confirm:8 accept:YES] != 6 || pair.replies) result = 4;
                if ([owner confirm:7 accept:YES] != 0 || !pair.accepted || pair.replies != 1)
                    result = 5;
                [owner cancel:7];
                [owner devicePairingUserConfirmationRequest:pair numericValue:123456];
                if (pair.accepted || pair.replies != 2) result = 6;
                break;
            case 3: {
                owner.cancelled = YES;
                [owner rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:kIOReturnSuccess];
                if (channel.closes != 1 || owner.bridgeFD != -1 || owner.events.count) result = 7;
                if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) { result = 8; break; }
                LBTEvent *event = [LBTEvent new];
                event->wire.kind = 5; event->wire.generation = 7; event->wire.fd = sockets[0];
                sockets[0] = -1; [owner.events addObject:event]; event = nil;
                [owner cancel:7];
                char byte;
                if (recv(sockets[1], &byte, 1, MSG_DONTWAIT) != 0) result = 9;
                break;
            }
            case 4: {
                if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) { result = 10; break; }
                owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                owner.bridgeFD = sockets[0]; sockets[0] = -1;
                [owner withdraw]; [owner withdraw];
                [owner rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)channel];
                char byte;
                if (recv(sockets[1], &byte, 1, MSG_DONTWAIT) != 0 || channel.closes != 1 ||
                    channel.closedWhileAvailable || owner.available || owner.channel) result = 11;
                break;
            }
            case 5:
                owner.record = (IOBluetoothSDPServiceRecord *)owner.fakeRecord;
                owner.pair = (IOBluetoothDevicePair *)pair; owner.pairDeadline = 60;
                [owner tick:61];
                if (!owner.cancelled || owner.events.lastObject->wire.value != 8) result = 12;
                [owner devicePairingFinished:pair error:kIOReturnSuccess];
                if (owner.pair || owner.querying || owner.channel) result = 13;
                break;
            case 6:
                if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) { result = 14; break; }
                owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                owner.bridgeFD = sockets[0]; sockets[0] = -1;
                [owner rfcommChannelData:(IOBluetoothRFCOMMChannel *)channel data:NULL length:262145];
                if (owner.bridgeFD != -1 || owner.channel || !owner.cancelled ||
                    owner.events.lastObject->wire.value != 10) result = 15;
                break;
            case 7:
                owner.querying = YES; PendingQueryOwner = owner;
                [owner cancel:7];
                if ([owner connectAddress:@"02:01:02:03:04:05" generation:8] != 4) result = 16;
                [owner sdpQueryComplete:nil status:kIOReturnSuccess];
                if (owner.querying || PendingQueryOwner || owner.channel) result = 17;
                break;
            case 8:
                owner.opening = YES; PendingOpenOwner = owner;
                [owner rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:kIOReturnError];
                if (PendingOpenOwner || owner.channel || owner.opening ||
                    !owner.cancelled || channel.closes != 1 ||
                    owner.events.lastObject->wire.value != 9) result = 18;
                break;
            case 9:
                owner.opening = YES; PendingOpenOwner = owner;
                owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                [owner cancel:7];
                if ([owner connectAddress:@"02:01:02:03:04:05" generation:8] != 4) result = 19;
                [owner rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:kIOReturnSuccess];
                if (PendingOpenOwner || owner.channel || owner.events.count ||
                    owner.bridgeFD != -1) result = 20;
                break;
            case 10: {
                owner.opening = YES; PendingOpenOwner = owner;
                owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                __weak LBTAdapter *weakOwner = owner;
                void *retained = (__bridge_retained void *)owner;
                livi_bt_destroy(retained);
                owner = nil;
                if (!weakOwner || !weakOwner.closing || weakOwner.bridgeFD != -1) result = 21;
                [PendingOpenOwner rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel
                    status:kIOReturnSuccess];
                if (PendingOpenOwner || weakOwner) result = 22;
                break;
            }
            case 11:
            case 12: {
                owner.fakeDevice = [LBTFakeDevice new];
                owner.fakeDevice.paired = scenario == 12;
                uint32_t code = [owner connectAddress:@"02:01:02:03:04:05"
                    generation:7 pairedOnly:YES];
                if (owner.pair || owner.awaitingConsent ||
                    (scenario == 11 && (code != 6 || owner.pages)) ||
                    (scenario == 12 && (code || owner.pages != 1 || !owner.pairedOnly))) result = 23;
                break;
            }
            case 13:
                owner.fakeDevice = [LBTFakeDevice new];
                owner.target = (IOBluetoothDevice *)owner.fakeDevice;
                owner.pairedOnly = YES; owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                [owner adopt:(IOBluetoothRFCOMMChannel *)channel];
                if (owner.bridgeFD != -1 || owner.channel || channel.closes != 1 ||
                    !owner.cancelled || owner.events.lastObject->wire.value != 6) result = 24;
                break;
            case 14:
                owner.fakeDevice = [LBTFakeDevice new];
                owner.target = (IOBluetoothDevice *)owner.fakeDevice;
                owner.pairedOnly = YES; owner.querying = YES; PendingQueryOwner = owner;
                [owner sdpQueryComplete:(IOBluetoothDevice *)owner.fakeDevice status:kIOReturnSuccess];
                if (PendingQueryOwner || owner.querying || owner.opening ||
                    !owner.cancelled || owner.events.lastObject->wire.value != 6) result = 25;
                break;
            case 15:
                owner.querying = YES; PendingQueryOwner = owner;
                livi_bt_stop((__bridge void *)owner);
                if (livi_bt_diagnostic_idle((__bridge void *)owner)) result = 26;
                [owner sdpQueryComplete:nil status:kIOReturnSuccess];
                if (!livi_bt_diagnostic_idle((__bridge void *)owner)) result = 27;
                break;
            case 16:
                owner.pairedOnly = YES; owner.pair = (IOBluetoothDevicePair *)pair;
                [owner devicePairingUserConfirmationRequest:pair numericValue:123456];
                if (pair.accepted || pair.replies != 1 || !owner.cancelled ||
                    [owner confirm:7 accept:YES] != 6) result = 28;
                break;
            case 17:
            case 18:
            case 19:
            case 20: {
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                if (scenario == 17) owner.enabled = NO;
                if (scenario == 18) owner.pair = (IOBluetoothDevicePair *)pair;
                if (scenario == 20) owner.fakeDevice = [LBTFakeDevice new];
                uint32_t code = [owner connectAddress:@"02:01:02:03:04:05" generation:7 pairedOnly:YES];
                uint32_t codes[] = {1,4,5,6}, categories[] = {1,2,3,4}, operations[] = {2,2,3,4};
                unsigned i = scenario - 17;
                if (code != codes[i] || !DiagnosticMatches(owner, categories[i], operations[i], 0, NO) ||
                    owner.pages || (scenario != 18 && owner.pair)) result = 30 + scenario;
                break;
            }
            case 21:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.fakeDevice = [LBTFakeDevice new]; owner.fakeDevice.paired = YES;
                owner.fakeDevice.queryResult = kIOReturnNotPermitted;
                owner.target = (IOBluetoothDevice *)owner.fakeDevice; owner.useRealPage = YES;
                [owner page];
                if (!DiagnosticMatches(owner, 5, 5, kIOReturnNotPermitted, YES) ||
                    owner.querying || PendingQueryOwner || !owner.cancelled) result = 51;
                break;
            case 22:
            case 23:
            case 24:
            case 25:
            case 26:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.fakeDevice = [LBTFakeDevice new]; owner.fakeDevice.paired = YES;
                owner.target = (IOBluetoothDevice *)owner.fakeDevice;
                owner.querying = YES; PendingQueryOwner = owner;
                if (scenario >= 24) owner.fakeDevice.remoteRecord = owner.fakeRecord;
                if (scenario == 24) owner.fakeRecord.conflict = YES;
                if (scenario == 25) owner.fakeRecord.channelResult = kIOReturnBadArgument;
                if (scenario == 26) owner.fakeDevice.openResult = kIOReturnNotOpen;
                [owner sdpQueryComplete:owner.target status:scenario == 22 ? kIOReturnError : kIOReturnSuccess];
                if (scenario == 22 && !DiagnosticMatches(owner, 5, 6, kIOReturnError, YES)) result = 52;
                if (scenario == 23 && !DiagnosticMatches(owner, 5, 7, 0, NO)) result = 53;
                if (scenario == 24 && !DiagnosticMatches(owner, 5, 8, 0, NO)) result = 54;
                if (scenario == 25 && !DiagnosticMatches(owner, 5, 8, kIOReturnBadArgument, YES)) result = 55;
                if (scenario == 26 && !DiagnosticMatches(owner, 6, 9, kIOReturnNotOpen, YES)) result = 56;
                // SDP failures still wait; immediate open failure still cancels.
                if (owner.cancelled != (scenario == 26) || owner.querying || PendingQueryOwner ||
                    PendingOpenOwner || owner.opening) result = 57;
                break;
            case 27:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.opening = YES; PendingOpenOwner = owner;
                [owner rfcommChannelOpenComplete:(IOBluetoothRFCOMMChannel *)channel status:kIOReturnNotOpen];
                if (!DiagnosticMatches(owner, 6, 10, kIOReturnNotOpen, YES) ||
                    !owner.cancelled || channel.closes != 1) result = 58;
                break;
            case 28:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.record = (IOBluetoothSDPServiceRecord *)owner.fakeRecord;
                owner.querying = YES; PendingQueryOwner = owner; owner.queryDeadline = 1;
                [owner tick:2];
                if (!DiagnosticMatches(owner, 7, 5, 0, NO) || !owner.cancelled) result = 59;
                break;
            case 29:
            case 30: {
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                LBTFakePrivacy *privacy = [LBTFakePrivacy new];
                privacy.state = scenario == 29 ? CBManagerStateUnauthorized : CBManagerStateUnsupported;
                owner.privacy = (CBCentralManager *)privacy;
                [owner centralManagerDidUpdateState:(CBCentralManager *)privacy];
                if (!DiagnosticMatches(owner, scenario == 29 ? 8 : 9, 13, 0, NO) ||
                    owner.available || owner.events.count != 2) result = 60;
                break;
            }
            case 31:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.notReady = YES; [owner tick:1];
                if (!DiagnosticMatches(owner, 9, 14, 0, NO) || owner.available) result = 61;
                break;
            case 32: {
                uint32_t out[4] = {0};
                owner.enabled = NO;
                if ([owner connectAddress:@"02:01:02:03:04:05" generation:7 pairedOnly:YES] != 1 ||
                    livi_bt_diagnostic_failure((__bridge void *)owner, out) || owner.diagnosticCategory) result = 62;
                break;
            }
            case 33:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                [owner diagnose:5 operation:5 raw:kIOReturnError present:YES];
                [owner diagnose:7 operation:15 raw:0 present:NO];
                livi_bt_stop((__bridge void *)owner);
                if (!DiagnosticMatches(owner, 5, 5, kIOReturnError, YES) ||
                    !livi_bt_diagnostic_idle((__bridge void *)owner)) result = 63;
                break;
            case 34:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                [owner rfcommChannelWriteComplete:(IOBluetoothRFCOMMChannel *)channel
                    refcon:NULL status:kIOReturnNotOpen];
                if (!DiagnosticMatches(owner, 6, 12, kIOReturnNotOpen, YES) || !owner.cancelled) result = 64;
                break;
            case 35:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets)) { result = 65; break; }
                owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                owner.bridgeFD = sockets[0]; sockets[0] = -1; channel.writeResult = kIOReturnNotOpen;
                if (send(sockets[1], "x", 1, 0) != 1) { result = 66; break; }
                [owner pump];
                if (!DiagnosticMatches(owner, 6, 11, kIOReturnNotOpen, YES) || !owner.cancelled) result = 67;
                break;
            case 36:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.channel = (IOBluetoothRFCOMMChannel *)channel;
                [owner rfcommChannelClosed:(IOBluetoothRFCOMMChannel *)channel];
                if (!DiagnosticMatches(owner, 6, 20, 0, NO) || !owner.cancelled) result = 68;
                break;
            case 37: {
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                LBTFakePrivacy *privacy = [LBTFakePrivacy new];
                privacy.state = CBManagerStatePoweredOff;
                owner.privacy = (CBCentralManager *)privacy; owner.available = NO;
                if (!DiagnosticMatches(owner, 9, 14, 0, NO)) result = 69;
                break;
            }
            case 38:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                [owner diagnose:5 operation:7 raw:0 present:NO];
                [owner diagnose:6 operation:10 raw:kIOReturnNotOpen present:YES];
                if (!DiagnosticMatches(owner, 6, 10, kIOReturnNotOpen, YES)) result = 70;
                break;
            case 39:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                [owner fail:9];
                if (!DiagnosticMatches(owner, 10, 21, 0, NO) || !owner.cancelled) result = 71;
                break;
            case 40:
                livi_bt_diagnostic_reporting((__bridge void *)owner);
                owner.fakeDevice = [LBTFakeDevice new]; owner.fakeDevice.paired = YES;
                owner.fakeDevice.loseBondAfterFirstCheck = YES;
                if ([owner connectAddress:@"02:01:02:03:04:05" generation:7 pairedOnly:YES] != 6 ||
                    owner.pair || owner.pages || !owner.cancelled ||
                    !DiagnosticMatches(owner, 4, 4, 0, NO)) result = 72;
                break;
            default: result = 99; break;
        }
        owner.enabled = NO; [owner withdraw]; [owner.events removeAllObjects];
        owner.pair = nil;
        if (PendingQueryOwner == owner) PendingQueryOwner = nil;
        if (PendingOpenOwner == owner) PendingOpenOwner = nil;
        if (sockets[0] >= 0) close(sockets[0]);
        if (sockets[1] >= 0) close(sockets[1]);
    }
    pthread_mutex_unlock(&lock);
    return result;
}
