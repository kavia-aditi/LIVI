# Native Bluetooth development receiver

[Demonstrations](README.md)

## Scope and acceptance

This is a separate experimental macOS application for real classic Bluetooth
discovery, explicit pairing, RFCOMM bootstrap and wireless Android Auto reception.
It is **not** the Slint Bluetooth simulation. Build/test success, OS pairing,
RFCOMM connection, WPP completion, Wi-Fi join and TLS establishment are distinct
milestones. Only actual, visible Android Auto projection on the Mac satisfies
end-to-end acceptance. No such hardware success is claimed here.

The installed application and helper must remain untouched and running as-is.
Do not use production helper sockets, reset Bluetooth/network settings, stop
unrelated links, or rerun GStreamer packaging repair. No entitlement, privileged
API, USB/AOA implementation, HFP implementation or existing session transport is
changed by this experiment.

## Architecture and decoupling

1. `livi-native-bt-dev` owns a main-thread AppKit/IOBluetooth adapter. CoreBluetooth
   supplies only the public consent/state gate; it does not carry AA data.
2. Adapter availability is withdrawn before transient SDP registrations and
   owned RFCOMM resources are released. Controller availability is checked at
   one-second intervals; only the owned resources are republished on recovery.
3. Public IOBluetooth inquiry, pairing, SDP and RFCOMM APIs are used. The transient
   AA service UUID is `4de17a00-52cb-11e6-bdf4-0800200c9a66`. The assigned RFCOMM
   channel is read back after publication rather than assumed.
4. An owned RFCOMM stream crosses the existing `Arrival<OwnedFd>` boundary through
   a bounded local socket bridge. Rust reuses the existing WPP handshake. The only
   helper bootstrap source change is crate-level visibility of that function.
   Its unused dongle setup functions are compiled but are not invoked here.
5. The existing pure reconnect policy is reused for disconnected owned-channel
   attempts, not OS ACL state. It cannot issue baseband disconnects. Automatic
   paging is suppressed while unavailable, busy, disabled or projecting.
6. The receiver binds only the configured Mac LAN address/port. Bind failure is
   fatal; it never steals a port. Each accepted Wi-Fi session gets a fresh socket
   under a newly created, mode-0700 `/tmp/livi-bt-dev-<pid>` directory.
   Existing AA TLS/media/session code is reused without modification.
7. The receiver launches the optional Slint projection UI and separately built core.
   Both use private paths. The `livi-core/native-bt-dev` feature plus a validated
   runtime directory are required to activate isolation. Normal builds reject
   the development environment variable.
8. Development core startup omits the production helper, USB/dongle watchers,
   Wi-Fi provisioning, radio worker, host setup and unrelated host workers.
   Production Bluetooth/Wi-Fi controls are rejected. Private control requests
   reject USB/HFP operations and have bounded size/read time.
9. The existing grouped projection composition retains a CP handle on an unused,
   ephemeral **loopback-only** listener. Its helper endpoint is deliberately
   unserved; it does not advertise a CarPlay service on the LAN.
10. Development binaries use the existing development media environment, not the
    installed application's resource layout. GStreamer packaging is not rebuilt
    or repaired.

Pairing consent must be confirmed on the actual Mac and phone. Codes are not
invented or auto-accepted. Outstanding OS pairing/SDP operations remain serialized
through cancellation; no private cancellation or ACL-reset workaround is used.
Some stalled OS operations can therefore require quitting this development app
and retrying explicitly. Closing an owned RFCOMM channel does not tear down a
Wi-Fi session or the system's phone ACL.

## Build only

From the repository root:

```sh
python3 utils/build_native_bluetooth_dev.py
```

This builds debug components, creates and ad-hoc signs
`temp-attachments/LIVI Native Bluetooth Dev.app`, and verifies its signature.
It does not launch it, modify `/Applications`, install a helper, alter
entitlements or change existing packaging. It refuses to overwrite an existing
development app. `--assemble-only` assembles binaries already built with the
required feature.

The separate bundle identifier is
`dev.f-io.livi.native-bluetooth-development`. Its own Info.plist includes Bluetooth
and local-network usage descriptions. Grant consent only to that development
app. Do not reset TCC or change the installed application's permissions.

## Physical prerequisites and configuration

The built-in Mac adapter replaces the absent dongle only for Bluetooth.
The operator must supply an already provisioned 2.4 GHz or supported non-DFS
5 GHz WPA2-Personal AP, with
client isolation disabled and a route from the phone to the Mac. This code does
not create, reconfigure or join an AP, and does not use private hotspot APIs.
The experimental parser accepts channels 1–13 and the existing 5 GHz allowlist
below; this is not a regulatory authorization. Channels 12/13 are not permitted
in every regulatory domain, and both devices and the AP must support the chosen
channel. Channel 14 and DFS channels remain rejected. The Mac path does not
automatically verify regulatory permission or that configuration matches the AP.
Third-party AP and 2.4 GHz support by the phone remain unverified; a phone may
require 5 GHz or additional profile behavior.

Create a private JSON file outside version control with actual values:

```json
{
  "ssid": "<actual AP SSID>",
  "password": "<actual WPA2 password>",
  "security": "wpa2",
  "bssid": "<actual AP BSSID>",
  "channel": 36,
  "mac_ipv4": "<Mac IPv4 on that AP>",
  "port": 5277,
  "phone_address": "<OnePlus classic Bluetooth address>",
  "phone_not_projecting_usb": true,
  "wired_ids": []
}
```

Placeholders are not runnable credentials. Protect this file with owner-only
permissions and never paste the password, phone address or identifiers into
shared evidence. The password must be 8–63 ASCII bytes. Use a permitted channel
from 1–13, or 36, 40, 44, 48, 149, 153, 157, 161 or 165. WPA3-only, enterprise
and open networks are not supported by this experimental parser. Supply the
actual AP BSSID, not the router IP or phone address; parsing checks MAC syntax
and rejects zero/broadcast addresses but does not verify the observed AP.
The listener must bind to the Mac,
not the router/gateway. This is a trusted-LAN experiment, not a hardened
multi-client service: the TCP endpoint accepts the first client to connect.

For channel 1, the unchanged WPP flow advertises 2412 MHz, the configured Mac
IPv4/port, and the supplied SSID/password/BSSID as WPA2-Personal on a static AP.
Channel width is not configured by this receiver; an existing 20 MHz AP needs
no receiver width setting. Socket-only tests verify these fields, not a real
phone's acceptance, AP join, LAN reachability, TLS or projection. Configuration
parsing cannot prove client isolation is disabled or the Mac firewall permits
the connection. No router changes are performed.

The target phone must not currently project over USB. The development receiver
does not inspect or interfere with the installed helper's USB state. Therefore
the explicit operator confirmation is essential; its local wired-ID guard is
not proof of the installed core's state. Never unplug or disrupt another active
session merely to run this experiment.

## Physical procedure

First reconcile any mismatch with the supplied installed-binary baselines.
Record read-only hashes of the installed UI/helper, USB source and entitlements
before and after the test. Never replace a mismatching installed binary as an
automatic repair.

1. Run bounded discovery only:

   ```sh
   open -n "temp-attachments/LIVI Native Bluetooth Dev.app" --args --scan-seconds 12
   ```

   Grant Bluetooth consent yourself if prompted. Keep the OnePlus Bluetooth
   settings open. Discovery ends automatically. OS-paired/ACL-connected status
   is reported separately and is not a LIVI or AA connection.
2. With the actual AP configuration available and the target not projecting
   over USB, launch the receiver and separate projection UI:

   ```sh
   open -n "temp-attachments/LIVI Native Bluetooth Dev.app" --args \
     --config "/absolute/path/to/private-config.json" --projection
   ```

3. Select **Pair / connect target**. Compare the physical phone's code with the
   native confirmation dialog. Reject mismatches. If an unpaired target already
   has a system link, pair through macOS Settings rather than taking over it.
4. Record each actual milestone: OS pairing; selected owned RFCOMM connection;
   WPP version/start/info exchanges; phone AP join status; incoming TCP session;
   TLS/media activity; and finally visible Android Auto frames and interaction.
   WPP failure or absent remote AA service is a compatibility result, not success.
5. Only after the operator explicitly confirms a disruption test, use
   **Disconnect owned** and confirm its dialog. It closes only this app's RFCOMM
   channel and disables automatic reconnect. It intentionally leaves Wi-Fi
   projection and unrelated OS links alone.
6. After operator confirmation, select **Reconnect target**. Record whether the
   selected owned channel reconnects and whether projection remains usable.
   Test actual link loss only by an operator-approved action affecting this
   development connection/phone. Do not toggle the system radio or reset a
   network to simulate adapter loss.
7. Quit this development app and verify its private runtime disappears. It
   shuts down only its own UI/core child handles, with a private core lifeline
   and a bounded shutdown wait. A hung development child is force-stopped only
   through that owned handle, never by a process-name search.
   Confirm the installed application/helper and existing USB functionality were
   not interrupted.

HFP is intentionally not registered or changed. If this phone requires HFP or
additional receiver identity/profile behavior before enabling wireless AA, stop
and report that limitation; changing those boundaries needs new authorization.

## Software validation, separate from hardware

From `native`:

```sh
CI=1 cargo test -p livi-native-bt-dev --no-fail-fast
CI=1 cargo test -p livi-core --features native-bt-dev --no-fail-fast
CI=1 cargo test -p livi-runtime --lib bluetooth -- --test-threads=1
CI=1 cargo check -p livi-core
```

From the repository root:

```sh
python3 utils/verify_bluetooth_boundaries.py
codesign --verify --deep --strict "temp-attachments/LIVI Native Bluetooth Dev.app"
```

Native tests use a software-only channel fixture; WPP tests use local streams.
Neither is a hardware validation. Coverage includes explicit AP configuration,
all 2.4 GHz channels 1–13 and their WPP frequencies, the retained 5 GHz allowlist,
invalid/DFS/channel-14/type/overflow rejection, a channel-1 receiver bootstrap,
owned teardown, late callbacks, paging suppression, bounded/rejected control
requests, WPP info/ping behavior and local wired-phone exclusion. Core tests
exercise the private directory guard; existing runtime tests cover pure
reconnect/lifecycle behavior.

Source changes do not update an already assembled development app. Rebuild into
a new development bundle before any separately authorized hardware test; do not
overwrite or modify the installed production app. The validation record below
describes the original implementation run, not the later channel-support change.

Hardware discovery, pairing, loss/reconnect and visible projection must each be
recorded separately. No simulated screenshot is evidence for these steps.

## Validation record for this implementation

| Check | Observed result |
| --- | --- |
| Native receiver software tests, final rerun | 9 passed |
| Bluetooth runtime unit tests | 30 passed |
| Core tests with `native-bt-dev` | 304 passed |
| Default core `cargo check` | Passed |
| Receiver/core/UI/media-host development build | Passed |
| Separate development app assembly | Completed |
| Ad-hoc bundle signature, deep/strict verification | Passed |
| Separate bundle identifier and usage descriptions | Verified |
| Assembled receiver `--help` | Passed; no hardware initialization |
| Assembled core, invalid development runtime | Rejected before startup |
| Frozen Bluetooth privilege/transport/HFP boundaries | Passed |
| Protected USB source and both entitlement checksums | Match supplied baselines |
| Hardware discovery, pairing, loss/reconnect | Not performed in this implementation run |
| Full development UI/media startup and visible AA projection | Not hardware-validated |

The app artifact is `temp-attachments/LIVI Native Bluetooth Dev.app`. Only its
help command and the core's fail-closed guard were executed; no hardware UI,
discovery, pairing, SDP registration, AP change or phone disruption was started.
Build/test process sessions were cleaned up.

Read-only installed-binary checks repeatedly found:

- Helper: matches the supplied baseline
  `bd9a2048911861a8a7a6a152536ef36d22c5d3d67e70683b60a3a7ad5cffa4c3`.
- UI: observed
  `f67910d24a2f2f11f5cafab5409e4c4cb83397d5275b3d65dd9ead2bdf19c26a`,
  which differs from the supplied
  `95365ae996c743000210ae97e9b33dcbcb25a86331cafb99bc03237b74c20a63`.

The origin of the UI discrepancy is not established. Neither installed binary
was written, replaced, restarted or signalled by this work. No automatic repair
was attempted. Reconcile that baseline before hardware launch, and provide the
actual AP configuration privately. Physical phone consent and separately
confirmed owned-link disruption tests remain necessary. End-to-end completion
must not be reported until visible real Android Auto projection is observed.

## Source map

- [Native adapter](../native/crates/livi-native-bt-dev/src/adapter.m)
- [Receiver and tests](../native/crates/livi-native-bt-dev/src/receiver.rs)
- [Explicit AP configuration](../native/crates/livi-native-bt-dev/src/config.rs)
- [Core isolation guard](../native/crates/livi-core/src/native_bt_dev.rs)
- [Development app builder](../utils/build_native_bluetooth_dev.py)
- [Reused WPP bootstrap](../native/crates/livi-helperd/src/aa.rs)
- [Unchanged session transport](../native/crates/livi-aa/src/session.rs)
