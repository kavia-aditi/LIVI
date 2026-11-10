"""Explicit isolated decoded-video validation; never launches production LIVI."""
from pathlib import Path
import os
import shlex
import signal
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
NATIVE = ROOT / "native"
OUT = NATIVE / "target/debug"
FIXTURE = OUT / "experimental-local-frame.h264"
BINARY = OUT / "examples/experimental_decode_metal"
env = os.environ.copy()
env["LIVI_GST_SWDEC"] = "1"
env["PKG_CONFIG_PATH"] = (
    "/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/pkgconfig"
    + (":" + env["PKG_CONFIG_PATH"] if env.get("PKG_CONFIG_PATH") else "")
)


def run(command, timeout, cwd=ROOT):
    command = list(map(str, command))
    print("COMMAND", shlex.join(command), flush=True)
    process = subprocess.Popen(command, cwd=cwd, env=env, start_new_session=True)
    try:
        result = process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        # Only this driver's newly created process group; never installed processes.
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        raise
    if result:
        raise subprocess.CalledProcessError(result, command)


try:
    run([
        "gst-launch-1.0", "-q", "-e", "videotestsrc", "num-buffers=1",
        "!", "video/x-raw,width=320,height=180,framerate=30/1",
        "!", "x264enc", "tune=zerolatency", "key-int-max=1",
        "bframes=0", "byte-stream=true", "!", "h264parse",
        "!", "video/x-h264,stream-format=byte-stream,alignment=au",
        "!", "filesink", f"location={FIXTURE}",
    ], 30)
    run([
        "cargo", "build", "--offline", "--locked", "-p", "livi-video-view",
        "--features", "experimental-metal-validation",
        "--example", "experimental_decode_metal",
    ], 180, cwd=NATIVE)
    run(["/usr/bin/time", "-l", BINARY, FIXTURE], 40)
except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as error:
    print(f"VALIDATION FAILED OR BLOCKED: {error}", file=sys.stderr)
    sys.exit(1)
