"""Temporary Linux cross-check adapter: translate Rust's target spelling for Zig."""
import os
import sys

args = [
    "--target=x86_64-linux-gnu"
    if arg == "--target=x86_64-unknown-linux-gnu"
    else arg
    for arg in sys.argv[1:]
]
os.execv(
    "/opt/homebrew/bin/zig",
    ["zig", "cc", "-target", "x86_64-linux-gnu", *args],
)
