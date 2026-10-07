#!/usr/bin/env python3
"""Identify compatible CI build caches without caching credentials or release packages."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


COMPILER_ENVIRONMENT = (
    "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion",
    "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET",
    "PRAXIS_CARGO_PROFILE", "MACOSX_DEPLOYMENT_TARGET", "SDKROOT",
)
TARGET_PATHS = (
    "target/**/.fingerprint",
    "target/**/build",
    "target/**/deps",
    "target/**/incremental",
    "target/.rustc_info.json",
)


def cache_prefix(name, compiler, recipe, environment):
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,100}", name):
        raise ValueError("A cache name must identify one platform, architecture, and build profile")
    if not compiler.strip() or not recipe.strip():
        raise ValueError("Compiler and build-recipe identities must be present")
    if not environment.get("RUNNER_OS") or not environment.get("RUNNER_ARCH"):
        raise ValueError("Runner OS and architecture must be present")
    identity = {
        "compiler": compiler.strip(),
        "recipe": recipe,
        "incremental": "1",
        "environment": {key: environment.get(key, "") for key in COMPILER_ENVIRONMENT},
    }
    digest = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    return f"rust-build-state-v2-{name}-{digest}"


def cache_key(prefix, source, run_id, attempt):
    if not re.fullmatch(r"[0-9a-f]{40}", source):
        raise ValueError("The cache source must be the immutable checked-out commit")
    if not re.fullmatch(r"[0-9]+", run_id) or not re.fullmatch(r"[0-9]+", attempt):
        raise ValueError("The cache needs a CI run ID and attempt")
    # GitHub caches are immutable. A new key per build lets prefix restoration
    # advance to the latest workspace artifacts instead of a permanently old hit.
    return f"{prefix}-{source}-{run_id}-{attempt}"


def cache_paths(cargo_home):
    home = Path(cargo_home).as_posix().rstrip("/")
    return (f"{home}/registry", f"{home}/git", *TARGET_PATHS)


def main():
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("This helper is for GitHub Actions, not local builds")
    compiler = subprocess.check_output(["rustc", "-vV"], text=True, timeout=300)
    source = subprocess.check_output(
        ["git", "--no-pager", "rev-parse", "HEAD"], text=True, timeout=30,
    ).strip()
    prefix = cache_prefix(
        os.environ["CACHE_NAME"], compiler, os.environ["CACHE_RECIPE"], os.environ,
    )
    key = cache_key(prefix, source, os.environ["GITHUB_RUN_ID"], os.environ["GITHUB_RUN_ATTEMPT"])
    paths = cache_paths(os.environ.get("CARGO_HOME") or Path.home() / ".cargo")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write(f"prefix={prefix}-\nkey={key}\npaths<<CACHE_PATHS\n")
        output.write("\n".join(paths) + "\nCACHE_PATHS\n")
    print(f"Incremental build cache: {key}")


if __name__ == "__main__":
    main()
