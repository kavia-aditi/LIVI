#!/usr/bin/env python3
"""Offline tests: no application, native Bluetooth, plugin, or helper execution."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

import prepare_paired_launch as prep

launcher = prep.load_launcher()


class LauncherTests(unittest.TestCase):
    def test_import_does_not_launch_or_create_runtime(self):
        with patch("os.execve", side_effect=AssertionError("launch forbidden")), \
             patch("tempfile.mkdtemp", side_effect=AssertionError("runtime forbidden")):
            module = prep.load_launcher()
            self.assertEqual(module.BUNDLE_ID, "dev.f-io.livi.paired-discovery")

    def test_environment_is_private_and_drops_all_overrides(self):
        runtime = Path("/tmp/livi-bt-dev-offline-test")
        inherited = {
            "HOME": "/private-original-home",
            "LIVI_UI_STATE": "fixture",
            "LIVI_CORE_SOCKET": "/tmp/existing.sock",
            "LIVI_HELPER_BIN": "/private/helper",
            "LIVI_NATIVE_HARDWARE_AUTHORIZED": "1",
            "DYLD_INSERT_LIBRARIES": "/private/injected.dylib",
            "GST_PLUGIN_PATH": "/private/plugins",
            "DBUS_SYSTEM_BUS_ADDRESS": "unix:path=/private/bus",
            "SLINT_BACKEND": "unexpected",
            "LANG": "en_US.UTF-8",
        }
        env = launcher.launch_environment(runtime, inherited)
        self.assertEqual(env["LIVI_NATIVE_BT_DEV_DIR"], str(runtime))
        self.assertEqual(env["XDG_RUNTIME_DIR"], str(runtime))
        self.assertEqual(env["LIVI_CORE_SOCKET"], str(runtime / "livi/core.sock"))
        self.assertEqual(env["HOME"], str(runtime / "home"))
        self.assertEqual(env["LIVI_UI_SETTINGS"], "devices")
        for key in ("LIVI_UI_STATE", "LIVI_HELPER_BIN", "LIVI_NATIVE_HARDWARE_AUTHORIZED",
                    "DYLD_INSERT_LIBRARIES", "GST_PLUGIN_PATH",
                    "DBUS_SYSTEM_BUS_ADDRESS", "SLINT_BACKEND"):
            self.assertNotIn(key, env)

    def test_socket_path_length_is_checked(self):
        with self.assertRaisesRegex(ValueError, "socket"):
            launcher.launch_environment(Path("/tmp/" + "a" * 120), {})

    def test_missing_bundle_is_rejected_without_creation(self):
        missing = prep.ROOT / "nonexistent-offline-fixture.app"
        self.assertFalse(missing.exists())
        with self.assertRaises(ValueError):
            launcher.check_layout(missing)
        self.assertFalse(missing.exists())

    def test_check_mode_never_creates_runtime_or_executes(self):
        with patch("sys.argv", ["launch.py", "--check"]), \
             patch.object(launcher, "check_layout", return_value=(Path("/ui"), Path("/core"))), \
             patch("os.execve", side_effect=AssertionError("launch forbidden")), \
             patch("tempfile.mkdtemp", side_effect=AssertionError("runtime forbidden")):
            self.assertEqual(launcher.main(), 0)

    def test_abi_version_order(self):
        self.assertLess(prep.version_tuple("8201.4.0"), prep.version_tuple("9001.0.0"))
        self.assertGreaterEqual(prep.version_tuple("2809.0.0"), prep.version_tuple("2809.0.0"))

    def test_extensionless_dylib_is_not_an_executable(self):
        header = b"Mach header\nMH_MAGIC_64 ARM64 ALL 0x00 DYLIB 51 3080 NOUNDEFS\n"
        with patch.object(prep.pkg, "run", return_value=header):
            self.assertFalse(prep.executable_image(Path("/offline/GStreamer")))

    def test_mach_execute_gets_existing_executable_signing_policy(self):
        header = b"Mach header\nMH_MAGIC_64 ARM64 ALL 0x00 EXECUTE 51 3080 NOUNDEFS\n"
        with patch.object(prep.pkg, "run", return_value=header):
            self.assertTrue(prep.executable_image(Path("/offline/program")))

    def abi_fixture(self, required):
        image = prep.APP / "Contents/Resources/lib/source.dylib"
        dependency = image.with_name("libdep.dylib")

        def metadata(tool, flag, path):
            self.assertEqual(tool, "otool")
            if flag == "-D":
                identity = "@rpath/libdep.dylib\n" if path == dependency else ""
                return f"{path}:\n{identity}".encode()
            self.assertEqual(flag, "-L")
            if path == image:
                return (f"{path}:\n\t@loader_path/libdep.dylib "
                        f"(compatibility version {required}, current version {required})\n").encode()
            return (f"{path}:\n\t@rpath/libdep.dylib "
                    "(compatibility version 8201.0.0, current version 8201.4.0)\n").encode()

        with patch.object(prep.pkg, "run", side_effect=metadata), \
             patch.object(Path, "resolve", lambda path, strict=False: path):
            return prep.verify_abi([image, dependency])

    def test_abi_accepts_compatible_bundled_library_without_loading_it(self):
        self.assertEqual(self.abi_fixture("8201.0.0"), 1)

    def test_abi_rejects_the_observed_newer_glib_requirement(self):
        with self.assertRaisesRegex(ValueError, "ABI mismatch"):
            self.abi_fixture("9001.0.0")


if __name__ == "__main__":
    unittest.main()
