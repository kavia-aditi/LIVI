# Isolated Android phone Bluetooth test

For the separate version-only diagnostic, see
[Bluetooth-only diagnostic](README-bluetooth-only-diagnostic.md). It has no TCP
listener, AP configuration or projection path and requires separate hardware
authorization. Do not use the full harness below for Bluetooth-only testing.

**Do not run a real-phone projection test yet.** The circular pre-required phone
IDs have been removed, but authenticated Bluetooth-to-TCP binding is not available.
Real native listener permits fail closed with `IdentityUnverified`, including
when optional phone pins match. See [identity bootstrap status and remaining
work](README-native-phone-identity.md). The run command below describes the
eventual operator flow; it is not authorization to bypass this blocker.

This is a **source-checkout test harness**, not a production DMG or a
self-contained distributable app. Entry point: [`native_phone_test.py`](native_phone_test.py).

The ordinary application's hardware gates remain false. This harness builds the
existing default-off isolated helper and isolated UI, using the production native
Bluetooth/handoff and projection dependencies. It does not use the old
`development-receiver` app or modify elevated privileged APIs.

## Preparation status

The launcher has hardware-free regression tests. A fresh executable build and
real-phone validation are separate gates: neither a passing Python test nor a
`ready.json` file proves that a phone projects successfully.

The harness requires at least **20 GiB free** before starting its fresh, private
debug build. No existing output is deleted to make room. GStreamer was found
through Homebrew, version 1.28.8. Preparations predating the identity-bootstrap
source changes are stale: preserve them, but do not edit their manifests or use
them to test the new code.

## Prerequisites

- macOS ARM64, a non-root operator, Rust/Cargo and Xcode command-line tools.
- An installed GStreamer development SDK, found either at the standard framework
  location or through `pkg-config`. The test uses this installation; it does not
  bundle or alter it.
- At least 20 GiB free for an exclusive debug target directory; actual build
  requirements can be higher.
- Dependencies already cached for `cargo build --locked --offline`.
- An **Android phone supporting wireless Android Auto**, already paired through
  macOS Bluetooth settings. iPhone/CarPlay is outside this harness.
- A separately provisioned WPA2-Personal access point reachable by the Mac and
  phone. The AP IPv4 address in the configuration must be an address currently
  assigned to the Mac, not merely the router/gateway.
- The phone must not be projecting through USB or another receiver. Disconnect
  its USB cable for this test.

Do not run the legacy packager, the old development app builder, or installed LIVI
as a substitute for these steps.

## 1. Prepare without contacting hardware

From the repository root, choose a **new** direct child of `temp-attachments`:

    python3 -B utils/native_phone_test.py prepare --output temp-attachments/phone-test-001

The command:

1. Checks the immutable Bluetooth verifier and ordinary false hardware gates.
2. Refuses an existing output directory and inadequate free space.
3. Builds only `livi-core`, `livi-ui`, `livi-gst-host`, and `livi-native-bt-dev`,
   with explicit isolated features, using a fresh target directory.
4. Rejects the old receiver, experimental Metal validation and offline admission
   test-support features.
5. Checks ARM64 executables, signatures and static Mach-O dependency resolution.
6. Records source identity, binary/dependency hashes and feature evidence in
   `ready.json`.

It does **not** start LIVI, the helper, a renderer, plugins, Bluetooth discovery,
AP provisioning, installation, signing of existing artifacts, or phone testing.
Partial build outputs and build diagnostics are retained on failure. These are
fresh test inputs, not recovered historical approval.

Sources, binaries or linked dependencies changing after preparation requires a
new output directory and a new preparation. The source digest covers public
Rust/UI build sources; it is not a complete reproducible-build attestation.

## 2. Supply private AP/phone configuration

Create an operator-owned regular JSON file outside shared locations, with exact
permissions **0600**, no symlink, and at most **2048 bytes**. Do not send its
contents to chat or commit it. No credential file is created by this change.

Required fields, enforced by the Rust configuration parser:

| Field | Requirement |
| --- | --- |
| `ssid` | Actual AP SSID, 1–32 bytes, no control characters |
| `passphrase` | Actual WPA2 password, 8–63 printable ASCII bytes |
| `security` | Exactly `wpa2` |
| `channel` | Actual channel: 1–13, 36/40/44/48, or 149/153/157/161/165 |
| `bssid` | Actual AP unicast MAC address in colon-separated hex |
| `ap_ip` | Mac's assigned private IPv4 address; not a loopback or network/broadcast address |
| `port` | Exactly 5277; must not already be in use |
| `phone_address` | Already-paired Android phone's Bluetooth MAC, not a BLE random identifier |

The optional **paired** fields `instance_id` and `serial` are phone-supplied WPP
identity pins, not receiver identifiers. Omit both on first bootstrap. If present,
both must be nonempty, at most 128 ASCII graphic bytes; null and partial pairs
are rejected. Never invent values. A LIVI Link dongle is not a prerequisite.

Use [the local configuration wizard](README-private-phone-config.md) to collect
only the actual AP and paired-phone values. Unknown fields are rejected. The
launcher checks file privacy without printing or copying its contents. The helper
and UI independently parse the configuration. No network settings or OS
permissions are changed automatically. Learning WPP labels does not authenticate
the TCP peer, so admission remains blocked.

## 3. Explicitly authorize one bounded test

Do not execute until the identity-proof blocker above has been resolved and the
hardware-free admission tests verify an authenticated binding. Only the operator
should execute this command after completing those gates and the prerequisites.
Replace the config path locally; never put a password in command-line arguments.

    python3 -B utils/native_phone_test.py run \
      --output temp-attachments/phone-test-001 \
      --ap-config /absolute/path/to/private-phone-test.json \
      --authorize-native-hardware \
      --paired-phone \
      --existing-ap \
      --no-usb-projection \
      --seconds 600

All four confirmations are mandatory; omitting one refuses the run before
checking hardware or spawning children. The duration must be 30–1800 seconds.

This command **does start real Bluetooth and network activity**. It launches only
its isolated helper, then core, then UI after readiness. Its private owned
`/tmp/livi-bt-dev-*` directory holds runtime sockets and core data. It does not
connect to the installed helper or reuse ordinary LIVI user settings.
The UI may produce a normal macOS Bluetooth permission prompt; the harness
cannot grant permission. It never automatically approves a pairing confirmation.
If new pairing is required, stop and pair through macOS settings first.

Child output is suppressed to avoid collecting credentials or private runtime
details into logs. The launcher reports only readiness/failure and the private
runtime location. Do not infer successful projection merely from a running UI.

## 4. Real-phone acceptance checklist

Perform on a stationary test setup, not while driving:

- [ ] The intended already-paired phone connects; unrelated phones are not selected.
- [ ] Android Auto starts over Wi-Fi after Bluetooth bootstrap.
- [ ] A real phone-generated projection is visible and updates continuously.
- [ ] Touch/input and media audio work; no HFP/SCO phone-call support is claimed.
- [ ] Disconnecting the phone ends the attempt without affecting other Bluetooth devices.
- [ ] Closing the UI, Ctrl-C, or the duration limit stops the launcher-owned children.
- [ ] A fresh subsequent run works without changing ordinary hardware gates.

Record pass/fail and sanitized symptoms manually. Do not record AP credentials,
raw configuration, pairing secrets, or claim device validation until these steps
have actually been performed.

## Stop and failure behavior

Ctrl-C stops the test. The launcher also stops if a helper/core exits, UI fails,
readiness times out, or the duration expires. Cleanup signals only child sessions
it created, closes the core lifeline, and waits for termination.

Private runtime and build output are retained; no recursive cleanup of existing
artifacts is performed. Treat retained private runtime data as sensitive.
The production DMG workflow remains separate and unchanged.

Offline launcher tests:

    python3 -B -m unittest discover -s utils -p test_native_phone_test.py -v

Frozen privileged/transport checks:

    python3 -B utils/verify_bluetooth_boundaries.py
