#!/usr/bin/env python3
"""Offline tests for script/praxis-integration-tests.py."""

import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).with_name("praxis-integration-tests.py")
SPEC = importlib.util.spec_from_file_location("integration_tests", SCRIPT)
integration_tests = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(integration_tests)


def suite(package, kind, binary, *names):
    return {"package-name": package, "kind": kind, "binary-name": binary, "testcases": {name: {} for name in names}}


class IntegrationListTests(unittest.TestCase):
    def test_the_checked_in_list_parses(self):
        packages, features, targets, expressions, entries = integration_tests.parse(
            integration_tests.LIST.read_text(encoding="utf-8").splitlines()
        )
        self.assertIn("project/test-support", features)
        self.assertEqual(targets, ["file_inventory"])
        self.assertEqual(len(expressions), len(entries))
        self.assertIn("agent", packages)

    def test_each_line_kind_becomes_a_filterset(self):
        _, features, targets, expressions, _ = integration_tests.parse([
            "agent_settings",
            "agent db::tests::",
            "agent =internal_tests::exact_name  # comment",
            "project+test-support @file_inventory",
        ])
        self.assertEqual(expressions, [
            "package(agent_settings) & kind(lib)",
            "package(agent) & kind(lib) & test(db::tests::)",
            "package(agent) & kind(lib) & test(=internal_tests::exact_name)",
            "package(project) & kind(test) & binary(file_inventory)",
        ])
        self.assertEqual(features, ["project/test-support"])
        self.assertEqual(targets, ["file_inventory"])

    def test_stray_characters_are_rejected(self):
        for line in ('agent session_mode_tests::"', "agent a b", "agent test(x)"):
            with self.subTest(line=line), self.assertRaises(ValueError):
                integration_tests.parse([line])

    def test_build_only_omits_run_flags(self):
        build = integration_tests.command(["project"], ["project/test-support"], ["file_inventory"], ["e"], ["--no-run"])
        self.assertNotIn("--no-fail-fast", build)
        self.assertNotIn("-E", build)
        self.assertEqual(build[build.index("--features") + 1], "project/test-support")
        self.assertEqual(build[build.index("--test") + 1], "file_inventory")
        run = integration_tests.command(["agent"], [], [], ["package(agent) & kind(lib)"], [])
        self.assertIn("--no-fail-fast", run)
        self.assertEqual(run[run.index("-E") + 1], "package(agent) & kind(lib)")

    def test_filters_that_select_no_test_are_reported(self):
        listing = {"rust-suites": {
            "agent": suite("agent", "lib", "agent", "db::tests::test_roundtrip", "thread::tests::test_exact"),
            "project::file_inventory": suite("project", "test", "file_inventory", "test_remote"),
            "worktree": suite("worktree", "lib", "worktree", "file_inventory::tests::test_rules"),
        }}
        entries = [
            (1, "agent", "db::tests::"),
            (2, "agent", "=thread::tests::test_exact"),
            (3, "agent", "=thread::tests::test_ex"),
            (4, "project", "@file_inventory"),
            (5, "worktree", 'file_inventory::tests::"'),
            (6, "agent", "test_remote"),
            (7, "worktree", ""),
            (8, "missing_package", ""),
        ]
        self.assertEqual(
            [line for line, _, _ in integration_tests.unmatched(entries, listing)],
            [3, 5, 6, 8],
        )


if __name__ == "__main__":
    unittest.main()
