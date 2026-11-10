"""Build a separate development-only app. Does not launch it or touch /Applications."""
from pathlib import Path
import argparse
import os
import plistlib
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
NATIVE = ROOT / "native"
APP = ROOT / "temp-attachments/LIVI Native Bluetooth Dev.app"

def run(*args, **kwargs):
    subprocess.run(args, check=True, **kwargs)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assemble-only", action="store_true",
                        help="assemble already-built debug binaries")
    args = parser.parse_args()
    if not args.assemble_only:
        run("cargo", "build", "-p", "livi-native-bt-dev", "-p", "livi-core",
            "-p", "livi-ui", "-p", "livi-gst-host", "--features", "livi-core/native-bt-dev",
            cwd=NATIVE, env={**os.environ, "CI": "1"})
    # Never overwrite a running proposal or installed app.
    if APP.exists():
        raise SystemExit(f"{APP} already exists; move it aside explicitly before rebuilding")
    binaries = {
        "MacOS/livi-native-bt-dev": "livi-native-bt-dev",
        "Resources/native-dev/LIVI": "LIVI",
        "Resources/native-dev/livi-core": "livi-core",
        "Resources/native-dev/livi-gst-host": "livi-gst-host",
    }
    for dst, src in binaries.items():
        source = NATIVE / "target/debug" / src
        if not source.is_file():
            raise SystemExit(f"missing development binary: {source}")
    for dst, src in binaries.items():
        dest = APP / "Contents" / dst
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(NATIVE / "target/debug" / src, dest)
        run("codesign", "--force", "--sign", "-", str(dest))
    info = {
        "CFBundleIdentifier": "dev.f-io.livi.native-bluetooth-development",
        "CFBundleName": "LIVI Native Bluetooth Dev",
        "CFBundleDisplayName": "LIVI Native Bluetooth Dev",
        "CFBundleExecutable": "livi-native-bt-dev",
        "CFBundlePackageType": "APPL",
        "CFBundleVersion": "1",
        "CFBundleShortVersionString": "0.1",
        "NSBluetoothAlwaysUsageDescription":
            "This separate LIVI development app uses Bluetooth to discover and pair your Android phone and bootstrap experimental wireless Android Auto.",
        "NSLocalNetworkUsageDescription":
            "This separate LIVI development app receives Android Auto over your provisioned Wi-Fi network.",
        "NSHighResolutionCapable": True,
    }
    with (APP / "Contents/Info.plist").open("wb") as file:
        plistlib.dump(info, file)
    run("codesign", "--force", "--sign", "-", str(APP))
    run("codesign", "--verify", "--deep", "--strict", str(APP))
    print(f"Built only; not launched: {APP}")
    print("Discovery: open -n <app> --args --scan-seconds 12")
    print("Projection: open -n <app> --args --config <private-config.json> --projection")

if __name__ == "__main__":
    main()
