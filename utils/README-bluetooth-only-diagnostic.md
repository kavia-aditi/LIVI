# Bluetooth-only diagnostic — no Wi-Fi bootstrap or projection

For the opt-in test view inside the actual LIVI app, see
[Real LIVI app Bluetooth test](README-livi-bluetooth-app-test.md).
It reuses this protocol runner without enabling production projection.

This is a separate, default-off software diagnostic for one already-paired
Android phone, such as the operator-selected OnePlus 11R.

**Software implementation is not hardware validation or hardware authorization.**
No real Bluetooth diagnostic has been run as part of this implementation.
The earlier isolated phone-test binaries remain stale and must not be reused.
Wireless Android Auto remains **NOT READY**: native TCP admission still fails
closed with `IdentityUnverified`.

## Hard boundaries

The diagnostic:

- Verifies the selected target has an existing macOS Bluetooth bond.
- Never initiates pairing, confirms pairing, supplies a PIN, discovers arbitrary
  devices, resets the radio, unpairs, or disconnects the entire device ACL.
- Uses the existing native Bluetooth backend on the process main thread.
- Publishes a temporary local AA RFCOMM server service and arms an incoming-only
  listener for the selected bonded peer. It issues no remote SDP query, outgoing
  RFCOMM open, discovery, pairing or retry. Unrelated and stale arrivals are rejected.
- Sends exactly one WPP `WifiVersionRequest` (ID 4), version 6.0. The request
  contains only version fields: no AP, endpoint, car metadata or invented
  capability declarations.
- Expects `WifiVersionResponse` (ID 5), version 6.0, status 0, a phone
  `mobile_device_identity.mobile_device_id` and `device_serial`. Both labels must
  be 1–128 ASCII graphic bytes.
- Never sends start, Wi-Fi credentials, setup information, TCP endpoint data,
  pongs, retries or fallback messages.
- Never creates an IP socket/listener or launches helper, core, UI, media, TLS,
  microphone, renderer or projection processes.
- Does not read the private AP configuration or ordinary LIVI user settings.
- Does not print or persist identity values, addresses, raw frames or tokens.
  The supplied target address is a local process argument; do not paste it into
  shared chat or capture process listings.

Labels are self-reported protocol data, **not authenticated identity proof**.
A Bluetooth-only PASS does not unblock TCP admission.

## Timing and ownership

One process owns one attempt, with no reconnect or target fallback:

1. Existing-bond verification and waiting for selected-peer incoming RFCOMM.
2. One version request.
3. One bounded version response.
4. Immediate cancellation, owned-channel closure and service withdrawal.

The overall deadline is 60 seconds, including permission and service startup;
it is not a guaranteed 60 seconds of armed listening. There is no 15-second SDP
deadline in the standalone listener-only path because it starts no query.
The version phase, including write
backpressure, has a 10-second absolute deadline, capped by the overall deadline.
Response bodies are capped at 4096 bytes; the length is checked before allocation.
Slow partial input does not reset deadlines.

Cleanup has a separate 2-second grace period. A pending SDP/open callback prevents
PASS until ownership is released. An inert native callback target may remain
retained while awaiting a late OS callback, but owns no active service or file
descriptor. Unconfirmed cleanup reports `Cleanup`, never PASS. Ctrl-C and SIGTERM
cancel the attempt; cancellation wins until the final result is committed.

The Python launcher also enforces a 65-second watchdog. It signals only its own
child process group, gives cancellation 3 seconds, then kills a hung child.
Forced termination is not evidence of confirmed native cleanup.

Existing USB, CarPlay, HFP, dongle and production gate behavior is not changed.
The paired-only API is opt-in; ordinary native connection behavior is retained.

## Redacted native failure detail

The standalone diagnostic explicitly enables an error-detail side channel.
Ordinary native Bluetooth users and the separate app-test mode do not enable it.
The standalone backend now uses paired listener-only selection instead of its
previous outgoing SDP path. Ordinary callers retain their connection entrypoint.
No pairing fallback or retry is added. Existing query/open callback ownership is
not weakened: any pending owner still prevents confirmed cleanup until its
matching callback releases it. The standalone path should create no such owner.
Adoption requires an open selected-peer channel, nonzero MTU and an owned bridge.

Failure output leads with fixed `category` and `operation` names, followed by
`IOReturn=0xXXXXXXXX` when the failing macOS API supplied an error. Application
checks, missing records/channels, CoreBluetooth state changes and deadline
failures have `IOReturn=unavailable`; application error codes are never presented
as macOS errors. Output contains no device names, addresses, identifiers,
credentials, raw frames or device-supplied error strings.

Categories distinguish `OwnerUnavailable`, `Busy`, `DeviceNotFound`,
`UnpairedTarget`, `Sdp`, `Rfcomm`, `Timeout`, `Permission` and `Unavailable`.
`Transport` and `Registration` cover bridge and local-service failures.
Operations distinguish lookup/bond checks, SDP query/completion/record/channel,
RFCOMM open/completion/write/close, permission/availability checks and deadlines.
An `IOReturn` preserves its original 32-bit pattern; its presence alone does not
establish the device or OS root cause.

The separate `outcome` is the runner's lifecycle result. For example, an SDP miss
may be reported with `outcome=Timeout` because existing behavior waits for an
incoming connection. A later terminal failure replaces a provisional SDP miss;
successful channel adoption clears that provisional context. Cleanup never
erases the retained failure. `owned cleanup=confirmed` means the native owned-
resource idle predicate passed; `unconfirmed` is not a successful disconnect.
Startup refusal does not claim independently confirmed cleanup.

Older `FAIL: Pairing` output cannot retrospectively recover a discarded error.
After these source changes, preserve all old artifacts and prepare a new one in
a previously unused directory. Preparation and `check` are software-only; a
hardware run still requires separate authorization. Neither improved reporting
nor a software check establishes Bluetooth or Android Auto readiness.

## Listener and legacy SDP lifecycle transcript

Only the standalone diagnostic opts into this stream. Ordinary production and
the app-test mode do not enable it. Events contain a fixed enum token and an
optional 32-bit macOS `IOReturn`; no addresses, identities, credentials, frames,
device-supplied strings or timestamps are captured.

In the standalone listener-only mode, expect `LocalServiceReady` followed by
`PairedListenerArmed`, not `QueryInitiated` or `QueryAccepted`. SDP tokens remain
documented below for earlier artifacts and native regression tests; they do not
describe a query performed by this standalone mode.

The transcript is printed **after the final runner outcome is committed**,
including observations collected during cleanup. Silence before that point is
not evidence that no callback occurred. Native capture and the runner transcript
each have a 64-event bound, reserving the last slot for a sticky `Overflow`.
Overflow stops further retention, never connection processing. After overflow,
absence of any subsequent event proves nothing. Output interruption or watchdog
termination may also leave an incomplete transcript.

| Event | Exactly what it proves | What it does not prove |
| --- | --- | --- |
| `LocalServiceReady` | Local service publication, assigned-channel lookup and incoming notification registration succeeded. | Remote visibility, phone discovery or a connection. |
| `PairedListenerArmed` | The existing bond passed selection checks and the selected peer is eligible for incoming acceptance without a remote query. | Phone activation, incoming traffic or ownership of a channel. |
| `IncomingChannelOpen` / `IncomingChannelNotOpen` | The incoming notification passed token, direction, channel, peer and lifecycle guards; native `isOpen()` returned true/false. | An outgoing open-completion callback or its status. |
| `DelegateAttached` / `DelegateRejected` | `setDelegate:` returned the recorded real `IOReturn`, including zero on success. | Successful bridge adoption or phone payload delivery. |
| `IncomingRejected` | At least one incoming notification acceptance guard failed. | Which guard failed, a malicious peer, or a connection owned by this attempt. |
| `OpenCallbackEntered` | The RFCOMM open-completion delegate method was invoked with the recorded real status. | That an incoming channel must generate this outgoing-open callback, or that adoption succeeded. |
| `QueryInitiated` | The existing page guards and bond check passed, pending callback ownership was installed, and execution reached the public SDP call site. | Successful request submission, over-the-air traffic, permission, remote response or service support. |
| `QueryAccepted` | `performSDPQuery:uuids:` returned success (raw zero). | Completion, delivery, or that the phone advertises the requested UUID. It is logged on API return, so a reentrant callback could precede it. |
| `QueryRejected` | That call returned the recorded nonzero `IOReturn`. | The OS/device root cause. Existing failure handling and no-retry behavior remain unchanged. |
| `CallbackEntered` | This adapter's SDP completion selector was invoked with the recorded status. | A matching selected target, current pending query, acceptance, success or timely delivery. |
| `CallbackAccepted` | The target pointer/pending-query guards matched and the attempt was active, available, and had no adopted channel. Pending query ownership was released by the existing code. | Successful SDP status, retained bond, record/channel availability or connection success. Inspect the raw status and subsequent events. |
| `CallbackRejected` | The target/pending-query guard failed, or an existing channel prevented further SDP handling. | Which of those guards failed, malicious input, or any device identity. Wrong-target callbacks leave pending ownership unchanged; matching existing-channel callbacks release it as before. |
| `CallbackLate` | A matching pending callback entered after cancellation, closing, disablement or unavailability; ownership was released and no further SDP action taken. | Wall-clock lateness or which inactive condition applied. A wrong-target/nonpending callback is rejected, not classified late. |
| `ServiceRecordFound` | The existing cache lookup for the unchanged UUID returned a record object. | Fresh remote discovery, successful completion status, the phone's server role, channel availability or authentication. Cached presence is recorded even on error status. |
| `ServiceRecordMissing` | That cache lookup returned no record object. | That the phone cannot support AA or never advertises the service. No additional query is performed. |
| `RfcommChannelFound` | On a successful SDP callback with a record, the existing channel lookup returned success and a nonzero channel. Its raw return is retained. | Successful RFCOMM open/adoption, WPP, or projection. No additional channel validation is introduced. |
| `RfcommChannelMissing` | The existing channel lookup returned an error or zero channel. The raw lookup return, including zero, is retained. | Which condition failed when raw zero, remote compatibility, or root cause. This path still waits for incoming RFCOMM as before. |
| `RfcommChannelNotChecked` | The channel lookup was short-circuited because completion status was nonzero or the record was absent. | A missing channel. Bond-loss or earlier callback guards can return without any record/channel event at all. |
| `IncomingObserved` | The adapter's incoming RFCOMM notification method was entered. | Selected-peer identity, acceptance or adoption; unrelated/stale notifications also count and retain the same rejection behavior. |
| `OwnedTransport` | Adoption checked an open selected-peer channel and nonzero MTU, created the owned socket bridge, and queued the descriptor-bearing connection event. | Phone activation, over-the-air payload delivery, WPP response, authenticated identity, projection or continued connectivity after cleanup. |
| `IncomingNotObservedAtStop` | No incoming notification had entered this adapter between diagnostic reporting opt-in and its first stop call. | Absence of OS/radio traffic, a future callback, a remote attempt, or successful cleanup. Later cleanup callbacks may still append `IncomingObserved`. |
| `QueryDeadline` | The 15-second query deadline branch fired while the attempt was active, `querying` remained set, and no adopted bridge existed. | No callback ever occurred: a wrong-target callback may have been rejected, and a matching callback may arrive later in cleanup. |
| `OpenDeadline` / `PairDeadline` | The existing native open/pair timeout branch fired. Pairing remains forbidden in the isolated diagnostic; its timeout path is tested with fakes only. | OS root cause, cleanup success, or any authorization to pair. |
| `Overflow` | Native capture or runner retention exceeded its fixed bound; later observations were omitted. | The number or kind of omitted events. |

Native deadlines have no underlying API return and report `IOReturn=unavailable`.
Callback events preserve their actual callback status, including success.
Application checks never masquerade as macOS errors. Overall/version deadlines
continue to use the final failure's existing `OverallDeadline`/`VersionDeadline`;
cleanup timeout remains the authoritative `Cleanup` outcome.

The unchanged UUID `4de17a00-52cb-11e6-bdf4-0800200c9a66` identifies the local
AA server service. The intended incoming path is phone as RFCOMM initiator and
LIVI as server; it does not require the phone to advertise that server record.
The service retains its L2CAP/RFCOMM descriptors, browse group and SPP profile
metadata. macOS may reassign the requested channel 8: the listener uses the
published record's actual assigned channel, not a hardcoded acceptance channel.
The public SDK defines the notification as a channel that has opened; acceptance
checks incoming direction and `isOpen()`, then attaches the delegate and adopts.
No outgoing callback status is fabricated for an incoming notification.

For older artifacts, `QueryAccepted` followed by `QueryDeadline` establishes
submission acceptance and expiration of the local query deadline, not why the
phone stayed silent. Pending SDP/open ownership is never cleared to manufacture
idle. A paired/ACL-connected phone is not an owned LIVI RFCOMM channel.
If the new listener is armed but accepts no incoming channel, phone-side AA
discovery/activation remains unestablished. The exact activation trigger is
unknown and requires phone-side evidence; do not invent an HFP/BLE trigger or
weaken authentication to compensate.

Stage descriptions explicitly distinguish a checked bond awaiting RFCOMM, an
accepted owned stream, a version request queued to the local bridge, and validated
response labels. `TransportEvidence: selected_peer_owned_stream_observed=true`
means the runner accepted the selected peer's descriptor during this attempt.
It is historical evidence printed after cleanup, not a claim of a currently active
channel. A WPP failure can coexist with an observed transport; only the final PASS
establishes the complete bounded diagnostic sequence.

Hardware-free tests cover every SDP callback guard, error/success status,
record/channel lookup branch, duplicate callback, cancellation/stop/destruction,
native query/open/pair deadlines, callback arrival after timeout and during
cleanup, incoming notification observation on rejected/adoption paths, no
incoming snapshot, opt-out behavior, raw returns and sticky overflow. Fake
adoption records only the branch taken and never calls a real controller.
Runner tests additionally cover bounded draining, late cleanup observations,
version/overall deadlines and cancellation under telemetry floods.

Additional native tests exercise the actual adoption, descriptor transfer and
bidirectional socket bridge with fake channels and a fake local-address provider.
They cover incoming adoption while SDP remains pending, survival past that query's
deadline, late SDP errors, cancellation, channel closure, rejection of closed or
zero-MTU channels, and continued timeout when no bridge exists. No controller is
queried. These software tests are not evidence of a real Bluetooth connection.

Passive-listener tests also exercise actual native selection and registration:
59 seconds without any query/open, reassigned service channels, real bridge
ownership and closure using fake channels, delegate error preservation, wrong
peer/direction/channel/token, closed channels, bond loss, cancellation and
availability loss. These tests neither use the controller nor prove that a
OnePlus initiates RFCOMM.

## Hardware-free software verification

From the repository root:

    python3 -B -m unittest discover -s utils -p test_bluetooth_only_diagnostic.py -v
    python3 -B utils/verify_bluetooth_boundaries.py

From `native/`, use a dedicated validation target directory, not a prepared
phone-test artifact directory:

    CI=1 CARGO_INCREMENTAL=0 cargo test --locked --offline \
      --target-dir ../temp-attachments/bt-only-validation-NEW \
      -p livi-bt-diagnostic -p livi-native-bluetooth \
      --features livi-bt-diagnostic/bluetooth-diagnostic -- --test-threads=1

These tests use synthetic Unix streams, fake radio events and native NSObject
test doubles. The test binary's main entry point is not executed. The suite
covers authorization, paired-only checks, bond loss, selected/stale peers,
fragmentation, malformed input, missing/unsafe labels, unexpected messages,
short writes, interrupted I/O, backpressure, absolute deadlines, event floods,
cancellation races, late callbacks, cleanup failures and dependency isolation.
Python launch tests mock every potential process launch.

Error-reporting tests additionally inject every category, raw SDP/RFCOMM errors,
missing records/channels, permission denial, availability loss, native and runner
timeouts, provisional SDP replacement, unexpected channel closure and bridge
failure. They check ordinary reporting remains disabled, detail survives cleanup,
cancellation remains authoritative, and connection rejection never retries.

The package's executable requires its explicit `bluetooth-diagnostic` feature.
Its runtime dependency graph contains no full AA session or runtime crate.

## Future preparation — software only

Do not use the legacy packager, old app builder, installed LIVI or existing
phone-test launcher as a substitute. No GStreamer SDK or AP settings are needed.

Choose a new direct child of `temp-attachments`. Preparation refuses any existing
output, retains partial outputs on failure, and never overwrites prepared artifacts:

    python3 -B utils/bluetooth_only_diagnostic.py prepare \
      --output temp-attachments/bt-only-oneplus11r-NEW

This builds only the diagnostic with locked, offline dependencies for macOS ARM64.
It checks frozen boundaries and ordinary false gates, allowed build features,
source freshness, executable architecture/signature and static dependency hashes.
It does not run the executable, probe Bluetooth/Wi-Fi, or install an app.

Preparation writes a diagnostic-specific `ready.json`. It is not a
self-contained distribution or complete reproducible-build attestation.
Validate it without contacting hardware:

    python3 -B utils/bluetooth_only_diagnostic.py check \
      --output temp-attachments/bt-only-oneplus11r-NEW

Any source, binary or dependency change requires a new preparation in a new
directory. The old full phone-test manifest is intentionally not accepted.

## Separately authorized real Bluetooth-only test

**Do not execute under software-implementation authorization.**
Obtain explicit permission for one real Bluetooth-only attempt first.

Operator prerequisites:

- Non-root macOS ARM64 session, stationary test setup.
- Intended OnePlus 11R already paired in macOS; no pairing repair during this test.
- No USB projection or other active projection/receiver. Disconnect its USB cable
  beforehand; this diagnostic does not create USB watchers.
- No ordinary LIVI or another test competing for the AA Bluetooth service.
- Set `PHONE_BT_ADDRESS` privately to the intended paired phone's Classic Bluetooth
  address. A device name or BLE random address is not a valid substitute.
- Review the fresh artifact with `check`. A macOS Bluetooth permission prompt may
  occur during the authorized run; the software cannot grant permission.
- Stop if the OS requests new pairing; do not approve pairing as a diagnostic step.

Only after that separate authorization, from the repository root:

    python3 -B utils/bluetooth_only_diagnostic.py run \
      --output temp-attachments/bt-only-oneplus11r-NEW \
      --phone-address "${PHONE_BT_ADDRESS:?Set the paired phone address locally}" \
      --authorize-bluetooth-hardware \
      --paired-phone-only

This command launches exactly one diagnostic child and accesses real Bluetooth.
No run has been performed by this implementation.

Expected incoming evidence is `LocalServiceReady`, `PairedListenerArmed`,
`IncomingObserved`, `IncomingChannelOpen`, `DelegateAttached` with raw success,
and `OwnedTransport`, followed by
`selected_peer_owned_stream_observed=true` if the runner accepts the descriptor.
This is not a guarantee that the phone will initiate. An armed silent listener
must still time out as FAIL, with ownership cleanup reported independently.
Neither service registration nor an ACL Connected label satisfies acceptance.

Expected PASS: selected bond verified, incoming RFCOMM connected, version/status/identity
valid, owned cleanup confirmed. Stage output may skip an intermediate stage when
several callbacks are processed in one tick; only the final PASS establishes the
complete sequence. Do not collect raw identity values or protocol dumps.

A timeout, incompatible response, missing identity, revoked bond, conflicting
registration, denied OS permission or unconfirmed cleanup is a diagnostic FAIL.
Do not compensate by starting full bootstrap, sending credentials, changing
pairing, opening TCP, or disabling authorization. A phone may simply not support
this version-only interaction; report that result without fallback.

A successful run validates only this Bluetooth-only sequence on that device.
Wireless Android Auto remains NOT READY regardless of this outcome.
