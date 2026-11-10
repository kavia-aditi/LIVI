#!/usr/bin/env python3
"""Build/check main LIVI with Devices diagnostics. Open is a manual app launch, not a test."""
import hashlib
import json
from pathlib import Path
import signal
import subprocess
import sys
import tempfile

import livi_bluetooth_app_test as legacy

checks = legacy.checks
ROOT = Path(__file__).resolve().parents[1]
TARGET = legacy.TARGET
KIND = "livi-main-devices-development"
BINS = ("LIVI", "livi-core")
DENIED = legacy.DENIED | {"native-bt-dev", "isolated-helper", "isolated-development",
                          "isolated-native-projection"}
FORBIDDEN_EXE = {"livi-helperd", "livi-native-bt-dev", "livi-bt-diagnostic"}
require = legacy.require


def identity():
    sources = checks.source_identity()
    wrappers = "".join(checks.sha(ROOT / "utils" / name) for name in (
        "livi_devices_dev.py", "livi_bluetooth_app_test.py", "bluetooth_only_diagnostic.py"))
    return hashlib.sha256((sources + wrappers).encode()).hexdigest()


def command(output):
    return ["cargo", "build", "--locked", "--offline", "--target", TARGET,
            "--target-dir", str(output / "target"), "-p", "livi-ui", "-p", "livi-core",
            "--bins", "--no-default-features", "--features", "livi-ui/bluetooth-app-test",
            "--message-format=json-render-diagnostics"]


def verify_features(path):
    executables = set()
    paired = runner = app = finished = False
    with path.open() as stream:
        for line in stream:
            event = json.loads(line)
            if event.get("reason") == "build-finished":
                finished = event.get("success") is True
            if event.get("reason") != "compiler-artifact":
                continue
            name = event["target"]["name"].replace("_", "-")
            features = set(event.get("features", []))
            require(not features.intersection(DENIED))
            if event.get("executable"):
                require(name in BINS and name not in FORBIDDEN_EXE)
                executables.add(name)
            if name == "LIVI":
                app |= "bluetooth-app-test" in features
            if name == "livi-native-bluetooth":
                paired |= "paired-diagnostic" in features
            if name == "livi-bt-diagnostic":
                runner = True
    require(finished and executables == set(BINS) and app and paired and runner)


def prepare(output):
    legacy.diagnostic.host_check()
    checks.boundary_check()
    legacy.diagnostic.directory(output, fresh=True)
    sdk = checks.gstreamer_sdk()
    before = identity()
    output.mkdir(mode=0o700)
    env = checks.environment()
    env.update(CI="1", CARGO_INCREMENTAL="0", CARGO_PROFILE_DEV_DEBUG="0",
               CARGO_PROFILE_TEST_DEBUG="0", PKG_CONFIG_PATH=str(sdk / "lib/pkgconfig"))
    with (output / "build.events.jsonl").open("x") as events, \
            (output / "build.log").open("x") as errors:
        subprocess.run(command(output), cwd=ROOT / "native", env=env,
                       stdout=events, stderr=errors, check=True)
    require(identity() == before)
    verify_features(output / "build.events.jsonl")
    directory = output / "target" / TARGET / "debug"
    binaries = {name: checks.verify_image(directory / name) for name in BINS}
    links = {name: checks.dependency_closure(directory / name) for name in BINS}
    legacy.verify_metadata(directory / "LIVI")
    report = {"schema": 1, "kind": KIND, "source_identity": before,
              "binaries": binaries, "links": links, "hardware_executed": False,
              "gstreamer_sdk": str(sdk), "self_contained_app": False}
    with (output / "ready.json").open("x") as stream:
        json.dump(report, stream, sort_keys=True, indent=2)
    print("Prepared main LIVI + core; Devices diagnostics integrated. NOT launched or phone-validated.")


def validate(output):
    legacy.diagnostic.host_check()
    checks.boundary_check()
    legacy.diagnostic.directory(output)
    manifest = output / "ready.json"
    require(not manifest.is_symlink())
    report = json.loads(manifest.read_text())
    require(report["schema"] == 1 and report["kind"] == KIND)
    require(report["source_identity"] == identity())
    require(report["gstreamer_sdk"] == str(checks.gstreamer_sdk()))
    require(set(report["binaries"]) == set(BINS) and set(report["links"]) == set(BINS))
    verify_features(output / "build.events.jsonl")
    directory = output / "target" / TARGET / "debug"
    for name in BINS:
        require(checks.verify_image(directory / name) == report["binaries"][name])
        require(checks.dependency_closure(directory / name) == report["links"][name])
    legacy.verify_metadata(directory / "LIVI")
    return directory


def launch_environment(runtime):
    sdk = checks.gstreamer_sdk()
    env = checks.environment()
    # macOS core data and backup paths are HOME-relative, not XDG_CONFIG_HOME.
    # No existing settings, native-dev credentials, core socket or app is reused.
    env.update(HOME=str(runtime), XDG_RUNTIME_DIR=str(runtime),
               LIVI_CORE_SOCKET=str(runtime / "livi/core.sock"),
               LIVI_UI_PAGE="settings", LIVI_UI_SETTINGS="devices",
               LIVI_ROOT=str(ROOT), GST_PLUGIN_SYSTEM_PATH="",
               GST_PLUGIN_PATH=str(sdk / "lib/gstreamer-1.0"),
               GST_PLUGIN_SCANNER=str(sdk / "libexec/gstreamer-1.0/gst-plugin-scanner"),
               GST_REGISTRY=str(runtime / "gst-registry.bin"))
    return env


def open_app(output):
    binaries = validate(output)
    runtime = Path(tempfile.mkdtemp(prefix="livi-devices-", dir="/tmp"))
    child = None
    previous = signal.getsignal(signal.SIGTERM)

    def terminate(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    print("Manual main LIVI session; authorize/connect only on Devices. Runtime retained:", runtime)
    try:
        child = subprocess.Popen([str(binaries / "LIVI")], cwd=ROOT,
                                 env=launch_environment(runtime), stdin=subprocess.DEVNULL,
                                 start_new_session=True)
        # No automatic Bluetooth action. The in-app one-shot attempt is <=60s+2s cleanup.
        return child.wait(timeout=600)
    finally:
        try:
            if child is not None:
                # Main app gets SIGINT for its in-loop cleanup; then the owned group only.
                legacy.diagnostic.stop_owned(child)
        finally:
            signal.signal(signal.SIGTERM, previous)


def main():
    try:
        parser = legacy.diagnostic.Parser(description=__doc__)
        sub = parser.add_subparsers(dest="action", required=True,
                                   parser_class=legacy.diagnostic.Parser)
        for action in ("prepare", "check", "open"):
            item = sub.add_parser(action)
            item.add_argument("--output", type=Path, required=True)
        args = parser.parse_args()
        output = args.output.absolute()
        if args.action == "prepare":
            prepare(output)
        elif args.action == "check":
            validate(output)
            print("Main Devices artifact checks passed; no app or hardware launched.")
        else:
            return open_app(output)
    except KeyboardInterrupt:
        print("Cancelled; owned development process group stopped.", file=sys.stderr)
        return 130
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError):
        print("REFUSED/FAILED: main Devices development artifact; no fallback; files retained.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
