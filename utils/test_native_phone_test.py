"""Offline launcher tests: never starts LIVI, a helper, a renderer or Bluetooth."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import native_phone_test as phone


class PhoneTest(unittest.TestCase):
    def test_build_is_locked_offline_and_uses_separate_target(self):
        command = phone.cargo_command(Path("/fixture/new-test"))
        for flag in ("--locked", "--offline", "--target-dir"):
            self.assertIn(flag, command)
        self.assertIn("/fixture/new-test/target", command)
        self.assertIn(phone.FEATURES, command)
        self.assertNotIn("development-receiver", phone.FEATURES)
        self.assertNotIn("livi-helperd", command)

    def test_environment_does_not_inherit_overrides(self):
        with patch.dict(os.environ, {"HOME": "/fixture/home", "PATH": "/bin",
                                    "LIVI_UI_STATE": "/secret",
                                    "LIVI_HELPER_BIN": "/wrong",
                                    "RUSTFLAGS": "--cfg wrong",
                                    "DYLD_INSERT_LIBRARIES": "/wrong",
                                    "GST_PLUGIN_PATH": "/wrong"}, clear=True):
            env = phone.environment()
        self.assertEqual(env, {"HOME": "/fixture/home", "PATH": "/bin"})

    def test_runtime_environment_uses_only_private_layout(self):
        with patch.dict(os.environ, {"LIVI_UI_STATE": "/secret"}, clear=True), \
                patch.object(phone, "gstreamer_sdk", return_value=Path("/fixture/gst")):
            env = phone.runtime_environment(Path("/tmp/livi-bt-dev-fixture"),
                                            Path("/fixture/bins"))
        self.assertEqual(env["LIVI_CORE_SOCKET"],
                         "/tmp/livi-bt-dev-fixture/livi/core.sock")
        self.assertEqual(env["LIVI_GST_HOST_BIN"], "/fixture/bins/livi-gst-host")
        self.assertEqual(env["LIVI_LIFELINE"], "1")
        self.assertNotIn("LIVI_UI_STATE", env)
        self.assertNotIn("LIVI_RESOURCES", env)

    def test_no_hardware_confirmation_means_no_host_check_or_spawn(self):
        for absent in ("authorize_native_hardware", "paired_phone",
                       "existing_ap", "no_usb_projection"):
            args = type("Args", (), dict.fromkeys(
                ("authorize_native_hardware", "paired_phone",
                 "existing_ap", "no_usb_projection"), True))()
            setattr(args, absent, False)
            with patch.object(phone, "host_check") as host, \
                    patch.object(phone.subprocess, "Popen") as spawn:
                with self.assertRaises(ValueError):
                    phone.run_test(args)
                host.assert_not_called()
                spawn.assert_not_called()

    def test_private_config_accepts_only_owned_small_regular_0600_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config"
            path.write_bytes(b"offline-fixture")
            path.chmod(0o600)
            phone.private_config(path)
            for mode in (0o644, 0o666, 0o400):
                path.chmod(mode)
                with self.assertRaises(ValueError):
                    phone.private_config(path)
            path.chmod(0o600)
            path.write_bytes(b"x" * 2049)
            with self.assertRaises(ValueError):
                phone.private_config(path)

    def test_symlink_and_directory_config_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config"
            path.write_bytes(b"offline")
            path.chmod(0o600)
            link = Path(directory) / "link"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                phone.private_config(link)
            with self.assertRaises(ValueError):
                phone.private_config(Path(directory))

    def events(self, path, extra=None, success=True):
        pairs = [("livi-core", ["native-bt-dev"]),
                 ("LIVI", ["isolated-native-projection"]),
                 ("livi-gst-host", []),
                 ("livi-native-bt-dev", ["isolated-helper"]),
                 ("livi-video-view", ["native-projection"])]
        events = [{"reason": "compiler-artifact", "target": {"name": name},
                   "features": features, "executable": f"/fixture/{name}"
                   if name in phone.BINS else None} for name, features in pairs]
        if extra:
            events.append(extra)
        events.append({"reason": "build-finished", "success": success})
        path.write_text("\n".join(json.dumps(event) for event in events))

    def test_complete_isolated_features_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events"
            self.events(path)
            result = phone.verify_features(path)
            self.assertIn("isolated-helper", result["livi-native-bt-dev"])

    def test_forbidden_features_and_failed_build_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events"
            for feature in phone.DENIED:
                self.events(path, {"reason": "compiler-artifact",
                                   "target": {"name": "fixture"}, "features": [feature]})
                with self.assertRaises(ValueError):
                    phone.verify_features(path)
            self.events(path, success=False)
            with self.assertRaises(ValueError):
                phone.verify_features(path)

    def test_prepare_refuses_existing_output_without_building(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "temp-attachments/existing"
            output.mkdir(parents=True)
            with patch.object(phone, "ROOT", root), \
                    patch.object(phone, "host_check"), \
                    patch.object(phone, "boundary_check"), \
                    patch.object(phone, "source_identity") as snapshot:
                with self.assertRaises(FileExistsError):
                    phone.prepare(output)
                snapshot.assert_not_called()

    def test_prepare_refuses_outside_or_symlink_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            actual = root / "actual"
            actual.mkdir()
            (root / "temp-attachments").symlink_to(actual)
            with patch.object(phone, "ROOT", root), \
                    patch.object(phone, "host_check"), \
                    patch.object(phone, "boundary_check"):
                with self.assertRaises(ValueError):
                    phone.prepare(root / "temp-attachments/new")

    def test_low_disk_refused_without_creating_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "temp-attachments").mkdir()
            output = root / "temp-attachments/new"
            with patch.object(phone, "ROOT", root), \
                    patch.object(phone, "host_check"), \
                    patch.object(phone, "boundary_check"), \
                    patch.object(phone.shutil, "disk_usage") as disk:
                disk.return_value.free = 1024
                with self.assertRaises(ValueError):
                    phone.prepare(output)
            self.assertFalse(output.exists())

    def test_homebrew_sdk_discovery_is_static(self):
        with tempfile.TemporaryDirectory() as directory:
            sdk = Path(directory)
            for name in ("lib/pkgconfig/gstreamer-1.0.pc",
                         "libexec/gstreamer-1.0/gst-plugin-scanner"):
                path = sdk / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
            (sdk / "lib/gstreamer-1.0").mkdir()
            with patch.object(phone, "FRAMEWORK", sdk / "absent"), \
                    patch.object(phone, "command_output", return_value=str(sdk)):
                self.assertEqual(phone.gstreamer_sdk(), sdk)

    def test_missing_sdk_payload_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            sdk = Path(directory)
            with patch.object(phone, "FRAMEWORK", sdk / "absent"), \
                    patch.object(phone, "command_output", return_value=str(sdk)):
                with self.assertRaises(ValueError):
                    phone.gstreamer_sdk()

    def test_dependency_closure_rejects_missing_non_system_image(self):
        with tempfile.TemporaryDirectory() as directory:
            image = Path(directory) / "fixture"
            image.write_bytes(b"not-executed")
            def output(*args):
                if args[0] == "lipo":
                    return "arm64\n"
                if args[1] == "-L":
                    return f"{image}:\n\t@rpath/missing.dylib (compatibility version 1.0.0)\n"
                return f"{image}:\n"
            with patch.object(phone, "command_output", side_effect=output):
                with self.assertRaises(ValueError):
                    phone.dependency_closure(image)

    def test_stale_source_refused_before_image_inspection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "temp-attachments/build"
            output.mkdir(parents=True)
            (output / "ready.json").write_text(json.dumps({
                "schema": 1, "kind": "isolated-phone-test-not-production",
                "source_identity": "old"}))
            with patch.object(phone, "ROOT", root), \
                    patch.object(phone, "source_identity", return_value="new"), \
                    patch.object(phone, "verify_image") as image:
                with self.assertRaises(ValueError):
                    phone.validate_prepared(output)
                image.assert_not_called()

    def test_readiness_checks_child_exit_before_accepting_marker(self):
        child = unittest.mock.Mock()
        child.poll.return_value = 1
        with self.assertRaises(ValueError):
            phone.wait_ready(lambda: True, [child], seconds=0.1)

    def test_owned_child_is_reaped_and_stop_is_idempotent(self):
        # Sleep only: no app or helper entrypoint is executed.
        child = subprocess.Popen(["/bin/sleep", "30"], start_new_session=True,
                                 stdin=subprocess.PIPE)
        try:
            phone.stop(child)
            self.assertIsNotNone(child.poll())
            phone.stop(child)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()


if __name__ == "__main__":
    unittest.main()
