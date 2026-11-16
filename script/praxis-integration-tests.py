#!/usr/bin/env python3
"""Runs the tests listed in praxis-integration-tests.txt with one cargo-nextest call.

A single invocation resolves features once, so shared crates compile once and
each test binary links once, and `--no-fail-fast` reports every failure in one
run. Extra arguments, such as `--no-run`, are passed to cargo-nextest.
"""

import pathlib
import subprocess
import sys

LIST = pathlib.Path(__file__).with_name("praxis-integration-tests.txt")


def filtersets(lines):
    packages = []
    expressions = []
    for line_number, raw_line in enumerate(lines, start=1):
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) > 2:
            raise ValueError(f"{LIST.name}:{line_number}: expected `<package> [<test filter>]`")
        package = parts[0]
        if package not in packages:
            packages.append(package)
        if len(parts) == 1:
            expressions.append(f"package({package})")
        elif parts[1].startswith("="):
            expressions.append(f"package({package}) & test(={parts[1][1:]})")
        else:
            expressions.append(f"package({package}) & test({parts[1]})")
    return packages, expressions


def main():
    packages, expressions = filtersets(LIST.read_text(encoding="utf-8").splitlines())
    build_only = "--no-run" in sys.argv[1:]
    command = ["cargo", "nextest", "run", "--locked", "--lib"]
    for package in packages:
        command += ["-p", package]
    if not build_only:
        command.append("--no-fail-fast")
        for expression in expressions:
            command += ["-E", expression]
    command += sys.argv[1:]
    print(f"{len(expressions)} filters across {len(packages)} packages", flush=True)
    return subprocess.call(command)


if __name__ == "__main__":
    sys.exit(main())
