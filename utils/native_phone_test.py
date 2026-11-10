#!/usr/bin/env python3
"""Prepare offline; run only with explicit real-phone authorization. No installation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import secrets
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
TARGET = "aarch64-apple-darwin"
BINS = ("LIVI", "livi-core", "livi-gst-host", "livi-native-bt-dev")
FEATURES = (
    "livi-core/native-bt-dev,livi-ui/isolated-native-projection,"
    "livi-native-bt-dev/isolated-helper"
)
VERIFIER_SHA = "ac7c4c3dd0a9627631ec6344ce7f06210d8927ec4d94aff07be57cfa11df9187"
FRAMEWORK = Path("/Library/Frameworks/GStreamer.framework/Versions/1.0")
MIN_FREE_BYTES = 20 * 1024 ** 3
DENIED = {"development-receiver", "experimental-metal-validation",
          "offline-admission-test-support"}


class Refused(ValueError):
    """Fixed-text safety failures without configuration or child diagnostics."""


def require(ok, message):
    if not ok:
        raise Refused(message)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def environment():
    # Never inherit LIVI/GST/DYLD/RUSTFLAGS overrides or arbitrary cargo features.
    allowed = ("PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "SHELL",
               "CARGO_HOME", "RUSTUP_HOME", "DEVELOPER_DIR", "SDKROOT",
               "MACOSX_DEPLOYMENT_TARGET")
    return {key: os.environ[key] for key in allowed if key in os.environ}


def host_check():
    require(platform.system() == "Darwin" and platform.machine() == "arm64",
            "macOS ARM64 required")
    require(os.geteuid() != 0, "do not run as root")
    return gstreamer_sdk()


def gstreamer_sdk():
    if FRAMEWORK.is_dir():
        prefix = FRAMEWORK
    else:
        prefix = Path(command_output("pkg-config", "--variable=prefix", "gstreamer-1.0").strip())
    require(prefix.is_absolute() and prefix.is_dir(), "GStreamer SDK unavailable")
    for relative in ("lib/pkgconfig/gstreamer-1.0.pc",
                     "lib/gstreamer-1.0",
                     "libexec/gstreamer-1.0/gst-plugin-scanner"):
        require((prefix / relative).exists(), "incomplete GStreamer SDK")
    return prefix


def boundary_check():
    verifier = ROOT / "utils/verify_bluetooth_boundaries.py"
    require(sha(verifier) == VERIFIER_SHA, "immutable boundary verifier changed")
    subprocess.run([sys.executable, "-B", str(verifier)], check=True,
                   env=environment(), cwd=ROOT)
    helper = (ROOT / "native/crates/livi-helperd/src/native_tcp.rs").read_text()
    ui = (ROOT / "native/crates/livi-ui/src/native_bluetooth.rs").read_text()
    require("const NATIVE_AUTHORIZED: bool = false;" in helper,
            "ordinary helper gate changed")
    production = ui.split("fn production_gates(", 1)[1].split("\n}", 1)[0]
    require("hardware_authorized: false" in production, "ordinary UI gate changed")


def source_identity():
    """Public Rust/UI build sources, not private settings or historical approval."""
    rows = {}
    extensions = {".rs", ".toml", ".lock", ".slint", ".proto", ".c", ".h",
                  ".m", ".mm", ".metal", ".wgsl"}
    for folder in ("native/crates", "native/vendor", "native/xtask"):
        for directory, dirs, names in os.walk(ROOT / folder, followlinks=False):
            dirs[:] = sorted(d for d in dirs if d not in ("target", ".git"))
            require(not any((Path(directory) / d).is_symlink() for d in dirs),
                    "symlinked source directory refused")
            for name in sorted(names):
                path = Path(directory) / name
                if path.suffix in extensions:
                    require(not path.is_symlink(), "symlinked source refused")
                    rows[str(path.relative_to(ROOT))] = sha(path)
    for name in ("native/Cargo.toml", "native/Cargo.lock",
                 "utils/native_phone_test.py", "utils/verify_bluetooth_boundaries.py"):
        rows[name] = sha(ROOT / name)
    return hashlib.sha256(json.dumps(rows, sort_keys=True).encode()).hexdigest()


def cargo_command(output):
    return ["cargo", "build", "--locked", "--offline", "--target", TARGET,
            "--target-dir", str(output / "target"),
            "--message-format=json-render-diagnostics",
            "-p", "livi-core", "-p", "livi-ui", "-p", "livi-gst-host",
            "-p", "livi-native-bt-dev", "--features", FEATURES]


def verify_features(path):
    artifacts = {}
    executables = set()
    finished = False
    with path.open() as stream:
        for line in stream:
            event = json.loads(line)
            if event.get("reason") == "build-finished":
                finished = event.get("success") is True
            if event.get("reason") != "compiler-artifact":
                continue
            name = event["target"]["name"].replace("_", "-")
            features = set(event.get("features", []))
            require(not features.intersection(DENIED), "forbidden test-build feature")
            artifacts.setdefault(name, set()).update(features)
            if event.get("executable"):
                executables.add(Path(event["executable"]).name)
    require(finished and set(BINS) <= executables, "incomplete test build")
    for name, feature in (("livi-core", "native-bt-dev"),
                          ("LIVI", "isolated-native-projection"),
                          ("livi-native-bt-dev", "isolated-helper"),
                          ("livi-video-view", "native-projection")):
        require(feature in artifacts.get(name, set()), "required isolated feature missing")
    return {name: sorted(value) for name, value in sorted(artifacts.items())}


def command_output(*args):
    return subprocess.check_output(args, env=environment(), stderr=subprocess.PIPE).decode()


def verify_image(path):
    require(path.is_file() and not path.is_symlink(), "missing or unsafe test executable")
    require(command_output("lipo", "-archs", str(path)).split() == ["arm64"],
            "test binary is not ARM64 only")
    subprocess.run(["codesign", "--verify", "--strict", str(path)], check=True,
                   env=environment(), stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return sha(path)


def dependency_closure(executable):
    """Resolve static Mach-O dependencies without loading any image or plugin."""
    seen = {}

    def expand(value, loader):
        return value.replace("@loader_path", str(loader)).replace(
            "@executable_path", str(executable.parent))

    def visit(image, inherited=()):
        image = image.resolve(strict=True)
        if str(image) in seen:
            return
        require("arm64" in command_output("lipo", "-archs", str(image)).split(),
                "dependency lacks ARM64")
        seen[str(image)] = sha(image)
        commands = command_output("otool", "-l", str(image)).splitlines()
        rpaths = []
        for i, line in enumerate(commands):
            if line.strip() == "cmd LC_RPATH":
                for entry in commands[i + 1:i + 4]:
                    if entry.strip().startswith("path "):
                        value = entry.strip()[5:].split(" (offset ", 1)[0]
                        rpaths.append(Path(expand(value, image.parent)))
        rpaths.extend(inherited)
        ids = set(command_output("otool", "-D", str(image)).splitlines()[1:])
        for line in command_output("otool", "-L", str(image)).splitlines()[1:]:
            dependency = line.strip().split(" (", 1)[0]
            if dependency in ids or dependency.startswith(("/System/Library/", "/usr/lib/")):
                continue
            if dependency.startswith("@rpath/"):
                relative = dependency[len("@rpath/"):]
                candidates = [root / relative for root in rpaths]
            else:
                candidates = [Path(expand(dependency, image.parent))]
            target = next((path for path in candidates if path.is_absolute() and path.is_file()), None)
            require(target is not None, "unresolved static Mach-O dependency")
            visit(target, rpaths)

    visit(executable)
    return seen


def prepare(output):
    sdk = host_check()
    boundary_check()
    parent = ROOT / "temp-attachments"
    require(parent.is_dir() and not parent.is_symlink(), "unsafe output parent")
    require(not output.is_symlink() and output.parent == parent,
            "output must be a new direct child of temp-attachments")
    if os.path.lexists(output):
        raise FileExistsError("test output already exists")
    require(shutil.disk_usage(parent).free >= MIN_FREE_BYTES,
            "at least 20 GiB free required for a fresh isolated debug build")
    output.mkdir(mode=0o700)  # Exclusive: never reuse, overwrite or remove an artifact.
    before = source_identity()
    env = environment()
    env.update(CI="1", CARGO_INCREMENTAL="0",
               PKG_CONFIG_PATH=str(sdk / "lib/pkgconfig"))
    print(f"Building isolated test binaries only: {output}", flush=True)
    with (output / "build.events.jsonl").open("x") as events, \
            (output / "build.log").open("x") as errors:
        subprocess.run(cargo_command(output), cwd=ROOT / "native", env=env,
                       stdout=events, stderr=errors, check=True)
    require(source_identity() == before, "sources changed during build; output retained")
    features = verify_features(output / "build.events.jsonl")
    binary_dir = output / "target" / TARGET / "debug"
    binaries = {name: verify_image(binary_dir / name) for name in BINS}
    # Static link inspection only: no plugins, app or helper are executed here.
    links = {name: dependency_closure(binary_dir / name) for name in BINS}
    report = {"schema": 1, "kind": "isolated-phone-test-not-production",
              "source_identity": before, "binaries": binaries, "features": features,
              "links": links, "hardware_executed": False,
              "gstreamer_sdk": str(sdk), "not_a_self_contained_app": True}
    with (output / "ready.json").open("x") as stream:
        json.dump(report, stream, indent=2, sort_keys=True)
    print(f"Prepared, NOT phone-validated: {output}", flush=True)


def validate_prepared(output):
    require(output.parent == ROOT / "temp-attachments"
            and not output.is_symlink(), "invalid test-build directory")
    require(not (ROOT / "temp-attachments").is_symlink(), "unsafe output parent")
    report_path = output / "ready.json"
    require(not report_path.is_symlink(), "unsafe test manifest")
    report = json.loads(report_path.read_text())
    require(report["schema"] == 1
            and report["kind"] == "isolated-phone-test-not-production",
            "not an isolated test build")
    require(report["source_identity"] == source_identity(),
            "source changed; prepare a fresh test build")
    require(report["gstreamer_sdk"] == str(gstreamer_sdk()),
            "GStreamer installation changed; prepare a fresh build")
    directory = output / "target" / TARGET / "debug"
    require(set(report["binaries"]) == set(BINS), "incomplete binary manifest")
    for name in BINS:
        require(verify_image(directory / name) == report["binaries"][name],
                "test executable changed; prepare a fresh build")
        require(dependency_closure(directory / name) == report["links"][name],
                "runtime dependency changed; prepare a fresh build")
    return directory


def private_config(path):
    # The launcher never prints or copies configuration contents.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        meta = os.fstat(fd)
        require(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.geteuid()
                and stat.S_IMODE(meta.st_mode) == 0o600 and meta.st_size <= 2048,
                "config must be an owned, regular, non-symlink 0600 file <=2048 bytes")
    finally:
        os.close(fd)


def runtime_environment(runtime, binaries):
    sdk = gstreamer_sdk()
    env = environment()
    env.update(LIVI_NATIVE_BT_DEV_DIR=str(runtime), XDG_RUNTIME_DIR=str(runtime),
               LIVI_CORE_SOCKET=str(runtime / "livi/core.sock"),
               LIVI_UI_PLANES=str(runtime / "livi/ui-planes.sock"),
               LIVI_GST_HOST_BIN=str(binaries / "livi-gst-host"),
               LIVI_ROOT=str(ROOT), LIVI_NO_UI="1", LIVI_LIFELINE="1",
               GST_PLUGIN_SYSTEM_PATH="", GST_PLUGIN_PATH=str(sdk / "lib/gstreamer-1.0"),
               GST_PLUGIN_SCANNER=str(sdk / "libexec/gstreamer-1.0/gst-plugin-scanner"),
               GST_REGISTRY=str(runtime / "gst-registry.bin"),
               DYLD_FRAMEWORK_PATH=str(sdk.parent.parent) if sdk == FRAMEWORK else "")
    return env


def stop(child):
    if child.stdin:
        child.stdin.close()
    if child.poll() is not None:
        return
    # Each child is started in its own session; never signal an unrelated process.
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        child.wait(timeout=8)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=3)


def wait_ready(predicate, children, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        require(all(child.poll() is None for child in children),
                "an isolated child stopped before readiness")
        if predicate():
            return
        time.sleep(0.05)
    raise ValueError("isolated child readiness timed out")


def run_test(args):
    require(args.authorize_native_hardware and args.paired_phone
            and args.existing_ap and args.no_usb_projection,
            "explicit hardware, already-paired phone, existing AP and no-USB confirmations required")
    host_check()
    boundary_check()
    binaries = validate_prepared(args.output)
    config = args.ap_config.absolute()
    private_config(config)
    # Retained for diagnostics; no existing path is reused or removed.
    runtime = Path(tempfile.mkdtemp(prefix="livi-bt-dev-", dir="/tmp"))
    os.chmod(runtime, 0o700)
    require(len(str(runtime)) <= 64, "runtime path too long")
    runtime_id = secrets.randbits(63) or 1
    env = runtime_environment(runtime, binaries)
    children = []

    def spawn(name, extra=(), lifeline=False):
        child = subprocess.Popen([str(binaries / name), *extra], cwd=ROOT, env=env,
                                 stdin=subprocess.PIPE if lifeline else subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True)
        children.append(child)
        return child

    print(f"Authorized isolated test, maximum {args.seconds}s. Runtime: {runtime}", flush=True)
    print("Child output is suppressed to avoid recording credentials. Ctrl-C stops owned children.",
          flush=True)
    def terminate(_signum, _frame):
        raise KeyboardInterrupt

    previous = signal.signal(signal.SIGTERM, terminate)
    try:
        spawn("livi-native-bt-dev", ["--isolated-helper", "--authorize-native-hardware",
              "--runtime-dir", str(runtime), "--runtime-id", str(runtime_id),
              "--ap-config", str(config)])
        marker = runtime / "native-runtime"
        wait_ready(lambda: marker.is_file(), children)
        require(marker.read_text().strip() == str(runtime_id), "runtime identity mismatch")
        spawn("livi-core", lifeline=True)
        wait_ready(lambda: (runtime / "livi/core.sock").is_socket(), children)
        ui = spawn("LIVI", ["--isolated-native-projection", "--authorize-native-hardware",
                   "--native-runtime-id", str(runtime_id), "--native-ap-config", str(config)])
        deadline = time.monotonic() + args.seconds
        while ui.poll() is None and time.monotonic() < deadline:
            require(all(child.poll() is None for child in children[:-1]),
                    "isolated helper or core stopped; test aborted")
            time.sleep(0.1)
        if ui.poll() is not None:
            require(ui.returncode == 0, "isolated UI stopped with an error")
    finally:
        try:
            for child in reversed(children):
                stop(child)
        finally:
            signal.signal(signal.SIGTERM, previous)
            print(f"Test stopped. Private runtime retained at {runtime}; no production gate changed.",
                  flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prepare_parser = commands.add_parser("prepare", help="offline build only; no hardware")
    prepare_parser.add_argument("--output", type=Path, required=True)
    run_parser = commands.add_parser("run", help="explicitly authorized real-phone test")
    run_parser.add_argument("--output", type=Path, required=True)
    run_parser.add_argument("--ap-config", type=Path, required=True)
    run_parser.add_argument("--seconds", type=int, choices=range(30, 1801), default=600,
                            metavar="30..1800")
    for flag in ("authorize-native-hardware", "paired-phone", "existing-ap", "no-usb-projection"):
        run_parser.add_argument("--" + flag, action="store_true")
    args = parser.parse_args()
    args.output = args.output.absolute()
    try:
        if args.command == "prepare":
            prepare(args.output)
        else:
            run_test(args)
    except KeyboardInterrupt:
        print("Stopped by operator.", file=sys.stderr)
        return 130
    except Refused as error:
        print(f"Phone test refused: {error}. Existing outputs retained.", file=sys.stderr)
        return 1
    except (ValueError, OSError, KeyError, subprocess.SubprocessError):
        # No exception details: paths/configuration/child diagnostics may contain secrets.
        print("Phone-test preparation/run refused or failed; incomplete output retained.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
