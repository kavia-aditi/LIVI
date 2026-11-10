# Native wireless phone identity bootstrap

Related: [phone-test harness](README-native-phone-test.md) /
[private configuration](README-private-phone-config.md).

## Finding: the bootstrap dependency was circular

Verified in public source:

- [`wpp.rs`](../native/crates/livi-aa/src/wpp.rs), `checked_identity`:
  `instance_id` is `WifiVersionResponse.mobile_device_identity.mobile_device_id`;
  `serial` is `WifiVersionResponse.device_serial`. They are phone labels.
- [`aa.rs`](../native/crates/livi-helperd/src/aa.rs), `emit_device`:
  the legacy event publishes labels only after a phone response. The strict
  native path deliberately does not broadcast or log them.
- [`config.rs`](../native/crates/livi-native-handoff/src/config.rs):
  the old configuration required both labels before opening the native WPP
  exchange. An operator without a previous phone response could not supply them.
  A receiver or LIVI Link dongle identity is not a substitute.

Removing the precondition is necessary, but it does not authenticate a phone.

## Implemented safe subset

1. **Configuration phase:** require the existing paired-phone Bluetooth MAC,
   AP credentials, BSSID, channel, assigned Mac IPv4 and fixed port/security.
   Phone identity pins may be omitted together; supplied pins must still be a
   complete valid pair and must match. Null, partial, empty or malformed pins
   fail closed. Configuration/debug errors remain redacted.
2. **Current-stream learning:** only the helper's owned arrival for the selected
   MAC and pending generation reaches the strict WPP exchange. The first valid
   6.0/success response must contain two bounded, nonempty ASCII graphic labels.
   Optional pins are checked. The labels are latched in that exchange's local
   memory; a later response changing either label fails.
3. **Attempt isolation:** no global cache, disk persistence, operator-generated
   ID, fallback to a previous attempt, or `aa-device` publication was added.
   Cancellation/EOF drops the owned exchange. Existing runtime, generation,
   authorization-grant, pending-arrival and deadline checks remain in place.
4. **USB protection:** check the selected Bluetooth MAC and optional pins before
   starting, then the learned phone labels before AP start/credential responses
   and in the existing post-handshake monitoring path.
5. **Session boundary:** pass the successful exchange's learned identity to
   `Permit::wpp_complete`. A real listener permit rejects it as
   `Failure::IdentityUnverified` and revokes the attempt. It does not open the
   TCP admission window or create a projection session. Synthetic `Permit::Offline`
   exists only under `cfg(test)` for protocol tests.
6. **Operator setup:** the local wizard asks only for known AP/phone settings.
   It does not ask for receiver IDs or generate phone IDs. Existing file privacy,
   no-overwrite, hidden-input and redacted-output behavior is retained.

The learned labels describe a **current routed stream**, not a cryptographically
authenticated peer. No API has been added that claims otherwise. Matching optional
pins does not bypass the session rejection. The native path is deliberately not
phone-projection-ready; this is a partial fix with a safety blocker.

## Blocker: no verified cross-transport phone identity proof

Verified evidence:

- [`admission.rs`](../native/crates/livi-aa/src/server/admission.rs) explicitly
  states that network/attempt admission is not Bluetooth/TCP peer authentication.
- [`tls.rs`](../native/crates/livi-aa/src/tls.rs), `AcceptAny`, accepts the peer
  certificate and handshake signatures without authenticating a pinned phone.
  This change does not modify TLS or shared legacy behavior.
- [`service_discovery.proto`](../native/crates/livi-aa-proto/proto/service_discovery.proto)
  offers a self-reported `mobile_device_identity`, not a signature proving that a
  TCP peer owns the Bluetooth connection. Comparing that string is insufficient.
- [`wireless_setup.proto`](../native/crates/livi-aa-proto/proto/wireless_setup.proto)
  declares `setup_token` in `WifiVersionResponse` and `WifiSetupInfoNotification`.
  No token consumer/verifier exists in the current AA session/stack source.
  TCP `VersionResponseOptions` in
  [`control.proto`](../native/crates/livi-aa-proto/proto/control.proto) contains
  connection/ping configuration, not an implemented token proof exchange.
  These fields warrant protocol research; their presence alone is not a safe
  basis for inventing token-to-certificate or token-to-session semantics.

No supported Android-compatible trust/bootstrap mechanism binding the paired
Bluetooth peer to the TCP cryptographic endpoint has been established by this
investigation. An IP/subnet, matching labels, successful WPP join status, or the
existing TLS completion must not be promoted into such a proof. Pairing and
receiver authorization are separate prerequisites, not substitutes for binding.

## Remaining design and minimal changes before real-phone testing

These items are proposed, not implemented:

1. Establish an Android-compatible authenticated binding supported by the actual
   phone protocol and available unprivileged APIs. Supply protocol/implementation
   evidence and a trust source; do not invent a certificate pin, phone ID, shared
   secret, or unsupported phone-side challenge. If this requires changing any
   frozen privileged API, stop and request approval instead.
2. Verify that binding against the selected OS-paired Bluetooth peer and current
   handshake. Treat the learned WPP identity as provisional until verification
   completes. Existing MAC/AP validation and receiver consent must remain.
3. Replace the unconditional listener refusal only after adding a non-forgeable
   verified-binding capability to admission. It must bind the learned identity
   and selected peer to the exact runtime, generation, attempt, and TCP endpoint;
   be single-use and deadline-bound; and be revoked on cancellation, EOF,
   helper restart, listener/address change or USB takeover.
4. Do not publish a session socket, readiness/projection success, or media/input
   path before verification. Reject wrong/missing proof, replay, another device
   with identical labels, and an unrelated same-subnet TCP client.
   Concretely, `admission.rs::serve_qualified` currently calls `bind_session`
   and `on_session` before `session.rs::run` performs TLS. A supported proof
   would need a bounded native-only quarantined setup path before those calls,
   factoring the necessary setup from `session::run`/`TlsEngine` while preserving
   legacy routing. Retain the existing single TCP listener; do not add a fallback
   listener or expose a node socket just to obtain the proof.
5. Add hardware-free positive and negative proof tests, including cross-device,
   cross-channel and stale-runtime/attempt cases. Existing fail-closed tests must
   continue passing. A test-only proof fixture must not become a production bypass.
6. Re-run immutable boundary verification, ordinary/isolated compile matrices,
   and the Python safety tests. Build into a **new** preparation directory;
   existing build outputs/manifests remain untouched. Source changes intentionally
   invalidate the earlier prepared binaries.
7. Only then ask the operator to create/validate private settings locally and
   separately authorize a bounded real-phone test. No dongle is required.

There is no safe configuration-only switch that completes step 1.

## Hardware-free regression coverage

- Parser: omitted pins round-trip; partial/null/empty pins fail; valid supplied
  pins continue constraining identity.
- WPP: learn from the current synthetic response, identical repeats permitted,
  changed labels rejected before credentials, malformed/missing identity rejected,
  no previous-stream identity reuse, and learned-label USB suppression.
- Real private router/shared listener fixture: normal authorization and WPP
  exchange with omitted IDs, premature TCP rejection, `IdentityUnverified`
  revocation, no session callback, and stale TCP rejection after cancellation.
- Existing authorization replay/runtime/expiry, wrong-MAC/stale-arrival,
  cancellation, WPP timeout and production-gate tests remain applicable.
- Python: local-only wizard privacy/schema tests and isolated launcher safety
  regressions use only synthetic values and test-owned local resources.

Run from the repository root:

    python3 -B -m unittest discover -s utils -p test_setup_phone_test_config.py -v
    python3 -B -m unittest discover -s utils -p test_native_phone_test.py -v
    python3 -B utils/verify_bluetooth_boundaries.py

Run Rust tests from `native/` with the locally installed SDK configured:

    CI=1 CARGO_INCREMENTAL=0 cargo test --locked --offline \
      -p livi-native-handoff -p livi-native-bt-dev \
      --features livi-native-bt-dev/isolated-helper -- --test-threads=1

No real private configuration, phone, LIVI application, helper service, renderer,
Bluetooth/Wi-Fi radio, installed app, legacy packager or candidate packaging is
needed for these tests. Elevated privileged APIs and the nine frozen files are
outside the change set.
