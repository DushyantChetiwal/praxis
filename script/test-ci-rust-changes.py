#!/usr/bin/env python3
"""Offline tests for script/ci-rust-changes.py."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import unittest

SCRIPT = Path(__file__).with_name("ci-rust-changes.py")
SPEC = importlib.util.spec_from_file_location("rust_changes", SCRIPT)
rust_changes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(rust_changes)
BASE = "a" * 40
HEAD = "b" * 40


def responding(paths, returncode=0):
    def run(command, **_):
        assert command[:2] == ["gh", "api"], command
        return type("Result", (), {"returncode": returncode, "stdout": json.dumps(paths), "stderr": "boom"})()
    return run


def decide(base, run):
    with contextlib.redirect_stdout(io.StringIO()):
        return rust_changes.decide("owner/praxis", base, HEAD, run)


class RustChangeTests(unittest.TestCase):
    def test_android_docs_and_top_level_markdown_skip_rust(self):
        self.assertFalse(rust_changes.needs_rust([
            "remote-android/app/src/main/java/Main.kt",
            "docs/src/praxis.md",
            "README.md",
            ".github/workflows/android_remote.yml",
        ]))

    def test_any_other_path_needs_rust(self):
        for path in (
            "crates/gpui/README.md",
            "crates/agent/src/thread.rs",
            "Cargo.lock",
            "assets/settings/default.json",
            ".github/workflows/architect_quality.yml",
            "script/praxis-integration-tests.txt",
            ".rules",
        ):
            with self.subTest(path=path):
                self.assertTrue(rust_changes.needs_rust(["docs/a.md", path]))

    def test_an_empty_diff_runs_the_full_suite(self):
        self.assertTrue(rust_changes.needs_rust([]))

    def test_uncertain_inputs_run_the_full_suite(self):
        self.assertTrue(decide("", responding(["docs/a.md"])))
        self.assertTrue(decide("0" * 40, responding(["docs/a.md"])))
        self.assertTrue(decide(BASE, responding(["docs/a.md"], returncode=1)))
        self.assertTrue(decide(BASE, responding(["docs/a.md"] * rust_changes.COMPARE_FILE_LIMIT)))

    def test_a_listed_non_rust_diff_skips_rust(self):
        self.assertFalse(decide(BASE, responding(["remote-android/build.gradle.kts"])))
        self.assertTrue(decide(BASE, responding(["remote-android/build.gradle.kts", "crates/agent/src/agent.rs"])))


if __name__ == "__main__":
    unittest.main()
