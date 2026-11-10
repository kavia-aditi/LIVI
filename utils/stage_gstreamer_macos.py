#!/usr/bin/env python3
"""Stage checksum-verified, pkgutil-expanded official macOS GStreamer packages.

This does not install packages or execute their installer scripts. Verify downloaded
packages against upstream .sha256sum files before calling this utility.
"""

import argparse
import filecmp
import os
from pathlib import Path
import shutil
import xml.etree.ElementTree as ET

FRAMEWORK = "/Library/Frameworks/GStreamer.framework"
PREFIX = FRAMEWORK + "/Versions/1.0"


def merge(source, target, development=False, relative=Path()):
    for item in source.iterdir():
        rel = relative / item.name
        if development and not (
            rel.parts[0] == "include"
            or str(rel) in ("lib", "lib/pkgconfig", "lib/glib-2.0", "lib/gstreamer-1.0")
            or str(rel).startswith((
                "lib/pkgconfig/", "lib/glib-2.0/include", "lib/gstreamer-1.0/include"
            ))
        ):
            continue
        dest = target / item.name
        if item.is_symlink():
            link = os.readlink(item)
            if dest.is_symlink() and os.readlink(dest) == link:
                continue
            if dest.exists() or dest.is_symlink():
                raise ValueError(f"conflicting staged symlink: {dest}")
            dest.symlink_to(link)
        elif item.is_dir():
            dest.mkdir(exist_ok=True)
            merge(item, dest, development, rel)
        else:
            if dest.exists() and not filecmp.cmp(item, dest, shallow=False):
                relocated_metadata = (
                    item.suffix == ".pc"
                    and item.read_text().replace(PREFIX, str(target.parents[1]))
                    == dest.read_text()
                )
                if not relocated_metadata:
                    raise ValueError(f"conflicting staged file: {dest}")
            shutil.copy2(item, dest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path)
    parser.add_argument("--development", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--cleanup-extracted", action="store_true")
    args = parser.parse_args()
    if args.runtime is None and args.development is None:
        parser.error("at least one expanded package is required")
    output = args.output.resolve()
    for directory in (args.runtime, args.development):
        if directory is not None:
            if not list(directory.glob("*.pkg/PackageInfo")):
                raise ValueError(f"no expanded package metadata in {directory}")
            if output.is_relative_to(directory.resolve()):
                raise ValueError("output must not be inside an extracted input")
    output.mkdir(parents=True, exist_ok=True)
    count = 0
    for directory, development in ((args.runtime, False), (args.development, True)):
        if directory is None:
            continue
        for metadata in sorted(directory.glob("*.pkg/PackageInfo")):
            info = ET.parse(metadata).getroot()
            if info.get("version") != args.version:
                raise ValueError(f"unexpected package version: {metadata}")
            location = info.get("install-location", "").rstrip("/")
            payload = metadata.parent / "Payload"
            if location == FRAMEWORK:
                payload = payload / "Versions/1.0"
            elif location != PREFIX:
                raise ValueError(f"unexpected install location {location}: {metadata}")
            if payload.is_dir():
                merge(payload, output, development)
                count += 1
    for metadata in output.glob("lib/pkgconfig/*.pc"):
        text = metadata.read_text()
        metadata.write_text(text.replace(PREFIX, str(output)))
    print(f"Staged {count} matching payloads at {output}")
    if args.cleanup_extracted:
        for directory in (args.runtime, args.development):
            if directory is not None:
                shutil.rmtree(directory)
                print(f"Removed extracted staging input: {directory}")


if __name__ == "__main__":
    main()
