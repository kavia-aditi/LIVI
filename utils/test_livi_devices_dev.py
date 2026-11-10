import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import livi_devices_dev as dev


def events():
    return [
        {"reason": "compiler-artifact", "target": {"name": "LIVI"},
         "features": ["bluetooth-app-test"], "executable": "/synthetic/LIVI"},
        {"reason": "compiler-artifact", "target": {"name": "livi-core"},
         "features": [], "executable": "/synthetic/livi-core"},
        {"reason": "compiler-artifact", "target": {"name": "livi-native-bluetooth"},
         "features": ["paired-diagnostic"]},
        {"reason": "compiler-artifact", "target": {"name": "livi-bt-diagnostic"}, "features": []},
        {"reason": "build-finished", "success": True},
    ]


class DevicesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.output = self.root / "new-devices"
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)

    def path(self, rows):
        path = self.root / "events"
        path.write_text("".join(json.dumps(row) + "\n" for row in rows))
        return path

    def test_build_is_normal_main_and_core_without_privileged_development_features(self):
        command = dev.command(self.output)
        self.assertEqual([command[i + 1] for i, v in enumerate(command) if v == "-p"],
                         ["livi-ui", "livi-core"])
        self.assertEqual(command[command.index("--features") + 1], "livi-ui/bluetooth-app-test")
        for flag in ("--locked", "--offline", "--no-default-features", "--bins"):
            self.assertIn(flag, command)
        self.assertNotIn("--bluetooth-only-test", command)

    def test_feature_checks_require_both_binaries_and_reject_auth_or_helper_modes(self):
        dev.verify_features(self.path(events()))
        for denied in dev.DENIED:
            rows = events()
            rows[1]["features"] = [denied]
            with self.assertRaises(ValueError):
                dev.verify_features(self.path(rows))
        for name in dev.FORBIDDEN_EXE | {"unknown"}:
            rows = events()
            rows[1]["target"]["name"] = name
            with self.assertRaises(ValueError):
                dev.verify_features(self.path(rows))
        for missing in range(5):
            rows = events()
            rows.pop(missing)
            with self.assertRaises(ValueError):
                dev.verify_features(self.path(rows))

    def prepare_mocks(self):
        for obj, name, value in (
            (dev.legacy.diagnostic, "host_check", None), (dev.checks, "boundary_check", None),
            (dev.legacy.diagnostic, "directory", None),
            (dev.checks, "gstreamer_sdk", Path("/sdk")),
            (dev, "identity", "current"), (dev.checks, "verify_image", "image"),
            (dev.checks, "dependency_closure", {"lib": "hash"}),
            (dev.legacy, "verify_metadata", None),
        ):
            self.stack.enter_context(mock.patch.object(obj, name, return_value=value))

    def build(self, _command, **kwargs):
        kwargs["stdout"].write("".join(json.dumps(row) + "\n" for row in events()))

    def test_prepare_and_check_never_launch_and_never_overwrite(self):
        self.prepare_mocks()
        with mock.patch.object(dev.subprocess, "run", side_effect=self.build), \
                mock.patch.object(dev.subprocess, "Popen") as popen, \
                contextlib.redirect_stdout(io.StringIO()):
            dev.prepare(self.output)
            manifest = self.output / "ready.json"
            original = manifest.read_bytes()
            self.assertFalse(json.loads(original)["hardware_executed"])
            self.assertEqual(set(json.loads(original)["binaries"]), set(dev.BINS))
            dev.validate(self.output)
            with self.assertRaises(FileExistsError):
                dev.prepare(self.output)
            self.assertEqual(original, manifest.read_bytes())
            popen.assert_not_called()

    def test_changed_source_or_invalid_metadata_cannot_become_ready(self):
        self.prepare_mocks()
        with mock.patch.object(dev.subprocess, "run", side_effect=self.build), \
                mock.patch.object(dev, "identity", side_effect=["first", "second"]):
            with self.assertRaises(ValueError):
                dev.prepare(self.output)
        self.assertFalse((self.output / "ready.json").exists())

    def test_launch_environment_is_fresh_home_runtime_and_no_fixture_or_native_auth(self):
        with mock.patch.object(dev.checks, "environment",
                               return_value={"HOME": "/private-original", "PATH": "/safe"}), \
                mock.patch.object(dev.checks, "gstreamer_sdk", return_value=Path("/sdk")):
            env = dev.launch_environment(Path("/tmp/synthetic"))
        self.assertEqual(env["HOME"], "/tmp/synthetic")
        self.assertEqual(env["LIVI_CORE_SOCKET"], "/tmp/synthetic/livi/core.sock")
        self.assertEqual(env["LIVI_UI_SETTINGS"], "devices")
        for denied in ("LIVI_UI_STATE", "LIVI_NATIVE_BT_DEV_DIR", "LIVI_LIFELINE"):
            self.assertNotIn(denied, env)

    def test_open_uses_main_without_standalone_flags_and_stops_only_owned_child(self):
        child = mock.Mock()
        child.wait.return_value = 0
        with mock.patch.object(dev, "validate", return_value=Path("/synthetic/bin")), \
                mock.patch.object(dev.tempfile, "mkdtemp", return_value="/tmp/synthetic"), \
                mock.patch.object(dev, "launch_environment", return_value={"HOME": "/tmp/synthetic"}), \
                mock.patch.object(dev.subprocess, "Popen", return_value=child) as popen, \
                mock.patch.object(dev.legacy.diagnostic, "stop_owned") as stop, \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(dev.open_app(self.output), 0)
        self.assertEqual(popen.call_args.args[0], ["/synthetic/bin/LIVI"])
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        stop.assert_called_once_with(child)

    def test_stale_artifact_never_launches(self):
        with mock.patch.object(dev, "validate", side_effect=ValueError), \
                mock.patch.object(dev.subprocess, "Popen") as popen, \
                mock.patch.object(dev.tempfile, "mkdtemp") as temp:
            with self.assertRaises(ValueError):
                dev.open_app(self.output)
            popen.assert_not_called()
            temp.assert_not_called()


if __name__ == "__main__":
    unittest.main()
