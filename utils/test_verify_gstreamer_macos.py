import subprocess
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from verify_gstreamer_macos import (
    audit, dependency_names, is_system_dependency, resolve_dependency, rpath_names,
)


class DependencyAuditTests(unittest.TestCase):
    def test_parses_dependencies_without_header_or_architecture_labels(self):
        text = (
            "plugin.dylib (architecture arm64):\n"
            "\t@rpath/libgstbase-1.0.0.dylib "
            "(compatibility version 2809.0.0, current version 2809.0.0)\n"
        )
        self.assertEqual(dependency_names(text), ["@rpath/libgstbase-1.0.0.dylib"])

    def test_parses_loader_rpath(self):
        text = "cmd LC_RPATH\n cmdsize 32\n path @loader_path/.. (offset 12)\n"
        self.assertEqual(rpath_names(text), ["@loader_path/.."])

    def test_only_system_libraries_are_exempt(self):
        self.assertTrue(is_system_dependency("/usr/lib/libSystem.B.dylib"))
        self.assertTrue(is_system_dependency("/System/Library/Frameworks/AppKit.framework/AppKit"))
        self.assertFalse(is_system_dependency("/opt/homebrew/lib/libglib.dylib"))
        self.assertFalse(is_system_dependency("/Library/Frameworks/GStreamer.framework/GStreamer"))

    def test_resolves_internal_dependency_and_rejects_escape_or_missing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            lib = root / "lib"
            lib.mkdir()
            plugin_dir = lib / "gstreamer-1.0"
            plugin_dir.mkdir()
            image = plugin_dir / "plugin.dylib"
            dependency = lib / "base.dylib"
            dependency.touch()
            self.assertEqual(
                resolve_dependency("@rpath/base.dylib", image, ["@loader_path/.."], root),
                dependency,
            )
            self.assertIsNone(
                resolve_dependency("@rpath/missing.dylib", image, ["@loader_path/.."], root)
            )
            self.assertIsNone(
                resolve_dependency("@rpath/base.dylib", image, [], root)
            )
            self.assertIsNone(
                resolve_dependency("/usr/bin/true", image, [], root)
            )

    def test_audit_uses_requested_architecture(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            image = root / "image"
            image.write_bytes(b"\xcf\xfa\xed\xfe")
            with patch("verify_gstreamer_macos.run", side_effect=[
                "",
                "image:\n\t/usr/lib/libSystem.B.dylib "
                "(compatibility version 1.0.0, current version 1.0.0)\n",
                "image:\n",
                "",
            ]) as command:
                self.assertEqual(audit(root, "x86_64"), 1)
                command.assert_any_call("lipo", str(image), "-verify_arch", "x86_64")
                command.assert_any_call("otool", "-arch", "x86_64", "-L", str(image))

    def test_audit_rejects_missing_dependency(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            (root / "image").write_bytes(b"\xcf\xfa\xed\xfe")
            with patch("verify_gstreamer_macos.run", side_effect=[
                "",
                "image:\n\t@rpath/missing.dylib "
                "(compatibility version 1.0.0, current version 1.0.0)\n",
                "image:\n",
                "cmd LC_RPATH\n cmdsize 32\n path @loader_path (offset 12)\n",
            ]):
                with self.assertRaisesRegex(ValueError, "missing.dylib"):
                    audit(root)

    def test_package_scanner_skips_exact_framework_install_id(self):
        script = Path(__file__).resolve().parents[1] / "scripts/gstreamer/package-macos.sh"
        body = script.read_text().split("scan_deps() {", 1)[1].split("\n}\n", 1)[0]
        function = "scan_deps() {" + body + "\n}\n"
        harness = r"""
otool() {
  if [[ "$1" == "-D" ]]; then
    printf '%s\n' fake '@rpath/GStreamer.framework/Versions/1.0/lib/GStreamer'
  else
    printf '%s\n' fake \
      $'\t@rpath/GStreamer.framework/Versions/1.0/lib/GStreamer (compatibility version 0.0.0)' \
      $'\t@rpath/libgstbase-1.0.0.dylib (compatibility version 1.0.0)' \
      $'\t@rpath/missing.dylib (compatibility version 1.0.0)'
  fi
}
"""
        output = subprocess.check_output(
            ["/bin/bash", "-c", harness + function + "\nscan_deps ignored"], text=True
        )
        self.assertEqual(output.splitlines(), ["libgstbase-1.0.0.dylib", "missing.dylib"])


if __name__ == "__main__":
    unittest.main()
