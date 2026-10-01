#!/usr/bin/env python3
"""Offline provenance regression tests; run in Architect quality, not locally."""

import contextlib
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch
import zipfile


SCRIPT = Path(__file__).with_name("praxis-validation.py")
SPEC = importlib.util.spec_from_file_location("praxis_validation", SCRIPT)
validation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validation)

REPOSITORY = "owner/praxis"
ROOT = f"repos/{REPOSITORY}"
HEAD = "1" * 40
BASE = "2" * 40
MERGE = "3" * 40
TARGET = "4" * 40
TREE = "5" * 40
BLOB = "6" * 40
OTHER = "7" * 40
RUN_PATH = f"{ROOT}/actions/runs/10"
JOBS_PATH = f"{RUN_PATH}/attempts/1/jobs"
ARTIFACTS_PATH = f"{RUN_PATH}/artifacts"
ZIP_PATH = f"{ROOT}/actions/artifacts/20/zip"
RUNS_PATH = f"{ROOT}/actions/workflows/30/runs?per_page=100&page=1"


def archive_bytes(receipt, name="receipt.json", extra=None):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(name, json.dumps(receipt))
        if extra:
            archive.writestr(extra, "unexpected")
    return stream.getvalue()


class FakeGitHub:
    def __init__(self):
        self.repository = REPOSITORY
        self.root = ROOT
        self.responses = {}
        self.sequences = {}
        self.calls = []
        self.deadline = None

    def get(self, endpoint):
        if self.deadline is not None and validation.time.monotonic() >= self.deadline:
            raise validation.DeadlineExceeded("mock request deadline expired")
        self.calls.append(endpoint)
        if self.sequences.get(endpoint):
            value = self.sequences[endpoint].pop(0)
        else:
            value = self.responses[endpoint]
        if isinstance(value, Exception):
            raise value
        return copy.deepcopy(value)

    def raw(self, endpoint):
        return self.get(endpoint)

    def pages(self, endpoint, key, limit=10):
        return self.get(endpoint)[key]

    commit = validation.GitHub.commit
    workflow_blob = validation.GitHub.workflow_blob


class Clock:
    def __init__(self):
        self.now = 0

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class ProvenanceTests(unittest.TestCase):
    def setUp(self):
        self.environment = patch.dict(os.environ, {}, clear=True)
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.git = patch.object(validation, "git_object", side_effect=lambda source, kind: source if kind == "commit" else TREE)
        self.git.start()
        self.addCleanup(self.git.stop)
        self.api = FakeGitHub()
        self.api.responses[ROOT] = {"id": 40, "full_name": REPOSITORY, "default_branch": "main"}
        self.api.responses[f"{ROOT}/actions/workflows/architect_quality.yml"] = {
            "id": 30, "path": validation.WORKFLOW,
        }
        repository = {"id": 40, "full_name": REPOSITORY}
        self.run = {
            "id": 10, "run_attempt": 1, "workflow_id": 30,
            "path": validation.WORKFLOW, "event": "pull_request",
            "head_sha": HEAD, "status": "completed", "conclusion": "success",
            "repository": copy.deepcopy(repository), "head_repository": copy.deepcopy(repository),
            "pull_requests": [{
                "number": 9,
                "head": {"sha": HEAD, "repo": copy.deepcopy(repository)},
                "base": {"sha": BASE, "repo": copy.deepcopy(repository)},
            }],
        }
        self.receipt = {
            "schema": 2, "kind": "original-full", "repository": REPOSITORY,
            "repository_id": 40, "source_sha": MERGE, "source_tree": TREE,
            "run_id": 10, "run_attempt": 1, "run_head_sha": HEAD,
            "event": "pull_request", "workflow_id": 30, "workflow_path": validation.WORKFLOW,
            "workflow_sha": MERGE, "workflow_blob": BLOB, "pr_number": 9,
            "pr_base_sha": BASE, "pr_head_sha": HEAD,
        }
        self.event = {
            "repository": copy.deepcopy(repository), "number": 9,
            "pull_request": copy.deepcopy(self.run["pull_requests"][0]),
        }
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.event_path = Path(directory.name) / "event.json"
        self.jobs = [{
            "name": name, "run_id": 10, "status": "completed", "conclusion": "success",
            "steps": [{"name": anchor, "status": "completed", "conclusion": "success"}],
        } for name, anchor in validation.EXPECTED_JOBS.items()]
        self.artifact = {
            "id": 20, "name": validation.artifact_name(10, 1), "expired": False,
            "size_in_bytes": 1024,
            "workflow_run": {"id": 10, "head_sha": HEAD, "repository_id": 40, "head_repository_id": 40},
        }
        self.api.responses[RUN_PATH] = self.run
        self.api.responses[RUNS_PATH] = {"workflow_runs": [self.run]}
        self.api.responses[JOBS_PATH] = {"jobs": self.jobs}
        self.api.responses[ARTIFACTS_PATH] = {"artifacts": [self.artifact]}
        self.api.responses[f"{ROOT}/pulls/9"] = copy.deepcopy(self.run["pull_requests"][0])
        self.api.responses[f"{ROOT}/commits/main"] = {"sha": TARGET}
        self.api.responses[f"{ROOT}/git/ref/pull/9/merge"] = {"object": {"sha": MERGE}}
        for source in (HEAD, BASE, MERGE, TARGET, OTHER):
            self.api.responses[f"{ROOT}/git/commits/{source}"] = {
                "sha": source, "tree": {"sha": TREE},
                "parents": [{"sha": BASE}, {"sha": HEAD}],
            }
            self.api.responses[f"{ROOT}/contents/{validation.WORKFLOW}?ref={source}"] = {
                "path": validation.WORKFLOW, "type": "file", "sha": BLOB,
            }
        self.store_receipt()
        self.validator = validation.Validator(self.api)

    def store_receipt(self):
        self.api.responses[ZIP_PATH] = archive_bytes(self.receipt)

    def check(self, wait=0, quality=False):
        with contextlib.redirect_stdout(io.StringIO()):
            return self.validator.check(TARGET, wait, quality)

    def reject_receipt(self):
        self.store_receipt()
        self.assertFalse(self.check()[0])

    def test_pr_merge_tree_reuses_for_main_with_different_commit_sha(self):
        validated, explanation = self.check()
        self.assertTrue(validated)
        self.assertIn(MERGE, explanation)
        self.assertNotEqual(HEAD, MERGE)
        self.assertNotEqual(MERGE, TARGET)

    def test_base_drift_changes_tree_even_when_pr_head_is_identical(self):
        self.api.responses[f"{ROOT}/git/commits/{MERGE}"]["tree"]["sha"] = OTHER
        self.receipt["source_tree"] = OTHER
        self.reject_receipt()

    def test_any_source_script_or_toolchain_tree_change_requires_full_validation(self):
        with patch.object(validation, "git_object", side_effect=lambda source, kind: source if kind == "commit" else OTHER):
            self.api.responses[f"{ROOT}/git/commits/{TARGET}"]["tree"]["sha"] = OTHER
            self.assertFalse(self.check()[0])

    def test_event_base_must_be_parent_of_tested_merge(self):
        self.api.responses[f"{ROOT}/git/commits/{MERGE}"]["parents"][0]["sha"] = OTHER
        self.reject_receipt()

    def test_head_sha_alone_is_not_merge_provenance(self):
        self.receipt["source_sha"] = HEAD
        self.receipt["workflow_sha"] = HEAD
        self.api.responses[f"{ROOT}/git/commits/{HEAD}"]["parents"] = [{"sha": BASE}]
        self.reject_receipt()

    def test_receipt_tree_is_checked_against_actual_git_commit(self):
        self.receipt["source_tree"] = OTHER
        self.reject_receipt()

    def test_target_git_tree_must_match_github(self):
        self.api.responses[f"{ROOT}/git/commits/{TARGET}"]["tree"]["sha"] = OTHER
        with self.assertRaises(validation.Unverified):
            self.check()

    def test_changed_workflow_definition_rejects_receipt(self):
        self.api.responses[f"{ROOT}/contents/{validation.WORKFLOW}?ref={MERGE}"]["sha"] = OTHER
        self.receipt["workflow_blob"] = OTHER
        self.reject_receipt()

    def test_changed_default_branch_workflow_for_bundle_fails_closed(self):
        self.api.responses[f"{ROOT}/commits/main"]["sha"] = OTHER
        self.api.responses[f"{ROOT}/contents/{validation.WORKFLOW}?ref={OTHER}"]["sha"] = OTHER
        with self.assertRaises(validation.Unverified):
            self.check()

    def test_fabricated_execution_definition_rejected(self):
        self.receipt["workflow_sha"] = TARGET
        self.reject_receipt()

    def test_fork_run_rejected(self):
        self.run["head_repository"] = {"id": 99, "full_name": "fork/praxis"}
        self.reject_receipt()

    def test_fork_association_rejected_even_with_same_run_repository(self):
        self.run["pull_requests"][0]["head"]["repo"]["id"] = 99
        self.reject_receipt()

    def test_live_fork_pr_rejected(self):
        self.api.responses[f"{ROOT}/pulls/9"]["head"]["repo"]["id"] = 99
        self.reject_receipt()

    def test_live_base_drift_does_not_replace_recorded_event(self):
        self.api.responses[f"{ROOT}/pulls/9"]["base"]["sha"] = OTHER
        self.api.responses[f"{ROOT}/pulls/9"]["head"]["sha"] = OTHER
        self.assertTrue(self.check()[0])

    def test_missing_pr_association_rejected(self):
        self.run["pull_requests"] = []
        self.reject_receipt()

    def test_mutated_association_shas_do_not_replace_recorded_event(self):
        self.run["pull_requests"][0]["head"]["sha"] = OTHER
        self.run["pull_requests"][0]["base"]["sha"] = OTHER
        self.assertTrue(self.check()[0])

    def test_invalid_receipt_parent_head_rejected(self):
        self.api.responses[f"{ROOT}/git/commits/{MERGE}"]["parents"][1]["sha"] = OTHER
        self.reject_receipt()

    def test_recorded_event_head_must_equal_immutable_run_head(self):
        self.receipt["pr_head_sha"] = OTHER
        self.reject_receipt()

    def test_receipts_without_event_provenance_fail_closed(self):
        del self.receipt["pr_base_sha"]
        self.reject_receipt()
        self.receipt["schema"] = 1
        self.reject_receipt()

    def test_wrong_receipt_pr_rejected(self):
        self.receipt["pr_number"] = 12
        self.reject_receipt()

    def test_failed_cancelled_and_incomplete_runs_rejected(self):
        for conclusion in ("failure", "cancelled", "timed_out", "skipped", None):
            with self.subTest(conclusion=conclusion):
                self.run["conclusion"] = conclusion
                self.assertFalse(self.check()[0])
        self.run.update(status="in_progress", conclusion="success")
        self.assertFalse(self.check()[0])

    def test_successful_skip_receipt_is_not_an_original_full_receipt(self):
        self.receipt["kind"] = "reused"
        self.reject_receipt()

    def test_missing_and_expired_artifacts_fail_closed(self):
        self.artifact["expired"] = True
        self.assertFalse(self.check()[0])
        self.api.responses[ARTIFACTS_PATH]["artifacts"] = []
        self.assertFalse(self.check()[0])

    def test_artifact_cannot_be_borrowed_from_another_run(self):
        self.artifact["workflow_run"]["id"] = 99
        self.assertFalse(self.check()[0])

    def test_receipt_cannot_be_borrowed_from_another_run_or_attempt(self):
        for key, value in (("run_id", 99), ("run_attempt", 2), ("repository", "fork/praxis")):
            original = self.receipt[key]
            with self.subTest(key=key):
                self.receipt[key] = value
                self.reject_receipt()
            self.receipt[key] = original

    def test_skipped_job_cannot_create_a_recursive_validation_chain(self):
        for job in self.jobs:
            job["conclusion"] = "skipped"
        self.reject_receipt()

    def test_each_of_six_jobs_must_actually_succeed(self):
        for job in self.jobs:
            for conclusion in ("failure", "cancelled", "skipped"):
                with self.subTest(job=job["name"], conclusion=conclusion):
                    job["conclusion"] = conclusion
                    self.assertFalse(self.check()[0])
            job["conclusion"] = "success"

    def test_skipped_test_step_and_missing_anchor_rejected(self):
        self.jobs[0]["steps"][0]["conclusion"] = "skipped"
        self.assertFalse(self.check()[0])
        self.jobs[0]["steps"] = []
        self.assertFalse(self.check()[0])

    def test_missing_or_duplicate_expected_job_rejected(self):
        removed = self.jobs.pop()
        self.assertFalse(self.check()[0])
        self.jobs.extend([removed, removed])
        self.assertFalse(self.check()[0])

    def test_restarted_run_does_not_reuse_old_attempt(self):
        restarted = copy.deepcopy(self.run)
        restarted.update(run_attempt=2, status="in_progress", conclusion=None)
        self.api.sequences[RUN_PATH] = [restarted]
        self.assertFalse(self.check()[0])

    def test_wrong_workflow_id_path_or_called_definitions_rejected(self):
        for key, value in (("workflow_id", 99), ("path", ".github/workflows/sync_upstream.yml"),
                           ("referenced_workflows", [{"path": validation.WORKFLOW}])):
            original = self.run.get(key)
            with self.subTest(key=key):
                self.run[key] = value
                self.assertFalse(self.check()[0])
            self.run[key] = original

    def make_dispatch(self):
        self.run.update(event="workflow_dispatch", head_sha=TARGET, pull_requests=[])
        self.receipt.update(event="workflow_dispatch", run_head_sha=TARGET,
                            source_sha=TARGET, workflow_sha=TARGET, pr_number=None,
                            pr_base_sha=None, pr_head_sha=None)
        self.artifact["workflow_run"]["head_sha"] = TARGET
        self.store_receipt()

    def test_direct_dispatch_at_execution_sha_is_reusable(self):
        self.make_dispatch()
        self.assertTrue(self.check()[0])

    def test_dispatch_input_different_from_run_ref_not_attested(self):
        self.make_dispatch()
        self.receipt["source_sha"] = MERGE
        self.reject_receipt()

    def test_push_at_execution_sha_is_reusable(self):
        self.make_dispatch()
        self.run["event"] = "push"
        self.receipt["event"] = "push"
        self.store_receipt()
        self.assertTrue(self.check()[0])

    def set_execution_environment(self, source=MERGE):
        self.event_path.write_text(json.dumps(self.event), encoding="utf-8")
        os.environ.update({
            "GITHUB_EVENT_PATH": str(self.event_path), "GITHUB_EVENT_NAME": "pull_request",
            "GITHUB_REPOSITORY": REPOSITORY, "GITHUB_RUN_ID": "10", "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_SHA": source, "GITHUB_WORKFLOW_SHA": source,
            "GITHUB_WORKFLOW_REF": f"{REPOSITORY}/{validation.WORKFLOW}@refs/pull/9/merge",
        })

    def test_original_pr_never_shortcuts_even_with_matching_receipt(self):
        self.set_execution_environment()
        with self.assertRaisesRegex(validation.Unverified, "PR jobs always"):
            self.check(quality=True)

    def test_reusable_quality_gate_falls_back(self):
        self.set_execution_environment()
        self.run["path"] = ".github/workflows/sync_upstream.yml"
        with self.assertRaisesRegex(validation.Unverified, "reusable provenance"):
            self.check(quality=True)

    def test_quality_execution_definition_checked_not_only_source_definition(self):
        self.set_execution_environment(TARGET)
        self.make_dispatch()
        os.environ["GITHUB_WORKFLOW_SHA"] = OTHER
        with self.assertRaisesRegex(validation.Unverified, "workflow SHA"):
            self.check(quality=True)

    def test_current_run_is_not_its_own_evidence(self):
        os.environ["GITHUB_RUN_ID"] = "10"
        with patch.object(self.validator, "execution_matches"):
            self.assertFalse(self.check()[0])

    def bundle_environment(self):
        caller = copy.deepcopy(self.run)
        caller.update(id=99, path=".github/workflows/bundle_fork.yml", event="workflow_dispatch", head_sha=OTHER)
        self.api.responses[f"{ROOT}/actions/runs/99"] = caller
        os.environ.update({
            "GITHUB_REPOSITORY": REPOSITORY, "GITHUB_RUN_ID": "99",
            "GITHUB_WORKFLOW_SHA": OTHER,
            "GITHUB_WORKFLOW_REF": f"{REPOSITORY}/.github/workflows/bundle_fork.yml@refs/heads/main",
        })

    def test_bundle_checks_actual_executing_relative_workflow_definition(self):
        self.bundle_environment()
        self.assertTrue(self.check()[0])
        self.api.responses[f"{ROOT}/contents/{validation.WORKFLOW}?ref={OTHER}"]["sha"] = OTHER
        with self.assertRaisesRegex(validation.Unverified, "actual reusable definition"):
            self.check()

    def test_unknown_or_foreign_bundle_execution_cannot_claim_source_definition(self):
        self.bundle_environment()
        self.api.responses[f"{ROOT}/actions/runs/99"]["path"] = ".github/workflows/unknown.yml"
        with self.assertRaises(validation.Unverified):
            self.check()

    def test_missing_actions_execution_context_fails_closed(self):
        os.environ["GITHUB_ACTIONS"] = "true"
        with self.assertRaises(KeyError):
            self.check()

    def test_record_original_full_merge_not_pr_head(self):
        self.set_execution_environment()
        receipt = self.validator.record(MERGE)
        self.assertEqual(receipt, self.receipt)

    def reported_mutated_association(self):
        # Review observed run 36875953557 retaining bfafd65... while its
        # association advanced to ac58ad.... Unprovided SHA suffixes are synthetic.
        original_head = "bfafd65" + "0" * 33
        newer_head = "ac58ad" + "0" * 34
        run_id = 36875953557
        self.run.update(id=run_id, head_sha=original_head)
        self.run["pull_requests"][0]["head"]["sha"] = newer_head
        self.run["pull_requests"][0]["base"]["sha"] = OTHER
        self.api.responses[f"{ROOT}/pulls/9"] = copy.deepcopy(self.run["pull_requests"][0])
        self.api.responses[f"{ROOT}/git/commits/{MERGE}"]["parents"][1]["sha"] = original_head
        self.event["pull_request"]["head"]["sha"] = original_head
        self.receipt.update(run_id=run_id, run_head_sha=original_head, pr_head_sha=original_head)
        self.artifact["name"] = validation.artifact_name(run_id, 1)
        self.artifact["workflow_run"].update(id=run_id, head_sha=original_head)
        for job in self.jobs:
            job["run_id"] = run_id
        path = f"{ROOT}/actions/runs/{run_id}"
        self.api.responses[path] = self.run
        self.api.responses[f"{path}/attempts/1/jobs"] = {"jobs": self.jobs}
        self.api.responses[f"{path}/artifacts"] = {"artifacts": [self.artifact]}
        self.store_receipt()
        return run_id

    def test_reported_run_with_mutated_association_reuses_original_event_merge(self):
        self.reported_mutated_association()
        self.assertTrue(self.check()[0])

    def test_reported_run_records_event_not_mutated_api_association(self):
        run_id = self.reported_mutated_association()
        self.set_execution_environment()
        os.environ["GITHUB_RUN_ID"] = str(run_id)
        self.assertEqual(self.validator.record(MERGE), self.receipt)

    def test_missing_event_payload_prevents_receipt(self):
        self.set_execution_environment()
        self.event_path.unlink()
        with self.assertRaises(OSError):
            self.validator.record(MERGE)

    def test_invalid_event_head_base_number_and_fork_prevent_receipt(self):
        original = copy.deepcopy(self.event)
        for field in ("head", "base", "number", "fork", "repository"):
            with self.subTest(field=field):
                self.event = copy.deepcopy(original)
                if field in ("head", "base"):
                    self.event["pull_request"][field]["sha"] = OTHER
                elif field == "number":
                    self.event["number"] = 12
                elif field == "fork":
                    self.event["pull_request"]["head"]["repo"]["id"] = 99
                else:
                    self.event["repository"]["id"] = 99
                self.set_execution_environment()
                with self.assertRaises(validation.Unverified):
                    self.validator.record(MERGE)

    def test_record_requires_all_six_successful_jobs(self):
        self.set_execution_environment()
        self.jobs[-1]["conclusion"] = "skipped"
        with self.assertRaises(validation.Unverified):
            self.validator.record(MERGE)

    def test_record_rejects_non_event_source_and_stale_attempt(self):
        self.set_execution_environment()
        with self.assertRaises(validation.Unverified):
            self.validator.record(TARGET)
        os.environ["GITHUB_RUN_ATTEMPT"] = "2"
        with self.assertRaises(validation.Unverified):
            self.validator.record(MERGE)

    def test_join_matching_original_pr_requires_final_receipt(self):
        active = copy.deepcopy(self.run)
        active.update(status="in_progress", conclusion=None)
        self.api.responses[RUNS_PATH]["workflow_runs"] = [active]
        self.api.sequences[RUN_PATH] = [self.run, self.run]
        clock = Clock()
        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
            validated, explanation = self.check(wait=60)
        self.assertTrue(validated)
        self.assertIn("Joined original full run", explanation)
        self.assertEqual(clock.now, 30)
        self.assertTrue(all("cancel" not in call and "dispatch" not in call for call in self.api.calls))

    def test_join_timeout_is_bounded_and_never_cancels_any_revision(self):
        self.run.update(status="in_progress", conclusion=None)
        clock = Clock()
        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
            self.assertFalse(self.check(wait=20)[0])
        self.assertEqual(clock.now, 20)
        self.assertTrue(all("cancel" not in call for call in self.api.calls))

    def test_join_does_not_follow_superseding_attempt(self):
        active = copy.deepcopy(self.run)
        active.update(status="in_progress", conclusion=None)
        newer = copy.deepcopy(active)
        newer.update(run_attempt=2, head_sha=OTHER)
        self.api.responses[RUNS_PATH]["workflow_runs"] = [active]
        self.api.sequences[RUN_PATH] = [newer]
        clock = Clock()
        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
            with self.assertRaisesRegex(validation.Unverified, "superseded"):
                self.check(wait=60)
        self.assertTrue(all("cancel" not in call for call in self.api.calls))

    def test_changed_parent_head_of_running_pr_is_not_joined(self):
        self.run.update(status="in_progress", conclusion=None)
        self.api.responses[f"{ROOT}/git/commits/{MERGE}"]["parents"][1]["sha"] = OTHER
        with patch.object(validation.time, "sleep") as sleep:
            self.assertFalse(self.check(wait=30)[0])
        sleep.assert_not_called()

    def test_newer_pr_revision_is_neither_joined_nor_cancelled_for_old_tree(self):
        self.run["conclusion"] = "cancelled"
        newer = copy.deepcopy(self.run)
        newer.update(id=11, head_sha=OTHER, status="in_progress", conclusion=None)
        newer["pull_requests"][0]["head"]["sha"] = OTHER
        self.api.responses[f"{ROOT}/actions/runs/11"] = newer
        self.api.responses[RUNS_PATH]["workflow_runs"] = [newer, self.run]
        with patch.object(validation.time, "sleep") as sleep:
            self.assertFalse(self.check(wait=30)[0])
        sleep.assert_not_called()
        self.assertTrue(all("cancel" not in call for call in self.api.calls))

    def test_non_pr_run_is_not_joined_to_avoid_reuse_wait_chains(self):
        self.make_dispatch()
        self.run.update(status="in_progress", conclusion=None)
        with patch.object(validation.time, "sleep") as sleep:
            self.assertFalse(self.check(wait=30)[0])
        sleep.assert_not_called()

    def make_push(self):
        self.make_dispatch()
        self.run["event"] = "push"
        self.receipt["event"] = "push"
        self.store_receipt()

    def test_main_quality_does_not_join_another_main_push(self):
        self.make_push()
        self.run.update(status="in_progress", conclusion=None)
        current = copy.deepcopy(self.run)
        current["id"] = 99
        self.api.responses[f"{ROOT}/actions/runs/99"] = current
        os.environ.update({
            "GITHUB_REPOSITORY": REPOSITORY, "GITHUB_RUN_ID": "99",
            "GITHUB_WORKFLOW_SHA": TARGET,
            "GITHUB_WORKFLOW_REF": f"{REPOSITORY}/{validation.WORKFLOW}@refs/heads/main",
        })
        with patch.object(validation.time, "sleep", side_effect=AssertionError("main must not join main")) as sleep:
            self.assertFalse(self.check(wait=300, quality=True)[0])
        sleep.assert_not_called()
        self.assertNotIn(RUN_PATH, self.api.calls)
        with self.assertRaisesRegex(validation.Unverified, "verified bundle"):
            self.check(wait=10800, quality=True)

    def test_bundle_does_not_join_dispatch_with_unverifiable_input(self):
        self.bundle_environment()
        self.make_dispatch()
        self.run.update(status="in_progress", conclusion=None)
        with patch.object(validation.time, "sleep", side_effect=AssertionError("dispatch cannot be joined")) as sleep:
            self.assertFalse(self.check(wait=10800)[0])
        sleep.assert_not_called()

    def test_unverified_external_caller_cannot_join_push(self):
        self.make_push()
        self.run.update(status="in_progress", conclusion=None)
        with patch.object(validation.time, "sleep", side_effect=AssertionError("unverified caller joined push")) as sleep:
            self.assertFalse(self.check(wait=300)[0])
        sleep.assert_not_called()
        with self.assertRaisesRegex(validation.Unverified, "verified bundle"):
            self.check(wait=10800)

    def test_bundle_waits_for_direct_push_full_receipt(self):
        self.bundle_environment()
        self.make_push()
        active = copy.deepcopy(self.run)
        active.update(status="in_progress", conclusion=None)
        self.api.responses[RUNS_PATH]["workflow_runs"] = [active]
        clock = Clock()
        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
            validated, explanation = self.check(wait=10800)
        self.assertTrue(validated)
        self.assertIn("Joined original full run 10", explanation)
        self.assertEqual(clock.now, 30)
        self.assertEqual(self.api.calls.count(RUN_PATH), 2)

    def test_bundle_push_join_requires_matching_tree_and_workflow(self):
        self.bundle_environment()
        self.make_push()
        self.run.update(head_sha=OTHER, status="in_progress", conclusion=None)
        # Keep the bundle's own definition fixed while varying the push source.
        caller = self.api.responses[f"{ROOT}/actions/runs/99"]
        caller["head_sha"] = TARGET
        os.environ["GITHUB_WORKFLOW_SHA"] = TARGET
        for field in ("tree", "workflow"):
            with self.subTest(field=field):
                self.api.responses[f"{ROOT}/git/commits/{OTHER}"]["tree"]["sha"] = OTHER if field == "tree" else TREE
                self.api.responses[f"{ROOT}/contents/{validation.WORKFLOW}?ref={OTHER}"]["sha"] = OTHER if field == "workflow" else BLOB
                with patch.object(validation.time, "sleep", side_effect=AssertionError("mismatched push joined")) as sleep:
                    self.assertFalse(self.check(wait=10800)[0])
                sleep.assert_not_called()

    def test_failed_push_join_falls_back(self):
        self.bundle_environment()
        self.make_push()
        active = copy.deepcopy(self.run)
        active.update(status="in_progress", conclusion=None)
        self.api.responses[RUNS_PATH]["workflow_runs"] = [active]
        for conclusion in ("failure", "cancelled"):
            with self.subTest(conclusion=conclusion):
                self.run["conclusion"] = conclusion
                clock = Clock()
                with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
                    with self.assertRaisesRegex(validation.Unverified, "joined run did not succeed"):
                        self.check(wait=10800)
        self.assertNotIn(ARTIFACTS_PATH, self.api.calls)

    def test_successful_skipped_push_without_receipt_is_never_accepted(self):
        self.bundle_environment()
        self.make_push()
        active = copy.deepcopy(self.run)
        active.update(status="in_progress", conclusion=None)
        self.api.sequences[RUNS_PATH] = [{"workflow_runs": [active]}, {"workflow_runs": [self.run]}]
        self.api.responses[ARTIFACTS_PATH] = {"artifacts": []}
        for job in self.jobs:
            job["conclusion"] = "skipped"
        clock = Clock()
        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
            self.assertFalse(self.check(wait=10800)[0])
        self.assertEqual(self.api.calls.count(ARTIFACTS_PATH), 1)
        self.assertNotIn(JOBS_PATH, self.api.calls)

    def test_bundle_rescans_originals_after_joined_push_reused_pr(self):
        self.bundle_environment()
        push = copy.deepcopy(self.run)
        push.update(id=11, event="push", head_sha=TARGET, pull_requests=[], status="in_progress", conclusion=None)
        finished = copy.deepcopy(push)
        finished.update(status="completed", conclusion="success")
        original_active = copy.deepcopy(self.run)
        original_active.update(status="in_progress", conclusion=None)
        self.api.sequences[RUNS_PATH] = [
            {"workflow_runs": [push, original_active]},
            {"workflow_runs": [finished, self.run]},
        ]
        self.api.responses[f"{ROOT}/actions/runs/11"] = finished
        self.api.responses[f"{ROOT}/actions/runs/11/artifacts"] = {"artifacts": []}
        clock = Clock()
        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
            validated, explanation = self.check(wait=10800)
        self.assertTrue(validated)
        self.assertIn("Reused original full run 10", explanation)
        self.assertNotIn("full run 11", explanation)
        self.assertEqual(self.api.calls.count(RUNS_PATH), 2)
        self.assertEqual(clock.now, 30)

    def test_history_time_is_part_of_total_wait_budget(self):
        self.run.update(status="in_progress", conclusion=None)
        clock = Clock()
        original_get = self.api.get

        def slow_history(endpoint):
            if endpoint == RUNS_PATH:
                clock.sleep(45)
            return original_get(endpoint)

        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep), patch.object(self.api, "get", side_effect=slow_history):
            self.assertFalse(self.check(wait=60)[0])
        self.assertEqual(clock.now, 60)
        self.assertNotIn(RUN_PATH, self.api.calls)

    def test_history_search_is_bounded_even_with_long_bundle_wait(self):
        self.bundle_environment()
        clock = Clock()
        original_get = self.api.get

        def exhausted_history(endpoint):
            if endpoint == RUNS_PATH:
                clock.sleep(validation.SCAN_SECONDS)
            return original_get(endpoint)

        with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep), patch.object(self.api, "get", side_effect=exhausted_history):
            self.assertFalse(self.check(wait=10800)[0])
        self.assertEqual(clock.now, validation.SCAN_SECONDS)
        self.assertNotIn(ARTIFACTS_PATH, self.api.calls)

    def test_failed_listing_entries_need_no_per_run_requests(self):
        failed = copy.deepcopy(self.run)
        failed["conclusion"] = "failure"
        self.api.responses[RUNS_PATH]["workflow_runs"] = [failed]
        self.assertFalse(self.check()[0])
        self.assertNotIn(RUN_PATH, self.api.calls)
        self.assertNotIn(ARTIFACTS_PATH, self.api.calls)

    def test_receiptless_history_has_bounded_cheap_scan(self):
        for page in range(1, 4):
            runs = []
            for index in range(100):
                run = copy.deepcopy(self.run)
                run["id"] = page * 1000 + index
                runs.append(run)
                self.api.responses[f"{ROOT}/actions/runs/{run['id']}/artifacts"] = {"artifacts": []}
            self.api.responses[f"{ROOT}/actions/workflows/30/runs?per_page=100&page={page}"] = {"workflow_runs": runs}
        self.assertFalse(self.check()[0])
        self.assertEqual(sum(call.endswith("/artifacts") for call in self.api.calls), 300)
        self.assertFalse(any("/attempts/" in call or call.endswith("/zip") for call in self.api.calls))
        self.assertNotIn(f"{ROOT}/git/commits/{MERGE}", self.api.calls)

    def test_join_failed_run_or_missing_final_artifact_rejected(self):
        active = copy.deepcopy(self.run)
        active.update(status="in_progress", conclusion=None)
        for missing in (False, True):
            with self.subTest(missing=missing):
                finished = copy.deepcopy(self.run)
                if missing:
                    self.api.responses[ARTIFACTS_PATH] = {"artifacts": []}
                else:
                    finished["conclusion"] = "cancelled"
                self.api.responses[RUNS_PATH]["workflow_runs"] = [active]
                self.api.sequences[RUN_PATH] = [finished]
                clock = Clock()
                with patch.object(validation.time, "monotonic", clock.monotonic), patch.object(validation.time, "sleep", clock.sleep):
                    if missing:
                        self.assertFalse(self.check(wait=60)[0])
                    else:
                        with self.assertRaises(validation.Unverified):
                            self.check(wait=60)


class ArchiveAndCliTests(unittest.TestCase):
    def test_safe_archive_read_without_extraction(self):
        with patch.object(zipfile.ZipFile, "extract") as extract, patch.object(zipfile.ZipFile, "extractall") as extractall:
            self.assertEqual(validation.read_receipt(archive_bytes({"schema": 1})), {"schema": 1})
        extract.assert_not_called()
        extractall.assert_not_called()

    def test_unsafe_paths_and_extra_members_rejected(self):
        for name in ("../receipt.json", "/receipt.json", "nested/receipt.json", "..\\receipt.json"):
            with self.subTest(name=name), self.assertRaises(validation.Unverified):
                validation.read_receipt(archive_bytes({}, name=name))
        with self.assertRaises(validation.Unverified):
            validation.read_receipt(archive_bytes({}, extra="unexpected"))

    def test_symlink_and_zip_bomb_rejected(self):
        stream = io.BytesIO()
        with zipfile.ZipFile(stream, "w") as archive:
            entry = zipfile.ZipInfo("receipt.json")
            entry.create_system = 3
            entry.external_attr = (stat.S_IFLNK | 0o777) << 16
            archive.writestr(entry, "elsewhere")
        with self.assertRaises(validation.Unverified):
            validation.read_receipt(stream.getvalue())
        with self.assertRaises(validation.Unverified):
            validation.read_receipt(archive_bytes({"padding": "x" * 70000}))

    def test_malformed_archive_rejected(self):
        with self.assertRaises(zipfile.BadZipFile):
            validation.read_receipt(b"not a zip")

    def test_check_errors_write_false_and_explain_fallback(self):
        for error in (validation.Unverified("missing artifact"), KeyError("provenance"),
                      subprocess.TimeoutExpired("gh", 30), OSError("gh missing"),
                      json.JSONDecodeError("invalid", "", 0)):
            with self.subTest(error=error), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True), patch.object(validation, "Validator", side_effect=error):
                    with contextlib.redirect_stdout(io.StringIO()) as printed:
                        result = validation.main(["check", "--repository", REPOSITORY, "--source-sha", TARGET])
                self.assertEqual(result, 0)
                self.assertEqual(output.read_text(), "validated=false\n")
                self.assertIn("Cannot prove validation", printed.getvalue())

    def test_check_success_output_contract(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True), patch.object(validation, "Validator") as validator:
                validator.return_value.check.return_value = (True, "Original full run 10")
                with contextlib.redirect_stdout(io.StringIO()) as printed:
                    validation.main(["check", "--repository", REPOSITORY, "--source-sha", TARGET])
            self.assertEqual(output.read_text(), "validated=true\n")
            self.assertIn("Original full run 10", printed.getvalue())

    def test_gh_transport_only_performs_bounded_get_requests(self):
        result = subprocess.CompletedProcess([], 0, b"{}", b"")
        with patch.object(validation.subprocess, "run", return_value=result) as run:
            validation.GitHub(REPOSITORY).get(ROOT)
        self.assertEqual(run.call_args.args[0], ["gh", "api", "--method", "GET", ROOT])
        self.assertEqual(run.call_args.kwargs["timeout"], 30)

    def test_request_timeout_is_clipped_to_total_deadline(self):
        api = validation.GitHub(REPOSITORY)
        api.deadline = 12
        result = subprocess.CompletedProcess([], 0, b"{}", b"")
        with patch.object(validation.time, "monotonic", return_value=10), patch.object(validation.subprocess, "run", return_value=result) as run:
            api.get(ROOT)
        self.assertEqual(run.call_args.kwargs["timeout"], 2)
        with patch.object(validation.time, "monotonic", return_value=12), patch.object(validation.subprocess, "run") as run:
            with self.assertRaises(validation.DeadlineExceeded):
                api.get(ROOT)
        run.assert_not_called()

    def test_cli_accepts_bundle_wait_cap_but_rejects_larger_values(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True), patch.object(validation, "Validator") as validator:
                validator.return_value.check.return_value = (False, "No receipt")
                with contextlib.redirect_stdout(io.StringIO()):
                    validation.main(["check", "--repository", REPOSITORY, "--source-sha", TARGET, "--wait-seconds", "10800"])
                validator.return_value.check.assert_called_once_with(TARGET, 10800, False)
                validator.reset_mock()
                with contextlib.redirect_stdout(io.StringIO()):
                    validation.main(["check", "--repository", REPOSITORY, "--source-sha", TARGET, "--wait-seconds", "10801"])
                validator.assert_not_called()
            self.assertEqual(output.read_text(), "validated=false\nvalidated=false\n")

    def test_permission_failure_is_not_silently_trusted(self):
        result = subprocess.CompletedProcess([], 1, b"", b"HTTP 403 forbidden")
        with patch.object(validation.subprocess, "run", return_value=result):
            with self.assertRaises(validation.Unverified):
                validation.GitHub(REPOSITORY).get(ROOT)

    def test_pagination_includes_later_jobs_and_has_a_bound(self):
        api = validation.GitHub(REPOSITORY)
        with patch.object(api, "get", side_effect=[{"jobs": [1] * 100}, {"jobs": [2]}]):
            self.assertEqual(len(api.pages("jobs", "jobs")), 101)
        with patch.object(api, "get", return_value={"jobs": [1] * 100}):
            with self.assertRaises(validation.Unverified):
                api.pages("jobs", "jobs", limit=2)


class WorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = (SCRIPT.parent.parent / validation.WORKFLOW).read_text(encoding="utf-8")

    def test_pr_supersession_and_distinct_non_pr_concurrency(self):
        self.assertIn("format('pr-{0}', github.event.pull_request.number)", self.workflow)
        self.assertIn("format('run-{0}-{1}', github.run_id, github.run_attempt)", self.workflow)
        self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}", self.workflow)
        self.assertNotIn("group: architect-quality-${{ inputs.source_ref", self.workflow)

    def test_pr_gate_never_reuses_and_all_six_jobs_use_pinned_source(self):
        self.assertIn('if [ "$GITHUB_EVENT_NAME" = pull_request ]; then', self.workflow)
        self.assertEqual(self.workflow.count("SOURCE_REF: ${{ needs.prepare.outputs.source_sha }}"), 6)
        for name in validation.EXPECTED_JOBS:
            self.assertIn(f"name: {name}\n", self.workflow)

    def test_full_receipt_checks_every_job_and_reuse_has_explicit_aggregate(self):
        receipt = self.workflow.split("  full-receipt:\n", 1)[1].split("  source-validated:\n", 1)[0]
        for job in ("formatting", "architect-tests", "agent-tests", "updater-tests", "windows-updater-tests", "clippy"):
            self.assertIn(f"needs.{job}.result == 'success'", receipt)
        self.assertIn("needs.prepare.outputs.validated != 'true'", receipt)
        self.assertIn("name: Quality source validated", self.workflow)
        self.assertIn("if: always()", self.workflow)
        self.assertIn('assert all(job["result"] == "success"', self.workflow)
        self.assertIn('assert all(job["result"] == "skipped"', self.workflow)
        self.assertIn("run: python script/test-praxis-validation.py", self.workflow)

    def test_aggregate_rejects_failed_or_skipped_checks_without_verified_reuse(self):
        block = self.workflow.split("          python - <<'PY'\n", 1)[1].split("          PY\n", 1)[0]
        code = textwrap.dedent(block)
        results = {"prepare": {"result": "success", "outputs": {"validated": "false"}}}
        for name in (*validation.EXPECTED_JOBS, "full-receipt"):
            results[name] = {"result": "success"}

        def aggregate():
            with patch.dict(os.environ, {"RESULTS": json.dumps(results)}), contextlib.redirect_stdout(io.StringIO()):
                exec(code, {})

        aggregate()
        for result in ("failure", "cancelled", "skipped"):
            results["Formatting"]["result"] = result
            with self.subTest(result=result), self.assertRaises(AssertionError):
                aggregate()
        results["prepare"]["outputs"]["validated"] = "true"
        for name, job in results.items():
            if name != "prepare":
                job["result"] = "skipped"
        aggregate()
        results["Formatting"]["result"] = "failure"
        with self.assertRaises(AssertionError):
            aggregate()


if __name__ == "__main__":
    unittest.main(verbosity=2)
