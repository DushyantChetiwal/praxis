#!/usr/bin/env python3
"""Offline build-checkpoint regressions; execute in GitHub Actions, not locally."""

import contextlib

import importlib.util
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("ci-build-checkpoint.py")
SPEC = importlib.util.spec_from_file_location("checkpoint", SCRIPT)
checkpoint = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checkpoint)
PREFIX = "rust-build-state-v2-bundle-linux-x86_64-" + "a" * 64 + "-"
KEY = PREFIX + "b" * 40 + "-10-1"
REPOSITORY = "owner/praxis"
ROOT = "repos/" + REPOSITORY


class SelectionTests(unittest.TestCase):
    def setUp(self):
        environment = patch.dict(os.environ, {}, clear=True)
        environment.start()
        self.addCleanup(environment.stop)
        self.repository = {"id": 1, "private": False, "visibility": "public", "default_branch": "main"}
        self.run = {
            "id": 10, "path": checkpoint.BUNDLE_WORKFLOW, "event": "workflow_dispatch",
            "head_branch": "main", "head_sha": "c" * 40,
            "repository": {"id": 1}, "head_repository": {"id": 1},
        }
        self.artifact = {
            "id": 20, "name": KEY, "expired": False,
            "workflow_run": {"id": 10, "head_sha": "c" * 40, "repository_id": 1, "head_repository_id": 1},
        }
        self.responses = {
            ROOT: self.repository,
            f"{ROOT}/actions/runs/10": self.run,
            f"{ROOT}/actions/artifacts?per_page=100&page=1": {"artifacts": [self.artifact]},
        }
        self.calls = []

    def request(self, endpoint):
        self.calls.append(endpoint)
        return self.responses[endpoint]

    def find(self):
        with contextlib.redirect_stdout(io.StringIO()):
            return checkpoint.find_checkpoint(REPOSITORY, PREFIX, self.request)

    def test_exact_compatible_checkpoint_from_owned_main_bundle_is_selected(self):
        self.assertEqual(self.find(), self.artifact)

    def test_private_repository_never_downloads_or_lists_checkpoints(self):
        self.repository.update(private=True, visibility="private")
        self.assertIsNone(self.find())
        self.assertEqual(self.calls, [ROOT])

    def test_missing_visibility_does_not_default_to_public(self):
        self.repository.pop("visibility")
        self.assertIsNone(self.find())
        self.assertEqual(self.calls, [ROOT])

    def test_fork_pr_branch_and_foreign_workflow_artifacts_are_not_restored(self):
        for field, value in (
            ("event", "pull_request"), ("head_branch", "feature"),
            ("path", ".github/workflows/other.yml"),
            ("repository", {"id": 2}), ("head_repository", {"id": 2}),
            ("head_sha", "d" * 40),
        ):
            with self.subTest(field=field):
                previous = self.run[field]
                self.run[field] = value
                self.assertIsNone(self.find())
                self.run[field] = previous

    def test_expired_and_incompatible_checkpoints_are_cache_misses(self):
        self.artifact["expired"] = True
        self.assertIsNone(self.find())
        self.artifact.update(expired=False, name=KEY.replace("a" * 64, "e" * 64))
        self.assertIsNone(self.find())

    def test_artifact_run_association_and_named_run_must_match(self):
        for field, value in (("repository_id", 2), ("head_repository_id", 2), ("head_sha", "d" * 40)):
            previous = self.artifact["workflow_run"][field]
            with self.subTest(field=field):
                self.artifact["workflow_run"][field] = value
                self.assertIsNone(self.find())
            self.artifact["workflow_run"][field] = previous
        self.artifact["name"] = KEY.removesuffix("-10-1") + "-11-1"
        self.assertIsNone(self.find())

    def test_search_paginates_without_unbounded_history_reads(self):
        self.responses[f"{ROOT}/actions/artifacts?per_page=100&page=1"] = {
            "artifacts": [{"expired": True, "name": "old"}] * 100,
        }
        self.responses[f"{ROOT}/actions/artifacts?per_page=100&page=2"] = {"artifacts": [self.artifact]}
        self.assertEqual(self.find(), self.artifact)
        for page in (2, 3):
            self.responses[f"{ROOT}/actions/artifacts?per_page=100&page={page}"] = {
                "artifacts": [{"expired": True, "name": "old"}] * 100,
            }
        self.assertIsNone(self.find())


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        environment = patch.dict(os.environ, {}, clear=True)
        environment.start()
        self.addCleanup(environment.stop)
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.workspace = self.root / "source"
        self.cargo = self.root / "cargo"
        self.archive = self.root / "archive"
        self.workspace.mkdir()
        self.cargo.mkdir()
        self.archive.mkdir()
        self.originals = {}
        for name, content in {
            "target/release/incremental/agent-123/session/dep-graph.bin": b"incremental data",
            "target/release/.fingerprint/agent-123/lib-agent": b"fingerprint",
            "target/release/deps/libagent-123.rlib": b"compiled library",
            "target/release/build/helper/out/generated.rs": b"generated source",
            "target/triple/release/incremental/agent-456/session/work-products.bin": b"target-specific data",
            "target/.rustc_info.json": b"compiler information",
        }.items():
            path = self.workspace / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
            os.utime(path, (1700000000, 1700000000))
            self.originals[name] = content
        for name in ("registry/cache/dependency.crate", "git/db/dependency/HEAD"):
            path = self.cargo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("dependency")
        (self.cargo / "credentials.toml").write_text("must never be cached")
        (self.workspace / "README.md").write_text("workspace source")
        (self.workspace / "target/release/installer.tar.gz").write_text("release package")
        os.utime(self.workspace / "target/release/.fingerprint", (1700000000, 1700000000))

    def pack(self):
        with contextlib.redirect_stdout(io.StringIO()):
            return checkpoint.pack(self.workspace, self.cargo, self.archive, KEY)

    def restore(self, workspace=None, cargo=None, key=KEY):
        with contextlib.redirect_stdout(io.StringIO()):
            checkpoint.restore(workspace or self.workspace, cargo or self.cargo, self.archive, key)

    def custom_archive(self, paths, members):
        archive_path = self.archive / checkpoint.ARCHIVE
        with tarfile.open(archive_path, "w:gz") as archive:
            metadata = json.dumps({"schema": checkpoint.SCHEMA, "key": KEY, "paths": paths}).encode()
            entry = tarfile.TarInfo(checkpoint.MANIFEST)
            entry.size = len(metadata)
            archive.addfile(entry, io.BytesIO(metadata))
            for name, content in members:
                entry = tarfile.TarInfo(name)
                entry.size = len(content)
                archive.addfile(entry, io.BytesIO(content))
        (self.archive / checkpoint.CHECKSUM).write_text(checkpoint.digest(archive_path))

    def test_fresh_workspace_restores_full_incremental_state_and_file_timestamps(self):
        manifest = self.pack()
        fresh = self.root / "fresh-workspace"
        cargo = self.root / "fresh-cargo"
        fresh.mkdir()
        cargo.mkdir()
        (fresh / "README.md").write_text("new checkout")
        (cargo / "credentials.toml").write_text("new credentials")
        self.restore(fresh, cargo)
        for name, content in self.originals.items():
            with self.subTest(name=name):
                self.assertEqual((fresh / name).read_bytes(), content)
                self.assertEqual(int((fresh / name).stat().st_mtime), 1700000000)
        self.assertEqual(int((fresh / "target/release/.fingerprint").stat().st_mtime), 1700000000)
        self.assertEqual((cargo / "registry/cache/dependency.crate").read_text(), "dependency")
        self.assertEqual((fresh / "README.md").read_text(), "new checkout")
        self.assertEqual((cargo / "credentials.toml").read_text(), "new credentials")
        self.assertFalse((fresh / "target/release/installer.tar.gz").exists())
        self.assertNotIn("cargo/credentials.toml", manifest["paths"])
        self.assertNotIn("target", manifest["paths"])

    def test_checkpoint_identity_and_integrity_are_verified_before_restoration(self):
        self.pack()
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            self.restore(key=KEY.replace("b" * 40, "d" * 40))
        archive = self.archive / checkpoint.ARCHIVE
        archive.write_bytes(archive.read_bytes() + b"tampered")
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.restore()
        self.assertEqual((self.workspace / "README.md").read_text(), "workspace source")

    def test_archive_cannot_overwrite_source_or_credentials(self):
        for name in ("README.md", "cargo/credentials.toml", "target/release/deps/../../../README.md", "/outside"):
            with self.subTest(name=name):
                self.custom_archive(["target/release/deps"], [(name, b"unexpected")])
                with self.assertRaises((ValueError, tarfile.FilterError)):
                    self.restore()
                self.assertEqual((self.workspace / "README.md").read_text(), "workspace source")
                self.assertEqual((self.cargo / "credentials.toml").read_text(), "must never be cached")

    def test_manifest_roots_must_be_explicit_generated_state(self):
        for root in ("target", "cargo", "cargo/credentials.toml", "target/../../deps", "../deps", "target\\release\\deps"):
            with self.subTest(root=root):
                self.custom_archive([root], [])
                with self.assertRaisesRegex(ValueError, "paths"):
                    self.restore()

    def test_overlapping_roots_are_rejected(self):
        self.custom_archive(["target/release/build", "target/release/build/nested/deps"], [])
        with self.assertRaisesRegex(ValueError, "Overlapping"):
            self.restore()

    def test_internal_symlinks_are_portable_and_external_links_are_not_packed(self):
        link = self.workspace / "target/release/deps/linked.rlib"
        original = self.workspace / "target/release/deps/libagent-123.rlib"
        try:
            link.symlink_to(original)
        except OSError as error:
            self.skipTest(f"Symlinks are unavailable on this runner: {error}")
        self.pack()
        fresh = self.root / "linked-workspace"
        cargo = self.root / "linked-cargo"
        fresh.mkdir()
        cargo.mkdir()
        self.restore(fresh, cargo)
        restored = fresh / "target/release/deps/linked.rlib"
        self.assertTrue(restored.is_symlink())
        self.assertEqual(restored.read_bytes(), original.read_bytes())
        self.assertTrue(restored.resolve().is_relative_to(fresh))
        link.unlink()
        link.symlink_to(self.cargo / "credentials.toml")
        with self.assertRaisesRegex(ValueError, "symlink leaves"):
            self.pack()

    def test_destination_symlinks_cannot_redirect_a_restore_into_source(self):
        self.pack()
        fresh = self.root / "unsafe-workspace"
        source = fresh / "source"
        source.mkdir(parents=True)
        try:
            (fresh / "target").symlink_to(source, target_is_directory=True)
        except OSError as error:
            self.skipTest(f"Symlinks are unavailable on this runner: {error}")
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.restore(fresh, self.root / "unsafe-cargo")
        self.assertEqual(list(source.iterdir()), [])

    def test_source_only_workspace_cannot_seed_a_checkpoint(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "No compiler state"):
                checkpoint.pack(Path(directory), self.cargo, self.archive, KEY)


class PolicyTests(unittest.TestCase):
    def test_private_or_missing_visibility_disables_all_operations(self):
        for visibility in ("private", "internal", ""):
            for operation in ("find", "pack", "restore"):
                with self.subTest(visibility=visibility, operation=operation), patch.dict(
                    os.environ, {"GITHUB_ACTIONS": "true", "REPOSITORY_VISIBILITY": visibility}, clear=True,
                ), patch.object(checkpoint, "find_checkpoint") as find, patch.object(checkpoint, "pack") as pack, patch.object(checkpoint, "restore") as restore:
                    with contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(checkpoint.main([operation]), 0)
                    find.assert_not_called()
                    pack.assert_not_called()
                    restore.assert_not_called()

    def test_only_read_only_github_requests_are_used_for_selection(self):
        result = type("Response", (), {"stdout": "{}"})()
        with patch.object(checkpoint.subprocess, "run", return_value=result) as run:
            self.assertEqual(checkpoint.github("repos/owner/praxis"), {})
        self.assertEqual(run.call_args.args[0], ["gh", "api", "--method", "GET", "repos/owner/praxis"])
        self.assertEqual(run.call_args.kwargs["timeout"], 30)


if __name__ == "__main__":
    unittest.main()
