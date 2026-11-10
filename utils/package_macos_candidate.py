#!/usr/bin/env python3
"""Candidate-only macOS ARM64 packaging. Never executes bundled code or installs.

Outputs are retained on every failure. A source snapshot identifies current inputs,
not recovered historical approval. No legacy output or Applications path is used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import stat
import subprocess
import sys

import verify_gstreamer_macos as gst

GST = Path("gstreamer/macos-arm64")
BINS = {
    "LIVI": "MacOS/LIVI",
    "livi-core": "Resources/core/livi-core",
    "livi-helperd": "Resources/driver/livi-helperd",
    "livi-gst-host": "Resources/gst-host/livi-gst-host",
}
FORBIDDEN = {
    "isolated-helper", "isolated-development", "isolated-native-projection",
    "development-receiver", "experimental-metal-validation",
    "offline-admission-test-support",
}
SOURCE_EXTENSIONS = {
    ".rs", ".toml", ".lock", ".slint", ".json", ".proto", ".h", ".c",
    ".cc", ".cpp", ".m", ".mm", ".metal", ".wgsl", ".glsl", ".vert",
    ".frag", ".png", ".jpg", ".jpeg", ".svg", ".ttf", ".otf", ".bin",
    ".pem", ".der", ".crt", ".txt", ".md", ".sh", ".py", ".conf",
}
SKIP_DIRS = {".git", "target", "__pycache__", "node_modules", ".env"}
VERIFIER_SHA = "ac7c4c3dd0a9627631ec6344ce7f06210d8927ec4d94aff07be57cfa11df9187"
FIRMWARE_INPUTS = (
    "assets/livi-link/v821b_aic8800d80/livi-link-v821b.lfwb",
    "assets/livi-link/ax520_aic8800d80/livi-link-ax520.lfwb",
    "assets/livi-link/imx6ul_iw416/livi-link-imx6ull.lfwb",
    "assets/livi-link/imx6ul_rtl8822cs/livi-link-imx6ull-rtl8822cs.lfwb",
    "assets/livi-link/imx6ul_rtl8822bs/livi-link-imx6ull-rtl8822bs.lfwb",
    "assets/livi-link/imx6ul_bcm4354/livi-link-imx6ull-bcm4354.lfwb",
    "native/crates/livi-link-provision/assets/bindshell/armv7",
    "native/crates/livi-link-provision/assets/bindshell/riscv32",
    "native/crates/livi-link-provision/assets/bindshell/mipsel",
)


def require(ok, message):
    if not ok:
        raise ValueError(message)


def run(*args):
    result = subprocess.run([str(a) for a in args], capture_output=True)
    if result.returncode:
        detail = result.stderr.decode(errors="replace").strip()
        raise ValueError(f"{args[0]} failed ({result.returncode}): {detail}")
    return result.stdout


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def write_json(path, value):
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, sort_keys=True, indent=2)
        stream.write("\n")


def absent(path):
    require(not os.path.lexists(path), f"target already exists: {path}")


def safe_candidate(project, candidate):
    project = project.resolve(strict=True)
    require(".." not in candidate.parts, "candidate traversal is forbidden")
    if not candidate.is_absolute():
        candidate = project / candidate
    require(candidate.parent == project / "dist/candidates",
            "candidate must be directly under project/dist/candidates")
    for ancestor in candidate.parents:
        if ancestor == project:
            break
        require(not ancestor.is_symlink(), f"symlinked ancestor: {ancestor}")
    require(candidate.is_dir() and not candidate.is_symlink(),
            "candidate must be an exclusively reserved real directory")
    return project, candidate


def selected_files(project):
    """Explicit public build inputs; no user configuration or environment files."""
    files = set()
    for name in FIRMWARE_INPUTS:
        path = project / name
        require(path.is_file() and not path.is_symlink(),
                f"missing build input (refusing source-generating stub fallback): {name}")
        files.add(path)
    for name in ("native/Cargo.toml", "native/Cargo.lock", "LICENSE", "NOTICE",
                 "assets/icons/mac/livi.icns",
                 "assets/mac/entitlements.mac.plist",
                 "assets/mac/entitlements.mac.inherit.plist",
                 "utils/package_macos_candidate.py",
                 "utils/test_package_macos_candidate.py",
                 "utils/verify_gstreamer_macos.py",
                 "utils/verify_bluetooth_boundaries.py"):
        path = project / name
        require(path.is_file() and not path.is_symlink(), f"missing/unsafe input: {name}")
        files.add(path)
    for name in ("rust-toolchain.toml", "native/rust-toolchain.toml",
                 "native/.cargo/config.toml"):
        path = project / name
        if path.exists():
            require(not path.is_symlink(), f"unsafe build input: {name}")
            files.add(path)
    for name in ("native/crates", "native/vendor", "native/xtask"):
        base = project / name
        require(base.is_dir() and not base.is_symlink(), f"unsafe source root: {name}")
        for directory, dirs, names in os.walk(base, followlinks=False):
            dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS)
            for child in dirs:
                require(not (Path(directory) / child).is_symlink(), "symlink in source tree")
            for filename in names:
                path = Path(directory) / filename
                if path.suffix.lower() in SOURCE_EXTENSIONS or filename in ("LICENSE", "NOTICE"):
                    require(not path.is_symlink(), f"symlink source: {path.relative_to(project)}")
                    files.add(path)
    runtime = project / "assets" / GST
    require(runtime.is_dir() and not runtime.is_symlink(), "missing/unsafe runtime")
    for path in runtime.rglob("*"):
        if path.is_symlink():
            require(path.resolve(strict=True).is_relative_to(runtime),
                    "escaping runtime symlink")
            files.add(path)
        elif path.is_file():
            files.add(path)
    return sorted(files)


def inventory(project):
    rows = {}
    folded = set()
    for path in selected_files(project):
        name = path.relative_to(project).as_posix()
        require(name.casefold() not in folded, f"case collision: {name}")
        folded.add(name.casefold())
        mode = stat.S_IMODE(path.lstat().st_mode)
        if path.is_symlink():
            rows[name] = {"symlink": os.readlink(path), "mode": mode}
        else:
            rows[name] = {"sha256": digest(path), "size": path.stat().st_size, "mode": mode}
    return rows


def check_gates(project):
    require(digest(project / "utils/verify_bluetooth_boundaries.py") == VERIFIER_SHA,
            "immutable Bluetooth verifier changed")
    helper = (project / "native/crates/livi-helperd/src/native_tcp.rs").read_text()
    ui = (project / "native/crates/livi-ui/src/native_bluetooth.rs").read_text()
    require(re.search(r"const\s+NATIVE_AUTHORIZED\s*:\s*bool\s*=\s*false", helper),
            "ordinary helper native authorization is not false")
    start = ui.index("fn production_gates")
    end = ui.index("\n}", start)
    require("hardware_authorized: false" in ui[start:end],
            "ordinary UI native authorization is not false")


def snapshot(project, candidate):
    check_gates(project)
    body = {
        "schema": 1, "provenance": "fresh candidate snapshot; NOT historical approval",
        "architecture": "aarch64-apple-darwin",
        "rustc": run("rustc", "-Vv").decode().strip(),
        "cargo": run("cargo", "-V").decode().strip(),
        "sdk": run("xcrun", "--show-sdk-version").decode().strip(),
        "gstreamer_sdk": run("pkg-config", "--modversion", "gstreamer-1.0").decode().strip(),
        "files": inventory(project),
    }
    identity = "candidate-" + hashlib.sha256(canonical(body)).hexdigest()
    write_json(candidate / "candidate-source.json", {"identity": identity, "body": body})
    return identity


def verify(project, candidate):
    manifest = json.loads((candidate / "candidate-source.json").read_text())
    require(manifest["identity"] == "candidate-" + hashlib.sha256(canonical(manifest["body"])).hexdigest(),
            "manifest canonical hash mismatch")
    require(manifest["body"]["files"] == inventory(project),
            "candidate source inventory changed (content, additions, deletion, or permissions)")
    check_gates(project)
    return manifest["identity"]


def verify_features(candidate):
    seen = {}
    executables = set()
    complete = False
    with (candidate / "build.events.jsonl").open() as stream:
        for line in stream:
            event = json.loads(line)
            if event.get("reason") == "build-finished":
                complete = event["success"]
            if event.get("reason") != "compiler-artifact":
                continue
            name = event["target"]["name"].replace("_", "-")
            require(name != "livi-native-bt-dev", "development Bluetooth receiver included")
            features = set(event.get("features", []))
            require(not features.intersection(FORBIDDEN),
                    f"development-only artifact feature: {name}: {sorted(features & FORBIDDEN)}")
            seen.setdefault(name, set()).update(features)
            if event.get("executable"):
                executables.add(Path(event["executable"]).name)
    require(complete and set(BINS).issubset(executables),
            "missing successful release build or production executables")
    require({"livi-native-bluetooth", "livi-native-handoff", "livi-video-view"} <= seen.keys(),
            "required native production dependencies missing")
    require("native-projection" in seen["livi-video-view"],
            "production native rendering feature missing")
    return {name: sorted(features) for name, features in sorted(seen.items())}


def mach(path):
    if not path.is_file():
        return False
    with path.open("rb") as stream:
        return stream.read(4) in gst.MACH_MAGICS


def copy_new(source, target):
    absent(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    require(source.is_file() and not source.is_symlink(), f"unsafe copy input: {source}")
    if mach(source):
        archs = run("lipo", "-archs", source).decode().split()
        require("arm64" in archs, f"missing ARM64 slice: {source}")
        if archs != ["arm64"]:
            run("lipo", source, "-thin", "arm64", "-output", target)
            shutil.copystat(source, target)
            return
    with source.open("rb") as src, target.open("xb") as dst:
        shutil.copyfileobj(src, dst)
    shutil.copystat(source, target)


def images_under(root):
    return sorted(p for p in root.rglob("*") if not p.is_symlink() and mach(p))


def relink(app):
    images = images_under(app)
    names = {}
    for image in images:
        ids = run("otool", "-D", image).decode().splitlines()
        aliases = [image.name] + [Path(s.strip()).name for s in ids[1:] if s.startswith(("@", "/"))]
        for alias in aliases:
            if alias in names and names[alias] != image:
                raise ValueError(f"ambiguous bundled dependency: {alias}")
            names[alias] = image
    for image in images:
        ids = {s.strip() for s in run("otool", "-D", image).decode().splitlines()[1:]}
        for dependency in gst.dependency_names(run("otool", "-L", image).decode()):
            if dependency in ids or gst.is_system_dependency(dependency):
                continue
            target = names.get(Path(dependency).name)
            require(target is not None, f"{image.name}: missing bundled dependency {dependency}")
            relative = os.path.relpath(target, image.parent)
            run("install_name_tool", "-change", dependency, "@loader_path/" + relative, image)
        for path in set(gst.rpath_names(run("otool", "-l", image).decode())):
            run("install_name_tool", "-delete_rpath", path, image)
        if ids:
            run("install_name_tool", "-id", "@rpath/" + image.name, image)


def assemble(project, candidate):
    verify(project, candidate)
    features = verify_features(candidate)
    write_json(candidate / "build-features.json", features)
    app = candidate / "LIVI.app"
    absent(app)
    app.mkdir()
    contents = app / "Contents"
    resources = contents / "Resources"
    binaries = candidate / "target/aarch64-apple-darwin/release"
    for name, destination in BINS.items():
        copy_new(binaries / name, contents / destination)
    source = project / "assets" / GST
    runtime = resources / GST
    runtime.mkdir(parents=True)
    for path in sorted(source.rglob("*")):
        relative = path.relative_to(source)
        if "share" in relative.parts or path.suffix == ".a":
            continue
        target = runtime / relative
        if path.is_symlink():
            require(path.resolve(strict=True).is_relative_to(source), "runtime symlink escape")
            target.parent.mkdir(parents=True, exist_ok=True)
            absent(target)
            target.symlink_to(os.readlink(path))
        elif path.is_dir():
            target.mkdir(exist_ok=True)
        else:
            copy_new(path, target)
    copy_new(project / "assets/icons/mac/livi.icns", resources / "livi.icns")
    for name in ("LICENSE", "NOTICE"):
        copy_new(project / name, resources / name)
    workspace = (project / "native/Cargo.toml").read_text().split("[workspace.package]", 1)[1]
    version = re.search(r'^version = "([^"]+)"', workspace, re.MULTILINE).group(1)
    info = {
        "CFBundleIdentifier": "dev.f-io.livi", "CFBundleName": "LIVI",
        "CFBundleDisplayName": "LIVI", "CFBundleExecutable": "LIVI",
        "CFBundleIconFile": "livi.icns", "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": version, "CFBundleVersion": version,
        "CFBundleInfoDictionaryVersion": "6.0", "LSMinimumSystemVersion": "11.0",
        "LSApplicationCategoryType": "public.app-category.utilities",
        "NSHighResolutionCapable": True,
        "NSHumanReadableCopyright": "Copyright © 2026 Lasse Heitgres",
        "NSCameraUsageDescription": "LIVI requires camera access for CarPlay.",
        "NSMicrophoneUsageDescription": "LIVI requires microphone access for voice control.",
        "NSLocalNetworkUsageDescription": "LIVI talks to a LIVI Link dongle on the local network.",
        "NSBluetoothAlwaysUsageDescription": "LIVI uses Bluetooth to discover and pair your Android phone and start experimental wireless Android Auto.",
    }
    with (contents / "Info.plist").open("xb") as stream:
        plistlib.dump(info, stream)
    relink(app)
    for image in images_under(resources):
        sign(project, image, executable_image(image))
    sign(project, app, True, main=True)


def executable_image(path):
    return path.suffix not in (".dylib", ".so") and bool(path.stat().st_mode & 0o111)


def sign(project, image, executable, main=False):
    args = ["codesign", "--force", "--sign", "-", "--timestamp=none", "--options", "runtime"]
    if executable:
        name = "entitlements.mac.plist" if main else "entitlements.mac.inherit.plist"
        args += ["--entitlements", project / "assets/mac" / name]
    run(*args, image)


def tree_hash(root):
    rows = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            rows[path.relative_to(root).as_posix()] = {"symlink": os.readlink(path)}
        elif path.is_file():
            rows[path.relative_to(root).as_posix()] = {
                "sha256": digest(path), "mode": stat.S_IMODE(path.stat().st_mode),
            }
    return rows


def verify_static(project, candidate):
    verify(project, candidate)
    verify_features(candidate)
    app = candidate / "LIVI.app"
    for destination in BINS.values():
        require((app / "Contents" / destination).is_file(), f"missing app executable {destination}")
    runtime = app / "Contents/Resources" / GST
    for relative in (
        "libexec/gstreamer-1.0/gst-plugin-scanner",
        "lib/libgstreamer-1.0.0.dylib", "lib/libgstapp-1.0.0.dylib",
        "lib/gstreamer-1.0/libgstcoreelements.dylib",
        "lib/gstreamer-1.0/libgstapp.dylib",
        "lib/gstreamer-1.0/libgstvideoparsersbad.dylib",
        "lib/gstreamer-1.0/libgstapplemedia.dylib",
        "lib/gstreamer-1.0/libgstlibav.dylib",
    ):
        require((runtime / relative).is_file(), f"missing required GStreamer payload: {relative}")
    images = images_under(app)
    for image in images:
        require(run("lipo", "-archs", image).decode().split() == ["arm64"],
                f"non-ARM64-only image: {image}")
        run("codesign", "--verify", "--strict", image)
        display = subprocess.run(["codesign", "-d", "--verbose=4", str(image)],
                                 capture_output=True, text=True, check=True)
        require("Signature=adhoc" in display.stderr and "runtime" in display.stderr,
                f"expected ad-hoc hardened-runtime signature: {image}")
        entitlements = run("codesign", "-d", "--entitlements", ":-", image)
        actual = plistlib.loads(entitlements) if entitlements.strip() else {}
        main = image == app / "Contents/MacOS/LIVI"
        expected = {}
        if main or executable_image(image):
            name = "entitlements.mac.plist" if main else "entitlements.mac.inherit.plist"
            expected = plistlib.loads((project / "assets/mac" / name).read_bytes())
        require(actual == expected, f"entitlement mismatch: {image}")
    run("codesign", "--verify", "--deep", "--strict", "--verbose=2", app)
    count = gst.audit(app, "arm64")
    write_json(candidate / "static-verification.json", {
        "source_identity": verify(project, candidate),
        "mach_o_arm64_only": count, "dependencies": "bundle or system only",
        "signing": "ad-hoc; strict and deep verified; not notarized",
        "entitlements": "exact existing main/inherit dictionaries",
        "gstreamer": "static closure and required decoder payload verified; no plugins loaded",
        "app_files": tree_hash(app),
        "hardware_authorization": False, "app_or_helper_executed": False,
    })


def stage(candidate):
    report = json.loads((candidate / "static-verification.json").read_text())
    app = candidate / "LIVI.app"
    require(tree_hash(app) == report["app_files"], "verified app changed before staging")
    staging = candidate / "dmg"
    absent(staging)
    staging.mkdir()
    run("ditto", app, staging / "LIVI.app")
    require(tree_hash(staging / "LIVI.app") == report["app_files"], "staging copy differs")
    # No Applications symlink: candidate is never installed by this workflow.


def create_dmg(candidate):
    path = candidate / "LIVI-macos-arm64-candidate.dmg"
    absent(path)
    report = json.loads((candidate / "static-verification.json").read_text())
    require(tree_hash(candidate / "dmg/LIVI.app") == report["app_files"], "staged app changed")
    run("hdiutil", "create", "-volname", "LIVI Candidate", "-format", "UDZO",
        "-noskipunreadable", "-srcowners", "any", "-srcfolder", candidate / "dmg", path)


def verify_dmg(project, candidate):
    verify(project, candidate)
    path = candidate / "LIVI-macos-arm64-candidate.dmg"
    require(path.is_file() and not path.is_symlink(), "missing/unsafe candidate DMG")
    verification = run("hdiutil", "verify", path).decode(errors="replace").strip()
    info = plistlib.loads(run("hdiutil", "imageinfo", "-plist", path))
    require(info.get("Format") == "UDZO", "unexpected DMG format")
    report = json.loads((candidate / "static-verification.json").read_text())
    require(tree_hash(candidate / "LIVI.app") == report["app_files"], "verified app changed")
    require(tree_hash(candidate / "dmg/LIVI.app") == report["app_files"], "staged app changed")
    result = {
        "dmg": str(path), "size_bytes": path.stat().st_size, "sha256": digest(path),
        "build": "locked offline ARM64 release; four production executables",
        "dmg_integrity": verification, "format": "UDZO",
        "source_identity": verify(project, candidate),
        "static_verification": "passed", "production_outputs": "never targeted for writing",
        "warnings": ["Ad-hoc signed, not notarized.",
                     "No app/helper/plugin execution or real-device projection validation.",
                     "Fresh candidate provenance, not historical approval.",
                     "DMG checksum verified without mounting."],
    }
    write_json(candidate / "candidate-result.json", result)
    print(json.dumps(result, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("snapshot", "verify", "assemble", "static",
                                               "stage", "dmg", "verify-dmg"))
    parser.add_argument("--project", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    args = parser.parse_args()
    try:
        require(platform.system() == "Darwin" and platform.machine() == "arm64",
                "macOS ARM64 host required")
        project, candidate = safe_candidate(args.project, args.candidate)
        if args.operation == "snapshot":
            print(snapshot(project, candidate))
        elif args.operation == "verify":
            print(verify(project, candidate))
        elif args.operation == "assemble":
            assemble(project, candidate)
        elif args.operation == "static":
            verify_static(project, candidate)
        elif args.operation == "stage":
            stage(candidate)
        elif args.operation == "dmg":
            create_dmg(candidate)
        else:
            verify_dmg(project, candidate)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"candidate packaging FAILED: {error}; incomplete output retained", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
