#!/usr/bin/env python3
"""Audit a self-contained macOS GStreamer runtime or app without loading plugins."""

import argparse
from pathlib import Path
import re
import subprocess
import sys

MACH_MAGICS = {
    b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe",
    b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe",
    b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",
    b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca",
}


def run(*args):
    return subprocess.run(
        args, check=True, capture_output=True, text=True, errors="replace"
    ).stdout


def is_system_dependency(name):
    return name.startswith(("/usr/lib/", "/System/Library/"))


def dependency_names(output):
    return re.findall(r"^\s+(\S+) \(compatibility version ", output, re.MULTILINE)


def rpath_names(output):
    return re.findall(
        r"cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset \d+\)", output
    )


def expand_path(name, image):
    for token in ("@loader_path", "@executable_path"):
        if name == token or name.startswith(token + "/"):
            return image.parent / name[len(token):].lstrip("/")
    return Path(name)


def resolve_dependency(name, image, rpaths, root):
    if name.startswith("@rpath/"):
        tail = name[len("@rpath/"):]
        candidates = [expand_path(path, image) / tail for path in rpaths]
    else:
        candidates = [expand_path(name, image)]
    for candidate in candidates:
        resolved = candidate.resolve()
        if resolved.is_relative_to(root) and resolved.is_file():
            return resolved
    return None


def audit(root, arch="arm64"):
    root = root.resolve(strict=True)
    errors = []
    images = set()
    for path in root.rglob("*"):
        if path.is_symlink():
            if not path.exists() or not path.resolve().is_relative_to(root):
                errors.append(f"broken or escaping symlink: {path.relative_to(root)}")
                continue
        if not path.is_file():
            continue
        with path.open("rb") as stream:
            if stream.read(4) in MACH_MAGICS:
                images.add(path.resolve())

    if not images:
        errors.append("no Mach-O files found")
    for image in sorted(images):
        label = image.relative_to(root)
        try:
            run("lipo", str(image), "-verify_arch", arch)
            deps = dependency_names(run("otool", "-arch", arch, "-L", str(image)))
            ids = run("otool", "-arch", arch, "-D", str(image)).splitlines()
            install_ids = {line.strip() for line in ids if line.startswith(("@", "/"))}
            rpaths = rpath_names(run("otool", "-arch", arch, "-l", str(image)))
            for name in deps:
                if name in install_ids or is_system_dependency(name):
                    continue
                if resolve_dependency(name, image, rpaths, root) is None:
                    errors.append(f"{label}: unresolved or external dependency {name}")
        except subprocess.CalledProcessError as error:
            detail = (error.stderr or error.stdout or "").strip()
            errors.append(f"{label}: {detail}")
    if errors:
        raise ValueError("\n".join(errors))
    return len(images)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--arch", choices=("arm64", "x86_64"), default="arm64")
    args = parser.parse_args()
    try:
        count = audit(args.root, args.arch)
    except (OSError, ValueError) as error:
        print(f"GStreamer dependency audit FAILED:\n{error}", file=sys.stderr)
        return 1
    print(f"GStreamer dependency audit passed: {count} {args.arch} Mach-O files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
