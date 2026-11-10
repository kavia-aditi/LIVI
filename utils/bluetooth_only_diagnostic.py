#!/usr/bin/env python3
"""Default-off Bluetooth-only diagnostic. Preparation never executes the binary."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import sys
import time

import native_phone_test as checks

ROOT = Path(__file__).resolve().parents[1]
NAME = "livi-bt-diagnostic"
TARGET = "aarch64-apple-darwin"
FORBIDDEN = {
    "livi-aa", "livi-aa-stack", "livi-core", "LIVI", "livi-ui",
    "livi-gst-host", "livi-helperd", "livi-native-bt-dev", "livi-native-handoff",
    "livi-runtime", "livi-hfp", "livi-cp", "gstreamer", "rustls",
}
DENIED_FEATURES = {
    "isolated-helper", "development-receiver", "offline-admission-test-support",
    "isolated-native-projection", "native-bt-dev",
}


def require(ok):
    if not ok:
        raise ValueError("Bluetooth-only diagnostic refused")


def host_check():
    require(platform.system() == "Darwin" and platform.machine() == "arm64")
    require(os.geteuid() != 0)


def source_identity():
    data = checks.source_identity() + checks.sha(Path(__file__))
    return hashlib.sha256(data.encode()).hexdigest()


def directory(output, fresh=False):
    parent = ROOT / "temp-attachments"
    require(parent.is_dir() and not parent.is_symlink())
    require(output.parent == parent and not output.is_symlink())
    if fresh:
        require(not os.path.lexists(output))
    else:
        require(output.is_dir() and output.stat().st_uid == os.geteuid()
                and output.stat().st_mode & 0o777 == 0o700)
    return output


def command(output):
    return ["cargo", "build", "--locked", "--offline", "--target", TARGET,
            "--target-dir", str(output / "target"), "-p", NAME,
            "--no-default-features", "--features", "bluetooth-diagnostic",
            "--message-format=json-render-diagnostics"]


def verify_features(path):
    executable = False
    paired = False
    finished = False
    for line in path.read_text().splitlines():
        event = json.loads(line)
        if event.get("reason") == "build-finished":
            finished = event.get("success") is True
        if event.get("reason") != "compiler-artifact":
            continue
        name = event["target"]["name"].replace("_", "-")
        features = set(event.get("features", []))
        require(name not in FORBIDDEN and not features.intersection(DENIED_FEATURES))
        if name == NAME and event.get("executable"):
            require("bluetooth-diagnostic" in features)
            executable = True
        if name == "livi-native-bluetooth":
            paired |= "paired-diagnostic" in features
    require(finished and executable and paired)


def prepare(output):
    host_check()
    checks.boundary_check()
    directory(output, fresh=True)
    before = source_identity()
    output.mkdir(mode=0o700)
    env = checks.environment()
    env.update(CI="1", CARGO_INCREMENTAL="0")
    with (output / "build.events.jsonl").open("x") as events, (output / "build.log").open("x") as errors:
        subprocess.run(command(output), cwd=ROOT / "native", env=env,
                       stdout=events, stderr=errors, check=True)
    require(source_identity() == before)
    verify_features(output / "build.events.jsonl")
    binary = output / "target" / TARGET / "debug" / NAME
    binary_hash = checks.verify_image(binary)
    links = checks.dependency_closure(binary)
    report = {"schema": 1, "kind": "bluetooth-only-diagnostic",
              "source_identity": before, "binary_sha256": binary_hash,
              "links": links, "hardware_executed": False}
    with (output / "ready.json").open("x") as stream:
        json.dump(report, stream, indent=2, sort_keys=True)
    print("Prepared Bluetooth-only software; hardware NOT tested. No existing artifact replaced.")


def validate(output):
    host_check()
    checks.boundary_check()
    directory(output)
    manifest = output / "ready.json"
    require(not manifest.is_symlink())
    report = json.loads(manifest.read_text())
    require(report["schema"] == 1 and report["kind"] == "bluetooth-only-diagnostic")
    require(report["source_identity"] == source_identity())
    verify_features(output / "build.events.jsonl")
    binary = output / "target" / TARGET / "debug" / NAME
    require(checks.verify_image(binary) == report["binary_sha256"])
    require(checks.dependency_closure(binary) == report["links"])
    return binary


def run(args):
    # These checks precede source inspection, platform work and any child creation.
    require(args.authorize_bluetooth_hardware and args.paired_phone_only)
    require(args.phone_address is not None)
    binary = validate(args.output)
    child = None
    previous = signal.getsignal(signal.SIGTERM)

    def terminate(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    try:
        child = subprocess.Popen(
            [str(binary), "--authorize-bluetooth-hardware", "--paired-phone-only",
             "--phone-address", args.phone_address],
            cwd=ROOT, env=checks.environment(), stdin=subprocess.DEVNULL,
            start_new_session=True)
        # Independent watchdog; the binary has its own 60s + 2s deadlines.
        return child.wait(timeout=65)
    finally:
        try:
            if child is not None:
                stop_owned(child)
        finally:
            signal.signal(signal.SIGTERM, previous)


def stop_owned(child):
    if child.poll() is not None:
        return
    try:
        os.killpg(child.pid, signal.SIGINT)
    except ProcessLookupError:
        pass
    try:
        child.wait(timeout=3)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=3)
        # Forced termination must never be mistaken for confirmed native cleanup.


class Parser(argparse.ArgumentParser):
    def error(self, _message):
        raise ValueError("invalid diagnostic options")


def main():
    try:
        parser = Parser(description=__doc__)
        commands = parser.add_subparsers(dest="action", required=True, parser_class=Parser)
        for name in ("prepare", "check", "run"):
            sub = commands.add_parser(name)
            sub.add_argument("--output", type=Path, required=True)
            if name == "run":
                sub.add_argument("--phone-address", required=True)
                sub.add_argument("--authorize-bluetooth-hardware", action="store_true")
                sub.add_argument("--paired-phone-only", action="store_true")
        args = parser.parse_args()
        args.output = args.output.absolute()
        if args.action == "prepare":
            prepare(args.output)
        elif args.action == "check":
            validate(args.output)
            print("Bluetooth-only artifact checks passed. No hardware accessed.")
        else:
            return run(args)
    except KeyboardInterrupt:
        print("Cancelled. Only the owned diagnostic child was signalled.", file=sys.stderr)
        return 130
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError):
        print("REFUSED/FAILED: Bluetooth-only checks or execution failed. No fallback; artifacts retained.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
