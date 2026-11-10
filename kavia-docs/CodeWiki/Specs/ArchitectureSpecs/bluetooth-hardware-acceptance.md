---
id: bluetooth-hardware-acceptance
title: Bluetooth Hardware Acceptance Checklist
status: draft
execution_status: not-run
---

[CodeWiki](../../index.md) / [Specs](../index.md) / [Architecture Specs](index.md)

# Bluetooth Hardware Acceptance Checklist

## Audit status and permission boundary

**Unexecuted future lab plan, not authorization.** All results remain NOT RUN. Current standing restrictions prohibit tests, builds, formatters, application/browser launches, sockets, connections, hardware operations, and restores. Do not execute the operator recipes below under this audit. No authentication/TLS/WPP, authorization-policy, elevated API, or Stage 5/6 changes are permitted. Production gates remain false and `IdentityUnverified` rejection is mandatory; paired inventory is UI-only and never `Registry::note_device`. HFP is not a proven Android Auto prerequisite.

Two original acceptance assumptions conflict with current source. They remain open requirements, not verified guarantees or instructions to change code:

| Affected cases | Current static evidence and unresolved gap |
| --- | --- |
| BT-03 — reconnect without Wi-Fi | [Linux reconnect](../../../../native/crates/livi-runtime/src/reconnect.rs), `run_with_wifi` and `paging_allowed`, gates disconnected paging on Wi-Fi (`connected || wifi_ready`). The original Linux AP-independent paging expectation is therefore not supported by the reviewed path. The dongle path must be assessed separately. No hardware result is inferred. |
| BT-04 and BT-10 prerequisite — HFP survival without projection | [Policy](../../../../native/crates/livi-runtime/src/bluetooth/policy.rs), `Reconnect::decide`, does not consult `hands_free` or `hfp_linked` to exempt stale connected observations without `link_active`. Its HFP fixture expects stale disconnection; the fixture was read, not run. HFP offering is not proof of persistent HFP service or bidirectional audio. |

Do not weaken these acceptance criteria to match code or report them as passed. Reconcile intended behavior and authorize any future investigation separately. Historical Stage 1–4 preservation remains **UNVERIFIED** without a valid baseline; no prior binary, application bundle, or configuration is inspected or restored here. Frozen source files and approved immutable tests must never be edited. The boundary verifier is **hash-only; never execute it**.

## Purpose and execution status

Validate the [Bluetooth decoupling plan](bluetooth-decoupling.md) on real equipment without changing application code, privileged APIs, transport framing, deployment permissions, or pairing-key storage.

**No hardware scenario in this document has been executed as part of its creation. Every result is NOT RUN.** Previously reported unit tests, private-D-Bus tests, and cross-compilation are not hardware acceptance evidence.

This is an authored operator checklist, not a collection of generated story-linked test artifacts. Procedures below are proposed acceptance work. Timing budgets are lab criteria, not new product guarantees.

## Safety and common prerequisites

- Use a parked vehicle or bench setup, safe speaker volume, and consenting call participants. Never use emergency numbers.
- Use the existing approved deployment and launch procedure. Record the exact command/service, executable digest, configuration source, OS version, kernel/BlueZ version where applicable, and phone OS/model. Do not assume a helper service name.
- Test the known-good and refactored binaries sequentially on the same equipment and configuration. Never run competing helpers against the same adapter or sockets.
- Back up relevant configuration and use dedicated test pairings. Do not erase production pairing keys or factory-reset equipment.
- Record the initial adapter alias, powered/pairable/discoverable state, selected adapter, phone pairing, network configuration, and active services. Restore them after testing.
- Capture helper/core logs with timestamps using the existing launch/logging facility. Record process identity/start time before and after fault injection.
- Use existing privileges only. Any command requiring authorization must run on the approved lab host. Do not add sudo rules, capabilities, D-Bus policies, or disable security controls to make a case pass.
- Logs and radio traces can contain device identifiers, Wi-Fi credentials, authentication data, and call information. Keep raw evidence access-restricted; publish only redacted excerpts. Do not include keys, certificate/signature payloads, passwords, or full traffic captures in this checklist.
- Confirm working baseline pairing/projection/audio before injecting faults. A missing baseline prerequisite is BLOCKED, not a refactor failure.
- Reset targets and stop existing sessions between cases. Restore the normal target list and AP configuration afterward.

### Timing and repetitions

Use one reconnect target unless a step explicitly requires two phones. Record elapsed times rather than judging by visual impressions.

| Measurement | Proposed lab criterion |
| --- | --- |
| Local socket responsiveness | Each probe returns a complete newline-terminated response within 2 seconds. |
| Startup/recovery after hardware and required network transport are ready | Within 60 seconds; record physical insertion separately from enumeration, name resolution, and Bluetooth readiness. |
| Basic wireless bootstrap with AP already ready | Within 60 seconds of the phone accepting/initiating connection. |
| Observation window after recovery | At least 60 seconds without repeated registration/restart loops. |
| Loss/recovery repetitions | Three cycles for discovery/loss, BlueZ restart, and hot-plug. |
| HFP without projection | Keep the established HFP link for at least 60 seconds. |
| Stale iAP | First stale observation starts the 10-second policy interval; with one target, expect disconnect on a later policy visit, approximately 10–15 seconds after that observation. Allow up to 30 seconds after independently confirming the stale precondition before marking an investigation failure. |
| Audio | At least 60 seconds per call; three successive call setup/teardown cycles. |

The one-second reconnect delay is not a promise of one attempt per second per device. Round-robin targets, device queries, and paging duration affect cadence. A failed started page has a one-second timeout; after 15 tracked failures the next eligible retry is 30 seconds after completion/timeout. Skipped/unavailable/busy targets are not necessarily failures.

Exceeding a proposed budget is FAIL for that acceptance run and requires triage. If the known-good binary also exceeds it under equivalent conditions, record a pre-existing limitation rather than claiming a new regression. Do not silently change budgets after observing results.

## Platform and equipment requirements

### Linux host

| Run label | Required equipment/configuration | Applicable cases |
| --- | --- | --- |
| L-local | Supported local BlueZ controller; Linux host with its normal Bluetooth deployment; Android phone and iPhone as applicable | BT-01, BT-03–06, BT-08–10 |
| L-link | Physical supported LIVI Link, approved firmware, USB/network transport, Linux BlueZ/VHCI path selected using `btAdapter=livi-link` or its effective environment equivalent | BT-01, BT-03–10 |

Linux configuration values in the loaded configuration take precedence over the corresponding environment defaults. Verify the **effective** `btAdapter` and `wifiInterface`; setting an environment variable alone may not select the intended hardware. The corresponding environment names are `LIVI_BT_ADAPTER` and `LIVI_WIFI_IFACE`.

Use `bluetoothctl list`, `bluetoothctl show`, and `bluetoothctl info "$PHONE_MAC"` for observation when available. Record controller address as well as `hciN`; numbering can change after hot-plug.

For BT-06, the operator must know how to restart the actual BlueZ service. The command below applies only to a systemd host whose unit is `bluetooth`.

### macOS host

| Run label | Required equipment/configuration | Applicable cases |
| --- | --- | --- |
| M-none | macOS host with no selected LIVI Link Bluetooth backend | BT-02 negative/control case; wired cases in BT-08/09 where supported by the deployment |
| M-link | Physical supported LIVI Link, approved firmware, working USB/network transport; `LIVI_BT_ADAPTER=livi-link` in the helper's actual launch environment | BT-02–05, BT-07–10 |

The macOS helper's Bluetooth path under test is the **LIVI Link dongle stack**, not the Mac's built-in Bluetooth controller. Do not use macOS Bluetooth settings or Linux `bluetoothctl` as proof of the dongle's ACL/HFP state.

macOS shared-socket Bluetooth device-control commands remain unavailable even when the dongle works. The projection socket's configured disconnect callback is a different capability. Do not treat unavailable shared pairing/control as a regression.

Wireless AP cases normally also require the existing LIVI Link AP configuration, including `LIVI_WIFI_IFACE=livi-link` where used by the deployment. Do not invent a native macOS AP alternative.

### Physical LIVI Link

Required for every L-link/M-link result:

- Supported dongle model/board and recorded firmware image/version.
- Known-good USB data cable, stable power, and a second known-good host port.
- Working host-to-Link network path and resolution of `livi-link.local`.
- Correct existing controller ownership: Linux uses the approved BlueZ/VHCI path; macOS uses the dongle host stack.
- Working AP for positive wireless bootstrap tests, and a way to stop only the AP for Wi-Fi-negative tests without cutting the USB/control/HCI transport.
- For CarPlay, an available approved MFi authentication source. If supplied by the unplugged dongle, its loss is a separate dependency.
- Microphone, speaker/output path, and existing core audio-feed routing for SCO acceptance.

A generic USB Bluetooth adapter is not a substitute for physical LIVI Link coverage. Record each supported hardware variant as a separate matrix row. Mark unavailable models NOT RUN, not implicitly covered.

## Operator probes and evidence conventions

### Single-response local socket probe

Run the following shell function in the operator terminal on either host OS with Python 3. It sends exactly one line, reads one response line, and times out; it does not write application files. Do not use it for long-lived subscriptions or tunnels.

```sh
sock_request() {
  python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.settimeout(2); s.connect(sys.argv[1]); s.sendall((sys.argv[2]+"\n").encode()); data=bytearray()
while not data.endswith(b"\n"):
    chunk=s.recv(1)
    if not chunk: raise SystemExit("EOF before response newline")
    data.extend(chunk)
print(data.decode().rstrip("\n")); s.close()' "$1" "$2"
}
sock_request /tmp/livi-shared.sock bogus
sock_request /tmp/cp-bt.sock bogus
```

Both probes should return an unknown-command error rather than hang; these requests do not require authentication hardware. They prove socket responsiveness, not projection health.

On Linux with BlueZ ready, `sock_request /tmp/livi-shared.sock list_paired` also exercises control. While unavailable, and on macOS, expect `{"ok":false,"error":"bluetooth unavailable"}` for shared Bluetooth commands.

### Controlled reconnect targets

Use the existing UI if it manages the reconnect list. For isolated policy cases, the projection socket can set an in-memory target. Set `PHONE_MAC` to the actual test phone address first; do not send an example address. Pause any UI producer that would overwrite the test list using existing operator controls, not code edits.

HFP gateway target:

```sh
sock_request /tmp/cp-bt.sock "reconnect-targets [[\"$PHONE_MAC\",\"0000111f-0000-1000-8000-00805f9b34fb\"]]"
```

iAP target:

```sh
sock_request /tmp/cp-bt.sock "reconnect-targets [[\"$PHONE_MAC\",\"00000000-deca-fade-deca-deafdecacafe\"]]"
```

To isolate automatic paging from discovery or a manually maintained stale ACL:

```sh
sock_request /tmp/cp-bt.sock 'reconnect-targets []'
```

Record the prior intended list and restore it afterward. An `ok` response acknowledges list handling; it does not prove a radio connection. Do not publish real addresses in shared evidence.

### Evidence bundle required for every case

Use a run ID such as `BT-04-M-link-candidate-cycle1`. Capture:

1. Build digest, run label, OS/firmware/phone versions, selected adapter identity, and redacted effective configuration.
2. Timestamped action log: fault injected, transport ready, adapter ready, first page, profile ready, bootstrap started/ended, recovery complete.
3. Relevant helper/core log excerpts, socket probe responses and durations, and phone/host UI observations.
4. Independent transport/profile evidence where possible: Linux device state, authorized dongle diagnostics, or a controlled peer trace. Absence of a log alone is not proof of absence of a connection.
5. Process identity before/after, session counts visible to the operator, and any repeated errors or duplicate audio/session symptoms.
6. Actual result, elapsed times, PASS/FAIL/BLOCKED/NOT RUN, issue/reference, tester, and restoration result.

## BT-01 — Linux adapter discovery, selection, and loss

**Platforms:** L-local and L-link. **Initial result: NOT RUN.**

**Prerequisites:** Two controllers for selected-adapter isolation; otherwise mark that subcase BLOCKED. Use a removable selected controller or approved reversible isolation. For L-link, record the controller exposed by the Link. Keep AP/authentication independent where testing unrelated-service survival.

**Steps:**

1. Record controller addresses and current state. Start the deployed helper with the intended selected adapter absent. Keep an unselected controller present.
2. Probe both local sockets three times. For a fixed missing `hciN`, allow the existing startup attempt/retry behavior; for `livi-link`, observe waiting for attachment.
3. Verify the unselected controller does not accept LIVI profile traffic. Use the controlled phone or authorized radio trace; discoverability alone is insufficient.
4. Attach the selected controller. Wait for enumeration and Bluetooth readiness; record both times. Pair/connect the test phone.
5. Remove the selected controller while idle; repeat while a Bluetooth bootstrap is active.
6. Repeat with an independent wired session active. Restore the controller and connect again. Perform three loss/recovery cycles.

**Expected:** Independent sockets stay responsive; unavailable shared Bluetooth commands return the existing error after withdrawal. Only the selected controller serves LIVI profiles. The helper recovers without manual restart. Work whose physical Bluetooth transport was removed may end; unrelated services remain usable.

**Evidence:** Common bundle, controller mapping per cycle, accepted/rejected phone connection evidence, wired-session observation, Linux control responses before/during/after loss.

**Pass/fail:** PASS if selection is correct, probes meet the budget, and all three recoveries succeed with no duplicate registrations/sessions. FAIL on wrong-controller acceptance, helper crash, unavailable unrelated services, or recovery requiring manual helper restart. Unavailable two-controller proof is BLOCKED for that subcase.

## BT-02 — macOS discovery and unsupported-control compatibility

**Platforms:** M-none and M-link. **Initial result: NOT RUN.**

**Prerequisites:** Normal macOS deployment; for the positive subcase, a physical Link and effective `LIVI_BT_ADAPTER=livi-link`.

**Subcase scope:** Record M-none steps 1–2 separately from M-link steps 3–5. M-none can pass its negative/control subcase without Link hardware; that does not pass or waive the M-link subcase.

**Steps:**

1. Start without a selected dongle backend. Run both `bogus` socket probes and shared `list_paired`.
2. Verify available wired functionality through its normal UI. Do not infer native Mac Bluetooth support.
3. Restart through the existing launch procedure with the Link backend selected but the Link absent. Repeat probes.
4. Attach the Link, wait for USB/network readiness and name resolution, then pair/connect a phone using the existing supported workflow.
5. With HFP or wireless bootstrap working, repeat shared `list_paired`. Remove and restore the Link once; run BT-07 for the repeated hot-plug gate.

**Expected:** Non-Bluetooth services work without a selected dongle. With a selected Link, Bluetooth becomes usable when its transport is ready. Shared Bluetooth control remains unavailable in every macOS configuration; it does not become a BlueZ facade.

**Evidence:** Common bundle, actual launch environment selection, Link-ready and profile-ready logs, unavailable response while Bluetooth demonstrably works.

**Pass/fail:** PASS if both absence and positive-Link behavior match expectations with no process restart on attachment. FAIL if built-in Bluetooth is accidentally required, unsupported shared control becomes exposed, or an absent Link blocks independent services.

## BT-03 — Bluetooth reconnect without Wi-Fi readiness

**Platforms:** L-local, L-link, M-link. **Initial result: NOT RUN.**

**Unresolved source conflict:** The Linux disconnected-paging path is Wi-Fi-gated; see the audit-status table. The steps and expected result below preserve the original proposed criterion, not current verified behavior. Execution is not authorized.

**Prerequisites:** Paired reachable phone and verified HFP reconnect baseline. Stop wireless projection sessions. A way to disable the AP while retaining Bluetooth and the Link's USB/network/HCI path. Do not use a global radio block on a shared Bluetooth/Wi-Fi chipset. A phone-Wi-Fi-off run is useful supplementary coverage, but does not prove that the helper tolerates an unavailable AP.

**Steps:**

1. Set only the HFP target using the probe above. Confirm HFP initially connects.
2. Stop only the AP using the existing approved control. Independently confirm AP unavailability through its service/interface state and a second device's scan; record that Bluetooth and Link control transport remain up.
3. Turn only the phone's Bluetooth off until disconnection is confirmed, then turn it on. Do not reconnect manually from the host.
4. Observe automatic paging and HFP reconnection while Wi-Fi remains unavailable. Keep it connected for 60 seconds.
5. Restore Wi-Fi/AP, restore the normal target list, and perform positive wireless bootstrap.
6. If the equipment supports a controllable iAP peer, repeat paging with the iAP target; distinguish successful RFCOMM connection from projection completion.

**Expected:** Bluetooth paging/HFP reconnection is not gated on AP readiness. Wireless projection need not complete without Wi-Fi. After AP restoration, normal bootstrap is usable.

**Evidence:** Common bundle plus independent AP-down and Bluetooth-up proof, page/profile timestamps, later successful wireless bootstrap.

**Pass/fail:** PASS if Bluetooth/HFP reconnects before Wi-Fi restoration and positive bootstrap works afterward. FAIL if paging begins only after AP restoration while Bluetooth transport was continuously available. BLOCKED if disabling Wi-Fi also cuts the Link control/HCI transport. An unaccepted iAP profile alone does not establish a Wi-Fi gating defect.

## BT-04 — HFP availability without projection

**Platforms:** L-local, L-link, M-link. **Initial result: NOT RUN.**

**Unresolved source conflict:** Current policy can stale-disconnect an HFP-marked connected observation without `link_active`. The survival expectation below remains an unexecuted requirement, not a source-established guarantee. BT-10 cannot assume this gate passed.

**Prerequisites:** Phone with call/HFP support and an approved output route. Disable wireless CarPlay/AA for this run using existing settings/launch controls (`LIVI_CP_WIRELESS=0`, `LIVI_AA_WIRELESS=0` where effective). Do not connect a USB projection phone.

**Steps:**

1. Restart normally with both wireless projection offers disabled. Confirm no projection session is active.
2. Pair if necessary, set the single HFP target, and wait for profile connection.
3. Leave idle for 60 seconds, exceeding the stale-link threshold.
4. Make an ordinary outgoing call and receive an incoming call; select the LIVI hands-free route if the phone requires it.
5. End both calls. Disconnect/reconnect phone Bluetooth with projection still disabled.
6. On M-link only, keep a call active on phone A while disconnected phone B is in the reconnect list; observe that new disconnected-target paging is suppressed during the call and resumes afterward. Restore a single target. This suppression subcase is N/A on both L-local and L-link because they use the Linux BlueZ reconnect policy.

**Expected:** HFP is offered independently of projection, survives beyond 10 seconds, and remains usable. Linux may continue HFP profile wake attempts while connected. The macOS dongle stack respects linked/in-use state and suppresses disconnected-target paging during calls; do not require that suppression from Linux, including L-link.

**Evidence:** HFP up/down observations, projection-disabled effective settings, call times/routes, and a radio/page trace for the two-phone suppression subcase.

**Pass/fail:** PASS if HFP remains connected and calls work without projection. FAIL on a stale-policy disconnect of the HFP link, projection dependency, or demonstrated new M-link paging during the protected call condition. On M-link, mark the two-phone subcase BLOCKED without two phones or suitable observability. Use BT-10 for full audio acceptance.

## BT-05 — Stale iAP ACL cleanup and recovery

**Platforms:** L-local, L-link, M-link. **Initial result: NOT RUN.**

**Prerequisites:** An approved controllable paired iAP peer, or a repeatable phone state that accepts ACL but refuses/ends iAP bootstrap. Must independently prove: ACL connected, no active LIVI session/bootstrap, wake target is iAP rather than HFP. For the dongle stale-disconnect subcase, ensure no active stream makes the connection legitimately in use. Clear reconnect targets before establishing this condition.

**Steps:**

1. Establish and independently confirm the stale precondition while automatic targets are empty. Record time and connection state.
2. Set the single iAP target. Record the first policy observation/page and all subsequent profile/disconnect events.
3. Maintain the peer's refusal of bootstrap while keeping ACL available. Observe beyond 10 seconds and up to the 30-second investigation limit.
4. Confirm that the policy disconnects the stale ACL, then attempts reconnection on a later visit.
5. Release the peer's bootstrap refusal, ensure AP/authentication readiness, and complete a fresh CarPlay bootstrap.
6. As a negative control, repeat with a genuinely active iAP session for at least 30 seconds; it must not be dropped merely for exceeding the stale interval.

**Expected:** A stale non-HFP ACL is disconnected and can recover. An active session clears/suppresses stale cleanup. HFP is not classified as stale iAP.

**Evidence:** Common bundle plus peer-side proof of ACL-without-session, first stale observation timestamp, disconnect/repage order, and recovered session.

**Pass/fail:** PASS only if the stale condition, cleanup, recovery, and active-session negative control are all demonstrated. FAIL if confirmed staleness persists beyond the lab limit without cleanup, a healthy session is dropped by stale policy, or recovery requires restart. BLOCKED if the phone immediately drops ACL or keeps a bootstrap active; toggling Bluetooth off is not a stale-ACL test.

## BT-06 — Linux BlueZ restart and generation recovery

**Platforms:** L-local and L-link only. **macOS: NOT APPLICABLE, not passed. Initial result: NOT RUN.**

**Prerequisites:** Approved Linux lab host, known BlueZ restart procedure, and a baseline with an active independent wired/network session whose AP/authentication does not depend on the failing controller.

**Steps:**

1. Record `org.bluez` owner, helper process identity, selected adapter state, shared control, and an active independent session.
2. On a systemd host with the verified unit name, run `sudo -n systemctl restart bluetooth`. Otherwise use the approved equivalent and record it. Do not execute without authorization.
3. Probe both sockets during restart. Record temporary command failures and their responses.
4. Verify the daemon owner changed. For example, inspect it before and after using:
   `busctl --system call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus GetNameOwner s org.bluez`.
5. Wait for the selected adapter/profile generation to recover without restarting helper/core. Test paired listing, playback-status handling via the normal UI, HFP, and one wireless bootstrap.
6. Confirm the independent session remained usable. Repeat three restarts and then observe for 60 seconds.

**Expected:** Health checks detect the lost/changed daemon generation; old availability/player state is withdrawn and registrations are re-established. Transient Bluetooth operations may fail; no accepted operation must silently be replayed against another generation. Independent services remain responsive.

**Evidence:** Common bundle, before/after unique bus owners, restart command/time, process identities, temporary errors, registration/player logs, independent-session continuity.

**Pass/fail:** PASS if all three generations recover within budget with no crash, persistent duplicate-registration error, stale player binding, or unrelated-service outage. FAIL otherwise. If the allegedly independent session also loses AP/authentication, isolate that dependency and rerun; do not label it a lifecycle regression without evidence.

## BT-07 — Physical LIVI Link hot-plug

**Platforms:** L-link and M-link; physical Link mandatory. **Initial result: NOT RUN.**

**Prerequisites:** Recorded Link model/firmware; known-good cable/power; normal target list; Link transport and wireless baseline verified. Have an independent wired phone/authentication path if testing survival.

**Steps:**

1. Start with the Link absent. Verify local socket responsiveness and independently available wired functions.
2. Insert the Link. Record USB detection, host network readiness, `livi-link.local` resolution, controller readiness, and successful HFP/wireless bootstrap separately.
3. Unplug while idle; reinsert and reconnect without restarting the application.
4. Repeat unplug/reinsert during wireless bootstrap, and then during an HFP call. Use ordinary calls only.
5. After each insertion, verify a fresh HFP call and wireless bootstrap; confirm only one logical connection/session is active.
6. Run three cycles per fault stage. Try the second known-good host port once and record enumeration/address changes.
7. Verify an independent wired session separately, keeping its phone cable and authentication source connected.

**Expected:** Link-dependent Bluetooth, AP, and authentication can become unavailable on unplug; affected sessions/audio may end cleanly. They recover after transport returns. Linux reacquires the currently exposed selected Link controller; macOS restarts the dongle stack. No manual helper restart or new pairing-key migration is required.

**Evidence:** Common bundle, physical action timeline, USB/network/controller mapping, dependency loss classification, reconnect and call results per cycle.

**Pass/fail:** PASS if every required cycle recovers within budget, no stuck audio/duplicate stack persists, and independent functions remain usable. FAIL on crashes, restart-required recovery, or persistent stale sessions. Do not require a wireless session to survive removal of its actual radio/AP/authentication hardware.

## BT-08 — Android Auto wired/wireless arbitration and recovery

**Platforms:** L-local, L-link, M-link for wireless; macOS/Linux wired paths only where the existing deployment supports them. **Initial result: NOT RUN.**

**Prerequisites:** Compatible Android phone, working USB data path and wireless baseline, AP ready, existing UI delivering wired-phone identities. Record phone MAC, instance ID/serial availability, and identity-matching evidence in restricted records.

**Wired-only subcase:** On M-none, or another supported wired-only deployment, no wireless/AP baseline is required. Start without a Bluetooth backend, connect the Android phone by USB, verify display/input/media for 60 seconds, disconnect/reconnect USB three times, and verify both socket probes remain responsive. Capture the wired-session timeline, UI observations, and probe responses. PASS this subcase only if all three cycles work without helper restart; otherwise FAIL. Wireless/arbitration steps below are N/A for that subcase, not passed.

**Steps:**

1. With no USB phone attached, complete wireless AA bootstrap and verify projection display, control input, and media through the normal UI.
2. End the session. Connect the same phone by USB; wait until wired projection and wired-identity reporting are confirmed.
3. Trigger a fresh Bluetooth AA bootstrap attempt using the phone's normal workflow. Confirm the initial identity exchange may occur but no wireless AP offer/start is made for the wired phone.
4. Repeat the identity match once by MAC and, with an approved controllable identity fixture, by instance ID/serial. If the fixture is unavailable, mark those matching subcases BLOCKED.
5. Remove USB, clear the confirmed wired state through the normal UI, and initiate wireless AA again. Do not assume immediate automatic handover is guaranteed.
6. With a different second phone wireless, connect the first phone by USB and verify the identity-specific decision does not incorrectly suppress the other phone.
7. Inject Bluetooth loss during bootstrap, restore it, and perform a fresh bootstrap. Separately verify an established Wi-Fi AA session can remain usable while only Bluetooth is removed, with AP/network unchanged.

**Expected:** Wired identity prevents a duplicate wireless AP offer for the same phone. A nonmatching phone is not rejected by that identity check. Bootstrap can recover; removing Bluetooth alone does not imply the Wi-Fi transport was removed.

**Evidence:** Common bundle, wired identity registration, AA bootstrap messages/logs, AP-offer presence/absence, session/control/media observations, AP continuity during Bluetooth-only loss.

**Pass/fail:** PASS if required transport paths work, wired precedence is correct, and no duplicate projection session is created. FAIL on an AP offer to a confirmed wired match, suppression of a proven nonmatch by that match, or inability to bootstrap after recovery. Unknown wired identity is BLOCKED for identity-specific assertions, not proof of precedence. Hardware-dependent session limits must be recorded separately.

## BT-09 — CarPlay wired/wireless arbitration and identity

**Platforms:** L-local, L-link, M-link; wired-only M-none where supported. **Initial result: NOT RUN.**

**Prerequisites:** Compatible iPhone, approved MFi source, known-good wired and wireless CarPlay, AP ready, normal core/helper identity configuration. Wireless CarPlay must be enabled for this run.

**Wired-only subcase:** On M-none, or another supported wired-only deployment, require only the normal wired CarPlay/authentication prerequisites, not a wireless/AP baseline. Start without a Bluetooth backend, connect the iPhone by USB, verify display/input/media for 60 seconds, disconnect/reconnect USB three times, and verify both socket probes remain responsive. Capture the wired-session timeline, UI observations, and probe responses. PASS this subcase only if all three cycles work without helper restart; otherwise FAIL. Wireless/arbitration steps below are N/A for that subcase, not passed.

**Steps:**

1. With no USB session, complete wireless CarPlay, verify display/input/media, and record the local adapter identity used in bootstrap.
2. End that session; establish wired CarPlay and verify the wired phone is registered in helper state by normal integration.
3. Trigger a Bluetooth CarPlay arrival from the same phone. Observe cable arbitration rejecting/dropping the Bluetooth link rather than creating a second bootstrap.
4. Where a controlled fixture can delay wired-phone identification, repeat before identity is known: the existing unidentified-wired-session protection must still take precedence.
5. With a known wired phone A, attempt wireless phone B, distinguishing arbitration from any existing product limit on simultaneous sessions. Confirm B is not rejected as though it were A.
6. Remove USB and confirm wired state clears; initiate a fresh wireless connection. Verify advertised local address agrees with the selected Linux controller or the Link address in display byte order on macOS.
7. While a truly independent wired session is active, cause a Bluetooth-only failure/recovery. Keep AP/authentication and phone USB intact. Verify wired operation is not cancelled merely by the Bluetooth owner's retry.

**Expected:** Cable arbitration retains its existing known/unknown-phone semantics. No duplicate same-phone iAP bootstrap. Wireless works again after cable removal. Identity/address ordering is unchanged. Independent wired work survives Bluetooth retry.

**Evidence:** Common bundle, wired registration and arbitration logs, rejected arrival, local address comparison, and separate wired/wireless session timeline.

**Pass/fail:** PASS if baseline transport paths, arbitration, identity, and independence behave as above. FAIL on duplicate same-phone bootstrap, wrong local address order, or demonstrated lifecycle cancellation of independent wired work. Mark unavailable delayed-identity/two-phone fixtures BLOCKED. Do not infer an unsupported automatic wireless handover requirement.

## BT-10 — HFP/SCO audio, routing, and teardown

**Platforms:** L-local, L-link, M-link; physical Link required for dongle runs. **Initial result: NOT RUN.**

**Prerequisites:** BT-04 passed on this exact topology; microphone and speaker/output working; existing core feed sink and microphone producer configured. HFP signaling alone is insufficient: without a configured SCO sink and microphone feed, downlink may not be routed and uplink can be silence. Use the normal integration; do not inject arbitrary raw bytes into `/tmp/aa-sco.mic`.

**Steps:**

1. Confirm no projection session is running. Place an outgoing ordinary call and select LIVI hands-free on the phone.
2. Have the remote participant speak a fixed phrase; confirm local speaker intelligibility. Speak a different phrase locally; have the remote participant repeat it. Test each direction separately, then normal conversation for at least 60 seconds.
3. Check SCO up/down events and microphone-tap/feed observations. Verify phone, helper, and output routes agree; do not accept phone-speaker audio as LIVI downlink.
4. End the call. Confirm SCO closes and audio resources release. Repeat for an incoming call, then a third call without restarting the helper.
5. Repeat one call while AA or CarPlay is active where that deployment supports concurrent operation. Confirm call routing and normal media behavior return after hangup, against the known-good baseline.
6. During an ordinary call, disconnect phone Bluetooth; in Link runs separately unplug the Link. Restore transport and start a fresh call. Do not require the interrupted call to remain on LIVI; the phone may reroute it.
7. Inspect `/tmp/aa-sco.mic` permissions without changing them. Linux: `stat -c '%a %n' /tmp/aa-sco.mic`. macOS: `stat -f '%Lp %N' /tmp/aa-sco.mic`. Expect existing mode `666`.

**Expected:** Bidirectional intelligible audio through LIVI, no persistent one-way silence or new distortion/dropouts relative to baseline, orderly SCO closure, no stuck microphone after hangup, and a usable subsequent call after loss/recovery. Do not require a new codec, sample rate, or packet format.

**Evidence:** Common bundle, route screenshots, consenting participant confirmation for each direction, call durations, SCO/tap/feed events, permission output, and post-fault call result. Avoid recording call contents unless explicitly authorized.

**Pass/fail:** PASS only if both audio directions, all three call cycles, teardown, permissions, and applicable fault recovery work. FAIL on route-proven one-way audio, stuck audio after hangup, persistent new distortion, wrong permissions, or recovery requiring restart. BLOCKED if sink/mic routing or a consenting remote participant is unavailable; HFP-up alone never passes audio.

## Result sheet and release decision

Create a separate run record per build, phone, controller/Link variant, and topology. Keep this checklist's initial state distinct from actual run records.

| Case | L-local | L-link | M-none | M-link |
| --- | --- | --- | --- | --- |
| BT-01 Linux discovery/loss | NOT RUN | NOT RUN | N/A | N/A |
| BT-02 macOS discovery/control | N/A | N/A | NOT RUN | NOT RUN |
| BT-03 Reconnect without Wi-Fi | NOT RUN | NOT RUN | N/A | NOT RUN |
| BT-04 HFP without projection | NOT RUN | NOT RUN | N/A | NOT RUN |
| BT-05 Stale iAP recovery | NOT RUN | NOT RUN | N/A | NOT RUN |
| BT-06 BlueZ restart | NOT RUN | NOT RUN | N/A | N/A |
| BT-07 Link hot-plug | N/A | NOT RUN | N/A | NOT RUN |
| BT-08 Android Auto | NOT RUN | NOT RUN | NOT RUN (wired only, if supported) | NOT RUN |
| BT-09 CarPlay | NOT RUN | NOT RUN | NOT RUN (wired only, if supported) | NOT RUN |
| BT-10 HFP/SCO audio | NOT RUN | NOT RUN | N/A | NOT RUN |

For each result record fill:

| Field | Required entry |
| --- | --- |
| Run ID / case / subcase / topology | Unique identifier and precise applicability |
| Tester / date / build digest | Actual execution identity and candidate/baseline |
| Host / controller / Link / phone versions | Exact equipment and firmware |
| Preconditions | Confirmed, or blocking prerequisite |
| Actual numbered actions and times | Include any deviations |
| Expected versus observed | Separate signaling, ACL, profile, projection, and audio |
| Evidence references | Restricted raw bundle and redacted excerpts |
| Result | PASS / FAIL / BLOCKED / NOT RUN / justified N/A |
| Baseline comparison / issue | Candidate-only regression, shared limitation, or unresolved |
| Restoration | Target list, AP, phone routes, pairings, and services restored |

Release checklist:

- [ ] Every applicable required topology/model and mandatory subcase has actual evidence and a reviewer.
- [ ] BLOCKED/NOT RUN cases remain explicit open gates; no untested hardware is represented as passed.
- [ ] Candidate failures are compared against the known-good build without retroactively weakening criteria.
- [ ] No persistent duplicate sessions, task/retry loops, stale player binding, or stuck audio observed.
- [ ] After normal helper exit, selected/unselected adapter state and registrations are checked against the pre-run baseline; unexpected retained state is investigated.
- [ ] Compare explicitly authorized current protected-file hashes before and after the work. `utils/verify_bluetooth_boundaries.py` is hash-only and must never be executed. Current hash equality does not establish historical Stage 1–4 preservation, API equivalence, or deployment-wide preservation.
- [ ] Broader protected-surface/deployment comparison and rollback to the prior binary are reviewed separately using the existing deployment procedure.
- [ ] Test configuration and target lists are restored; no privileged API or application code changes were needed.

## Source grounding and limitations

Repository paths below are relative to the repository root. They describe the inspected implementation, not measured hardware outcomes.

| Source | Relevant behavior used by this checklist |
| --- | --- |
| `native/crates/livi-helperd/src/linux_main.rs`, `serve` | Independent startup before adapter wait; effective configuration; registration/health loop; wired arbitration and separately spawned sessions |
| `native/crates/livi-helperd/src/bluetooth_linux.rs`, `adapters`, `Owner`, `healthy` | Fixed versus Link adapter selection, stored ownership, three-second health timeout |
| `native/crates/livi-runtime/src/bluetooth/lifecycle.rs`, `supervise` | Withdrawal/release ordering and five-second retry wait |
| `native/crates/livi-runtime/src/bluetooth/policy.rs`, `Reconnect` | One-second intervals/timeouts, 15-failure threshold, 30-second retry, 10-second stale handling, backend differences |
| `native/crates/livi-runtime/src/reconnect.rs`, `run_with_wifi`, `paging_allowed` | Linux BlueZ observations; disconnected paging is Wi-Fi-gated, unlike the original BT-03 criterion |
| `native/crates/livi-helperd/src/bt_dongle.rs`, `page`, `poke`, `hand`, `run` | Dongle call/busy rules, blocking paging lifetime, address reversal, HFP/SCO dispatch |
| `native/crates/livi-helperd/src/mac_main.rs`, `run`, `start_bluetooth`, `wireless_sessions` | Selected Link backend, unavailable shared control, independent HFP offer, AP wait and CarPlay arbitration |
| `native/crates/livi-helperd/src/aa.rs`, `handshake`, `watch`, `watch_dongle` | Wired MAC/instance/serial checks before wireless AP offer; backend-specific AP/bootstrap wrappers |
| `native/crates/livi-runtime/src/state.rs`, `carkit_claims` | Known-phone and unidentified-wired-phone arbitration |
| `native/crates/livi-runtime/src/shared_sock.rs`; `livi_sock.rs` | Socket paths, existing verbs/errors, reconnect-target handling, accept/request snapshot distinctions |
| `native/crates/livi-runtime/src/hfp.rs`, `Hfp`; `sco.rs`, `call`, `mic_socket` | HFP link/call observations, configured sink, framed microphone input, SCO events, existing mic socket mode |
| `native/crates/livi-helperd/src/link.rs`; `native/crates/livi-link-host/src/link.rs` | Physical presence, network name resolution, `livi-link` selection value |
| `utils/verify_bluetooth_boundaries.py` | Historical boundary-guard reference only; hash-only, never executed in this audit |

Normal UI workflows, installed service names, supported phone/firmware combinations, and existing audio-route controls depend on the lab deployment and must be recorded before execution. No new debug endpoint, service installation, permission change, or production test hook is required or authorized by this checklist.
