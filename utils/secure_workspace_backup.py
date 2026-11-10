#!/usr/bin/env python3
"""Create and verify a source-only backup; never launch application or radio code."""
import csv
from datetime import datetime, timezone
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import tarfile

ROOT = Path(__file__).resolve().parents[1]
BACKUPS = ROOT / ".workspace-backups"
SOURCE_ROOTS = {
    "native", "scripts", "utils", "assets", "docs", "demos", "kavia-docs",
    ".github", ".githooks", ".vscode", ".kavia",
}
PRUNED = {
    "target", "node_modules", "__pycache__", ".git", ".workspace-backups",
}
CONFIG_SUFFIXES = {".json", ".toml", ".yaml", ".yml", ".ini", ".conf", ".cfg"}
SESSION_REPORTED_EDITS = {
    "native/crates/livi-ui/src/bluetooth_app.rs",
    "native/crates/livi-ui/ui/app.slint",
    "native/crates/livi-ui/ui/bluetooth-test.slint",
    "utils/livi_bluetooth_app_test.py",
    "utils/test_livi_bluetooth_app_test.py",
    "utils/README-livi-bluetooth-app-test.md",
    "utils/cleanup_approved_build_cache.py",
}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def metadata(path):
    s = path.lstat()
    return (s.st_dev, s.st_ino, s.st_mode, s.st_size, s.st_mtime_ns, s.st_ctime_ns)


def walk():
    """Inventory all files, without following directory symlinks or reading contents."""
    for directory, dirs, files in os.walk(ROOT, followlinks=False):
        rel = Path(directory).relative_to(ROOT)
        if rel == Path("."):
            dirs[:] = [d for d in dirs if d != ".workspace-backups"]
        for name in list(dirs):
            p = Path(directory) / name
            if p.is_symlink():
                yield p
                dirs.remove(name)
        for name in files:
            yield Path(directory) / name


def exclusion(rel, mode):
    parts = rel.parts
    low = rel.name.lower()
    if not stat.S_ISREG(mode) and not stat.S_ISLNK(mode):
        return "non-regular filesystem object"
    if low == ".ds_store":
        return "filesystem metadata"
    if any(p.endswith((".app", ".dmg")) for p in parts):
        return "production/application artifact"
    if any(p in PRUNED for p in parts):
        return "build/dependency/generated output"
    if low.startswith(".env") or low in {
        "credentials", "credentials.toml", "id_rsa", "id_ed25519",
    }:
        return "potential private configuration"
    if rel.suffix.lower() in {".pem", ".key", ".p12", ".pfx"}:
        return "potential private key material"
    if rel.suffix.lower() in CONFIG_SUFFIXES and not any(
        word in low for word in ("template", "example", "sample")
    ) and any(word in low for word in ("private", "secret", "credential", "phone-config")):
        return "potential private configuration"
    if parts[0] in {"dist", "artifacts", "temp-attachments", "out", ".idea"}:
        return "preserved artifact/evidence/private or local state"
    if len(parts) > 1 and parts[0] not in SOURCE_ROOTS:
        return "outside selected source/configuration roots"
    if len(parts) == 1 and rel.suffix.lower() in {".log", ".dmg", ".zip", ".gz"}:
        return "artifact or evidence"
    if low.endswith((".log", ".jsonl", ".pyc")):
        return "diagnostic/generated evidence"
    return None


def categories(name):
    p = name.lower()
    groups = []
    if any(s in p for s in (
        "paired", "discovery", "livi-native-bluetooth", "bluetooth_app",
    )):
        groups.append("paired-device discovery")
    if any(s in p for s in (
        "diagnostic", "bluetooth_app", "bluetooth-app", "bluetooth_app_test",
        "livi-bt-diagnostic", "bluetooth-test.slint",
    )):
        groups.append("Bluetooth diagnostics")
    if any(s in p for s in (
        "native_tcp", "native_handoff", "native-handoff", "native-bt-dev",
        "native_bt_dev", "server/admission", "native_bluetooth",
    )):
        groups.append("listener-only transport / admission")
    if any(s in p for s in (
        "native_tcp", "native_handoff", "native-handoff", "native_identity",
        "phone-identity", "src/wpp.rs", "src/tls.rs", "helperd/src/aa.rs",
        "wireless_setup.proto", "server/admission",
    )):
        groups.append("authenticated handoff (including blocked scaffolding)")
    return groups


def write_json(path, value):
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def read_regular(path, expected):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        if metadata(path) != expected:
            raise RuntimeError(f"Source changed before reading: {path.relative_to(ROOT)}")
        data = stream.read()
        if metadata(path) != expected:
            raise RuntimeError(f"Source changed while reading: {path.relative_to(ROOT)}")
    return data


def main():
    os.umask(0o077)
    if BACKUPS.is_symlink():
        raise RuntimeError("Refusing symlink backup root")
    BACKUPS.mkdir(mode=0o700, exist_ok=True)
    now = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = BACKUPS / ("workspace-safety-" + now)
    output.mkdir(mode=0o700)  # Never reuse or overwrite an existing run.
    before = {str(p.relative_to(ROOT)): metadata(p) for p in walk()}
    selected, excluded = [], []
    for name, state in sorted(before.items()):
        reason = exclusion(Path(name), state[2])
        if reason:
            excluded.append({"path": name, "reason": reason, "metadata": state})
        else:
            selected.append(name)
    size = sum(before[n][3] for n in selected)
    free = os.statvfs(ROOT).f_bavail * os.statvfs(ROOT).f_frsize
    if free < size * 2 + 256 * 1024 * 1024:
        raise RuntimeError("Insufficient free space; no existing output will be deleted")
    archive = output / "source-config.tar.gz"
    records = []
    with tarfile.open(archive, "x:gz", dereference=False) as tar:
        for name in selected:
            path = ROOT / name
            state = before[name]
            info = tar.gettarinfo(str(path), arcname=name)
            if stat.S_ISLNK(state[2]):
                target = os.readlink(path)
                if metadata(path) != state:
                    raise RuntimeError(f"Symlink changed: {name}")
                info.linkname = target
                tar.addfile(info)
                records.append({
                    "path": name, "type": "symlink", "target": target,
                    "sha256": None, "size": 0, "categories": categories(name),
                })
            else:
                data = read_regular(path, state)
                tar.addfile(info, io.BytesIO(data))
                records.append({
                    "path": name, "type": "file", "sha256": digest(data),
                    "size": len(data), "categories": categories(name),
                })
    expected = {r["path"]: r for r in records}
    verified = set()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar:
            r = expected[member.name]
            if member.name in verified:
                raise RuntimeError("Duplicate archive member")
            if r["type"] == "file":
                with tar.extractfile(member) as stream:
                    if digest(stream.read()) != r["sha256"]:
                        raise RuntimeError(f"Archive hash mismatch: {member.name}")
                if digest(read_regular(ROOT / member.name, before[member.name])) != r["sha256"]:
                    raise RuntimeError(f"Source hash mismatch: {member.name}")
            elif not member.issym() or member.linkname != r["target"]:
                raise RuntimeError(f"Symlink mismatch: {member.name}")
            verified.add(member.name)
    if verified != set(expected):
        raise RuntimeError("Archive inventory mismatch")
    after = {str(p.relative_to(ROOT)): metadata(p) for p in walk()}
    if before != after:
        raise RuntimeError("Workspace metadata changed during backup; retain output for inspection")
    with (output / "SHA256SUMS").open("x", encoding="utf-8") as stream:
        for r in records:
            if r["type"] == "file":
                stream.write(f'{r["sha256"]}  {r["path"]}\n')
    with (output / "source-inventory.csv").open("x", newline="", encoding="utf-8") as stream:
        writer = csv.writer(stream)
        writer.writerow(["path", "type", "bytes", "sha256", "scope", "baseline_status"])
        for r in records:
            writer.writerow([
                r["path"], r["type"], r["size"], r["sha256"] or "",
                "; ".join(r["categories"]), "no historical baseline available",
            ])
    with (output / "preserved-exclusions.jsonl").open("x", encoding="utf-8") as stream:
        for item in excluded:
            stream.write(json.dumps(item, sort_keys=True) + "\n")
    write_json(output / "changed-file-summary.json", {
        "historical_diff_status": "UNAVAILABLE: no Git history or checkpoint-15 baseline",
        "warning": "Scope membership is not proof of modification, addition, or deletion.",
        "only_file_added_by_this_invocation": "utils/secure_workspace_backup.py",
        "earlier_session_reported_edits_not_baseline_verified": sorted(
            SESSION_REPORTED_EDITS.intersection(selected)
        ),
        "scope_mapping": [
            {"path": r["path"], "categories": r["categories"]}
            for r in records if r["categories"]
        ],
        "all_current_backed_up_files": "source-inventory.csv",
        "deletions_renames_and_unrecorded_prior_edits": "not determinable without baseline",
    })
    archive_hash = hashlib.file_digest(archive.open("rb"), "sha256").hexdigest()
    with (output / "ARCHIVE-SHA256SUMS").open("x", encoding="utf-8") as stream:
        stream.write(f"{archive_hash}  {archive.name}\n")
    write_json(output / "backup-report.json", {
        "schema": 1, "created_utc": now, "workspace": str(ROOT),
        "archive": archive.name, "archive_sha256": archive_hash,
        "manifest": "SHA256SUMS", "backed_up_entries": len(records),
        "regular_files": sum(r["type"] == "file" for r in records),
        "symlinks": sum(r["type"] == "symlink" for r in records),
        "source_bytes": size, "excluded_entries": len(excluded),
        "archive_hashes_verified": True, "source_hashes_verified": True,
        "workspace_metadata_unchanged_during_backup": True,
        "excluded_contents_read": False,
        "build_executed": False, "hardware_accessed": False,
        "historical_change_baseline_available": False,
        "scope_roots": sorted(SOURCE_ROOTS),
    })
    print(json.dumps({
        "backup": str(output), "archive": str(archive),
        "manifest": str(output / "SHA256SUMS"),
        "regular_files": sum(r["type"] == "file" for r in records),
        "symlinks": sum(r["type"] == "symlink" for r in records),
        "excluded_entries": len(excluded), "archive_sha256": archive_hash,
        "verification": "PASS",
    }, indent=2))


if __name__ == "__main__":
    main()
