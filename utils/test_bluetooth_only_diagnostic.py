import argparse
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import bluetooth_only_diagnostic as diagnostic


def events(name=diagnostic.NAME, features=None, paired=True, success=True):
    rows = [
        {"reason": "compiler-artifact", "target": {"name": "livi_native_bluetooth"},
         "features": ["paired-diagnostic"] if paired else []},
        {"reason": "compiler-artifact", "target": {"name": name},
         "features": ["bluetooth-diagnostic"] if features is None else features,
         "executable": "/synthetic/diagnostic"},
        {"reason": "build-finished", "success": success},
    ]
    return "\n".join(json.dumps(row) for row in rows) + "\n"


class DiagnosticTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "temp-attachments").mkdir()
        self.output = self.root / "temp-attachments" / "fresh"
        patcher = mock.patch.object(diagnostic, "ROOT", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)

    def manifest(self):
        self.output.mkdir(mode=0o700)
        (self.output / "build.events.jsonl").write_text(events())
        (self.output / "ready.json").write_text(json.dumps({
            "schema": 1, "kind": "bluetooth-only-diagnostic",
            "source_identity": "current", "binary_sha256": "binary",
            "links": {"synthetic": "dependency"}, "hardware_executed": False,
        }))

    def validation_mocks(self):
        stack = contextlib.ExitStack()
        self.addCleanup(stack.close)
        for name in ("host_check",):
            stack.enter_context(mock.patch.object(diagnostic, name))
        stack.enter_context(mock.patch.object(diagnostic, "source_identity", return_value="current"))
        stack.enter_context(mock.patch.object(diagnostic.checks, "boundary_check"))
        stack.enter_context(mock.patch.object(diagnostic.checks, "verify_image", return_value="binary"))
        stack.enter_context(mock.patch.object(diagnostic.checks, "dependency_closure",
                                              return_value={"synthetic": "dependency"}))

    def args(self, **overrides):
        values = dict(output=self.output, authorize_bluetooth_hardware=True,
                      paired_phone_only=True, phone_address="02:01:02:03:04:05")
        values.update(overrides)
        return argparse.Namespace(**values)

    def test_missing_authorization_never_inspects_or_spawns(self):
        for values in [dict(authorize_bluetooth_hardware=False),
                       dict(paired_phone_only=False), dict(phone_address=None)]:
            with mock.patch.object(diagnostic, "validate") as validate, \
                    mock.patch.object(diagnostic.subprocess, "Popen") as popen:
                with self.assertRaises(ValueError):
                    diagnostic.run(self.args(**values))
                validate.assert_not_called()
                popen.assert_not_called()

    def test_build_command_is_locked_offline_single_package_default_off(self):
        cmd = diagnostic.command(self.output)
        self.assertIn("--locked", cmd)
        self.assertIn("--offline", cmd)
        self.assertIn("--no-default-features", cmd)
        self.assertEqual(cmd.count("-p"), 1)
        self.assertEqual(cmd[cmd.index("-p") + 1], diagnostic.NAME)
        self.assertEqual(cmd[cmd.index("--features") + 1], "bluetooth-diagnostic")
        self.assertNotIn("--release", cmd)

    def test_artifact_features_reject_every_forbidden_dependency_and_feature(self):
        path = self.root / "events"
        for name in diagnostic.FORBIDDEN:
            path.write_text(events(name=name))
            with self.assertRaises(ValueError):
                diagnostic.verify_features(path)
        for feature in diagnostic.DENIED_FEATURES:
            path.write_text(events(features=["bluetooth-diagnostic", feature]))
            with self.assertRaises(ValueError):
                diagnostic.verify_features(path)
        for text in [events(features=[]), events(paired=False), events(success=False)]:
            path.write_text(text)
            with self.assertRaises(ValueError):
                diagnostic.verify_features(path)
        path.write_text(events())
        diagnostic.verify_features(path)

    def test_prepare_never_launches_or_overwrites_artifacts(self):
        self.validation_mocks()
        def build(_cmd, **kwargs):
            kwargs["stdout"].write(events())
        with mock.patch.object(diagnostic.subprocess, "run", side_effect=build) as build_call, \
                mock.patch.object(diagnostic.subprocess, "Popen") as popen, \
                contextlib.redirect_stdout(io.StringIO()):
            diagnostic.prepare(self.output)
            self.assertEqual(build_call.call_count, 1)
            popen.assert_not_called()
            original = (self.output / "ready.json").read_bytes()
            with self.assertRaises(ValueError):
                diagnostic.prepare(self.output)
            self.assertEqual((self.output / "ready.json").read_bytes(), original)
            self.assertFalse(json.loads(original)["hardware_executed"])

    def test_source_change_during_prepare_retains_output_without_ready_manifest(self):
        self.validation_mocks()
        def build(_cmd, **kwargs):
            kwargs["stdout"].write(events())
        with mock.patch.object(diagnostic, "source_identity", side_effect=["before", "after"]), \
                mock.patch.object(diagnostic.subprocess, "run", side_effect=build):
            with self.assertRaises(ValueError):
                diagnostic.prepare(self.output)
        self.assertTrue(self.output.exists())
        self.assertFalse((self.output / "ready.json").exists())

    def test_validate_checks_freshness_binary_and_dependency_hashes(self):
        self.manifest()
        self.validation_mocks()
        binary = diagnostic.validate(self.output)
        self.assertEqual(binary.name, diagnostic.NAME)
        for target, value in [("source", "stale"), ("binary", "changed"), ("links", {})]:
            patch = {
                "source": mock.patch.object(diagnostic, "source_identity", return_value=value),
                "binary": mock.patch.object(diagnostic.checks, "verify_image", return_value=value),
                "links": mock.patch.object(diagnostic.checks, "dependency_closure", return_value=value),
            }[target]
            with patch, mock.patch.object(diagnostic.subprocess, "Popen") as popen:
                with self.assertRaises(ValueError):
                    diagnostic.run(self.args())
                popen.assert_not_called()

    def test_reject_other_harness_manifest(self):
        self.manifest()
        self.validation_mocks()
        manifest = self.output / "ready.json"
        data = json.loads(manifest.read_text())
        data["kind"] = "isolated-phone-test-not-production"
        manifest.write_text(json.dumps(data))
        with self.assertRaises(ValueError):
            diagnostic.validate(self.output)

    def test_output_and_manifest_symlinks_and_nonprivate_directory_refused(self):
        self.manifest()
        self.validation_mocks()
        alias = self.output.with_name("alias")
        alias.symlink_to(self.output, target_is_directory=True)
        with self.assertRaises(ValueError):
            diagnostic.validate(alias)
        manifest = self.output / "ready.json"
        saved = self.root / "manifest"
        manifest.rename(saved)
        manifest.symlink_to(saved)
        with self.assertRaises(ValueError):
            diagnostic.validate(self.output)
        manifest.unlink()
        saved.rename(manifest)
        self.output.chmod(0o755)
        with self.assertRaises(ValueError):
            diagnostic.validate(self.output)

    def test_future_launch_only_uses_checked_binary_and_sanitized_environment(self):
        binary = self.output / diagnostic.NAME
        child = mock.Mock()
        child.wait.return_value = 0
        child.poll.return_value = 0
        with mock.patch.object(diagnostic, "validate", return_value=binary), \
                mock.patch.object(diagnostic.subprocess, "Popen", return_value=child) as popen, \
                mock.patch.object(diagnostic.checks, "environment", return_value={"PATH": "/synthetic"}):
            self.assertEqual(diagnostic.run(self.args()), 0)
        argv = popen.call_args.args[0]
        self.assertEqual(argv, [str(binary), "--authorize-bluetooth-hardware",
                               "--paired-phone-only", "--phone-address", "02:01:02:03:04:05"])
        self.assertEqual(popen.call_count, 1)
        self.assertEqual(popen.call_args.kwargs["env"], {"PATH": "/synthetic"})
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        child.wait.assert_called_with(timeout=65)

    def test_watchdog_and_interrupt_cleanup_only_owned_process(self):
        for cause in [subprocess_timeout(), KeyboardInterrupt()]:
            child = mock.Mock()
            child.pid = 123456789
            child.poll.return_value = None
            child.wait.side_effect = [cause, 0]
            with mock.patch.object(diagnostic, "validate", return_value=self.output / diagnostic.NAME), \
                    mock.patch.object(diagnostic.subprocess, "Popen", return_value=child), \
                    mock.patch.object(diagnostic.os, "killpg") as kill:
                with self.assertRaises(type(cause)):
                    diagnostic.run(self.args())
                kill.assert_called_once_with(child.pid, diagnostic.signal.SIGINT)

    def test_hung_owned_child_is_killed_without_signalling_other_processes(self):
        child = mock.Mock()
        child.pid = 123456789
        child.poll.return_value = None
        child.wait.side_effect = [subprocess_timeout(), 0]
        with mock.patch.object(diagnostic.os, "killpg") as kill:
            diagnostic.stop_owned(child)
        self.assertEqual(kill.call_args_list, [
            mock.call(child.pid, diagnostic.signal.SIGINT),
            mock.call(child.pid, diagnostic.signal.SIGKILL)])

    def test_invalid_cli_never_echoes_private_input(self):
        output = io.StringIO()
        with mock.patch.object(diagnostic.sys, "argv", ["diagnostic", "run", "--private-secret",
                                                       "do-not-echo"]), \
                contextlib.redirect_stderr(output):
            self.assertEqual(diagnostic.main(), 1)
        self.assertNotIn("do-not-echo", output.getvalue())

    def test_protocol_runtime_has_no_network_session_or_process_launch_apis(self):
        root = Path(__file__).resolve().parents[1]
        code = "\n".join((root / "native/crates/livi-bt-diagnostic/src" / name).read_text()
                         for name in ("lib.rs", "main.rs"))
        for denied in ["TcpListener", "TcpStream", "UdpSocket", "Command::", "TlsEngine",
                       "native_handshake", "wifi_start_request", "wifi_info_response",
                       "Permit::", "include_str!", "read_to_string"]:
            self.assertNotIn(denied, code)
        manifest = (root / "native/crates/livi-bt-diagnostic/Cargo.toml").read_text()
        self.assertIn('default = []', manifest)
        self.assertIn('required-features = ["bluetooth-diagnostic"]', manifest)
        for name in ["livi-aa.workspace", "livi-runtime", "livi-native-handoff", "tokio", "rustls"]:
            self.assertNotIn(name, manifest)

    def test_standalone_selects_passive_listener_without_outbound_fallback(self):
        root = Path(__file__).resolve().parents[1]
        code = (root / "native/crates/livi-bt-diagnostic/src/main.rs").read_text()
        self.assertIn("self.0.listen_paired(generation, peer)", code)
        self.assertNotIn("self.0.connect_paired(", code)
        self.assertIn("outgoing SDP/open disabled", code)
        self.assertIn("exact trigger unknown", code)


def subprocess_timeout():
    return diagnostic.subprocess.TimeoutExpired("synthetic-child", 65)


if __name__ == "__main__":
    unittest.main()
