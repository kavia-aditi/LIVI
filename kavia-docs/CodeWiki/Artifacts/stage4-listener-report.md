[CodeWiki](../index.md) / [Artifacts](index.md)

# Stage 4: Existing TCP Listener Integration

## Audit status and execution restrictions

**Historical record, not current validation.** The date, implementation descriptions, suite totals, environment observations, and snapshot below are retained as previously reported claims. This audit did not reproduce them. Historical Stage 1–4 preservation remains **UNVERIFIED**: there is no valid historical baseline, and none is reconstructed here. “Verified,” “passed,” and “current” in the retained record refer to that report's claimed context, not this audit.

Static source review confirms that `NativeTcp::production` has a false production authorization gate, real `Permit::wpp_complete` rejects with `IdentityUnverified`, UI `production_gates` keeps hardware authorization false, and `run_with_context` selects one listener implementation. See [helper gate and identity rejection](../../../native/crates/livi-helperd/src/native_tcp.rs), [UI gate](../../../native/crates/livi-ui/src/native_bluetooth.rs), and [listener selection](../../../native/crates/livi-aa/src/server.rs). These are source observations, not execution results.

Standing restrictions prohibit tests, builds, formatters, app/browser launches, sockets, connections, hardware operations, and restores. The boundary verifier is **hash-only; never execute it**. R0 remains NO-GO; no authentication/TLS/WPP, authorization-policy, elevated API, or Stage 5/6 changes are authorized. Paired inventory remains UI-only, never `Registry::note_device`; HFP is not a proven prerequisite.

## Outcome and scope

The original report states that software validation completed on macOS ARM64 on 2026-11-05; that date and completion claim are not independently verified. It covers the requested native wireless Android Auto handoff integration with the existing LIVI TCP listener, not similarly numbered phases in other architecture proposals.

The current implementation reuses the existing production port 5277 listener and existing Android Auto session runner. Production native hardware authorization remains disabled in both helper and UI. LIVI was not launched, no hardware connections were initiated, and Stage 5 was not started.

No cleanup or deletion of old build directories was performed during this continuation.

## Verified implementation

Paths in this section are relative to the repository root.

| Source and entry point | Verified behavior and evidence |
| --- | --- |
| `native/crates/livi-helperd/src/mac_main.rs`, `start_android_auto_network` | Starts `livi_aa::server::run_with_context(...)` with the shared control. The normal `wireless_android_auto()` startup condition remains in place. |
| `native/crates/livi-helperd/src/native_tcp.rs`, `NativeTcp::production` | Uses `Control::shared(runtime, ...)` with a passive dongle-facing address lookup. Creates no TCP listener. `const NATIVE_AUTHORIZED: bool = false;` independently prevents native authorization. |
| `native/crates/livi-aa/src/server.rs`, `run_with_context` | Selects the existing legacy implementation or qualified admission implementation for one owner; it does not start both listeners. |
| `native/crates/livi-aa/src/server/admission.rs`, `run_qualified` and `bind_once` | Requires `crate::consts::TCP_PORT`, binds once, and disables reuse-address/reuse-port. Conflicts fail closed without replacing another listener. |
| `native/crates/livi-aa/src/server/admission.rs`, `Control::dispatch` | Calls `self.tick(now)` before routing. Only an explicit dongle destination returns `Ok(None)`; failed native admission does not fall back to the legacy callback. |
| `native/crates/livi-aa/src/server/admission.rs`, `serve_qualified` | Reuses `crate::session::run(...)`. Native ownership has one reservation; the previous unintended eight-session legacy cap is absent. Socket tuning remains best effort for legacy sessions. |
| `native/crates/livi-aa/src/server/admission.rs`, `Lease`, `SessionLease`, and `SessionOwner` | Transfers active native ownership once, scopes cancellation to the exact attempt, and checks socket device/inode before unlinking owned session endpoints. |
| `native/crates/livi-helperd/src/mac_main.rs`, `native_android_auto_network_event` | Carries runtime, attempt, and generation in the native event. Invalid native context is rejected rather than downgraded to a legacy event. |
| `native/crates/livi-helperd/src/native_handoff.rs`, `serve_channel` | Connects private authorization/configuration/owned-arrival IPC to listener reservation, WPP, progress, cancellation, and session ownership. USB arbitration remains outside the protected USB implementation. |
| `native/crates/livi-helperd/src/native_tcp.rs`, `Permit::wpp_complete` | Real listener permits return `Failure::IdentityUnverified`. WPP labels, optional pins, and IP admission are not authenticated cross-transport identity proof. |
| `native/crates/livi-ui/src/native_bluetooth.rs`, `production_gates` | Keeps `hardware_authorized: false`, including when configuration and helper readiness are present. |

### Continuation changes

The previously proposed listener fixes and regression tests were already present on disk when Stage 4 resumed. They were inspected and tested rather than re-applied.

This continuation adds one regression in `native/crates/livi-aa/src/server/admission_tests.rs`:

`dongle_dispatch_preserves_active_native_ownership_after_pending_deadlines`

It verifies that dongle-driven deadline checks do not expire an active native session, a new native reservation remains busy until the active owner is released, and cancellation remains scoped to that native owner.

The report and its navigation pages are also added. No production hardware gate or protected subsystem source was changed in this continuation.

## Historically reported test results — not rerun

The original report describes these as distinct test cases, not cumulative counts across repeated runs. Counts are retained for provenance only, not as current pass evidence.

| Rust suite | Passed |
| --- | ---: |
| Android Auto, listener admission, WPP, framing, TLS, and USB policy | 59 |
| Helper, private handoff composition, authorization, and native event serialization | 37 |
| Native handoff configuration, IPC, and router | 42 |
| Native Bluetooth | 122 |
| Core protocol | 49 |
| Runtime unit and integration cases, including all private-bus cases | 110 |
| UI model | 106 |
| Isolated-helper development composition | 27 |
| Core | 322 |
| UI test harness, ordinary configuration | 19 |
| Bluetooth host unit and synthetic-phone integration | 26 |
| Bluetooth dongle daemon library | 4 |
| HFP library | 5 |
| **Total Rust cases passed** | **928** |
| **Python safety/build-wrapper cases passed** | **98** |

The main focused Rust run passed 543 cases and reported nine ignored cases. All nine were subsequently run and passed on a test-owned private D-Bus with fake BlueZ services, serially. None of those nine remains unexecuted in this validation scope.

The private-bus cases cover profile teardown and daemon-owner replacement, paired inventory contracts, projection-socket acceptance snapshots, four reconnect cases, and two shared-socket availability/command cases. They do not use a real controller.

Documentation-test targets completed with zero cases. The production `cargo check` for helper/core/UI passed without enabling isolated/offline test features. Formatting validation passed for the edited admission test file.

### Important Stage 4 cases exercised

- One shared listener routes native and dongle sessions, including scoped native cancellation while legacy sessions survive.
- Rejected native traffic never becomes legacy traffic.
- More than eight simultaneous legacy sessions remain accepted.
- WPP and waiting-TCP deadlines expire even during dongle dispatch.
- Active native ownership survives ticks beyond pending-handshake deadlines.
- Native rejection limits do not block the shared dongle route.
- Readiness precedes WPP/TCP admission; wrong interface, subnet, address, port, and conflicting binds fail closed.
- Existing session endpoints are not replaced; owned cancellation removes only owned endpoints.
- Authorization is single-use, expiring, generation/runtime bound, and independent of the production hardware gate.
- Real helper/router WPP fixtures reject unauthenticated TCP identity, including matching labels or optional pins.
- USB precedence, stale arrivals, EOF, helper loss, and listener loss remain covered.
- Ordinary UI polling and configuration cannot start hardware.

## Reproduction commands

Executable reproduction recipes have been retired from this historical report because they conflict with the current restrictions. The original record describes Rust suites, an ignored-case private D-Bus run, production compilation checks, formatting validation, Python tests, and a boundary-verifier run. None was executed during this audit. Old SDK paths and environment workarounds below are historical observations, not validated setup instructions.

Any future execution requires separate explicit permission and a reviewed procedure. The verifier remains hash-only and must never be executed under the standing restrictions.

### Environment issue resolved

The initial core/UI run passed all 322 core cases, but the UI harness aborted before its tests because dyld could not locate `@rpath/libgstapp-1.0.0.dylib`. The staged compatible SDK was already present. Supplying its `lib` directory through `DYLD_LIBRARY_PATH` resolved the loader issue; the rerun passed all 322 core and 19 UI cases.

No dependencies were installed, binary load commands changed, or application bundle launched. Disk remained approximately 26 GiB free at final validation.

## Protected boundaries

The original report states that `utils/verify_bluetooth_boundaries.py` passed; this is not reproduced or endorsed as an execution instruction:

- Nine frozen privilege/transport/HFP files match the Phase 0 snapshot.
- CLI is unchanged except for the permitted Linux module declaration.
- Pure policy has no I/O/implicit clock, and the supervisor has no backend/projection dependency.

The earlier 129-file protected-subsystem snapshot also matched after tests:

`7231833880c1f4bafd7f221b196ada64fbbbd9cb06d057635d0f60b9f65fedba`

The snapshot includes all regular files under `livi-cp`, `livi-hfp`, `livi-link-host`, `livi-btd`, `iap2-csm`, `iap2-mfi`, `iap2-link`, `iap2-usbmux`, and `iap2-wired`, plus `livi-aa/src/usb*` regular files and recursive `livi-aa/src/usb/` files. Paths are sorted within the recorded groups, and each project-relative path string followed by its file bytes is fed into SHA-256.

This is a historical snapshot assertion, not currently verified preservation evidence or measured real-hardware behavior. A hash string without an independently valid baseline cannot establish historical preservation.

## Limits and stopping point

The original report claims that Stage 4 software integration and the listed regressions passed. This audit does not certify that outcome. Static review confirms dormant production gates and real native handoff rejection at `IdentityUnverified`; it does not establish runtime correctness or historical preservation.

No hardware acceptance, real phone projection, AP provisioning, live USB/CarPlay/HFP interaction, Linux-native build, or Linux hardware test was performed. Those are not reported as passing. macOS compilation cannot establish Linux-gated behavior.

Stage 5 was not started.
