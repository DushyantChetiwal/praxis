#!/usr/bin/env python3
"""Retain public-repository Rust build state without using the Actions cache pool."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import shutil
import subprocess
import tarfile
import tempfile
import time


BUNDLE_WORKFLOW = ".github/workflows/bundle_fork.yml"
ROOT_NAMES = {".fingerprint", "build", "deps", "incremental"}
ARCHIVE = "checkpoint.tar.gz"
MANIFEST = "checkpoint.json"
CHECKSUM = "checkpoint.sha256"
SCHEMA = 1


def output(name, value):
    if "\n" in str(value) or "\r" in str(value):
        raise ValueError("Checkpoint outputs must be single-line values")
    destination = os.environ.get("GITHUB_OUTPUT")
    if destination:
        with open(destination, "a", encoding="utf-8") as stream:
            stream.write(f"{name}={value}\n")


def report(message):
    print(message)
    destination = os.environ.get("GITHUB_STEP_SUMMARY")
    if destination:
        with open(destination, "a", encoding="utf-8") as stream:
            stream.write(message + "\n\n")


def validate_key(key):
    if not re.fullmatch(r"rust-build-state-v2-[A-Za-z0-9_-]+-[0-9a-f]{64}-[0-9a-f]{40}-[0-9]+-[0-9]+", key):
        raise ValueError("Invalid immutable build-checkpoint identity")
    return key


def github(endpoint):
    response = subprocess.run(
        ["gh", "api", "--method", "GET", endpoint],
        check=True, capture_output=True, text=True, timeout=30,
    )
    return json.loads(response.stdout)


def find_checkpoint(repository, prefix, request=github):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("Expected OWNER/REPO")
    if not re.fullmatch(r"rust-build-state-v2-[A-Za-z0-9_-]+-[0-9a-f]{64}-", prefix):
        raise ValueError("Invalid checkpoint compatibility prefix")
    root = f"repos/{repository}"
    metadata = request(root)
    if metadata.get("private") is not False or metadata.get("visibility") != "public":
        report("Build checkpoints are disabled for private repositories; no paid storage is enabled.")
        return None
    deadline = time.monotonic() + 60
    runs = {}
    for page in range(1, 4):
        artifacts = request(f"{root}/actions/artifacts?per_page=100&page={page}")["artifacts"]
        for artifact in artifacts:
            if time.monotonic() >= deadline:
                report("Checkpoint lookup reached its deadline; continuing with a cold build.")
                return None
            if artifact.get("expired") or not artifact["name"].startswith(prefix):
                continue
            validate_key(artifact["name"])
            association = artifact.get("workflow_run", {})
            run_id = association.get("id")
            if not isinstance(run_id, int) or run_id <= 0:
                continue
            if run_id not in runs:
                runs[run_id] = request(f"{root}/actions/runs/{run_id}")
            run = runs[run_id]
            if not (
                run.get("path") == BUNDLE_WORKFLOW
                and run.get("event") == "workflow_dispatch"
                and run.get("head_branch") == metadata["default_branch"]
                and run.get("repository", {}).get("id") == metadata["id"]
                and run.get("head_repository", {}).get("id") == metadata["id"]
                and association.get("head_sha") == run.get("head_sha")
                and association.get("repository_id") == metadata["id"]
                and association.get("head_repository_id") == metadata["id"]
            ):
                continue
            _, named_run, _ = artifact["name"].rsplit("-", 2)
            if named_run != str(run_id):
                continue
            return artifact
        if len(artifacts) < 100:
            break
    return None


def state_paths(workspace, cargo_home):
    paths = []
    for name in ("registry", "git"):
        path = cargo_home / name
        if path.is_dir() and not path.is_symlink():
            paths.append((path, f"cargo/{name}"))
    target = workspace / "target"
    if target.is_symlink():
        raise ValueError("The target directory must not be a symlink")
    for name in sorted(ROOT_NAMES):
        for path in sorted(target.glob(f"**/{name}")):
            if path.is_dir() and not path.is_symlink():
                paths.append((path, path.relative_to(workspace).as_posix()))
    information = target / ".rustc_info.json"
    if information.is_file() and not information.is_symlink():
        paths.append((information, "target/.rustc_info.json"))
    selected = []
    for path, name in sorted(paths, key=lambda item: (len(PurePosixPath(item[1]).parts), item[1])):
        if not any(name.startswith(parent + "/") for _, parent in selected):
            selected.append((path, name))
    return selected


def safe_root(name):
    path = PurePosixPath(name)
    if path.is_absolute() or path.as_posix() != name or any(part in {"", ".", ".."} for part in path.parts) or "\\" in name:
        return False
    if name in {"cargo/registry", "cargo/git", "target/.rustc_info.json"}:
        return True
    return len(path.parts) >= 3 and path.parts[0] == "target" and path.parts[-1] in ROOT_NAMES


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def pack(workspace, cargo_home, destination, key):
    validate_key(key)
    paths = state_paths(workspace, cargo_home)
    if not any(name.startswith("target/") for _, name in paths):
        raise ValueError("No compiler state was produced; no checkpoint will be uploaded")
    destination.mkdir(parents=True, exist_ok=True)
    archive_path = destination / ARCHIVE
    manifest = {"schema": SCHEMA, "key": key, "paths": [name for _, name in paths]}
    metadata = json.dumps(manifest, sort_keys=True).encode("utf-8")
    counts = {"files": 0, "bytes": 0, "incremental_files": 0}

    def count(member):
        if member.issym():
            source_root, archive_root = next(
                (path, name) for path, name in paths
                if member.name == name or member.name.startswith(name + "/")
            )
            source = source_root / PurePosixPath(member.name).relative_to(archive_root)
            target = source.resolve()
            for candidate, target_root in paths:
                if target.is_relative_to(candidate.resolve()) and target_root.split("/")[0] == member.name.split("/")[0]:
                    target_name = PurePosixPath(target_root) / target.relative_to(candidate.resolve()).as_posix()
                    member.linkname = posixpath.relpath(target_name.as_posix(), PurePosixPath(member.name).parent.as_posix())
                    break
            else:
                raise ValueError(f"Checkpoint symlink leaves compiler-state roots: {member.name}")
        if member.isfile():
            counts["files"] += 1
            counts["bytes"] += member.size
            if "incremental" in PurePosixPath(member.name).parts:
                counts["incremental_files"] += 1
        return member

    started = time.monotonic()
    with tarfile.open(archive_path, "w:gz", compresslevel=1) as archive:
        entry = tarfile.TarInfo(MANIFEST)
        entry.size = len(metadata)
        archive.addfile(entry, io.BytesIO(metadata))
        for path, name in paths:
            archive.add(path, arcname=name, filter=count)
    (destination / CHECKSUM).write_text(digest(archive_path) + "\n", encoding="ascii")
    report(
        f"Build checkpoint saved locally: {key}\n\n"
        f"Files: {counts['files']}; incremental files: {counts['incremental_files']}; "
        f"unpacked bytes: {counts['bytes']}; archive bytes: {archive_path.stat().st_size}; "
        f"packing seconds: {time.monotonic() - started:.1f}."
    )
    output("archive-path", archive_path.as_posix())
    output("checksum-path", (destination / CHECKSUM).as_posix())
    return manifest


def restore(workspace, cargo_home, directory, key):
    validate_key(key)
    archive_path = directory / ARCHIVE
    checksum_path = directory / CHECKSUM
    if checksum_path.stat().st_size > 128:
        raise ValueError("Oversized build-checkpoint checksum")
    expected = checksum_path.read_text(encoding="ascii").strip()
    if not re.fullmatch(r"[0-9a-f]{64}", expected) or digest(archive_path) != expected:
        raise ValueError("Build checkpoint checksum does not match")
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="expanded-", dir=directory) as temporary:
        staging = Path(temporary)
        with tarfile.open(archive_path, "r:gz") as archive:
            first = archive.next()
            if first is None or first.name != MANIFEST or not first.isfile() or not 0 < first.size <= 65536:
                raise ValueError("Missing or oversized build-checkpoint manifest")
            stream = archive.extractfile(first)
            if stream is None:
                raise ValueError("Missing build-checkpoint manifest contents")
            with stream:
                manifest = json.load(stream)
            if manifest.get("schema") != SCHEMA or manifest.get("key") != key:
                raise ValueError("Build-checkpoint identity mismatch")
            paths = manifest.get("paths")
            if not isinstance(paths, list) or not paths or not all(isinstance(name, str) and safe_root(name) for name in paths):
                raise ValueError("Unexpected build-checkpoint paths")
            if len(paths) != len(set(paths)) or any(name.startswith(other + "/") for name in paths for other in paths if name != other):
                raise ValueError("Overlapping build-checkpoint paths")
            def filter_member(member, destination):
                if member.name == MANIFEST:
                    if member is not first:
                        raise ValueError("Duplicate build-checkpoint manifest")
                    return None
                if (
                    PurePosixPath(member.name).is_absolute()
                    or Path(member.name).is_absolute()
                    or ".." in PurePosixPath(member.name).parts
                    or ".." in Path(member.name).parts
                    or not any(member.name == name or member.name.startswith(name + "/") for name in paths)
                ):
                    raise ValueError("Archive member is outside compiler-state roots")
                if member.issym() or member.islnk():
                    if PurePosixPath(member.linkname).is_absolute() or Path(member.linkname).is_absolute() or "\\" in member.linkname:
                        raise ValueError("Unexpected checkpoint link target")
                    target = posixpath.normpath(
                        posixpath.join(posixpath.dirname(member.name), member.linkname)
                        if member.issym() else member.linkname
                    )
                    if target.split("/")[0] != member.name.split("/")[0] or not any(
                        target == name or target.startswith(name + "/") for name in paths
                    ):
                        raise ValueError("Checkpoint link leaves compiler-state roots")
                return tarfile.data_filter(member, destination)

            archive.extractall(staging, members=archive, filter=filter_member)
        # Validate the complete archive before replacing any generated cache root.
        # Never extract directly over the checked-out source or Cargo credentials.
        for name in paths:
            source = staging / name
            if not source.exists() or source.is_symlink():
                raise ValueError("Missing or linked build-checkpoint root")
            base = cargo_home if name.startswith("cargo/") else workspace
            relative = name.removeprefix("cargo/") if name.startswith("cargo/") else name
            destination = base / relative
            if not destination.resolve().is_relative_to(base.resolve()):
                raise ValueError("Build-checkpoint destination escapes its root")
            ancestor = base
            for part in Path(relative).parts:
                ancestor = ancestor / part
                if ancestor.is_symlink():
                    raise ValueError("Build-checkpoint destination contains a symlink")
        for name in paths:
            source = staging / name
            destination = cargo_home / name.removeprefix("cargo/") if name.startswith("cargo/") else workspace / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            if destination.is_dir():
                shutil.rmtree(destination)
            elif destination.exists():
                destination.unlink()
            shutil.move(str(source), str(destination))
    report(f"Restored complete build checkpoint: {key}; restore seconds: {time.monotonic() - started:.1f}.")
    output("restored", "true")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("find", "pack", "restore"))
    parser.add_argument("--directory", type=Path)
    arguments = parser.parse_args(argv)
    try:
        if os.environ.get("GITHUB_ACTIONS") != "true":
            raise ValueError("Checkpoint commands run only in GitHub Actions")
        if os.environ.get("REPOSITORY_VISIBILITY") != "public":
            report("Build checkpoints disabled: repository is not explicitly public.")
            return 0
        if arguments.command == "find":
            artifact = find_checkpoint(os.environ["GITHUB_REPOSITORY"], os.environ["CHECKPOINT_PREFIX"])
            if artifact:
                output("artifact-id", artifact["id"])
                output("run-id", artifact["workflow_run"]["id"])
                output("matched-key", artifact["name"])
                report(f"Compatible build checkpoint found: {artifact['name']} (artifact {artifact['id']}).")
            else:
                report("No compatible retained build checkpoint; this build will seed one.")
        else:
            if arguments.directory is None:
                raise ValueError("Checkpoint directory is required")
            workspace = Path.cwd()
            cargo_home = Path(os.environ.get("CARGO_HOME") or Path.home() / ".cargo")
            operation = pack if arguments.command == "pack" else restore
            operation(workspace, cargo_home, arguments.directory, os.environ["CHECKPOINT_KEY"])
    except Exception as error:
        print(f"::warning::Build checkpoint {arguments.command} unavailable: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
