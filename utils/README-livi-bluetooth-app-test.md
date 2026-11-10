# Real LIVI app Bluetooth connection test

The actual `LIVI` executable now has an opt-in `bluetooth-app-test` feature and
`--bluetooth-only-test` launch mode. It uses the real `AppWindow`, existing Slint
event loop and native Bluetooth backend. This is not the simulated Bluetooth
demo and does not spawn the standalone diagnostic executable.

## What is integrated

The LIVI window shows a real Bluetooth test page with Connect, Cancel/disconnect,
measured protocol stages and a final outcome. In-app address entry, an unchecked
hardware consent checkbox and an explicit Connect click are available through
the `open` action. The existing `run` action supplies consent and selection from
the command line instead. Opening either view does not allocate or start the
Bluetooth backend. Click within 60 seconds; an expired or consumed request cannot
be reused. A fresh process is required for another attempt.

After the explicit click, the app publishes its temporary Android Auto SDP
service and arms an incoming RFCOMM listener for one selected already-paired
phone, using the channel actually assigned by macOS. It does not query remote
SDP, open an outgoing channel, pair, discover or retry. The phone-side activation
trigger remains unverified; an existing bond or ACL link is not an RFCOMM stream.
Only after accepting an owned selected-peer stream does the app reuse the
tested version-only runner: one WPP message 4, one bounded message 5, validation
of version 6.0/status 0 and safe phone identity labels, followed immediately by
owned disconnection and cleanup. No pairing, discovery or automatic retry is
available through this view.

Fixed native lifecycle stages, original IOReturn values when present, historical
selected-stream observation and local cleanup evidence remain visible after the
runner is released. The status area scrolls without covering Connect or Cancel.
A timeout before stream acceptance explicitly says WPP was not reached; stream
observation is historical, not current connectivity or authentication.

Cancel, window close, SIGINT and SIGTERM cancel the current attempt. Window close
keeps the event loop running until cleanup completes or its 2-second grace
expires. Unconfirmed cleanup is FAIL, never PASS. Only a confirmed successful
exchange and cleanup gives process exit status zero; cancellation is not PASS.
An abrupt OS/process failure can only invoke best-effort Drop cleanup, not
establish that cleanup was confirmed.

## Deliberate limit: this is not full wireless projection

This implements real Bluetooth testing **inside the LIVI app**, not an enabled
production wireless Android Auto session. It bypasses ordinary startup before
bundle setup, renderer initialization, runtime directories, core connection or
spawn, settings/config loading and helper IPC.

Ordinary media/settings/navigation are deliberately absent in this test-only
view. Use it to validate the two test controls, not production navigation or
projection. Hardware-free pointer tests exercise the actual Slint test controls
and ordinary navigation separately; they do not establish macOS window-event
delivery or real-device connectivity.

The app binary retains its normal UI/video library dependencies, including
GStreamer. Linking those libraries is not evidence that projection or media was
started; the test path does not initialize the native projection renderer or
start a player.

Ordinary Bluetooth authorization gates, USB, CarPlay, HFP and legacy dongle
paths remain unchanged. The settings detail-row accessibility role is corrected
to compile with the current Slint version while preserving pick-row controls.
All nine frozen privilege/transport files and the immutable verifier are
preserved. Ordinary hardware gates remain false and native TCP admission
continues to fail closed with `IdentityUnverified`.

No Wi-Fi/AP settings, credentials, endpoints, start/setup messages, IP listener,
TLS, microphone, media or projection process is used by this test.
Self-reported phone identity labels are **not authentication**.
Wireless Android Auto remains **NOT READY**, even after a Bluetooth PASS.
Full projection still requires a supported authenticated admission design,
quarantine before session side effects, implementation and separate validation.
Do not bypass these requirements with labels, certificate acceptance or gates.

## Software checks only

No app window or real Bluetooth operation is launched by these unit tests.

From the repository root:

    python3 -B -m unittest discover -s utils -p test_livi_bluetooth_app_test.py -v
    python3 -B -m unittest discover -s utils -p test_bluetooth_only_diagnostic.py -v
    python3 -B utils/verify_bluetooth_boundaries.py

From `native/`, with the local GStreamer SDK's `lib/pkgconfig` supplied as needed:

    CI=1 CARGO_INCREMENTAL=0 cargo test --locked --offline \
      -p livi-ui --features bluetooth-app-test -- --test-threads=1

These are Rust test harnesses, not the application main function. Controller
tests use fake radios and synthetic Unix streams; Python process launches are
mocked. The existing diagnostic/backend suites validate malformed frames,
bounds, bond races, stale events and cleanup behavior.

## Fresh preparation — does not launch the app

From a normal shell prompt at the repository root:

    cd /Users/aditimishra/Downloads/LIVI-main

    python3 -B utils/livi_bluetooth_app_test.py prepare --output temp-attachments/livi-app-bt-01

    python3 -B utils/livi_bluetooth_app_test.py check --output temp-attachments/livi-app-bt-01

Run each command separately; only check after preparation succeeds. If the output
directory exists, preserve it and use a new unused name in both commands.
A fresh app build can need substantial disk space and time because it includes
Slint and normal app libraries. A local GStreamer SDK and cached locked Rust
dependencies are required. No installation or legacy packaging script is used.

The launcher checks platform/non-root use, immutable boundaries, ordinary closed
gates, requested feature isolation, source freshness, executable architecture and
signature and static dependency hashes. It records a new app-specific manifest.
Any source/binary/dependency change requires fresh preparation. Old full phone
test and standalone diagnostic manifests are rejected. Partial build outputs
are retained, never overwritten or removed.

The direct test executable already embeds feature-gated usage metadata in
`main.rs`. Preparation/check now validate its existing identifier and Bluetooth
usage description in the Mach-O `__TEXT,__info_plist` section without running
it. Missing, malformed or concatenated plist documents are refused. The existing
source fingerprint includes `main.rs`; no extra plist or second linker section
is needed. Metadata is not OS permission; normal macOS consent remains required.
The existing identifier, production bundle metadata and permission checks are
unchanged.

Preparation/check do not launch LIVI or access Bluetooth/Wi-Fi. No app-specific
ready manifest is created by unit tests.

## Separately authorized real test

Obtain explicit authorization for one real LIVI Bluetooth-only app test.
Do not use an installed or old LIVI.app.

### Enter phone details in the real app

After fresh preparation, open the checked test screen from the repository root:

    python3 -B utils/livi_bluetooth_app_test.py open --output temp-attachments/livi-app-bt-01

Use the unused output name chosen during preparation. This launches the real
LIVI executable in Bluetooth-only mode, with no phone selection or hardware
authorization passed on the command line. It does not connect automatically.
The `open` action rejects command-line hardware-authorization and phone-address
flags; use the on-screen controls or the separate `run` action instead.
Enter the already-paired phone's Classic Bluetooth address, check the explicit
authorization checkbox, and click Connect within 60 seconds of opening.
The address is kept in memory only, not saved to app settings or a report.
Invalid input or missing consent never creates the Bluetooth backend.
After expiry, cancellation or an attempt, close and open a fresh process;
there is no in-app retry. Keep the setup stationary and stop if pairing is requested.

### Command-line selection alternative

Prerequisites:

- Non-root macOS ARM64, stationary setup, selected phone already paired.
- No USB projection cable or other active receiver competing for the AA service.
- Stop if macOS asks for new pairing; do not approve pairing for this test.
- Privately set `PHONE_BT_ADDRESS` to the selected phone's Classic Bluetooth
  address, not its device name or a random BLE address.
- Fresh preparation and check must pass. Do not use private AP configuration.
- A Bluetooth privacy prompt may occur; the app cannot grant OS permission.
- Do not paste addresses, identity values, tokens or private settings into chat.

Only after authorization, from the repository root:

    python3 -B utils/livi_bluetooth_app_test.py run --output temp-attachments/livi-app-bt-01 --phone-address "${PHONE_BT_ADDRESS:?Set the paired phone address locally}" --authorize-bluetooth-hardware --paired-phone-only

This launches exactly one freshly checked `LIVI` child with the test-only flags.
The actual app window opens; review its notice and click Connect within 60
seconds. Merely opening the window does not connect. Wait for PASS or FAIL.
After reading the final result, close the window to return to the terminal.
The connection closes after the version exchange; PASS does not mean a
persistent Bluetooth session or projection readiness.

Cancel or close to stop. The wrapper watchdog allows 125 seconds for consent,
one bounded attempt and cleanup. On interruption/hang it signals only its owned
child group, grants 3 seconds, then kills a stuck child. Forced termination is
never confirmed native cleanup.

Report only the fixed status and redacted error. On failure, do not pair, retry
automatically, send credentials, enable gates or switch to the full harness.

See also:
- [Standalone Bluetooth diagnostic](README-bluetooth-only-diagnostic.md)
- [Full phone-test harness limitations](README-native-phone-test.md)
