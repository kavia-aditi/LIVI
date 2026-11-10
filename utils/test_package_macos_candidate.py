"""Narrow packaging safety tests. No app, helper, plugin, or hardware execution."""
import json
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

import package_macos_candidate as pack


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="livi-candidate-test-")
        self.addCleanup(self.temp.cleanup)
        self.project = Path(self.temp.name).resolve()
        self.candidate = self.project / "dist/candidates/new"
        self.candidate.mkdir(parents=True)

    def test_candidate_scope(self):
        self.assertEqual(pack.safe_candidate(self.project, self.candidate),
                         (self.project, self.candidate))
        for path in (self.project / "dist/LIVI.app", Path("/Applications/LIVI.app"),
                     self.candidate / "nested", self.project / "dist/candidates/../bad"):
            with self.assertRaises(ValueError):
                pack.safe_candidate(self.project, path)

    def test_symlink_candidate_rejected(self):
        path = self.project / "dist/candidates/link"
        path.symlink_to(self.candidate, target_is_directory=True)
        with self.assertRaises(ValueError):
            pack.safe_candidate(self.project, path)

    def test_write_json_is_exclusive(self):
        path = self.candidate / "record.json"
        pack.write_json(path, {"old": True})
        with self.assertRaises(FileExistsError):
            pack.write_json(path, {"new": True})
        self.assertEqual(json.loads(path.read_text()), {"old": True})

    def test_copy_preserves_source_and_refuses_collision(self):
        source = self.project / "input.txt"
        source.write_text("original")
        target = self.candidate / "copy.txt"
        pack.copy_new(source, target)
        with self.assertRaises(ValueError):
            pack.copy_new(source, target)
        self.assertEqual(source.read_text(), "original")
        self.assertEqual(target.read_text(), "original")

    def test_dangling_collision_is_not_adopted(self):
        target = self.candidate / "collision"
        target.symlink_to("missing")
        with self.assertRaises(ValueError):
            pack.absent(target)
        self.assertTrue(target.is_symlink())

    def test_missing_firmware_fails_without_creating_source_stub(self):
        with self.assertRaisesRegex(ValueError, "stub fallback"):
            pack.selected_files(self.project)
        for name in pack.FIRMWARE_INPUTS:
            self.assertFalse((self.project / name).exists())

    def test_manifest_tamper_and_inventory_changes(self):
        body = {"files": {"source.rs": {"sha256": "one"}}}
        identity = "candidate-" + pack.hashlib.sha256(pack.canonical(body)).hexdigest()
        record = self.candidate / "candidate-source.json"
        pack.write_json(record, {"identity": identity, "body": body})
        with patch.object(pack, "inventory", return_value=body["files"]), \
                patch.object(pack, "check_gates"):
            self.assertEqual(pack.verify(self.project, self.candidate), identity)
        with patch.object(pack, "inventory", return_value={"added.rs": {}}):
            with self.assertRaisesRegex(ValueError, "inventory changed"):
                pack.verify(self.project, self.candidate)
        record.write_text(json.dumps({"identity": identity, "body": {"files": {}}}))
        with self.assertRaisesRegex(ValueError, "canonical hash"):
            pack.verify(self.project, self.candidate)

    def build_events(self, forbidden=None, receiver=False):
        events = []
        for name in pack.BINS:
            events.append({"reason": "compiler-artifact", "target": {"name": name},
                           "features": [], "executable": "/candidate/" + name})
        for name in ("livi-native-bluetooth", "livi-native-handoff", "livi-video-view"):
            events.append({"reason": "compiler-artifact", "target": {"name": name},
                           "features": ["native-projection"] if name == "livi-video-view" else []})
        if forbidden:
            events[0]["features"] = [forbidden]
        if receiver:
            events.append({"reason": "compiler-artifact", "target": {"name": "livi-native-bt-dev"}})
        events.append({"reason": "build-finished", "success": True})
        return events

    def save_events(self, events):
        (self.candidate / "build.events.jsonl").write_text(
            "".join(json.dumps(event) + "\n" for event in events))

    def test_actual_production_graph_required(self):
        self.save_events(self.build_events())
        self.assertIn("native-projection", pack.verify_features(self.candidate)["livi-video-view"])
        events = self.build_events()
        events[-1]["success"] = False
        self.save_events(events)
        with self.assertRaises(ValueError):
            pack.verify_features(self.candidate)

    def test_every_development_feature_and_receiver_rejected(self):
        for feature in pack.FORBIDDEN:
            self.save_events(self.build_events(forbidden=feature))
            with self.assertRaisesRegex(ValueError, "development-only"):
                pack.verify_features(self.candidate)
        self.save_events(self.build_events(receiver=True))
        with self.assertRaisesRegex(ValueError, "receiver"):
            pack.verify_features(self.candidate)

    def test_staging_rejects_unverified_app_before_copy(self):
        pack.write_json(self.candidate / "static-verification.json", {"app_files": {"missing": {}}})
        with patch.object(pack, "run") as run:
            with self.assertRaises(ValueError):
                pack.stage(self.candidate)
            run.assert_not_called()
        self.assertFalse((self.candidate / "dmg").exists())

    def test_existing_dmg_never_invokes_tool(self):
        dmg = self.candidate / "LIVI-macos-arm64-candidate.dmg"
        dmg.write_bytes(b"preserve")
        with patch.object(pack, "run") as run:
            with self.assertRaises(ValueError):
                pack.create_dmg(self.candidate)
            run.assert_not_called()
        self.assertEqual(dmg.read_bytes(), b"preserve")

    def test_dmg_preserves_source_ownership_and_never_overwrites_or_attaches(self):
        staging = self.candidate / "dmg"
        staging.mkdir()
        (staging / "LIVI.app").mkdir()
        pack.write_json(self.candidate / "static-verification.json", {"app_files": {}})
        with patch.object(pack, "run") as run:
            pack.create_dmg(self.candidate)
        args = run.call_args.args
        self.assertEqual(args[args.index("-srcowners") + 1], "any")
        self.assertNotIn("-ov", args)
        self.assertNotIn("-attach", args)


if __name__ == "__main__":
    unittest.main()
