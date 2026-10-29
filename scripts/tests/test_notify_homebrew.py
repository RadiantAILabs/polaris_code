"""Checks for the release-to-Homebrew dispatch contract."""

import io
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import notify_homebrew


class HomebrewDispatchTests(unittest.TestCase):
    def test_release_goes_to_the_tap_main_workflow(self):
        captured = []

        def opener(request, timeout):
            captured.append(request)
            self.assertEqual(timeout, 30)
            return io.BytesIO(json.dumps({"html_url": "https://github.com/example/run/1"}).encode())

        url = notify_homebrew.dispatch("v0.2.0", "test-token", opener=opener)
        request = captured[0]
        self.assertEqual(request.full_url, notify_homebrew.DISPATCH_URL)
        self.assertEqual(request.method, "POST")
        self.assertEqual(request.get_header("Authorization"), "Bearer test-token")
        self.assertEqual(json.loads(request.data), {
            "ref": "main", "inputs": {"release_tag": "v0.2.0", "force_rebuild": "false"},
        })
        self.assertEqual(url, "https://github.com/example/run/1")

    def test_manual_replay_can_rebuild_the_current_version(self):
        self.assertEqual(notify_homebrew.payload("v0.1.1", True)["inputs"]["force_rebuild"], "true")

    def test_invalid_or_prerelease_tags_are_refused(self):
        for tag in ("main", "v0.2.0-rc.1", "v0.2.0\nmain", "../../main"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                notify_homebrew.payload(tag, False)

    def test_missing_secret_fails_before_any_request(self):
        with self.assertRaisesRegex(ValueError, "HOMEBREW_TAP_TOKEN"):
            notify_homebrew.dispatch("v0.2.0", "")

    def test_empty_dispatch_response_still_links_to_the_workflow(self):
        self.assertEqual(
            notify_homebrew.dispatch("v0.2.0", "test-token", opener=lambda *a, **k: io.BytesIO()),
            notify_homebrew.WORKFLOW_URL,
        )


if __name__ == "__main__":
    unittest.main()
