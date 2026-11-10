# Private isolated phone-test configuration

Run the wizard locally in your own Terminal. **Do not send credentials to chat.**
This utility neither launches LIVI/helper processes nor performs Bluetooth,
network discovery, AP provisioning or hardware authorization.

**Projection testing is blocked:** phone IDs can now be learned on the current
WPP stream, but there is no verified Bluetooth-to-TCP identity proof. Native TCP
admission refuses with `IdentityUnverified`; configuration setup does not enable
it. See [the bootstrap investigation](README-native-phone-identity.md).

## Before starting

Have the actual values available locally:

- Current AP SSID and WPA2-Personal password.
- AP BSSID and channel from its existing administration page or network records.
  Do not substitute the Mac's Wi-Fi MAC address for the AP BSSID.
- Mac's assigned private IPv4 address, visible in its network settings. This is
  **not** the router/gateway address.
- Already-paired OnePlus phone's Bluetooth MAC from its existing pairing/device
  records. Do not substitute a Wi-Fi address or BLE random address.

You do **not** need a LIVI Link dongle, receiver serial, or advance knowledge of
the phone's `instance_id` and `serial`. Those two fields come from the phone's
Android Auto WPP version response. The wizard omits both; it never generates them.

If a value is unknown, stop and obtain it locally from the device/network owner.
Do not invent values, scan radios, inspect secret configuration files through
chat tools, or paste a password into a shell command.

The schema fixes `security` to `wpa2` and `port` to 5277. The six prompted
phone/network fields must be supplied by the operator. Accepted channels are
1–13, 36/40/44/48 and 149/153/157/161/165. The password is 8–63 printable ASCII
characters; the SSID is 1–32 UTF-8 bytes without control characters.

For compatibility, an existing configuration may include **both** phone identity
pins (`instance_id` and `serial`), each 1–128 ASCII graphic bytes without spaces.
A lone pin, explicit null, empty value, or placeholder is not a discovery mode.
Pins constrain the WPP response but are not authentication or receiver consent.

## 1. Create locally

From the LIVI repository root, in a private Terminal session:

    python3 -B utils/setup_phone_test_config.py create

Every input is hidden, including the password and confirmation. There are no
credential command-line options, environment-variable inputs for values, or
generated defaults for phone/network values. There are no identity prompts.

The wizard uses `$HOME` to select exactly:

    $HOME/LIVI-private/private-config.json

It creates the directory with permissions **0700** and file with permissions
**0600**, then validates the saved file locally. Existing files are never
overwritten. Existing incorrect permissions are rejected, not silently repaired.
Do not use `sudo`, shell tracing, terminal session recording or screen sharing.

On success, output contains only fixed PASS checks. On failure, output contains
a fixed FAIL message, never input values or raw exception diagnostics.

## 2. Validate again without displaying contents

    python3 -B utils/setup_phone_test_config.py validate

This checks:

- No symlinks in the home path, private directory or config file.
- Current-user ownership, exact directory/file permissions, and a regular
  single-link file.
- At most 2048 bytes, valid UTF-8 JSON, no duplicate or unknown fields.
- Types and formats matching the existing Rust handoff configuration schema.
- RFC1918 Mac IPv4 formatting and allowed channels, MAC addresses and identifiers.

Validation does **not** prove that an IP is currently assigned, credentials are
correct, the phone is paired, or a TCP peer is the selected Bluetooth phone.
Phone labels learned during WPP are not proof of those facts. No hardware is
contacted by this utility.

Do not use `cat`, shell debug output, chat attachments or version control to
inspect/share this file. It remains outside the checkout in your home directory.

## Refusal or interruption

An empty private directory or a newly created partial file can remain after an
interruption or write failure. Nothing is silently deleted or overwritten.
Validation rejects a partial file. If a path already exists, stop and review it
locally; the wizard provides no overwrite, recursive cleanup or permission-repair
option.

Python cannot guarantee erasure of secrets from process memory, nor protect
against a compromised account, debugger, terminal recorder or external backup.
Run only in a trusted local session. The utility itself does not log, print or
transmit collected values.

## Offline developer tests

Synthetic fixtures only:

    python3 -B -m unittest discover -s utils -p test_setup_phone_test_config.py -v

The wizard itself is separate from prepared test binaries. The identity-bootstrap
Rust changes require a fresh preparation in a new output directory once the
authentication blocker is resolved. Existing prepared outputs must not be edited
or reused as current binaries. Ordinary hardware gates and elevated privileged
APIs remain unchanged.
