"""Build and run only the isolated native Metal validation window."""
from pathlib import Path
import os
import shlex
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
OBJECT = ROOT / "native/target/debug/experimental-metal-view-bridge.o"
BINARY = ROOT / "native/target/debug/experimental-metal-runtime-validation"
env = os.environ.copy()
env["PKG_CONFIG_PATH"] = (
    "/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/pkgconfig"
    + (":" + env["PKG_CONFIG_PATH"] if env.get("PKG_CONFIG_PATH") else "")
)


def run(command, timeout):
    print("COMMAND", shlex.join(map(str, command)), flush=True)
    return subprocess.run(
        list(map(str, command)), cwd=ROOT, env=env, timeout=timeout, check=True
    )


try:
    print("COMMAND pkg-config --cflags gstreamer-1.0", flush=True)
    flags = subprocess.check_output(
        ["pkg-config", "--cflags", "gstreamer-1.0"],
        cwd=ROOT, env=env, text=True, timeout=10,
    )
    run([
        "xcrun", "clang++", "-c", "-std=c++17", "-mmacosx-version-min=11.0",
        *shlex.split(flags),
        ROOT / "native/crates/livi-video-view/src/gst_video_mac.mm",
        "-o", OBJECT,
    ], 60)
    run([
        "xcrun", "clang++", "-std=c++17", "-fobjc-arc",
        "-mmacosx-version-min=11.0", "-Wall", "-Wextra", "-Werror",
        ROOT / "utils/metal_runtime_validation.mm", OBJECT,
        "-framework", "Cocoa", "-framework", "QuartzCore",
        "-framework", "CoreImage", "-framework", "Metal", "-o", BINARY,
    ], 60)
    run(["/usr/bin/time", "-l", BINARY], 20)
except subprocess.TimeoutExpired as error:
    print(f"BLOCKER: isolated command exceeded its timeout: {error.cmd}", file=sys.stderr)
    sys.exit(2)
except subprocess.CalledProcessError as error:
    print(f"VALIDATION FAILED: exit={error.returncode}", file=sys.stderr)
    sys.exit(1)
