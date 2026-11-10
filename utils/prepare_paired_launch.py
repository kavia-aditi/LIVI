#!/usr/bin/env python3
"""Assemble the fixed new paired-inventory artifact; never execute bundled code."""
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import re
import sys

import package_macos_candidate as pkg

PROJECT = Path(__file__).resolve().parents[1]
ROOT = PROJECT / "artifacts/macos-paired-launch-20261103-164726"
BUILD = PROJECT / "temp-attachments/macos-paired-launch-20261103-164726/target/debug"
APP = ROOT / "LIVI.app"


def require(ok, message):
    pkg.require(ok, message)


def load_launcher():
    spec = importlib.util.spec_from_file_location("paired_launcher", ROOT / "launch.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def protected_state():
    paths = [PROJECT / "dist/LIVI.app",
             PROJECT / "temp-attachments/macos-paired-discovery-20261103-162923"]
    state = {str(path): pkg.tree_hash(path) for path in paths}
    for path in (PROJECT / "dist").glob("*.dmg"):
        require(not path.is_symlink(), "refusing symlinked production DMG")
        state[str(path)] = (pkg.digest(path), path.stat().st_mode)
    state["verifier"] = pkg.digest(PROJECT / "utils/verify_bluetooth_boundaries.py")
    require(state["verifier"] == pkg.VERIFIER_SHA, "immutable verifier changed")
    return state


def check_build_features():
    fingerprints = BUILD / ".fingerprint"
    for crate, binary in (("livi-ui", "LIVI"), ("livi-core", "livi-core"),
                          ("livi-helperd", "livi-helperd"), ("livi-gst-host", "livi-gst-host")):
        rows = list(fingerprints.glob(f"{crate}-*/bin-{binary}.json"))
        rows += list((BUILD / "build" / crate).glob(f"*/fingerprint/bin-{binary}.json"))
        require(bool(rows), f"missing compiler feature metadata: {crate}")
        for path in rows:
            features = set(json.loads(json.loads(path.read_text())["features"]))
            require(not features.intersection(pkg.FORBIDDEN | {"bluetooth-app-test"}),
                    f"unsafe executable feature set: {crate}: {sorted(features)}")
            if crate == "livi-ui":
                require(features <= {"default"}, "UI is not an ordinary default-feature build")
            if crate == "livi-core":
                require(features == {"native-bt-dev"}, "core lacks existing development isolation")
            print(f"Build features: {crate}: {sorted(features)}")


def assemble():
    require(ROOT.is_dir() and not ROOT.is_symlink(), "unsafe artifact root")
    require(not (ROOT.parent.is_symlink() or APP.is_symlink()), "symlinked artifact path")
    check_build_features()
    resources = APP / "Contents/Resources"
    for name, destination in pkg.BINS.items():
        pkg.copy_new(BUILD / name, APP / "Contents" / destination)
    source = PROJECT / "assets/gstreamer/macos-arm64"
    for path in sorted(source.rglob("*")):
        require(not path.is_symlink(), "unexpected runtime symlink; review before copying")
        if path.is_file():
            pkg.copy_new(path, resources / "gstreamer/macos-arm64" / path.relative_to(source))
    pkg.copy_new(PROJECT / "assets/icons/mac/livi.icns", resources / "livi.icns")
    for name in ("LICENSE", "NOTICE"):
        pkg.copy_new(PROJECT / name, resources / name)
    finish_bundle()


def finish_bundle():
    resources = APP / "Contents/Resources"
    pkg.relink(APP)
    for image in pkg.images_under(resources):
        pkg.sign(PROJECT, image, executable_image(image))
    pkg.sign(PROJECT, APP, True, main=True)


def executable_image(path):
    header = pkg.run("otool", "-hv", path).decode()
    return any(line.split()[4:5] == ["EXECUTE"] for line in header.splitlines()
               if line.startswith("MH_MAGIC"))


def refresh_executables():
    require(not (APP / "Contents/_CodeSignature").exists(),
            "refusing to refresh an already signed bundle")
    load_launcher().check_layout()
    check_build_features()
    for name, destination in pkg.BINS.items():
        target = APP / "Contents" / destination
        require(target.is_file() and not target.is_symlink(), "unsafe partial executable")
        fresh = target.with_name(target.name + ".fresh")
        pkg.copy_new(BUILD / name, fresh)
        os.replace(fresh, target)
    finish_bundle()


def version_tuple(value):
    return tuple(int(piece) for piece in value.split("."))


def verify_abi(images):
    """Check each non-system load's required compatibility against bundled current."""
    cache = {image.resolve(): pkg.run("otool", "-L", image).decode() for image in images}
    count = 0
    for image in images:
        output = cache[image.resolve()]
        ids = {s.strip() for s in pkg.run("otool", "-D", image).decode().splitlines()[1:]}
        for name, required in re.findall(
                r"^\s+(\S+) \(compatibility version ([\d.]+), current version [\d.]+\)",
                output, re.MULTILINE):
            if name in ids or pkg.gst.is_system_dependency(name):
                continue
            require(name.startswith("@loader_path/"), f"nonlocal dependency: {name}")
            target = (image.parent / name[len("@loader_path/"):]).resolve(strict=True)
            require(target.is_relative_to(APP.resolve()), "dependency escapes new bundle")
            target_output = cache[target]
            target_ids = {s.strip() for s in
                          pkg.run("otool", "-D", target).decode().splitlines()[1:]}
            current = [value for dep, value in re.findall(
                r"^\s+(\S+) \(compatibility version [\d.]+, current version ([\d.]+)\)",
                target_output, re.MULTILINE) if dep in target_ids]
            require(len(current) == 1, f"missing unique dylib current version: {target}")
            require(version_tuple(current[0]) >= version_tuple(required),
                    f"ABI mismatch: {image.name} -> {target.name}: {required} > {current[0]}")
            count += 1
    return count


def verify():
    launcher = load_launcher()
    ui, core = launcher.check_layout()
    pkg.check_gates(PROJECT)
    images = pkg.images_under(APP)
    require(len(images) > 90, "incomplete media runtime")
    for image in images:
        require(pkg.run("lipo", "-archs", image).decode().split() == ["arm64"],
                f"not arm64-only: {image}")
        pkg.run("codesign", "--verify", "--strict", image)
        actual_bytes = pkg.run("codesign", "-d", "--entitlements", ":-", image)
        actual = plistlib.loads(actual_bytes) if actual_bytes.strip() else {}
        expected = {}
        if image == ui or executable_image(image):
            name = "entitlements.mac.plist" if image == ui else "entitlements.mac.inherit.plist"
            expected = plistlib.loads((PROJECT / "assets/mac" / name).read_bytes())
        require(actual == expected, f"entitlements changed: {image}")
    count = pkg.gst.audit(APP, "arm64")
    edges = verify_abi(images)
    pkg.run("codesign", "--verify", "--deep", "--strict", APP)
    print(f"PASS: {count} arm64-only Mach-O images, {edges} bundled ABI edges")
    print("PASS: all dependencies bundled/system; signatures strict/deep verified")
    print("PASS: entitlements equal existing main/inherit dictionaries; ordinary gates false")
    print(f"UI lookup: {ui}\nCore lookup: {core}")
    print("Compositor is Linux-only; helper and gst-host supplied from the same source build.")
    print("No app/helper/plugin execution, Bluetooth query, installation, or DMG modification.")


def main():
    require(sys.platform == "darwin" and os.uname().machine == "arm64",
            "Darwin arm64 required")
    require(len(sys.argv) == 2 and sys.argv[1] in ("assemble", "refresh", "check"),
            "usage: prepare_paired_launch.py assemble|refresh|check")
    before = protected_state()
    try:
        if sys.argv[1] == "assemble":
            assemble()
        elif sys.argv[1] == "refresh":
            refresh_executables()
        verify()
    finally:
        require(protected_state() == before, "protected artifact changed during operation")
    print("PASS: production app/DMG, previous discovery artifact, immutable verifier unchanged")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError) as error:
        print(f"Preparation failed: {error}; new partial output retained", file=sys.stderr)
        sys.exit(1)
