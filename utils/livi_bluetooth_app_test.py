#!/usr/bin/env python3
"""Prepare/check the real LIVI Bluetooth test view; run requires explicit hardware consent."""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import signal
import struct
import subprocess
import sys

import bluetooth_only_diagnostic as diagnostic

checks = diagnostic.checks
ROOT = Path(__file__).resolve().parents[1]
TARGET = diagnostic.TARGET
KIND = "livi-app-bluetooth-only"
FEATURE = "bluetooth-app-test"
DENIED = diagnostic.DENIED_FEATURES | {"bluetooth-diagnostic"}
FORBIDDEN = {"livi-core", "livi-helperd", "livi-native-bt-dev", "livi-aa", "livi-aa-stack"}


def require(value):
    if not value:
        raise ValueError("LIVI Bluetooth app test refused")


def identity():
    return hashlib.sha256((diagnostic.source_identity() + checks.sha(Path(__file__))).encode()).hexdigest()


def command(output):
    return ["cargo", "build", "--locked", "--offline", "--target", TARGET,
            "--target-dir", str(output / "target"), "-p", "livi-ui",
            "--bin", "LIVI", "--no-default-features", "--features", FEATURE,
            "--message-format=json-render-diagnostics"]


def verify_features(path):
    app = paired = runner = finished = False
    for line in path.read_text().splitlines():
        event = json.loads(line)
        if event.get("reason") == "build-finished":
            finished = event.get("success") is True
        if event.get("reason") != "compiler-artifact":
            continue
        name = event["target"]["name"].replace("_", "-")
        features = set(event.get("features", []))
        require(name not in FORBIDDEN and not features.intersection(DENIED))
        if event.get("executable"):
            require(name == "LIVI")
            require(FEATURE in features)
            app = True
        if name == "livi-native-bluetooth":
            paired |= "paired-diagnostic" in features
        if name == "livi-bt-diagnostic":
            runner = True
    require(app and paired and runner and finished)


def verify_metadata(binary):
    """Read a bounded ARM64 Mach-O section; never execute the image."""
    with binary.open("rb") as stream:
        total = os.fstat(stream.fileno()).st_size
        header = stream.read(32)
        require(len(header) == 32)
        magic, cpu, _, kind, count, size, _, _ = struct.unpack("<8I", header)
        require(magic == 0xFEEDFACF and cpu == 0x0100000C and kind == 2)
        require(0 < count <= 4096 and 8 * count <= size <= min(total - 32, 16 * 1024 * 1024))
        commands = stream.read(size)
        require(len(commands) == size)
        cursor = 0
        section = None
        for _ in range(count):
            require(cursor + 8 <= size)
            command, length = struct.unpack_from("<2I", commands, cursor)
            require(length >= 8 and length % 8 == 0 and cursor + length <= size)
            if command == 0x19:
                require(length >= 72)
                sections = struct.unpack_from("<I", commands, cursor + 64)[0]
                require(72 + sections * 80 <= length)
                for index in range(sections):
                    start = cursor + 72 + index * 80
                    name, segment, _, amount, offset = struct.unpack_from("<16s16sQQI", commands, start)
                    if name.rstrip(b"\0") == b"__info_plist" and segment.rstrip(b"\0") == b"__TEXT":
                        require(section is None and 0 < amount <= 65536)
                        require(offset >= 32 + size and offset + amount <= total)
                        section = (offset, amount)
            cursor += length
        require(cursor == size and section is not None)
        stream.seek(section[0])
        try:
            info = plistlib.loads(stream.read(section[1]))
        except Exception as error:
            raise ValueError("LIVI Bluetooth app metadata refused") from error
        require(isinstance(info, dict))
        require(info.get("CFBundleIdentifier") == "dev.f-io.livi.native-validation")
        description = info.get("NSBluetoothAlwaysUsageDescription")
        require(isinstance(description, str) and bool(description.strip()))


def prepare(output):
    diagnostic.host_check()
    checks.boundary_check()
    diagnostic.directory(output, fresh=True)
    sdk = checks.gstreamer_sdk()
    before = identity()
    output.mkdir(mode=0o700)
    env = checks.environment()
    env.update(CI="1", CARGO_INCREMENTAL="0", PKG_CONFIG_PATH=str(sdk / "lib/pkgconfig"))
    with (output / "build.events.jsonl").open("x") as events, (output / "build.log").open("x") as errors:
        subprocess.run(command(output), cwd=ROOT / "native", env=env,
                       stdout=events, stderr=errors, check=True)
    require(identity() == before)
    verify_features(output / "build.events.jsonl")
    binary = output / "target" / TARGET / "debug" / "LIVI"
    report = {"schema": 1, "kind": KIND, "source_identity": before,
              "binary_sha256": checks.verify_image(binary),
              "links": checks.dependency_closure(binary), "hardware_executed": False}
    verify_metadata(binary)
    with (output / "ready.json").open("x") as stream:
        json.dump(report, stream, sort_keys=True, indent=2)
    print("Prepared real LIVI Bluetooth test view; app NOT launched; hardware NOT tested.")


def validate(output):
    diagnostic.host_check()
    checks.boundary_check()
    diagnostic.directory(output)
    manifest = output / "ready.json"
    require(not manifest.is_symlink())
    report = json.loads(manifest.read_text())
    require(report["schema"] == 1 and report["kind"] == KIND)
    require(report["source_identity"] == identity())
    verify_features(output / "build.events.jsonl")
    binary = output / "target" / TARGET / "debug" / "LIVI"
    require(checks.verify_image(binary) == report["binary_sha256"])
    require(checks.dependency_closure(binary) == report["links"])
    verify_metadata(binary)
    return binary


def address_valid(text):
    if not isinstance(text, str) or len(text) != 17:
        return False
    parts = text.split(":")
    if len(parts) != 6 or any(len(part) != 2 or any(c not in "0123456789abcdefABCDEF"
                                                  for c in part) for part in parts):
        return False
    value = bytes(int(part, 16) for part in parts)
    return value not in (bytes(6), bytes([255] * 6)) and not value[0] & 1


def run(args):
    configure = getattr(args, "action", None) == "open"
    if not configure:
        require(args.authorize_bluetooth_hardware and args.paired_phone_only)
        require(address_valid(args.phone_address))
    binary = validate(args.output)
    launch = [str(binary), "--bluetooth-only-test"]
    if not configure:
        launch += ["--authorize-bluetooth-hardware", "--paired-phone-only",
                   "--phone-address", args.phone_address]
    child = None
    previous = signal.getsignal(signal.SIGTERM)

    def terminate(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    try:
        child = subprocess.Popen(
            launch,
            cwd=ROOT, env=checks.environment(), stdin=subprocess.DEVNULL,
            start_new_session=True)
        # Up to 60 seconds to click, then 60 seconds for one attempt + 2 seconds cleanup.
        return child.wait(timeout=125)
    finally:
        try:
            if child is not None:
                diagnostic.stop_owned(child)
        finally:
            signal.signal(signal.SIGTERM, previous)


def main():
    try:
        parser = diagnostic.Parser(description=__doc__)
        commands = parser.add_subparsers(dest="action", required=True, parser_class=diagnostic.Parser)
        for action in ("prepare", "check", "open", "run"):
            sub = commands.add_parser(action)
            sub.add_argument("--output", type=Path, required=True)
            if action == "run":
                sub.add_argument("--phone-address", required=True)
                sub.add_argument("--authorize-bluetooth-hardware", action="store_true")
                sub.add_argument("--paired-phone-only", action="store_true")
        args = parser.parse_args()
        args.output = args.output.absolute()
        if args.action == "prepare":
            prepare(args.output)
        elif args.action == "check":
            validate(args.output)
            print("LIVI Bluetooth app artifact checks passed; no hardware accessed.")
        else:
            return run(args)
    except KeyboardInterrupt:
        print("Cancelled; only the owned test app was signalled.", file=sys.stderr)
        return 130
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError):
        print("REFUSED/FAILED: LIVI Bluetooth app test; no fallback; artifacts retained.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
