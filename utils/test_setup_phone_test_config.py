"""Synthetic fixtures only; never reads the operator's private configuration."""
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import setup_phone_test_config as setup


def fixture():
    return {
        "ssid": "offline-fixture", "passphrase": "offline-test-only",
        "security": "wpa2", "channel": 36, "bssid": "02:00:00:00:00:01",
        "ap_ip": "192.168.50.2", "port": 5277,
        "phone_address": "02:00:00:00:00:02",
        "instance_id": "offline-receiver", "serial": "offline-serial",
    }


class SetupTests(unittest.TestCase):
    def home(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        return Path(temp.name).resolve()

    def test_valid_fixture_and_supported_channels(self):
        for channel in setup.CHANNELS:
            value = fixture()
            value["channel"] = channel
            setup.validate_values(value)

    def test_schema_rejects_wrong_types_values_and_unknown_fields(self):
        changes = {
            "ssid": ["", "é" * 17, "bad\nssid", 7],
            "passphrase": ["short", "x" * 64, "bad\tpassword", "é" * 10],
            "security": ["open", "WPA2"],
            "channel": [True, "36", 14, 35],
            "port": [True, "5277", 5278],
            "bssid": ["00:00:00:00:00:00", "03:00:00:00:00:01"],
            "phone_address": ["ff:ff:ff:ff:ff:ff", "not-an-address"],
            "ap_ip": ["127.0.0.1", "8.8.8.8", "192.168.1.0", "192.168.1.255",
                      "192.0.2.1", "169.254.1.2", "::1"],
            "instance_id": ["", "has space", "é", "x" * 129],
            "serial": ["", "has\nnewline", "x" * 129],
            "unknown": ["refuse"],
        }
        for key, values in changes.items():
            for bad in values:
                value = fixture()
                value[key] = bad
                with self.subTest(field=key), self.assertRaises(Exception):
                    setup.validate_values(value)

    def test_missing_fields_rejected(self):
        for key in setup.FIELDS:
            value = fixture()
            del value[key]
            with self.assertRaises(setup.Refused):
                setup.validate_values(value)

    def test_identity_pins_are_optional_paired_and_never_null_or_empty(self):
        value = fixture()
        for key in setup.PHONE_PINS:
            del value[key]
        self.assertEqual(setup.decode(json.dumps(value).encode()), value)
        for pins in ({"instance_id": "phone"},
                     {"serial": "phone"},
                     {"instance_id": None, "serial": None},
                     {"instance_id": "", "serial": ""}):
            with self.subTest(keys=sorted(pins)), self.assertRaises(setup.Refused):
                setup.validate_values(dict(value, **pins))

    def test_invalid_json_duplicate_fields_constants_and_oversize_rejected(self):
        for raw in (b"{", b"\xff", b"[]" , b'{"port":5277,"port":5277}',
                    b'{"ssid":NaN}', b"x" * 2049):
            with self.assertRaises(Exception):
                setup.decode(raw)

    def test_directory_and_file_modes_roundtrip(self):
        home = self.home()
        with setup.private_directory(home, create=True) as directory:
            setup.save(directory, fixture())
            setup.validate_directory_file(directory)
        self.assertEqual((home / setup.DIRECTORY).stat().st_mode & 0o7777, 0o700)
        self.assertEqual((home / setup.DIRECTORY / setup.FILENAME).stat().st_mode & 0o7777,
                         0o600)

    def test_existing_file_never_overwritten(self):
        home = self.home()
        with setup.private_directory(home, create=True) as directory:
            setup.save(directory, fixture())
            before = (home / setup.DIRECTORY / setup.FILENAME).read_bytes()
            with self.assertRaises(FileExistsError):
                setup.save(directory, fixture())
            self.assertEqual((home / setup.DIRECTORY / setup.FILENAME).read_bytes(), before)

    def test_wrong_directory_permissions_refused_not_repaired(self):
        home = self.home()
        path = home / setup.DIRECTORY
        path.mkdir(mode=0o755)
        path.chmod(0o755)
        with self.assertRaises(setup.Refused):
            with setup.private_directory(home):
                self.fail("unsafe directory accepted")
        self.assertEqual(path.stat().st_mode & 0o7777, 0o755)

    def test_wrong_file_permissions_refused_not_repaired(self):
        home = self.home()
        with setup.private_directory(home, create=True) as directory:
            setup.save(directory, fixture())
            path = home / setup.DIRECTORY / setup.FILENAME
            for mode in (0o644, 0o400, 0o660):
                path.chmod(mode)
                with self.assertRaises(setup.Refused):
                    setup.validate_directory_file(directory)
                self.assertEqual(path.stat().st_mode & 0o7777, mode)

    def test_symlinked_directory_and_home_refused(self):
        home = self.home()
        other = home / "other"
        other.mkdir(mode=0o700)
        (home / setup.DIRECTORY).symlink_to(other)
        with self.assertRaises(OSError):
            with setup.private_directory(home):
                self.fail("directory symlink accepted")
        alias = home / "alias"
        alias.symlink_to(other)
        with self.assertRaises(OSError):
            with setup.private_directory(alias, create=True):
                self.fail("home symlink accepted")

    def test_symlink_file_refused_without_reading_or_overwriting_target(self):
        home = self.home()
        target = home / "target"
        target.write_bytes(b"synthetic-not-a-config")
        with setup.private_directory(home, create=True) as directory:
            (home / setup.DIRECTORY / setup.FILENAME).symlink_to(target)
            with self.assertRaises(OSError):
                setup.validate_directory_file(directory)
            with self.assertRaises(FileExistsError):
                setup.save(directory, fixture())
            self.assertEqual(target.read_bytes(), b"synthetic-not-a-config")

    def test_hardlinked_file_refused(self):
        home = self.home()
        with setup.private_directory(home, create=True) as directory:
            setup.save(directory, fixture())
            path = home / setup.DIRECTORY / setup.FILENAME
            os.link(path, home / "another-link")
            with self.assertRaises(setup.Refused):
                setup.validate_directory_file(directory)

    def test_oversize_and_malformed_saved_files_refused(self):
        home = self.home()
        with setup.private_directory(home, create=True) as directory:
            setup.save(directory, fixture())
            path = home / setup.DIRECTORY / setup.FILENAME
            for raw in (b"x" * 2049, b"{", b""):
                path.write_bytes(raw)
                with self.assertRaises(Exception):
                    setup.validate_directory_file(directory)

    def test_noninteractive_collection_refused_before_prompt(self):
        with patch.object(setup.sys.stdin, "isatty", return_value=False), \
                patch.object(setup.getpass, "getpass") as prompt:
            with self.assertRaises(setup.Refused):
                setup.collect()
            prompt.assert_not_called()

    def test_collected_values_have_no_invented_identifiers_or_echo(self):
        value = fixture()
        for key in setup.PHONE_PINS:
            del value[key]
        answers = [value["ssid"], value["passphrase"], value["passphrase"],
                   value["bssid"], str(value["channel"]), value["ap_ip"],
                   value["phone_address"]]
        captured = io.StringIO()
        with patch.object(setup.sys.stdin, "isatty", return_value=True), \
                patch.object(setup.sys.stderr, "isatty", return_value=True), \
                patch.object(setup.getpass, "getpass", side_effect=answers), \
                contextlib.redirect_stdout(captured):
            self.assertEqual(setup.collect(), value)
        self.assertEqual(captured.getvalue(), "")

    def test_main_errors_and_success_never_echo_values(self):
        home = self.home()
        with setup.private_directory(home, create=True) as directory:
            setup.save(directory, fixture())
        with patch.dict(os.environ, {"HOME": str(home)}):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(setup.main(["validate"]), 0)
            for value in fixture().values():
                if isinstance(value, str):
                    self.assertNotIn(value, output.getvalue())
            path = home / setup.DIRECTORY / setup.FILENAME
            path.write_text('{"passphrase":"synthetic-private-marker",')
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(setup.main(["validate"]), 1)
            self.assertEqual(output.getvalue(),
                             "FAIL: local configuration checks; no contents displayed\n")


if __name__ == "__main__":
    unittest.main()
