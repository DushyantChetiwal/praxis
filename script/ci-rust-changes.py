#!/usr/bin/env python3
"""Decide whether a change needs the Rust build and test jobs.

Writes `rust=true` or `rust=false` to GITHUB_OUTPUT. Anything uncertain, such
as an unlisted diff or an unknown base, counts as a Rust change.
"""

import json
import os
import re
import subprocess

# Paths that never reach a Rust build. Everything else needs the full suite,
# including READMEs inside crates: some are compiled in with include_str!.
NON_RUST_PREFIXES = ("remote-android/", "docs/", "site/")
NON_RUST_FILES = frozenset({
    ".github/workflows/android_remote.yml",
    ".github/workflows/deploy_pages.yml",
})
# The compare API lists at most this many files, so a longer diff may hide one.
COMPARE_FILE_LIMIT = 300


def is_non_rust(path):
    if path in NON_RUST_FILES or path.startswith(NON_RUST_PREFIXES):
        return True
    return "/" not in path and path.endswith(".md")


def needs_rust(paths):
    return not paths or not all(is_non_rust(path) for path in paths)


def changed_paths(repository, base, head, run=subprocess.run):
    result = run(
        ["gh", "api", f"repos/{repository}/compare/{base}...{head}", "--jq", "[.files[]?.filename]"],
        capture_output=True, text=True, timeout=60,
    )
    if result.returncode != 0:
        print(f"::warning::Could not list the changed files: {result.stderr.strip()}")
        return None
    paths = json.loads(result.stdout or "[]")
    if len(paths) >= COMPARE_FILE_LIMIT:
        print(f"{len(paths)} files changed, at the compare limit; running the full suite")
        return None
    return paths


def decide(repository, base, head, run=subprocess.run):
    if not re.fullmatch(r"[0-9a-f]{40}", base or "") or set(base) == {"0"}:
        print("No base commit to compare against; running the full suite")
        return True
    paths = changed_paths(repository, base, head, run)
    if paths is None:
        return True
    rust = needs_rust(paths)
    kept = ", ".join(path for path in paths if not is_non_rust(path)) or "none"
    print(f"{len(paths)} files changed; files that need the Rust suite: {kept}")
    return rust


def main():
    rust = decide(os.environ["GITHUB_REPOSITORY"], os.environ.get("BASE_SHA", ""), os.environ["SOURCE_SHA"])
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write(f"rust={'true' if rust else 'false'}\n")
    if not rust:
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
            summary.write("No Rust-affecting files changed; the Rust build and test jobs are skipped.\n")


if __name__ == "__main__":
    main()
