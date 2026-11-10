// SPDX-License-Identifier: GPL-3.0-or-later

#import <Foundation/Foundation.h>
#import <IOBluetooth/IOBluetooth.h>

// No adapter owner, inquiry, remote-name request, pairing, SDP or channel access.
// The callback consumes UTF-8 strings synchronously while the pool is alive.
typedef void (*LBTPairedVisitor)(void *, const char *, const char *);

static void LBTVisitPaired(NSArray *devices, void *context, LBTPairedVisitor visit) {
    for (IOBluetoothDevice *device in devices) {
        if (![device isPaired]) continue;
        NSString *address = device.addressString;
        if (address.length == 0) continue;
        NSString *name = device.name;
        visit(context, name.UTF8String ?: "", address.UTF8String);
    }
}

unsigned int livi_bt_paired_devices(void *context, LBTPairedVisitor visit) {
    if (![NSThread isMainThread]) return 1;
    if (!visit) return 2;
    @autoreleasepool {
        @try {
            LBTVisitPaired([IOBluetoothDevice pairedDevices], context, visit);
            return 0;
        } @catch (NSException *exception) {
            (void)exception;
            return 2;
        }
    }
}

// Offline fixture: deliberately not an IOBluetoothDevice, so no system queries.
@interface LBTPairedFixture : NSObject
@property(nonatomic, copy) NSString *fixtureName;
@property(nonatomic, copy) NSString *fixtureAddress;
@property(nonatomic) BOOL fixturePaired;
- (BOOL)isPaired;
- (NSString *)name;
- (NSString *)addressString;
@end

@implementation LBTPairedFixture
- (BOOL)isPaired { return self.fixturePaired; }
- (NSString *)name { return self.fixtureName; }
- (NSString *)addressString { return self.fixtureAddress; }
@end

void livi_bt_paired_offline(void *context, LBTPairedVisitor visit) {
    @autoreleasepool {
        LBTPairedFixture *phone = [LBTPairedFixture new];
        phone.fixtureName = @"Offline phone";
        phone.fixtureAddress = @"02-00-00-00-00-02";
        phone.fixturePaired = YES;
        LBTPairedFixture *unpaired = [LBTPairedFixture new];
        unpaired.fixtureName = @"Must not appear";
        unpaired.fixtureAddress = @"02-00-00-00-00-03";
        unpaired.fixturePaired = NO;
        LBTVisitPaired(@[phone, phone, unpaired], context, visit);
    }
}
