import argparse
import contextlib
import io
import json
from pathlib import Path
import plistlib
import struct
import tempfile
import unittest
from unittest import mock

import livi_bluetooth_app_test as app


def events(name="LIVI", features=None, paired=True, runner=True, success=True):
    rows = [
        {"reason": "compiler-artifact", "target": {"name": "livi_native_bluetooth"},
         "features": ["paired-diagnostic"] if paired else []},
        {"reason": "compiler-artifact", "target": {"name": name},
         "features": [app.FEATURE] if features is None else features,
         "executable": "/synthetic/LIVI"},
        {"reason": "build-finished", "success": success},
    ]
    if runner:
        rows.insert(0, {"reason": "compiler-artifact", "target": {"name": "livi_bt_diagnostic"},
                        "features": []})
    return "\n".join(json.dumps(row) for row in rows) + "\n"


class AppTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "temp-attachments").mkdir()
        self.output = self.root / "temp-attachments" / "new-app"
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(mock.patch.object(app, "ROOT", self.root))
        self.stack.enter_context(mock.patch.object(app.diagnostic, "ROOT", self.root))

    def mocks(self):
        self.stack.enter_context(mock.patch.object(app.diagnostic, "host_check"))
        self.stack.enter_context(mock.patch.object(app.checks, "boundary_check"))
        self.stack.enter_context(mock.patch.object(app.checks, "gstreamer_sdk", return_value=Path("/sdk")))
        self.stack.enter_context(mock.patch.object(app, "identity", return_value="current"))
        self.stack.enter_context(mock.patch.object(app.checks, "verify_image", return_value="binary"))
        self.stack.enter_context(mock.patch.object(app.checks, "dependency_closure", return_value={"lib": "hash"}))
        self.stack.enter_context(mock.patch.object(app, "verify_metadata"))

    def manifest(self):
        self.output.mkdir(mode=0o700)
        (self.output / "build.events.jsonl").write_text(events())
        (self.output / "ready.json").write_text(json.dumps({
            "schema": 1, "kind": app.KIND, "source_identity": "current",
            "binary_sha256": "binary", "links": {"lib": "hash"}, "hardware_executed": False,
        }))

    def args(self, **changes):
        result = dict(output=self.output, authorize_bluetooth_hardware=True,
                      paired_phone_only=True, phone_address="02:01:02:03:04:05")
        result.update(changes)
        return argparse.Namespace(**result)

    def test_build_selects_only_real_app_and_optin_feature(self):
        command = app.command(self.output)
        for flag in ["--locked", "--offline", "--no-default-features"]:
            self.assertIn(flag, command)
        self.assertEqual(command[command.index("-p") + 1], "livi-ui")
        self.assertEqual(command[command.index("--bin") + 1], "LIVI")
        self.assertEqual(command[command.index("--features") + 1], app.FEATURE)
        self.assertEqual(command.count("-p"), 1)

    def test_metadata_decoder_requires_exact_public_test_identity_and_usage_text(self):
        def image(info):
            body = plistlib.dumps(info)
            commands = bytearray(152)
            struct.pack_into("<2I", commands, 0, 0x19, len(commands))
            struct.pack_into("<I", commands, 64, 1)
            struct.pack_into("<16s16sQQI", commands, 72,
                             b"__info_plist", b"__TEXT", 0, len(body), 184)
            return struct.pack("<8I", 0xFEEDFACF, 0x0100000C, 0, 2, 1, 152, 0, 0) + commands + body
        valid = {"CFBundleIdentifier": "dev.f-io.livi.native-validation",
                 "NSBluetoothAlwaysUsageDescription": "Explicit test consent"}
        concatenated = bytearray(image(valid) + plistlib.dumps(valid))
        struct.pack_into("<Q", concatenated, 32 + 72 + 40, len(concatenated) - 184)
        binary = self.root / "synthetic-image"
        binary.write_bytes(image(valid))
        with mock.patch.object(app.subprocess, "Popen") as popen:
            app.verify_metadata(binary)
            for content in [
                b"", image(valid)[:100], image(valid)[:-1], concatenated,
                image({}), image({**valid, "CFBundleIdentifier": "production"}),
                image({**valid, "NSBluetoothAlwaysUsageDescription": "  "}),
                image({**valid, "NSBluetoothAlwaysUsageDescription": False}),
                image(valid).replace(b"__info_plist", b"__other_info"),
                image(valid).replace(b"<?xml", b"?????"),
            ]:
                binary.write_bytes(content)
                with self.assertRaises(ValueError):
                    app.verify_metadata(binary)
            popen.assert_not_called()

    def test_bad_metadata_never_creates_ready_manifest_or_launches(self):
        self.mocks()
        def build(_command, **kwargs):
            kwargs["stdout"].write(events())
        with mock.patch.object(app, "verify_metadata", side_effect=ValueError), \
                mock.patch.object(app.subprocess, "run", side_effect=build):
            with self.assertRaises(ValueError):
                app.prepare(self.output)
        self.assertFalse((self.output / "ready.json").exists())
        (self.output / "ready.json").write_text(json.dumps({
            "schema": 1, "kind": app.KIND, "source_identity": "current",
            "binary_sha256": "binary", "links": {"lib": "hash"},
        }))
        with mock.patch.object(app, "verify_metadata", side_effect=ValueError), \
                mock.patch.object(app.subprocess, "Popen") as popen:
            with self.assertRaises(ValueError):
                app.run(self.args())
            popen.assert_not_called()

    def test_metadata_changes_invalidate_identity_and_embedding_is_feature_gated(self):
        with mock.patch.object(app.diagnostic, "source_identity", side_effect=["first", "second"]), \
                mock.patch.object(app.checks, "sha", return_value="wrapper"):
            self.assertNotEqual(app.identity(), app.identity())
        root = Path(app.__file__).resolve().parents[1]
        build = (root / "native/crates/livi-ui/build.rs").read_text()
        self.assertNotIn("-sectcreate,__TEXT,__info_plist", build)
        main = (root / "native/crates/livi-ui/src/main.rs").read_text()
        marker = '#[cfg(all(target_os = "macos", feature = "bluetooth-app-test"))]'
        body = main.split(marker + chr(10) + 'isolated_usage_metadata!(br#"', 1)[1].split('"#);', 1)[0]
        info = plistlib.loads(body.encode())
        self.assertEqual(info["CFBundleIdentifier"], "dev.f-io.livi.native-validation")
        self.assertTrue(info["NSBluetoothAlwaysUsageDescription"].strip())

    def test_missing_authorization_and_invalid_addresses_never_validate_or_spawn(self):
        values = [dict(authorize_bluetooth_hardware=False), dict(paired_phone_only=False)]
        values += [dict(phone_address=value) for value in [
            None, "", "phone-name", "00:00:00:00:00:00", "ff:ff:ff:ff:ff:ff",
            "03:01:02:03:04:05", "02:01:02:03:04:gg", "02:01:02:03:04:05\n"]]
        for change in values:
            with mock.patch.object(app, "validate") as validate, \
                    mock.patch.object(app.subprocess, "Popen") as popen:
                with self.assertRaises(ValueError):
                    app.run(self.args(**change))
                validate.assert_not_called()
                popen.assert_not_called()

    def test_feature_checks_reject_incompatible_modes_and_wrong_executables(self):
        path = self.root / "events"
        for feature in app.DENIED:
            path.write_text(events(features=[app.FEATURE, feature]))
            with self.assertRaises(ValueError):
                app.verify_features(path)
        for name in app.FORBIDDEN | {"livi-bt-diagnostic", "other-app"}:
            path.write_text(events(name=name))
            with self.assertRaises(ValueError):
                app.verify_features(path)
        for text in [events(paired=False), events(runner=False), events(features=[]),
                     events(success=False)]:
            path.write_text(text)
            with self.assertRaises(ValueError):
                app.verify_features(path)
        path.write_text(events())
        app.verify_features(path)

    def test_prepare_is_software_only_and_never_overwrites_existing_output(self):
        self.mocks()
        def build(_command, **kwargs):
            kwargs["stdout"].write(events())
        with mock.patch.object(app.subprocess, "run", side_effect=build), \
                mock.patch.object(app.subprocess, "Popen") as popen, \
                contextlib.redirect_stdout(io.StringIO()):
            app.prepare(self.output)
            original = (self.output / "ready.json").read_bytes()
            with self.assertRaises(ValueError):
                app.prepare(self.output)
            self.assertEqual((self.output / "ready.json").read_bytes(), original)
            self.assertFalse(json.loads(original)["hardware_executed"])
            popen.assert_not_called()

    def test_changed_source_during_build_never_writes_ready(self):
        self.mocks()
        def build(_command, **kwargs):
            kwargs["stdout"].write(events())
        with mock.patch.object(app, "identity", side_effect=["before", "after"]), \
                mock.patch.object(app.subprocess, "run", side_effect=build):
            with self.assertRaises(ValueError):
                app.prepare(self.output)
        self.assertTrue(self.output.exists())
        self.assertFalse((self.output / "ready.json").exists())

    def test_stale_source_binary_or_dependencies_never_launch(self):
        self.manifest()
        self.mocks()
        for patch in [
            mock.patch.object(app, "identity", return_value="stale"),
            mock.patch.object(app.checks, "verify_image", return_value="changed"),
            mock.patch.object(app.checks, "dependency_closure", return_value={}),
        ]:
            with patch, mock.patch.object(app.subprocess, "Popen") as popen:
                with self.assertRaises(ValueError):
                    app.run(self.args())
                popen.assert_not_called()

    def test_other_manifest_and_symlink_refused(self):
        self.manifest()
        self.mocks()
        manifest = self.output / "ready.json"
        original = manifest.read_text()
        data = json.loads(original)
        data["kind"] = "bluetooth-only-diagnostic"
        manifest.write_text(json.dumps(data))
        with self.assertRaises(ValueError):
            app.validate(self.output)
        manifest.write_text(original)
        alias = self.output.with_name("alias")
        alias.symlink_to(self.output, target_is_directory=True)
        with self.assertRaises(ValueError):
            app.validate(alias)
        saved = self.root / "saved"
        manifest.rename(saved)
        manifest.symlink_to(saved)
        with self.assertRaises(ValueError):
            app.validate(self.output)

    def test_only_checked_app_with_exact_mode_and_sanitized_environment_is_launched(self):
        binary = self.output / "LIVI"
        child = mock.Mock()
        child.wait.return_value = 0
        child.poll.return_value = 0
        with mock.patch.object(app, "validate", return_value=binary), \
                mock.patch.object(app.subprocess, "Popen", return_value=child) as popen, \
                mock.patch.object(app.checks, "environment", return_value={"PATH": "/safe"}):
            self.assertEqual(app.run(self.args()), 0)
        self.assertEqual(popen.call_args.args[0], [
            str(binary), "--bluetooth-only-test", "--authorize-bluetooth-hardware",
            "--paired-phone-only", "--phone-address", "02:01:02:03:04:05"])
        self.assertEqual(popen.call_args.kwargs["env"], {"PATH": "/safe"})
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        child.wait.assert_called_once_with(timeout=125)

    def test_timeout_and_interrupt_signal_only_owned_app_group(self):
        for error in [app.subprocess.TimeoutExpired("synthetic", 125), KeyboardInterrupt()]:
            child = mock.Mock()
            child.pid = 123456789
            child.poll.return_value = None
            child.wait.side_effect = [error, 0]
            with mock.patch.object(app, "validate", return_value=self.output / "LIVI"), \
                    mock.patch.object(app.subprocess, "Popen", return_value=child), \
                    mock.patch.object(app.diagnostic.os, "killpg") as kill:
                with self.assertRaises(type(error)):
                    app.run(self.args())
                kill.assert_called_once_with(child.pid, app.signal.SIGINT)

    def test_open_launches_only_inert_configuration_screen(self):
        child = mock.Mock()
        child.wait.return_value = 0
        child.poll.return_value = 0
        binary = self.output / "LIVI"
        args = argparse.Namespace(action="open", output=self.output)
        with mock.patch.object(app, "validate", return_value=binary) as validate, \
                mock.patch.object(app.subprocess, "Popen", return_value=child) as popen:
            self.assertEqual(app.run(args), 0)
        validate.assert_called_once_with(self.output)
        self.assertEqual(popen.call_args.args[0], [str(binary), "--bluetooth-only-test"])
        child.wait.assert_called_once_with(timeout=125)

    def test_open_refuses_stale_artifact_and_cli_hardware_flags(self):
        with mock.patch.object(app, "validate", side_effect=ValueError), \
                mock.patch.object(app.subprocess, "Popen") as popen:
            with self.assertRaises(ValueError):
                app.run(argparse.Namespace(action="open", output=self.output))
            popen.assert_not_called()
        with mock.patch.object(app.sys, "argv", [
            "app-test", "open", "--output", str(self.output), "--authorize-bluetooth-hardware"
        ]), contextlib.redirect_stderr(io.StringIO()), \
                mock.patch.object(app.subprocess, "Popen") as popen:
            self.assertEqual(app.main(), 1)
            popen.assert_not_called()

    def test_invalid_cli_is_redacted(self):
        output = io.StringIO()
        with mock.patch.object(app.sys, "argv", ["app-test", "run", "--secret", "not-for-output"]), \
                contextlib.redirect_stderr(output):
            self.assertEqual(app.main(), 1)
        self.assertNotIn("not-for-output", output.getvalue())


if __name__ == "__main__":
    unittest.main()
