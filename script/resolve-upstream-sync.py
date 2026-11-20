#!/usr/bin/env python3
"""Deterministically resolves mechanical upstream Zed merge conflicts for Praxis.

Rules are strictly conservative and never guess on code logic:
1. Upstream-only non-product paths (`legal/`, `script/terms/`, `script/snap-*`,
   `crates/zed/resources/snap/`, `tooling/xtask/src/tasks/workflows/`, and
   disabled upstream `.github/workflows/*.yml` outside Praxis's active list)
   take `--theirs`.
2. Modify/delete conflicts where Praxis intentionally deleted the file (stage 2
   absent, stages 1 and 3 present, and no sibling `.rs` file declares `mod <stem>;`)
   keep the deletion (`git rm -f`).
3. `crates/zed/Cargo.toml` conflict hunks that differ solely on `version = "..."`
   preserve Praxis's version (`ours`).
4. `README.md` top review-banner conflicts preserve Praxis's required two-line
   `[!IMPORTANT]` header above upstream's `README.md`.
5. Pure Rust `use ...;` import-only `diff3` conflict hunks compute the exact
   3-way set merge `(ours | theirs) - ((base - ours) | (base - theirs))`,
   leaving any hunk with attributes, comments, `pub use`, or non-import lines
   untouched.
6. `Cargo.lock`: once all `Cargo.toml` conflicts are resolved, takes `--theirs`,
   restores `zed`'s version from `crates/zed/Cargo.toml`, and (when `--with-cargo`
   is set) runs `cargo metadata --format-version 1` to reconcile workspace deps.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import shutil
import subprocess
import sys

PRAXIS_WORKFLOWS = frozenset({
    "architect_quality.yml",
    "bundle_fork.yml",
    "bundle_after_merge.yml",
    "sync_upstream.yml",
    "recover_windows_symbols.yml",
    "android_remote.yml",
    "remote_relay_probe.yml",
    "incremental_ci.yml",
    "warm_ci_caches.yml",
    "macos_path_checks.yml",
})

README_BANNER = (
    "> [!IMPORTANT]\n"
    "> Remove this line to confirm you've reviewed this PR before submitting.\n"
)

DIFF3_RE = re.compile(
    r"^<<<<<<< [^\n]*\n"
    r"(?P<ours>.*?)"
    r"^\|\|\|\|\|\|\| [^\n]*\n"
    r"(?P<base>.*?)"
    r"^=======\n"
    r"(?P<theirs>.*?)"
    r"^>>>>>>> [^\n]*\n",
    re.DOTALL | re.MULTILINE,
)

TWOWAY_RE = re.compile(
    r"^<<<<<<< [^\n]*\n"
    r"(?P<ours>.*?)"
    r"^=======\n"
    r"(?P<theirs>.*?)"
    r"^>>>>>>> [^\n]*\n",
    re.DOTALL | re.MULTILINE,
)

VERSION_LINE_RE = re.compile(r'^\s*version\s*=\s*"([^"]+)"\s*$')
SIMPLE_USE_STMT_RE = re.compile(
    r"^use\s+(?P<path>[A-Za-z0-9_:]+(?:::\{[A-Za-z0-9_,\s:as]+\}|(?:\s+as\s+[A-Za-z0-9_]+))?)\s*;$",
    re.DOTALL,
)


def is_upstream_only_path(path: str) -> bool:
    normalized = path.replace("\\", "/")
    if normalized.startswith((
        "legal/",
        "script/terms/",
        "crates/zed/resources/snap/",
        "tooling/xtask/src/tasks/workflows/",
    )):
        return True
    if normalized in ("script/generate-terms-rtf", "script/snap-build", "script/snap-try"):
        return True
    if normalized.startswith(".github/workflows/"):
        name = normalized.rsplit("/", 1)[-1]
        return name not in PRAXIS_WORKFLOWS
    return False


def is_intentionally_deleted_in_praxis(
    path: str,
    stages: set[int],
    repo_root: Path,
) -> bool:
    if stages != {1, 3}:
        return False
    normalized = path.replace("\\", "/")
    if not normalized.endswith(".rs"):
        return True
    target = repo_root / normalized
    stem = target.stem
    mod_pattern = re.compile(rf"\bmod\s+{re.escape(stem)}\s*;")
    search_dirs = [target.parent]
    if target.parent.parent != target.parent:
        search_dirs.append(target.parent.parent)
    for directory in search_dirs:
        if not directory.is_dir():
            continue
        for candidate in directory.glob("*.rs"):
            if candidate == target:
                continue
            try:
                content = candidate.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            if mod_pattern.search(content):
                return False
    return True


def resolve_zed_cargo_toml_text(text: str) -> tuple[str, bool]:
    changed = False

    def replace_hunk(match: re.Match[str]) -> str:
        nonlocal changed
        ours = match.group("ours").strip("\r\n")
        theirs = match.group("theirs").strip("\r\n")
        if VERSION_LINE_RE.match(ours) and VERSION_LINE_RE.match(theirs):
            changed = True
            return ours + "\n"
        return match.group(0)

    resolved = DIFF3_RE.sub(replace_hunk, text)
    resolved = TWOWAY_RE.sub(replace_hunk, resolved)
    still_conflicted = "<<<<<<< " in resolved
    return resolved, changed and not still_conflicted


def resolve_readme_banner(ours_text: str, theirs_text: str) -> str | None:
    if not ours_text.startswith(README_BANNER):
        return None
    stripped_theirs = theirs_text
    if stripped_theirs.startswith(README_BANNER):
        return stripped_theirs
    return README_BANNER + "\n" + stripped_theirs.lstrip("\n")


def parse_rust_use_block(block: str) -> list[str] | None:
    if not block.strip():
        return []
    for raw_line in block.splitlines():
        stripped = raw_line.strip()
        if stripped.startswith(("//", "/*", "#", "pub ")):
            return None
    normalized = " ".join(block.split())
    if not normalized.endswith(";"):
        return None
    parts = [stmt.strip() + ";" for stmt in normalized.split(";") if stmt.strip()]
    imports: list[str] = []
    for stmt in parts:
        match = SIMPLE_USE_STMT_RE.match(stmt)
        if not match:
            return None
        body = match.group("path").strip()
        if "::{" in body:
            if body.count("{") != 1 or body.count("}") != 1 or not body.endswith("}"):
                return None
            prefix, inner = body[:-1].split("::{", 1)
            for item in inner.split(","):
                cleaned = " ".join(item.strip().split())
                if not cleaned:
                    continue
                if "{" in cleaned or "}" in cleaned or "*" in cleaned:
                    return None
                if cleaned == "self":
                    imports.append(prefix)
                else:
                    imports.append(f"{prefix}::{cleaned}")
        else:
            if "*" in body:
                return None
            imports.append(" ".join(body.split()))
    return imports


def format_rust_imports(imports: list[str], indent: str) -> str:
    if not imports:
        return ""
    grouped: dict[str, list[str]] = {}
    standalone: list[str] = []
    for item in sorted(set(imports)):
        if "::" in item:
            prefix, leaf = item.rsplit("::", 1)
            grouped.setdefault(prefix, []).append(leaf)
        else:
            standalone.append(item)

    lines: list[str] = []
    for item in sorted(standalone):
        lines.append(f"{indent}use {item};")
    for prefix in sorted(grouped):
        leaves = sorted(set(grouped[prefix]))
        if len(leaves) == 1 and leaves[0] != "self":
            lines.append(f"{indent}use {prefix}::{leaves[0]};")
        else:
            joined = ", ".join(leaves)
            lines.append(f"{indent}use {prefix}::{{{joined}}};")
    return "\n".join(lines) + "\n"


def resolve_rust_use_hunks(text: str) -> tuple[str, bool]:
    changed = False

    def replace_diff3(match: re.Match[str]) -> str:
        nonlocal changed
        ours_raw = match.group("ours")
        base_raw = match.group("base")
        theirs_raw = match.group("theirs")
        ours = parse_rust_use_block(ours_raw)
        base = parse_rust_use_block(base_raw)
        theirs = parse_rust_use_block(theirs_raw)
        if ours is None or base is None or theirs is None:
            return match.group(0)

        ours_set = set(ours)
        base_set = set(base)
        theirs_set = set(theirs)
        removed = (base_set - ours_set) | (base_set - theirs_set)
        merged = sorted((ours_set | theirs_set) - removed)

        indent = ""
        for sample in (ours_raw, theirs_raw, base_raw):
            for line in sample.splitlines():
                if line.strip():
                    indent = line[: len(line) - len(line.lstrip())]
                    break
            if indent:
                break

        changed = True
        return format_rust_imports(merged, indent)

    resolved = DIFF3_RE.sub(replace_diff3, text)
    still_conflicted = "<<<<<<< " in resolved
    return resolved, changed and not still_conflicted


def patch_zed_lock_version(lock_text: str, zed_version: str) -> str:
    return re.sub(
        r'(\[\[package\]\]\nname = "zed"\nversion = ")[^"]+(")',
        rf"\g<1>{zed_version}\2",
        lock_text,
        count=1,
    )


def merge_cargo_lock_packages(ours_lock: str, theirs_lock: str, zed_version: str | None) -> str:
    """Preserves Praxis-only [[package]] blocks from `ours_lock` inside `theirs_lock`."""
    header, sep, theirs_body = theirs_lock.partition("[[package]]\n")
    _, ours_sep, ours_body = ours_lock.partition("[[package]]\n")
    if not sep or not ours_sep:
        return patch_zed_lock_version(theirs_lock, zed_version) if zed_version else theirs_lock

    def split_blocks(body: str) -> list[tuple[str, str, str]]:
        raw_blocks = body.split("\n[[package]]\n")
        parsed: list[tuple[str, str, str]] = []
        for index, chunk in enumerate(raw_blocks):
            block = "[[package]]\n" + chunk.strip() + "\n"
            name_match = re.search(r'^name = "([^"]+)"$', block, re.MULTILINE)
            ver_match = re.search(r'^version = "([^"]+)"$', block, re.MULTILINE)
            name = name_match.group(1) if name_match else f"~{index}"
            ver = ver_match.group(1) if ver_match else ""
            parsed.append((name, ver, block))
        return parsed

    theirs_blocks = split_blocks(theirs_body)
    theirs_names = {name for name, _, _ in theirs_blocks}
    extra_ours = [item for item in split_blocks(ours_body) if item[0] not in theirs_names]
    combined = sorted(theirs_blocks + extra_ours, key=lambda item: (item[0], item[1]))
    merged = header + "\n".join(block for _, _, block in combined)
    return patch_zed_lock_version(merged, zed_version) if zed_version else merged


def read_zed_crate_version(repo_root: Path) -> str | None:
    cargo_toml = repo_root / "crates" / "zed" / "Cargo.toml"
    if not cargo_toml.is_file():
        return None
    for line in cargo_toml.read_text(encoding="utf-8").splitlines():
        match = VERSION_LINE_RE.match(line)
        if match:
            return match.group(1)
    return None


def git(repo_root: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "--no-pager", *args],
        cwd=repo_root,
        text=True,
        capture_output=True,
        check=check,
    )


def unmerged_stages(repo_root: Path) -> dict[str, set[int]]:
    output = git(repo_root, "ls-files", "-u").stdout
    stages: dict[str, set[int]] = {}
    for line in output.splitlines():
        if not line.strip():
            continue
        meta, path = line.split("\t", 1)
        stage = int(meta.split()[2])
        stages.setdefault(path, set()).add(stage)
    return stages


def resolve_repository(repo_root: Path, with_cargo: bool = False) -> list[str]:
    stages = unmerged_stages(repo_root)
    if not stages:
        return []

    resolved: list[str] = []
    for path, file_stages in sorted(stages.items()):
        if path == "Cargo.lock":
            continue
        if is_upstream_only_path(path):
            if 3 in file_stages:
                git(repo_root, "checkout", "--theirs", "--", path)
                git(repo_root, "add", "--", path)
            else:
                git(repo_root, "rm", "-f", "--", path)
            resolved.append(path)
            continue

        if is_intentionally_deleted_in_praxis(path, file_stages, repo_root):
            git(repo_root, "rm", "-f", "--", path)
            resolved.append(path)
            continue

        full_path = repo_root / path
        if path == "crates/zed/Cargo.toml" and full_path.is_file():
            updated, ok = resolve_zed_cargo_toml_text(
                full_path.read_text(encoding="utf-8")
            )
            if ok:
                full_path.write_text(updated, encoding="utf-8")
                git(repo_root, "add", "--", path)
                resolved.append(path)
                continue

        if path == "README.md" and {2, 3}.issubset(file_stages):
            ours_text = git(repo_root, "show", f":2:{path}").stdout
            theirs_text = git(repo_root, "show", f":3:{path}").stdout
            merged_readme = resolve_readme_banner(ours_text, theirs_text)
            if merged_readme is not None:
                full_path.write_text(merged_readme, encoding="utf-8")
                git(repo_root, "add", "--", path)
                resolved.append(path)
                continue

        if path.endswith(".rs") and full_path.is_file():
            updated, ok = resolve_rust_use_hunks(full_path.read_text(encoding="utf-8"))
            if ok:
                full_path.write_text(updated, encoding="utf-8")
                git(repo_root, "add", "--", path)
                resolved.append(path)
                continue

    remaining = unmerged_stages(repo_root)
    toml_conflicts = [p for p in remaining if p.endswith("Cargo.toml")]
    if "Cargo.lock" in remaining and not toml_conflicts:
        lock_stages = remaining["Cargo.lock"]
        zed_version = read_zed_crate_version(repo_root)
        lock_path = repo_root / "Cargo.lock"
        if {2, 3}.issubset(lock_stages):
            ours_lock = git(repo_root, "show", ":2:Cargo.lock").stdout
            theirs_lock = git(repo_root, "show", ":3:Cargo.lock").stdout
            lock_path.write_text(
                merge_cargo_lock_packages(ours_lock, theirs_lock, zed_version),
                encoding="utf-8",
            )
        else:
            git(repo_root, "checkout", "--theirs", "--", "Cargo.lock")
            if zed_version and lock_path.is_file():
                lock_path.write_text(
                    patch_zed_lock_version(
                        lock_path.read_text(encoding="utf-8"), zed_version
                    ),
                    encoding="utf-8",
                )
        if with_cargo and shutil.which("cargo"):
            subprocess.run(
                ["cargo", "metadata", "--format-version", "1"],
                cwd=repo_root,
                stdout=subprocess.DEVNULL,
                check=True,
            )
        git(repo_root, "add", "--", "Cargo.lock")
        resolved.append("Cargo.lock")

    return resolved


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--with-cargo",
        action="store_true",
        help="Reconcile Cargo.lock via `cargo metadata` when cargo is available.",
    )
    args = parser.parse_args()
    repo_root = Path(__file__).resolve().parent.parent
    resolved = resolve_repository(repo_root, with_cargo=args.with_cargo)
    for path in resolved:
        print(f"auto-resolved: {path}")
    remaining = unmerged_stages(repo_root)
    return 0 if not remaining else 1


if __name__ == "__main__":
    sys.exit(main())
