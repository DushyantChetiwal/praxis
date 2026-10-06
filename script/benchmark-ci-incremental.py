"""Compare warm test/Clippy rebuilds in Actions without changing release policy."""

import json
import os
import re
from pathlib import Path
import statistics
import subprocess
import time

if os.environ.get("GITHUB_ACTIONS") != "true":
    raise SystemExit("Run this measurement in GitHub Actions, not on the desktop.")

mode = os.environ["CARGO_INCREMENTAL"]
if mode not in {"0", "1"}:
    raise SystemExit("CARGO_INCREMENTAL must be 0 or 1")
if mode != os.environ.get("EXPECTED_INCREMENTAL"):
    raise SystemExit("The measured incremental mode differs from the matrix setting")

root = Path(__file__).resolve().parent.parent
source = root / "crates/architect/src/architect.rs"
original = source.read_bytes()
measurements = []


def measure(label, command):
    started = time.perf_counter()
    subprocess.run(command, cwd=root, check=True)
    elapsed = time.perf_counter() - started
    measurements.append({"label": label, "seconds": round(elapsed, 3)})
    print(f"MEASUREMENT {label}: {elapsed:.3f}s", flush=True)


def build_pair(label):
    measure(label + "_test", ["cargo", "test", "--locked", "-p", "architect", "--lib"])
    measure(label + "_clippy", ["bash", "./script/clippy", "--locked", "-p", "architect"])


try:
    build_pair("initial")
    build_pair("unchanged")
    marker = re.search(rb"(?m)^#\[cfg\(test\)\]\r?$", original)
    if marker is None:
        raise SystemExit("The Architect test insertion point changed; update the probe.")
    insertion = marker.start()
    for sample in range(1, 4):
        probe = (
            "#[cfg(test)]\n#[test]\n"
            "fn praxis_incremental_ci_probe() {\n"
            f"    let value = std::hint::black_box({sample}usize);\n"
            f"    assert_eq!(value, {sample}usize);\n"
            "}\n\n"
        ).encode()
        source.write_bytes(original[:insertion] + probe + original[insertion:])
        build_pair(f"edited_{sample}")
finally:
    source.write_bytes(original)
    target = Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
    if not target.is_absolute():
        target = root / target
    artifact_bytes = sum(path.stat().st_size for path in target.rglob("*") if path.is_file()) if target.exists() else 0
    result = {
        "source_sha": subprocess.check_output(["git", "--no-pager", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "pr_head_sha": os.environ.get("PR_HEAD_SHA"),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "incremental": mode == "1",
        "scope": "Architect crate; same-runner warm edits; not a cross-run cache-hit benchmark",
        "measurements": measurements,
        "target_artifact_bytes": artifact_bytes,
        "incremental_artifact_bytes": sum(path.stat().st_size for path in target.rglob("*") if path.is_file() and "incremental" in path.parts),
    }
    for kind in ("test", "clippy"):
        values = [item["seconds"] for item in measurements if item["label"].startswith("edited_") and item["label"].endswith("_" + kind)]
        if values:
            result[f"median_edited_{kind}_seconds"] = statistics.median(values)
    output = root / f"ci-incremental-{mode}.json"
    output.write_text(json.dumps(result, indent=2) + "\n")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as stream:
            stream.write(f"## Incremental compilation {'on' if mode == '1' else 'off'}\n\n")
            stream.write("| Measurement | Seconds |\n| --- | ---: |\n")
            for item in measurements:
                stream.write(f"| {item['label']} | {item['seconds']:.3f} |\n")
            stream.write(f"\nTarget artifacts: {artifact_bytes / 1048576:.1f} MiB.\n\n")
            stream.write("Hosted runner hardware varies. Compare warm-edit medians, not just initial builds. This does not prove incremental directories survive our normal cache cleanup or predict agent_ui/release build gains. Release workflows are unchanged.\n")
