#!/usr/bin/env python3
"""Launch only this paired-inventory development bundle; --check executes nothing."""
import argparse
import os
from pathlib import Path
import plistlib
import sys
import tempfile

ROOT = Path(__file__).resolve().parent
APP = ROOT / "LIVI.app"
REQUIRED = (
    "MacOS/LIVI",
    "Resources/core/livi-core",
    "Resources/driver/livi-helperd",
    "Resources/gst-host/livi-gst-host",
    "Resources/gstreamer/macos-arm64/lib/libgstreamer-1.0.0.dylib",
    "Resources/gstreamer/macos-arm64/lib/libglib-2.0.0.dylib",
    "Resources/gstreamer/macos-arm64/lib/gstreamer-1.0/libgstcoreelements.dylib",
    "Resources/gstreamer/macos-arm64/libexec/gstreamer-1.0/gst-plugin-scanner",
    "Resources/gstreamer/macos-arm64/version.txt",
    "Resources/livi.icns",
    "Resources/LICENSE",
    "Resources/NOTICE",
)
BUNDLE_ID = "dev.f-io.livi.paired-discovery"


def check_layout(app=APP):
    contents = app / "Contents"
    for relative in ("Info.plist",) + REQUIRED:
        path = contents / relative
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"missing or symlinked resource: {relative}")
        if not path.resolve().is_relative_to(app.resolve()):
            raise ValueError(f"resource escapes bundle: {relative}")
    info = plistlib.loads((contents / "Info.plist").read_bytes())
    if info.get("CFBundleExecutable") != "LIVI" or info.get("CFBundleIdentifier") != BUNDLE_ID:
        raise ValueError("unexpected bundle identity or executable")
    if not info.get("NSBluetoothAlwaysUsageDescription"):
        raise ValueError("Bluetooth usage description is missing")
    for relative in REQUIRED[:4]:
        if not os.access(contents / relative, os.X_OK):
            raise ValueError(f"non-executable program: {relative}")
    return contents / "MacOS/LIVI", contents / "Resources/core/livi-core"


def launch_environment(runtime, inherited):
    """Pure construction: no files, IPC, hardware queries, or subprocesses."""
    env = {key: inherited[key] for key in
           ("LANG", "LC_ALL", "LC_CTYPE", "TERM", "USER", "LOGNAME")
           if key in inherited}
    env.update({
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(runtime / "home"),
        "TMPDIR": str(runtime),
        "XDG_RUNTIME_DIR": str(runtime),
        "XDG_CONFIG_HOME": str(runtime / "home/.config"),
        "XDG_CACHE_HOME": str(runtime / "home/.cache"),
        "XDG_DATA_HOME": str(runtime / "home/.local/share"),
        "LIVI_NATIVE_BT_DEV_DIR": str(runtime),
        "LIVI_CORE_SOCKET": str(runtime / "livi/core.sock"),
        "LIVI_UI_PAGE": "settings",
        "LIVI_UI_SETTINGS": "devices",
        "GST_REGISTRY": str(runtime / "gst-registry.bin"),
    })
    if len(os.fsencode(env["LIVI_CORE_SOCKET"])) >= 104:
        raise ValueError("isolated socket path exceeds Darwin's limit")
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="check paths only; do not create runtime or launch anything")
    args = parser.parse_args()
    ui, core = check_layout()
    if args.check:
        print(f"UI: {ui}\nCore selected by bundled UI: {core}")
        print("Layout OK; no programs executed, runtime created, or Bluetooth queried.")
        return 0
    if sys.platform != "darwin":
        raise ValueError("this bundle requires macOS arm64")
    runtime = Path(tempfile.mkdtemp(prefix="livi-bt-dev-paired-", dir="/tmp"))
    os.chmod(runtime, 0o700)
    (runtime / "home").mkdir(mode=0o700)
    env = launch_environment(runtime, os.environ)
    print(f"Isolated runtime/config: {runtime}", flush=True)
    print("Opening Devices for read-only paired inventory; no hardware authorization enabled.",
          flush=True)
    os.chdir(runtime)
    os.execve(ui, [str(ui)], env)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError) as error:
        print(f"Paired-discovery launcher: {error}", file=sys.stderr)
        sys.exit(1)
