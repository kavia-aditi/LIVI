# LIVI demonstrations

- [Media routing](media-routing.html)
- [Real native Bluetooth development receiver](native-bluetooth-development.md)
- [Native Bluetooth connectivity simulation](#native-bluetooth-connectivity-simulation)

## Native Bluetooth connectivity simulation

### Status for Chris

The existing native Bluetooth decoupling architecture is unchanged. A dedicated
Slint connectivity demonstration now runs in the existing `LIVI` binary using a
UI-only model, separate from projection `DeviceStatus`.

The native demonstration completed 20 checked UI steps without a crash and
produced 20 screenshots. The model suite passed 98 tests, Bluetooth-related
runtime unit tests passed 30, and the private-D-Bus integration suites passed
15, including all selected ignored tests. These are bounded test results, not
a guarantee of crash-free operation in every environment.

**Simulation — no Bluetooth connection established**

No real Bluetooth pairing, radio connection, HFP audio, or wireless projection
was demonstrated. No Android application or Android Emulator validation is
claimed.

### Launch and demonstrate

From the repository root, using the project's existing Rust/native build
environment:

```sh
cd native
cargo build -p livi-ui --bin LIVI
./target/debug/LIVI --bluetooth-demo
```

This flag takes an early branch before normal bundle, runtime-directory,
video-socket, core-client or helper startup. It does not connect to the running
core or helper. The normal application entry point and Settings → Devices
projection view are unchanged.

1. Select **Open Bluetooth connectivity**: Disconnected.
2. Select **Simulate connect**: Connecting.
3. Select **Complete simulated attempt**: Connected (simulation only).
4. Select **Simulate connection loss**: Connection lost.
5. Select **Simulate reconnect**: Reconnecting.
6. Complete the attempt, then select **Simulate disconnect**.
7. Lose the demo service while connected or connecting. Connection actions are
   disabled; **Recover demo service** returns to Disconnected.
8. Go **Back / cancel attempt** while connecting, reopen the view and verify
   Disconnected. Navigation cancels the simulated attempt.
9. Close the demo window. No real helper was started.

The simulation advances through explicit controls, not real asynchronous
Bluetooth operations. Production lifecycle behavior is covered separately by
the existing runtime tests.

### Automated capture

From `native`:

```sh
./target/debug/LIVI --bluetooth-demo \
  --capture-dir ../temp-attachments/bluetooth-demo-captures
```

The native window drives the same callbacks as the controls, checks model and
Slint properties, captures each rendered step, and exits after completion.
A failed state assertion, failed capture, or early close returns an error.
The capture timer is stopped and the simulated attempt cleared on exit.

This run exercised disconnect, connection loss, reconnect, service loss while
connected and while connecting, blocked connection attempts while unavailable,
service recovery, repeated transitions, and navigation cleanup.

Selected screenshots from the completed run:

| State/scenario | Screenshot |
| --- | --- |
| Disconnected | [00-disconnected.png](../temp-attachments/bluetooth-demo-captures/00-disconnected.png) |
| Connecting | [01-connecting.png](../temp-attachments/bluetooth-demo-captures/01-connecting.png) |
| Connected — simulated | [02-connected-simulated.png](../temp-attachments/bluetooth-demo-captures/02-connected-simulated.png) |
| Connection lost | [03-connection-lost.png](../temp-attachments/bluetooth-demo-captures/03-connection-lost.png) |
| Reconnecting | [04-reconnecting.png](../temp-attachments/bluetooth-demo-captures/04-reconnecting.png) |
| Service unavailable | [06-service-unavailable.png](../temp-attachments/bluetooth-demo-captures/06-service-unavailable.png) |
| Navigation cleanup | [10-navigation-cleanup.png](../temp-attachments/bluetooth-demo-captures/10-navigation-cleanup.png) |

These are generated local artifacts in `temp-attachments`, not packaged app
assets. Local macOS Vision OCR decoded all 20 PNGs and recognized the simulation
notice; direct image preview through the chat tool could not resolve host paths.
Automation invoked UI callbacks, not physical mouse/keyboard input.

### Tests and evidence

From `native`:

```sh
cargo test -p livi-ui-model -- --test-threads=1
cargo test -p livi-runtime --lib bluetooth -- --test-threads=1
dbus-run-session --config-file=crates/livi-runtime/tests/dbus-session.conf -- \
  cargo test -p livi-runtime \
  --test reconnect --test shared_sock --test livi_sock --test bluetooth_devices \
  -- --include-ignored --test-threads=1
```

From the repository root:

```sh
python3 utils/verify_bluetooth_boundaries.py
```

The D-Bus command launches a disposable private session with fake BlueZ objects.
Do not substitute the host/system bus or launch the real helper for simulation.

| Check | Result |
| --- | --- |
| Native `livi-ui` build | Passed |
| Rust formatting for new model/controller | Passed |
| Full UI-model suite | 98 passed, including 4 new demo tests |
| Repeated demo state-machine test | 100 connect/loss/reconnect/disconnect cycles |
| Runtime Bluetooth-related unit tests | 30 passed |
| Private-D-Bus paired-device contracts | 1 passed |
| Private-D-Bus control/session sockets | 5 passed |
| Private-D-Bus reconnect behavior | 4 passed |
| Private-D-Bus shared socket behavior | 5 passed |
| Actual native Slint capture run | 20 checked steps; cleanup logged |
| Native PNG decoding / simulation-notice OCR | 20 passed |
| Frozen Bluetooth boundary verification | Passed |

Logs in the repository's `temp-attachments` directory:

- [Build](../temp-attachments/bluetooth-demo-build.log)
- [Native demo and cleanup](../temp-attachments/bluetooth-demo-run.log)
- [UI-model tests](../temp-attachments/bluetooth-ui-model-tests.log)
- [Runtime Bluetooth tests](../temp-attachments/bluetooth-runtime-tests.log)
- [Private-D-Bus tests](../temp-attachments/bluetooth-private-dbus-tests.log)
- [Screenshot OCR](../temp-attachments/bluetooth-demo-screenshot-verification.log)

The USB/AOA source checksum and both macOS entitlement checksums match the
protected baselines. Privileged APIs, runtime Bluetooth architecture, transports,
HFP, packaging, and installed app/helper were not changed.

### Additional work for an actual Android target

There is no verified Android application manifest, Gradle application or APK
entry point in the inspected project. This native demonstration is not an
Android app.

A real Android target would require a Slint Android application/activity and
build configuration, Android-compatible dependency selection, and lifecycle,
navigation and rendering integration. The desktop Unix-socket/video/core/helper
assumptions would need a target-specific composition rather than silently
starting desktop services. Reuse of this pure presentation model does not prove
that the rest of the native stack builds or runs on Android.

Emulator UI/lifecycle checks would remain separate from Bluetooth hardware
validation. Real pairing, adapter disappearance, profiles/HFP and wireless
projection require suitable physical hardware and a separately agreed test
matrix. Any Android permissions or privileged-boundary changes require a
separate scope; none are introduced here.

### Source map

- [Presentation model and tests](../native/crates/livi-ui-model/src/bluetooth_demo.rs)
- [Slint view](../native/crates/livi-ui/ui/bluetooth-demo.slint)
- [Demo controller and capture sequence](../native/crates/livi-ui/src/bluetooth_demo.rs)
- [Early launch branch](../native/crates/livi-ui/src/main.rs)
