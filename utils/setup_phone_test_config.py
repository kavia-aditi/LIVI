#!/usr/bin/env python3
"""Local-only configuration setup. No network, Bluetooth, subprocesses or logging."""
import argparse
import contextlib
import getpass
import ipaddress
import json
import os
from pathlib import Path
import re
import stat
import sys
import unicodedata
import warnings

MAX_BYTES = 2048
DIRECTORY = "LIVI-private"
FILENAME = "private-config.json"
FIELDS = {"ssid", "passphrase", "security", "channel", "bssid", "ap_ip",
          "port", "phone_address"}
PHONE_PINS = {"instance_id", "serial"}
CHANNELS = set(range(1, 14)) | {36, 40, 44, 48, 149, 153, 157, 161, 165}


class Refused(Exception):
    """Never attach input values or underlying exception text."""


class RedactedParser(argparse.ArgumentParser):
    def error(self, _message):
        self.exit(2, "FAIL: command arguments; use create or validate without values\n")


def require(ok):
    if not ok:
        raise Refused()


def mac_valid(value):
    if not re.fullmatch(r"(?:[0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}", value):
        return False
    address = bytes.fromhex(value.replace(":", ""))
    return address not in (bytes(6), b"\xff" * 6) and not address[0] & 1


def validate_values(value):
    require(type(value) is dict and set(value) in (FIELDS, FIELDS | PHONE_PINS))
    for key in set(value) - {"channel", "port"}:
        require(type(value[key]) is str)
    require(type(value["channel"]) is int and value["channel"] in CHANNELS)
    require(type(value["port"]) is int and value["port"] == 5277)
    require(value["security"] == "wpa2")
    ssid = value["ssid"]
    require(1 <= len(ssid.encode("utf-8")) <= 32)
    require(not any(unicodedata.category(c) == "Cc" for c in ssid))
    password = value["passphrase"]
    require(8 <= len(password) <= 63 and all(32 <= ord(c) <= 126 for c in password))
    require(mac_valid(value["bssid"]) and mac_valid(value["phone_address"]))
    ip = ipaddress.IPv4Address(value["ap_ip"])
    require(any(ip in ipaddress.IPv4Network(net)
                for net in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")))
    require(ip.packed[-1] not in (0, 255))
    for key in set(value) & PHONE_PINS:
        require(1 <= len(value[key]) <= 128
                and all(33 <= ord(c) <= 126 for c in value[key]))


def unique_pairs(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result


def reject_constant(_value):
    raise Refused()


def decode(raw):
    require(len(raw) <= MAX_BYTES)
    value = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_pairs,
                       parse_constant=reject_constant)
    validate_values(value)
    return value


@contextlib.contextmanager
def private_umask():
    previous = os.umask(0o077)
    try:
        yield
    finally:
        os.umask(previous)


def directory_flags():
    return os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW


@contextlib.contextmanager
def home_descriptor(home):
    """Open every path component without following symlinks, including ancestors."""
    home = Path(home)
    require(home.is_absolute() and ".." not in home.parts)
    fd = os.open("/", directory_flags())
    try:
        for part in home.parts[1:]:
            next_fd = os.open(part, directory_flags(), dir_fd=fd)
            os.close(fd)
            fd = next_fd
        meta = os.fstat(fd)
        require(meta.st_uid == os.geteuid() and not stat.S_IMODE(meta.st_mode) & 0o022)
        yield fd
    finally:
        os.close(fd)


@contextlib.contextmanager
def private_directory(home, create=False):
    with home_descriptor(home) as parent:
        if create:
            with private_umask():
                try:
                    os.mkdir(DIRECTORY, 0o700, dir_fd=parent)
                except FileExistsError:
                    pass
        fd = os.open(DIRECTORY, directory_flags(), dir_fd=parent)
        try:
            meta = os.fstat(fd)
            require(stat.S_ISDIR(meta.st_mode) and meta.st_uid == os.geteuid()
                    and stat.S_IMODE(meta.st_mode) == 0o700)
            yield fd
        finally:
            os.close(fd)


def file_metadata(fd):
    meta = os.fstat(fd)
    require(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.geteuid()
            and stat.S_IMODE(meta.st_mode) == 0o600 and meta.st_nlink == 1
            and 0 < meta.st_size <= MAX_BYTES)
    return meta


def validate_directory_file(directory):
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
    fd = os.open(FILENAME, flags, dir_fd=directory)
    try:
        before = file_metadata(fd)
        raw = bytearray()
        while len(raw) <= MAX_BYTES:
            chunk = os.read(fd, MAX_BYTES + 1 - len(raw))
            if not chunk:
                break
            raw.extend(chunk)
        decode(bytes(raw))
        after = file_metadata(fd)
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns)
                == (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns))
        named = os.stat(FILENAME, dir_fd=directory, follow_symlinks=False)
        require((named.st_dev, named.st_ino) == (after.st_dev, after.st_ino))
    finally:
        os.close(fd)


def require_absent(directory):
    try:
        os.stat(FILENAME, dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return
    raise Refused()


def save(directory, value):
    validate_values(value)
    raw = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n"
    decode(raw)
    # An existing file, symlink or hard link is never replaced or repaired.
    with private_umask():
        fd = os.open(FILENAME, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=directory)
    try:
        view = memoryview(raw)
        while view:
            written = os.write(fd, view)
            require(written > 0)
            view = view[written:]
        os.fsync(fd)
        file_metadata(fd)
    finally:
        os.close(fd)
    # Failed/partial new files stay private and are never silently overwritten.
    validate_directory_file(directory)


def collect():
    require(sys.stdin.isatty() and sys.stderr.isatty())
    value = {"security": "wpa2", "port": 5277}
    prompts = (
        ("ssid", "Existing Wi-Fi SSID (hidden): "),
        ("passphrase", "Existing WPA2 password (hidden): "),
        ("bssid", "Existing AP BSSID (hidden): "),
        ("channel", "Existing AP channel (hidden): "),
        ("ap_ip", "Mac assigned private IPv4, NOT router IP (hidden): "),
        ("phone_address", "Already-paired OnePlus Bluetooth MAC (hidden): "),
    )
    with warnings.catch_warnings():
        # Refuse rather than allowing getpass's echoing fallback.
        warnings.simplefilter("error", getpass.GetPassWarning)
        for key, prompt in prompts:
            value[key] = getpass.getpass(prompt, stream=sys.stderr)
            if key == "passphrase":
                require(value[key] == getpass.getpass("Confirm WPA2 password (hidden): ",
                                                     stream=sys.stderr))
    require(re.fullmatch(r"[0-9]+", value["channel"]) is not None)
    value["channel"] = int(value["channel"])
    validate_values(value)
    return value


def main(argv=None):
    parser = RedactedParser(description=__doc__)
    parser.add_argument("operation", choices=("create", "validate"))
    args = parser.parse_args(argv)
    try:
        require(os.geteuid() != 0)
        home = os.environ["HOME"]
        with private_directory(home, create=args.operation == "create") as directory:
            if args.operation == "create":
                require_absent(directory)
                save(directory, collect())
            validate_directory_file(directory)
        print("PASS: private directory ownership and 0700 permissions")
        print("PASS: regular non-symlink single-link file ownership and 0600 permissions")
        print("PASS: JSON schema and 2048-byte limit")
        print("PASS: local configuration validation; no hardware launched")
        return 0
    except (Exception, KeyboardInterrupt):
        # Never format exceptions: JSON, OS and encoding errors may contain input.
        print("FAIL: local configuration checks; no contents displayed")
        return 1


if __name__ == "__main__":
    sys.exit(main())
