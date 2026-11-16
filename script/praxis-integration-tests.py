#!/usr/bin/env python3
"""Runs the tests listed in praxis-integration-tests.txt with one cargo-nextest call.

A single invocation resolves features once, so shared crates compile once and
each test binary links once, and `--no-fail-fast` reports every failure in one
run. Extra arguments, such as `--no-run`, are passed to cargo-nextest.
"""

import json
import pathlib
import re
import subprocess
import sys

LIST = pathlib.Path(__file__).with_name("praxis-integration-tests.txt")


def parse(lines):
    """Returns the packages, features, integration test targets, filtersets, and entries."""
    packages = []
    features = []
    targets = []
    expressions = []
    entries = []
    for line_number, raw_line in enumerate(lines, start=1):
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) > 2:
            raise ValueError(f"{LIST.name}:{line_number}: expected `<package>[+features] [<test filter>]`")
        if not all(re.fullmatch(r"[=@]?[A-Za-z0-9_:+-]+", part) for part in parts):
            raise ValueError(f"{LIST.name}:{line_number}: unexpected characters in `{line}`")
        package, *package_features = parts[0].split("+")
        entries.append((line_number, package, parts[1] if len(parts) == 2 else ""))
        if package not in packages:
            packages.append(package)
        for feature in package_features:
            qualified = f"{package}/{feature}"
            if qualified not in features:
                features.append(qualified)
        if len(parts) == 1:
            expressions.append(f"package({package}) & kind(lib)")
        elif parts[1].startswith("@"):
            target = parts[1][1:]
            if target not in targets:
                targets.append(target)
            expressions.append(f"package({package}) & kind(test) & binary({target})")
        elif parts[1].startswith("="):
            expressions.append(f"package({package}) & kind(lib) & test(={parts[1][1:]})")
        else:
            expressions.append(f"package({package}) & kind(lib) & test({parts[1]})")
    return packages, features, targets, expressions, entries


def unmatched(entries, listing):
    """Returns the entries that select no test in a `cargo nextest list` JSON listing."""
    suites = listing["rust-suites"].values()

    def matches(package, test_filter):
        for suite in suites:
            if suite["package-name"] != package:
                continue
            if test_filter.startswith("@"):
                if suite["kind"] == "test" and suite["binary-name"] == test_filter[1:] and suite["testcases"]:
                    return True
                continue
            if suite["kind"] != "lib":
                continue
            for name in suite["testcases"]:
                if test_filter.startswith("=") and name == test_filter[1:]:
                    return True
                if not test_filter.startswith("=") and test_filter in name:
                    return True
        return False

    return [entry for entry in entries if not matches(entry[1], entry[2])]


def command(packages, features, targets, expressions, extra, subcommand="run"):
    arguments = ["cargo", "nextest", subcommand, "--locked", "--lib"]
    for target in targets:
        arguments += ["--test", target]
    for package in packages:
        arguments += ["-p", package]
    if features:
        arguments += ["--features", ",".join(features)]
    if subcommand == "list":
        return arguments + ["--message-format", "json"]
    if "--no-run" not in extra:
        arguments.append("--no-fail-fast")
        for expression in expressions:
            arguments += ["-E", expression]
    return arguments + list(extra)


def main():
    packages, features, targets, expressions, entries = parse(LIST.read_text(encoding="utf-8").splitlines())
    print(f"{len(expressions)} filters across {len(packages)} packages", flush=True)
    extra = sys.argv[1:]
    if "--no-run" not in extra:
        # nextest runs nothing for a filter that matches nothing, so a typo in
        # the list would silently drop tests.
        listing = subprocess.run(
            command(packages, features, targets, expressions, [], "list"),
            check=True, capture_output=True, text=True,
        )
        missing = unmatched(entries, json.loads(listing.stdout))
    else:
        missing = []
    status = subprocess.call(command(packages, features, targets, expressions, extra))
    for line_number, package, test_filter in missing:
        print(f"::error::{LIST.name}:{line_number}: `{package} {test_filter}` selects no test")
    return status or (1 if missing else 0)


if __name__ == "__main__":
    sys.exit(main())
