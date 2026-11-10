#!/usr/bin/env python3
"""Constrained, explicitly approved non-browser cleanup; never runs LIVI."""
import argparse
from collections import Counter
import os
from pathlib import Path
import shutil
import stat
import subprocess

ROOT = Path(__file__).absolute().parents[1]
EXPECTED_ROOT = Path("/Users/aditimishra/Downloads/LIVI-main")
HOME = Path("/Users/aditimishra")
BUILD_ROOTS = (
    "native/target",
    "dist/candidates/stage6-20261103T085753Z-83780/target",
    "dist/candidates/stage6-corrected-20261103T092037Z-2173/target",
    "temp-attachments/macos-paired-launch-20261103-164726/target",
    "temp-attachments/macos-paired-discovery-20261103-162923/ui-target",
    "temp-attachments/macos-paired-discovery-20261103-162923/target",
    "temp-attachments/bt-cleanup-snapshot-validation-20261103-01",
    "temp-attachments/bt-diagnostic-validation-20261103-01",
    "temp-attachments/bt-error-report-validation-20261103-01",
    "temp-attachments/bt-listener-validation-20261103-192401",
    "temp-attachments/bt-sdp-callback-audit-validation-20261103-01",
    "temp-attachments/bt-sdp-lifecycle-validation-20261103-01",
    "temp-attachments/bt-transport-validation-20261103-190815",
)
CACHE_ROOTS = (
    HOME / "Library/Caches/Homebrew/downloads",
    HOME / ".npm/_cacache/content-v2",
    HOME / "Library/Caches/pip/http-v2",
)
PROTECTED_PARTS = {"livi-app-bt-01"}
COMPILED_SUFFIXES = {".rlib", ".rmeta", ".o", ".a", ".bc"}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def signature(meta):
    return (
        meta.st_dev, meta.st_ino, meta.st_mode, meta.st_uid,
        meta.st_nlink, meta.st_size, meta.st_mtime_ns, meta.st_ctime_ns,
    )


def safe_chain(path):
    """Refuse symlinked ancestors, including the approved root itself."""
    for parent in reversed((path, *path.parents)):
        meta = parent.lstat()
        require(stat.S_ISDIR(meta.st_mode), "Unsafe directory chain")
        require(not stat.S_ISLNK(meta.st_mode), "Symlinked directory refused")


def files_under(root):
    for directory, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = sorted(
            name for name in dirs
            if not (Path(directory) / name).is_symlink()
        )
        for name in sorted(files):
            yield Path(directory) / name


def has_ready_ancestor(path):
    return any((parent / "ready.json").exists() for parent in path.parents
               if parent != ROOT and ROOT in parent.parents)


def read_header(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        meta = os.fstat(fd)
        require(stat.S_ISREG(meta.st_mode), "Non-regular file refused")
        return os.read(fd, 512), signature(meta)
    finally:
        os.close(fd)


def compiled_header(suffix, data):
    if suffix in {".rlib", ".a"}:
        return data.startswith(b"!<arch>\n")
    if suffix == ".rmeta":
        return data.startswith(b"rust")
    if suffix == ".bc":
        return data.startswith((b"BC\xc0\xde", b"\xde\xc0\x17\x0b"))
    if suffix == ".o":
        if data.startswith(b"\xcf\xfa\xed\xfe"):
            return len(data) >= 16 and int.from_bytes(data[12:16], "little") == 1
        if data.startswith(b"\xfe\xed\xfa\xcf"):
            return len(data) >= 16 and int.from_bytes(data[12:16], "big") == 1
        if data.startswith(b"\x7fELF") and len(data) >= 18:
            order = {1: "little", 2: "big"}.get(data[5])
            return order is not None and int.from_bytes(data[16:18], order) == 1
    return False


def download_header(data):
    return data.startswith((
        b"\x1f\x8b", b"PK\x03\x04", b"\xfd7zXZ\x00",
        b"BZh", b"\x28\xb5\x2f\xfd",
    ))


def ensure_idle():
    output = subprocess.check_output(
        ["ps", "-axo", "comm="], text=True
    )
    busy = {"cargo", "rustc", "rustup", "brew", "npm", "npx", "pip", "pip3"}
    require(not any(Path(line.strip()).name in busy for line in output.splitlines()),
            "Build/download process active; cleanup refused")


def make_plan():
    plan = []
    totals = Counter()
    for root, is_build in (
        *((ROOT / name, True) for name in BUILD_ROOTS),
        *((path, False) for path in CACHE_ROOTS),
    ):
        safe_chain(root)
        require(root.stat().st_uid == os.geteuid(), "Unowned root refused")
        for path in files_under(root):
            meta = path.lstat()
            if (not stat.S_ISREG(meta.st_mode)
                    or meta.st_uid != os.geteuid()
                    or meta.st_nlink != 1
                    or meta.st_mode & 0o111):
                continue
            if is_build:
                if path.suffix not in COMPILED_SUFFIXES:
                    continue
                if PROTECTED_PARTS.intersection(path.parts) or has_ready_ancestor(path):
                    continue
            else:
                # Preserve npm metadata/indexes, pip metadata, logs, source,
                # and package-manager state. Never enter _npx.
                if root == CACHE_ROOTS[2] and path.suffix != ".body":
                    continue
                if root == CACHE_ROOTS[0] and not path.name.endswith((
                    ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz",
                    ".zip", ".whl",
                )):
                    continue
                if "livi" in path.name.lower():
                    continue
            data, opened = read_header(path)
            if opened != signature(meta):
                raise RuntimeError("Candidate changed during inspection")
            eligible = (compiled_header(path.suffix, data) if is_build
                        else download_header(data))
            if eligible:
                plan.append((path, opened, meta.st_blocks * 512))
                totals[str(root)] += meta.st_blocks * 512
    return plan, totals


def project_snapshot(excluded):
    """Metadata only: never read private settings or print their contents."""
    snapshot = {}
    for path in files_under(ROOT):
        if path not in excluded:
            snapshot[path] = signature(path.lstat())
    return snapshot


def apply(plan):
    ensure_idle()
    excluded = {path for path, _, _ in plan}
    protected = project_snapshot(excluded)
    deleted = 0
    for path, original, _ in plan:
        safe_chain(path.parent)
        require(signature(path.lstat()) == original, "Candidate changed; stopped")
        parent_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            current = os.stat(path.name, dir_fd=parent_fd, follow_symlinks=False)
            require(signature(current) == original, "Candidate changed; stopped")
            os.unlink(path.name, dir_fd=parent_fd)
            deleted += 1
        finally:
            os.close(parent_fd)
    for path, original in protected.items():
        require(signature(path.lstat()) == original,
                "Protected project file changed; inspect before proceeding")
    print(f"Removed {deleted} approved intermediate/download files.")
    print(f"Verified metadata unchanged for {len(protected)} protected project files.")
    print("No directories, executables, dynamic libraries, source, manifests, or logs removed.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply-approved-non-browser", action="store_true")
    args = parser.parse_args()
    require(ROOT == EXPECTED_ROOT and os.geteuid() != 0, "Unexpected checkout or root user")
    ensure_idle()
    plan, totals = make_plan()
    for root, size in totals.items():
        print(f"{size / 2**30:.3f} GiB  {root}", flush=True)
    allocated = sum(size for _, _, size in plan)
    print(f"Candidate allocated total: {allocated / 2**30:.3f} GiB; {len(plan)} files",
          flush=True)
    print("Preserving _npx, browser caches, Cargo registry, all ready directories, "
          "and livi-app-bt-01.", flush=True)
    before = shutil.disk_usage(ROOT).free
    if args.apply_approved_non_browser:
        apply(plan)
        after = shutil.disk_usage(ROOT).free
        print(f"Free before: {before / 2**30:.3f} GiB; after: {after / 2**30:.3f} GiB")
        print(f"Observed free-space increase: {(after - before) / 2**30:.3f} GiB")
    else:
        print("DRY RUN ONLY: no files changed.")


if __name__ == "__main__":
    main()
