#!/usr/bin/env python3
"""Fail-closed reuse of original Architect quality validations.

See praxis-validation.README.md for the trust boundary and caller contract.
Only Python's standard library, git, and gh are required. No API writes occur.
"""

import argparse
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
from urllib.parse import quote
import zipfile


WORKFLOW = ".github/workflows/architect_quality.yml"
RECEIPT = "receipt.json"
ARTIFACT_PREFIX = "praxis-quality-full"
EXPECTED_JOBS = {
    "Formatting": "Test validation reuse provenance",
    "Architect graph tests": "Test the Architect graph and runner",
    "Agent and canvas integration tests": "Test Praxis Remote",
    "Auto-update integrity tests": "Test the updater",
    "Windows update helper tests": "Test the Windows update helper",
    "Clippy package checks": "Check the Architect packages with Clippy",
}
ACTIVE = {"queued", "in_progress", "waiting", "pending", "requested"}
SCAN_SECONDS = 60
SCAN_CANDIDATES = 300
POLL_SECONDS = 30


class Unverified(Exception):
    pass


class DeadlineExceeded(Unverified):
    pass


def require(condition, explanation):
    if not condition:
        raise Unverified(explanation)


def sha(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value),
            "expected a full, lowercase Git commit/object SHA")
    return value


def output(name, value):
    destination = os.environ.get("GITHUB_OUTPUT")
    if destination:
        with open(destination, "a", encoding="utf-8") as stream:
            stream.write(f"{name}={value}\n")


def git_object(source, kind):
    sha(source)
    result = subprocess.run(
        ["git", "rev-parse", "--verify", f"{source}^{{{kind}}}"],
        check=True, capture_output=True, text=True, timeout=15,
    )
    return sha(result.stdout.strip())


class GitHub:
    def __init__(self, repository):
        require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository),
                "expected OWNER/REPO")
        self.repository = repository
        self.root = f"repos/{repository}"
        self.deadline = None

    def raw(self, endpoint):
        command = ["gh", "api", "--method", "GET", endpoint]
        timeout = 30
        if self.deadline is not None:
            timeout = min(timeout, self.deadline - time.monotonic())
            if timeout <= 0:
                raise DeadlineExceeded("validation search/join deadline expired")
        try:
            result = subprocess.run(command, capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            if self.deadline is not None and time.monotonic() >= self.deadline:
                raise DeadlineExceeded("validation search/join deadline expired") from error
            raise
        require(result.returncode == 0,
                "GitHub read failed: " + result.stderr.decode("utf-8", "replace")[:300])
        return result.stdout

    def get(self, endpoint):
        return json.loads(self.raw(endpoint))

    def pages(self, endpoint, key, limit=10):
        values = []
        separator = "&" if "?" in endpoint else "?"
        for page in range(1, limit + 1):
            response = self.get(f"{endpoint}{separator}per_page=100&page={page}")
            batch = response[key]
            require(isinstance(batch, list), "invalid paginated GitHub response")
            values.extend(batch)
            if len(batch) < 100:
                return values
        raise Unverified("GitHub pagination bound exceeded")

    def commit(self, source):
        source = sha(source)
        commit = self.get(f"{self.root}/git/commits/{source}")
        require(commit["sha"] == source, "GitHub returned a different commit")
        sha(commit["tree"]["sha"])
        return commit

    def workflow_blob(self, source):
        entry = self.get(f"{self.root}/contents/{WORKFLOW}?ref={sha(source)}")
        require(entry["type"] == "file" and entry["path"] == WORKFLOW,
                "workflow definition is not a regular file")
        return sha(entry["sha"])


class Validator:
    def __init__(self, api):
        self.api = api
        self.repository = api.repository
        self.metadata = api.get(api.root)
        require(self.metadata["full_name"] == self.repository, "repository mismatch")
        self.workflow = api.get(f"{api.root}/actions/workflows/architect_quality.yml")
        require(self.workflow["path"] == WORKFLOW, "workflow identity mismatch")

    def run(self, run_id):
        return self.api.get(f"{self.api.root}/actions/runs/{int(run_id)}")

    def direct_run(self, run):
        require(run["workflow_id"] == self.workflow["id"] and run["path"] == WORKFLOW,
                "not a direct Architect quality run (reusable provenance is not supported)")
        for key in ("repository", "head_repository"):
            require(run[key]["id"] == self.metadata["id"]
                    and run[key]["full_name"] == self.repository,
                    "foreign repository or fork run")
        require(run["event"] in {"pull_request", "push", "workflow_dispatch"},
                "unsupported source event")
        require(not run.get("referenced_workflows"), "unverified called workflow definitions")
        require(isinstance(run["run_attempt"], int) and run["run_attempt"] > 0,
                "invalid run attempt")
        sha(run["head_sha"])

    def pr_association(self, run, pr_number=None):
        associations = run["pull_requests"]
        if not associations:
            # GitHub can clear run.pull_requests after merge. Corroborate the
            # event/receipt's PR identity against the immutable run head instead.
            associations = self.api.get(
                f"{self.api.root}/commits/{sha(run['head_sha'])}/pulls?per_page=100")
            require(isinstance(associations, list), "invalid commit/PR association response")
            if pr_number is not None:
                associations = [pull for pull in associations if pull["number"] == pr_number]
        require(len(associations) == 1, "missing or ambiguous PR/run association")
        association = associations[0]
        if pr_number is not None:
            require(association["number"] == pr_number, "PR association identity mismatch")

        for side in ("head", "base"):
            require(association[side]["repo"]["id"] == self.metadata["id"],
                    "fork PR association")
        # Both run.pull_requests and the live PR contain moving SHAs. Only the
        # event payload captured by record can supply the tested base/head.
        pull = self.api.get(f"{self.api.root}/pulls/{int(association['number'])}")
        for side in ("head", "base"):
            require(pull[side]["repo"]["id"] == self.metadata["id"], "fork PR")
        return association

    def pr_event(self, run):
        require(os.environ["GITHUB_EVENT_NAME"] == "pull_request", "recording event mismatch")
        event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text(encoding="utf-8"))
        require(event["repository"]["id"] == self.metadata["id"]
                and event["repository"]["full_name"] == self.repository, "event repository mismatch")
        pull = event["pull_request"]
        association = self.pr_association(run, pull["number"])
        require(event["number"] == pull["number"] == association["number"], "event PR mismatch")
        for side in ("base", "head"):
            require(pull[side]["repo"]["id"] == self.metadata["id"], "fork PR event")
        base = sha(pull["base"]["sha"])
        head = sha(pull["head"]["sha"])
        require(head == run["head_sha"], "event head differs from immutable run head")
        return {"pr_number": pull["number"], "pr_base_sha": base, "pr_head_sha": head}

    def provenance(self, run, receipt):
        self.direct_run(run)
        require(receipt["schema"] == 2 and receipt["kind"] == "original-full",
                "not an original full receipt")
        for key, value in {
            "repository": self.repository,
            "repository_id": self.metadata["id"],
            "run_id": run["id"],
            "run_attempt": run["run_attempt"],
            "run_head_sha": run["head_sha"],
            "event": run["event"],
            "workflow_id": self.workflow["id"],
            "workflow_path": WORKFLOW,
        }.items():
            require(receipt[key] == value, f"receipt {key} mismatch")
        source = sha(receipt["source_sha"])
        commit = self.api.commit(source)
        require(commit["tree"]["sha"] == receipt["source_tree"], "receipt tree mismatch")
        definition = sha(receipt["workflow_sha"])
        if run["event"] == "pull_request":
            association = self.pr_association(run, receipt["pr_number"])
            require(receipt["pr_number"] == association["number"], "receipt PR mismatch")
            base = sha(receipt["pr_base_sha"])
            require(sha(receipt["pr_head_sha"]) == run["head_sha"], "receipt PR head/run mismatch")
            require([parent["sha"] for parent in commit["parents"]]
                    == [base, run["head_sha"]],
                    "tested commit is not the event's PR merge (base drift or wrong head)")
            require(definition == source, "PR workflow did not execute from the tested merge")
        else:
            # Dispatch inputs are not exposed reliably by the run API. Only
            # attest a dispatch whose pinned checkout equals its actual run ref.
            require(source == run["head_sha"] == definition,
                    "selected source differs from the executing push/dispatch ref")
            require(all(receipt[key] is None for key in ("pr_number", "pr_base_sha", "pr_head_sha")),
                    "unexpected PR provenance")
        blob = self.api.workflow_blob(definition)
        require(blob == receipt["workflow_blob"] == self.api.workflow_blob(source),
                "executed workflow differs from the source workflow")
        return commit

    def full_jobs(self, run):
        jobs = self.api.pages(
            f"{self.api.root}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs",
            "jobs",
        )
        for name, anchor in EXPECTED_JOBS.items():
            matches = [job for job in jobs if job["name"] == name]
            require(len(matches) == 1, f"missing or ambiguous full job: {name}")
            job = matches[0]
            require(job["run_id"] == run["id"] and job["status"] == "completed"
                    and job["conclusion"] == "success", f"full job did not succeed: {name}")
            steps = job["steps"]
            require(any(step["name"] == anchor for step in steps), f"missing test step: {name}")
            require(all(step["status"] == "completed" and step["conclusion"] == "success"
                        for step in steps), f"skipped or unsuccessful step: {name}")

    def artifact_receipt(self, run):
        artifacts = self.api.pages(f"{self.api.root}/actions/runs/{run['id']}/artifacts", "artifacts")
        name = artifact_name(run["id"], run["run_attempt"])
        matches = [artifact for artifact in artifacts if artifact["name"] == name]
        require(len(matches) == 1, "missing or ambiguous original full artifact")
        artifact = matches[0]
        require(not artifact["expired"] and 0 < artifact["size_in_bytes"] <= 262144,
                "expired or oversized receipt artifact")
        association = artifact["workflow_run"]
        require(association["id"] == run["id"]
                and association["head_sha"] == run["head_sha"]
                and association["repository_id"] == self.metadata["id"]
                and association["head_repository_id"] == self.metadata["id"],
                "artifact/run association mismatch")
        data = self.api.raw(f"{self.api.root}/actions/artifacts/{int(artifact['id'])}/zip")
        return read_receipt(data)

    def validated_run(self, run, target_tree, target_blob):
        self.direct_run(run)
        require(run["status"] == "completed" and run["conclusion"] == "success",
                "run did not complete successfully")
        receipt = self.artifact_receipt(run)
        commit = self.provenance(run, receipt)
        require(commit["tree"]["sha"] == target_tree, "tested source tree differs")
        require(receipt["workflow_blob"] == target_blob, "workflow definition changed")
        self.full_jobs(run)
        # Do not accept an old attempt while a re-run has invalidated it.
        latest = self.run(run["id"])
        require(latest["run_attempt"] == run["run_attempt"]
                and latest["status"] == "completed" and latest["conclusion"] == "success",
                "run was restarted or no longer successful")
        return receipt

    def target(self, source):
        require(git_object(source, "commit") == source, "source is not the requested commit")
        tree = git_object(source, "tree")
        require(self.api.commit(source)["tree"]["sha"] == tree, "local/API target tree mismatch")
        return tree, self.api.workflow_blob(source)

    def execution_matches(self, target_blob, quality_run):
        bundle = False
        if quality_run:
            require(os.environ.get("GITHUB_REPOSITORY") == self.repository,
                    "executing repository mismatch")
            current = self.run(os.environ["GITHUB_RUN_ID"])
            self.direct_run(current)
            require(current["event"] != "pull_request", "PR jobs always run in full")
            require(os.environ["GITHUB_WORKFLOW_REF"].startswith(f"{self.repository}/{WORKFLOW}@"),
                    "unverified executing workflow ref")
            definition = sha(os.environ["GITHUB_WORKFLOW_SHA"])
            require(definition == current["head_sha"], "unverified executing workflow SHA")
        else:
            # A relative reusable call executes from the caller's commit, which
            # can differ from both source_ref and today's default branch.
            if os.environ.get("GITHUB_RUN_ID") or os.environ.get("GITHUB_ACTIONS") == "true":
                require(os.environ["GITHUB_REPOSITORY"] == self.repository,
                        "bundle repository mismatch")
                caller = self.run(os.environ["GITHUB_RUN_ID"])
                caller_path = ".github/workflows/bundle_fork.yml"
                require(caller["path"] == caller_path and caller["event"] == "workflow_dispatch",
                        "unknown external caller; cannot prove its reusable definition")
                for key in ("repository", "head_repository"):
                    require(caller[key]["id"] == self.metadata["id"], "foreign bundle caller")
                require(os.environ["GITHUB_WORKFLOW_REF"].startswith(f"{self.repository}/{caller_path}@"),
                        "unverified bundle execution ref")
                definition = sha(os.environ["GITHUB_WORKFLOW_SHA"])
                require(definition == caller["head_sha"]
                        and self.api.workflow_blob(definition) == target_blob,
                        "bundle's actual reusable definition differs from target")
                bundle = True
            branch = quote(self.metadata["default_branch"], safe="")
            definition = self.api.get(f"{self.api.root}/commits/{branch}")["sha"]
        require(self.api.workflow_blob(definition) == target_blob,
                "executing/default-branch workflow differs from target; run the full suite")
        return bundle

    def matching_original_inflight(self, run, target_tree, target_blob, allow_push=False):
        self.direct_run(run)
        require(run["status"] in ACTIVE, "run is not active")
        if run["event"] == "pull_request":
            association = self.pr_association(run)
            reference = self.api.get(
                f"{self.api.root}/git/ref/pull/{int(association['number'])}/merge")
            source = reference["object"]["sha"]
            commit = self.api.commit(source)
            parents = commit["parents"]
            # This is only a wait hint: the mutable merge ref cannot prove the
            # original base. The eventual event-backed receipt must do that.
            require(len(parents) == 2 and parents[1]["sha"] == run["head_sha"],
                    "moving PR merge ref")
        else:
            require(allow_push and run["event"] == "push",
                    "only an independent bundle caller may join direct pushes")
            # A direct push has no source_ref input: this workflow checks out
            # its immutable head SHA and executes the definition at that SHA.
            source = run["head_sha"]
            commit = self.api.commit(source)
        require(commit["tree"]["sha"] == target_tree
                and self.api.workflow_blob(source) == target_blob, "in-flight source differs")
        return run["id"], run["run_attempt"]

    def scan(self, tree, blob, wait, allow_push, rejected, budget):
        own_run = os.environ.get("GITHUB_RUN_ID")
        inflight = []
        started = time.monotonic()
        total_deadline = self.api.deadline
        self.api.deadline = min(total_deadline, started + budget["seconds"])
        try:
            for page in range(1, 4):
                if budget["candidates"] <= 0 or time.monotonic() >= self.api.deadline:
                    break
                response = self.api.get(
                    f"{self.api.root}/actions/workflows/{self.workflow['id']}/runs?per_page=100&page={page}")
                runs = response["workflow_runs"]
                for run in runs:
                    if budget["candidates"] <= 0 or time.monotonic() >= self.api.deadline:
                        break
                    if str(run["id"]) == own_run:
                        continue
                    completed = run.get("status") == "completed"
                    # The listing already carries run metadata. Do not fetch
                    # details/jobs/commits for failures or receipt-less runs.
                    if completed and run.get("conclusion") != "success":
                        continue
                    if not completed and (not wait or run.get("status") not in ACTIVE):
                        continue
                    identity = (run["id"], run.get("run_attempt"))
                    if identity in rejected:
                        continue
                    budget["candidates"] -= 1
                    try:
                        if completed:
                            receipt = self.validated_run(run, tree, blob)
                            return (True, f"Reused original full run {run['id']} attempt {run['run_attempt']} ({receipt['source_sha']}, tree {tree})"), inflight
                        inflight.append(self.matching_original_inflight(run, tree, blob, allow_push))
                    except DeadlineExceeded:
                        raise
                    except (Unverified, KeyError, TypeError, ValueError, zipfile.BadZipFile) as error:
                        if completed:
                            rejected.add(identity)
                        print(f"Candidate {run['id']} not reusable: {error}")
                if len(runs) < 100:
                    break
        except DeadlineExceeded:
            print("History search budget exhausted")
        finally:
            budget["seconds"] = max(0, budget["seconds"] - (time.monotonic() - started))
            self.api.deadline = total_deadline
        return None, inflight

    def check(self, source, wait_seconds=0, quality_run=False):
        require(0 <= wait_seconds <= 10800, "wait must be between 0 and 10800 seconds")
        previous_deadline = self.api.deadline
        deadline = time.monotonic() + (wait_seconds or SCAN_SECONDS)
        if previous_deadline is not None:
            deadline = min(deadline, previous_deadline)
        self.api.deadline = deadline
        try:
            return self.check_until(source, wait_seconds, quality_run, deadline)
        finally:
            self.api.deadline = previous_deadline

    def check_until(self, source, wait_seconds, quality_run, deadline):
        tree, blob = self.target(source)
        bundle = self.execution_matches(blob, quality_run)
        require(wait_seconds <= 600 or bundle, "only a verified bundle caller may wait over 600 seconds")
        budget = {"seconds": SCAN_SECONDS, "candidates": SCAN_CANDIDATES}
        rejected = set()
        result, inflight = self.scan(tree, blob, bool(wait_seconds), bundle, rejected, budget)
        if result:
            return result
        if inflight and wait_seconds:
            # Join one immutable attempt. Main quality cannot join another push
            # (or a bundle); only an independent bundle may wait on main.
            run_id, attempt = inflight[0]
            print(f"Waiting within {wait_seconds}s total budget for run {run_id} attempt {attempt}")
            while time.monotonic() < deadline:
                time.sleep(min(POLL_SECONDS, max(0, deadline - time.monotonic())))
                if time.monotonic() >= deadline:
                    break
                run = self.run(run_id)
                require(run["run_attempt"] == attempt, "joined attempt was superseded")
                if run["status"] == "completed":
                    require(run["conclusion"] == "success", "joined run did not succeed")
                    try:
                        receipt = self.validated_run(run, tree, blob)
                        return True, f"Joined original full run {run_id} ({receipt['source_sha']}, tree {tree})"
                    except DeadlineExceeded:
                        raise
                    except (Unverified, KeyError, TypeError, ValueError, zipfile.BadZipFile) as error:
                        print(f"Joined run {run_id} is not original full evidence: {error}")
                        rejected.add((run_id, attempt))
                        # Main may itself have reused an original PR while we
                        # waited. Search originals once more; never trust main's
                        # skipped jobs or follow a new chain of waiting runs.
                        result, _inflight = self.scan(tree, blob, False, False, rejected, budget)
                        return result or (False, "Joined run has no verified original full receipt; run the full suite")
                require(run["status"] in ACTIVE, "joined run is no longer active")
            return False, "Search/join timed out; run the full suite"
        return False, "No verified original full validation for this tree; run the full suite"

    def record(self, source):
        require(os.environ["GITHUB_REPOSITORY"] == self.repository, "recording repository mismatch")
        run = self.run(os.environ["GITHUB_RUN_ID"])
        self.direct_run(run)
        require(run["run_attempt"] == int(os.environ["GITHUB_RUN_ATTEMPT"]), "recording attempt mismatch")
        require(os.environ["GITHUB_SHA"] == source, "input source is not the event's tested commit")
        require(os.environ["GITHUB_WORKFLOW_REF"].startswith(f"{self.repository}/{WORKFLOW}@"),
                "unverified workflow ref")
        tree, blob = self.target(source)
        pr = self.pr_event(run) if run["event"] == "pull_request" else {
            "pr_number": None, "pr_base_sha": None, "pr_head_sha": None,
        }
        receipt = {
            "schema": 2,
            "kind": "original-full",
            "repository": self.repository,
            "repository_id": self.metadata["id"],
            "source_sha": source,
            "source_tree": tree,
            "run_id": run["id"],
            "run_attempt": run["run_attempt"],
            "run_head_sha": run["head_sha"],
            "event": run["event"],
            "workflow_id": self.workflow["id"],
            "workflow_path": WORKFLOW,
            "workflow_sha": sha(os.environ["GITHUB_WORKFLOW_SHA"]),
            "workflow_blob": blob,
            **pr,
        }
        self.provenance(run, receipt)
        self.full_jobs(run)
        return receipt


def artifact_name(run_id, attempt):
    return f"{ARTIFACT_PREFIX}-{run_id}-{attempt}"


def read_receipt(data):
    require(len(data) <= 262144, "oversized receipt ZIP")
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        entries = archive.infolist()
        require(len(entries) == 1, "receipt ZIP must contain exactly one file")
        entry = entries[0]
        require(entry.filename == RECEIPT and not entry.is_dir()
                and stat.S_IFMT(entry.external_attr >> 16) in (0, stat.S_IFREG)
                and not entry.flag_bits & 1 and entry.file_size <= 65536,
                "unsafe receipt ZIP member")
        # Never extract paths supplied by an artifact.
        return json.loads(archive.read(entry))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "record"))
    parser.add_argument("--repository", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--wait-seconds", type=int, default=0)
    parser.add_argument("--quality-run", action="store_true",
                        help="internal: verify the current direct quality workflow definition")
    arguments = parser.parse_args(argv)
    try:
        require(0 <= arguments.wait_seconds <= 10800, "wait must be between 0 and 10800 seconds")
        source = sha(arguments.source_sha)
        api = GitHub(arguments.repository)
        if arguments.command == "check":
            api.deadline = time.monotonic() + (arguments.wait_seconds or SCAN_SECONDS)
        validator = Validator(api)
        if arguments.command == "check":
            validated, explanation = validator.check(source, arguments.wait_seconds, arguments.quality_run)
            output("validated", str(validated).lower())
            print(explanation)
        else:
            # Checkout is clean in the receipt job; never upload stale local data.
            require(not Path(RECEIPT).exists(), "receipt file already exists")
            receipt = validator.record(source)
            Path(RECEIPT).write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n", encoding="utf-8")
            output("recorded", "true")
            print(f"Recorded original full validation for tree {receipt['source_tree']}")
    except Exception as error:
        # API failures, permissions, expired artifacts, malformed data, and unknown
        # provenance must cost work, never authorize skipped work.
        output("validated" if arguments.command == "check" else "recorded", "false")
        print(f"Cannot prove validation: {error}; full validation remains required for reuse")
    return 0


if __name__ == "__main__":
    # GitHub captures stdout through a pipe; show progress before a long join ends.
    sys.stdout.reconfigure(line_buffering=True)
    raise SystemExit(main())
