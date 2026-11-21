#!/usr/bin/env python3
"""Offline tests for script/resolve-upstream-sync.py."""

import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("resolve-upstream-sync.py")
SPEC = importlib.util.spec_from_file_location("resolve_upstream_sync", SCRIPT)
resolver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resolver)


class ResolveUpstreamSyncTests(unittest.TestCase):
    def test_upstream_only_paths_keep_praxis_workflows_protected(self):
        self.assertTrue(resolver.is_upstream_only_path("legal/privacy-policy.md"))
        self.assertTrue(resolver.is_upstream_only_path("script/terms/terms.rtf"))
        self.assertTrue(resolver.is_upstream_only_path("crates/zed/resources/snap/snapcraft.yaml.in"))
        self.assertTrue(resolver.is_upstream_only_path(".github/workflows/release_nightly.yml"))
        self.assertFalse(resolver.is_upstream_only_path(".github/workflows/sync_upstream.yml"))
        self.assertFalse(resolver.is_upstream_only_path(".github/workflows/architect_quality.yml"))
        self.assertFalse(resolver.is_upstream_only_path(".github/workflows/android_remote.yml"))
        self.assertFalse(resolver.is_upstream_only_path("crates/agent/src/thread.rs"))

    def test_zed_cargo_toml_version_conflict_preserves_praxis_version(self):
        conflicted = (
            '[package]\n'
            'name = "zed"\n'
            '<<<<<<< HEAD\n'
            'version = "2.0.0"\n'
            '||||||| base\n'
            'version = "1.30.0"\n'
            '=======\n'
            'version = "1.31.0"\n'
            '>>>>>>> upstream/main\n'
            'publish.workspace = true\n'
        )
        resolved, ok = resolver.resolve_zed_cargo_toml_text(conflicted)
        self.assertTrue(ok)
        self.assertIn('version = "2.0.0"\n', resolved)
        self.assertNotIn("1.31.0", resolved)

        non_version_conflict = (
            '<<<<<<< HEAD\n'
            'foo = "1"\n'
            '=======\n'
            'foo = "2"\n'
            '>>>>>>> upstream/main\n'
        )
        _, ok = resolver.resolve_zed_cargo_toml_text(non_version_conflict)
        self.assertFalse(ok)

    def test_diff3_rust_use_merge_handles_additions_and_upstream_removals(self):
        # Exact pattern from crates/project/tests/integration/project_tests.rs in #96:
        # Praxis added `Fs as _`; upstream removed `encoding_rs` and added `RenameOptions`.
        conflicted = (
            "use collections::{BTreeSet, HashMap, HashSet};\n"
            "<<<<<<< HEAD\n"
            "use encoding_rs;\n"
            "use fs::{FakeFs, Fs as _, PathEventKind, RealFs};\n"
            "||||||| base\n"
            "use encoding_rs;\n"
            "use fs::{FakeFs, PathEventKind, RealFs};\n"
            "=======\n"
            "use fs::{FakeFs, PathEventKind, RealFs, RenameOptions};\n"
            ">>>>>>> upstream/main\n"
        )
        resolved, ok = resolver.resolve_rust_use_hunks(conflicted)
        self.assertTrue(ok)
        self.assertNotIn("encoding_rs", resolved)
        self.assertIn(
            "use fs::{FakeFs, Fs as _, PathEventKind, RealFs, RenameOptions};\n",
            resolved,
        )

    def test_diff3_rust_use_merge_leaves_cfg_or_code_hunks_untouched(self):
        code_hunk = (
            "<<<<<<< HEAD\n"
            "#[cfg(test)]\n"
            "use foo::Bar;\n"
            "||||||| base\n"
            "=======\n"
            "use foo::Baz;\n"
            ">>>>>>> upstream/main\n"
        )
        _, ok = resolver.resolve_rust_use_hunks(code_hunk)
        self.assertFalse(ok)

    def test_diff3_rust_use_merge_prunes_redundant_submodule_import_under_super_glob(self):
        conflicted = (
            "<<<<<<< HEAD\n"
            "use settings::{Settings as _, SettingsStore};\n"
            "||||||| base\n"
            "use settings::SettingsStore;\n"
            "=======\n"
            "use settings::{AgentConfigOptionValue, SettingsStore};\n"
            ">>>>>>> upstream/main\n\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    use super::*;\n"
            "    use settings::Settings as _;\n"
            "}\n"
        )
        resolved, ok = resolver.resolve_rust_use_hunks(conflicted)
        self.assertTrue(ok)
        self.assertIn(
            "use settings::{AgentConfigOptionValue, Settings as _, SettingsStore};\n",
            resolved,
        )
        self.assertNotIn("    use settings::Settings as _;\n", resolved)

    def test_diff3_rust_mod_merge_handles_sibling_modules_and_rejects_attrs(self):
        conflicted = (
            "<<<<<<< HEAD\n"
            "mod cursor_ext;\n"
            "||||||| base\n"
            "=======\n"
            "mod auth_status;\n"
            ">>>>>>> upstream/main\n"
            "mod debug_log;\n"
        )
        resolved, ok = resolver.resolve_rust_use_hunks(conflicted)
        self.assertTrue(ok)
        self.assertEqual(resolved, "mod auth_status;\nmod cursor_ext;\nmod debug_log;\n")

        attr_hunk = (
            "<<<<<<< HEAD\n"
            "#[cfg(test)]\n"
            "mod a;\n"
            "||||||| base\n"
            "=======\n"
            "mod b;\n"
            ">>>>>>> upstream/main\n"
        )
        _, ok = resolver.resolve_rust_use_hunks(attr_hunk)
        self.assertFalse(ok)

    def test_deleted_in_praxis_checks_for_mod_declarations(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "crates" / "agent_ui" / "src"
            src.mkdir(parents=True)
            (src / "agent_ui.rs").write_text("mod agent_panel;\n", encoding="utf-8")
            self.assertTrue(
                resolver.is_intentionally_deleted_in_praxis(
                    "crates/agent_ui/src/profile_selector.rs", {1, 3}, root
                )
            )
            (src / "agent_ui.rs").write_text(
                "mod agent_panel;\nmod profile_selector;\n", encoding="utf-8"
            )
            self.assertFalse(
                resolver.is_intentionally_deleted_in_praxis(
                    "crates/agent_ui/src/profile_selector.rs", {1, 3}, root
                )
            )

    def test_patch_zed_lock_version_and_merge_praxis_only_packages(self):
        ours = (
            "version = 4\n\n"
            '[[package]]\nname = "architect"\nversion = "0.1.0"\n\n'
            '[[package]]\nname = "zed"\nversion = "2.0.0"\ndependencies = [\n "architect",\n]\n'
        )
        theirs = (
            "version = 4\n\n"
            '[[package]]\nname = "other"\nversion = "1.31.0"\n\n'
            '[[package]]\nname = "zed"\nversion = "1.31.0"\ndependencies = [\n "other",\n]\n'
        )
        merged = resolver.merge_cargo_lock_packages(ours, theirs, "2.0.0")
        self.assertIn('name = "architect"\nversion = "0.1.0"', merged)
        self.assertIn('name = "other"\nversion = "1.31.0"', merged)
        self.assertIn('name = "zed"\nversion = "2.0.0"', merged)


if __name__ == "__main__":
    unittest.main()
