# Main LIVI Devices development build

This development feature adds paired-phone diagnostics to the existing main
LIVI Settings → Devices screen. It uses the normal window, navigation, settings
rows, and native Bluetooth backend. Normal testing does not require the legacy
Bluetooth-only window or its command-line mode.

## Prepare and check without launching

Run from the repository root:

```sh
python3 -B utils/livi_devices_dev.py prepare --output temp-attachments/livi-main-devices-20261105-02
python3 -B utils/livi_devices_dev.py check --output temp-attachments/livi-main-devices-20261105-02
```

Preparation requires a new direct child of `temp-attachments`; it never overwrites
an existing output directory. If this output already exists, check it rather
than preparing over it. For later rebuilds, choose a new unused output name.

The wrapper builds actual main LIVI and a fresh sibling core, validates their
source identity and binary metadata, and checks frozen Bluetooth boundaries.
Preparation and checking do not launch the app or access Bluetooth hardware.
Old apps, DMGs, backups, and development artifacts are preserved.

This is a source-tree development build, not a self-contained application bundle.
The public GStreamer SDK remains an external dependency.

## Optional manual main-app session

Only run this command when the operator chooses to launch the app:

```sh
python3 -B utils/livi_devices_dev.py open --output temp-attachments/livi-main-devices-20261105-02
```

A request for launch instructions is not authorization for an agent to execute
them. The wrapper validates the artifact before launch and uses a fresh HOME and
runtime directory rather than existing private configuration or an installed
app. Actual main LIVI starts its newly built sibling core. No fixture state or
hardware-authorization command-line flags are injected.

In Settings → Devices:

1. Select the intended already-paired phone from the local macOS inventory.
2. Choose **Authorize selected paired phone**.
3. Choose **Connect** within the 60-second, peer-scoped consent window.
4. Read the stage rows, result, and native diagnostic log inside Devices.
5. Use **Cancel**, leave Devices, or close the window to request cleanup.

Selection alone does not connect. Changing selection, refreshing inventory, or
leaving Devices revokes consent. There is no automatic connection or retry.
A cleanup failure blocks another attempt. Normal navigation remains available.

## Interpret the stages honestly

- **Discovery:** not performed by this passive diagnostic. Paired inventory is
  only a local selection source, not evidence of discovery, RFCOMM, or projection.
- **RFCOMM:** waits for an incoming channel from the selected, already-bonded
  peer. It does not pair, query remote SDP, or initiate an outgoing channel.
  A timeout without an incoming callback leaves phone activation unknown.
- **WPP:** runs only after an owned stream is accepted. The bounded exchange
  sends one version request for version 6.0 and validates the response format.
  A valid response is not authenticated identity or projection success.
- **TCP:** blocked/not attempted by this diagnostic. It does not provision an
  access point, negotiate endpoints, start TCP/TLS, or bypass authentication.
  The existing `IdentityUnverified` failure remains in force.

Native stages and errors are retained in the Devices view. The diagnostic does
not turn paired inventory into core registry entries, paired connection controls,
or reconnect state.

## Safety and verification limits

Production hardware gates remain false. The nine frozen privilege/transport/HFP
files and immutable boundary verifier must remain unchanged. No elevated API,
permission, authentication, or security-policy change is required.

Hardware-free tests cover controller actions, consent, cancellation, native
fakes, and headless navigation. Static artifact validation does not establish
real macOS window behavior, incoming phone activation, an actual WPP exchange,
or authenticated TCP/projection. Those require separately authorized observation.
