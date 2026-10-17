# Incremental release-build checkpoints

`bundle_fork.yml` enables `CARGO_INCREMENTAL=1` for Windows, macOS, and Linux desktop and remote-server builds, including the Linux server packaged for WSL. The bundle commands override it explicitly so a dependency-cache action cannot silently disable it. Release/test profiles, ThinLTO, codegen units, platform separation, validation gates, and publication requirements are unchanged.

## Why retained artifacts replace the build cache pool

The previous full-workspace caches were too large to retain together in the standard 10 GiB Actions cache pool. A production Windows archive exceeded 10 GiB, and a Linux archive of roughly 9.5 GiB disappeared from the cache inventory shortly after being saved. Both builds had reported cache misses. Merely enabling incremental compilation did not give them reusable prior state.

Complete build states now use short-lived Actions artifacts in this **public** repository, rather than enlarging the cache quota. GitHub's [Actions billing documentation](https://docs.github.com/en/billing/managing-billing-for-your-products/managing-billing-for-github-actions/about-billing-for-github-actions) describes free standard-runner Actions usage for public repositories. This does not promise that a provider's pricing or policies will never change.

Checkpoint upload and restoration require explicitly public repository visibility. Private, internal, or missing visibility disables them; no payment method, increased cache capacity, larger runner, or paid service is configured. Existing installer artifacts and the comparison workflow are not changed by this policy.

## Checkpoint lifecycle

1. `rust-build-cache` identifies the actual compiler, runner OS/architecture/image, relevant compiler environment, build recipe, and platform/desktop-or-server/profile namespace. Its immutable key also records the checked-out SHA, run ID, and attempt.
2. `ci-build-checkpoint.py find` searches a bounded recent artifact history for that compatibility prefix. Only artifacts associated with an owned, default-branch `bundle_fork.yml` dispatch are eligible. PR, fork, foreign-workflow, expired, and incompatible artifacts are not restored.
3. GitHub's maintained `actions/download-artifact` downloads the selected artifact. The helper verifies its checksum and manifest identity, then safely stages its archive before replacing explicit generated cache roots. File and directory timestamps are preserved. Workspace source, Cargo credential/configuration files, and top-level release packages are not checkpoint roots.
4. Compiler state includes target `build`, `.fingerprint`, `deps`, and `incremental` directories, compiler information, and Cargo registry/git dependencies. Safe internal links remain portable; links that escape the declared cache roots are rejected.
5. On a checkpoint miss, the old dependency cache may still be restored. Its `Swatinem/rust-cache` save hook is disabled because that cleanup deletes incremental state. Incremental mode is restored afterward and overridden again on the actual bundle command.
6. After a successful build, `save-rust-build-cache` packs the state once and uploads it with `actions/upload-artifact`, seven-day retention, and no redundant ZIP compression. Checkpoint names start with `rust-build-state-`, not the `praxis-` installer namespaces, so release publication never includes them.
7. Large temporary archives are removed after upload/restore. Checkpoint errors are visible warnings and fall back to compilation; they never authorize skipping validation or publishing a failed build.

A checkpoint can be useful even if another platform in the same bundle later fails. It is compiler input, **not validation evidence**. Only the independently verified original quality receipt authorizes validation reuse.

A compiler, target/profile, build recipe, lockfile/manifest, or runner-image change starts a distinct compatibility prefix. Rust and Cargo still decide which restored results are reusable. Expired artifacts or lookup/network failures result in a cold build. The first build with this new checkpoint format must seed it; it cannot manufacture a hit from an evicted older cache.

The quality workflow remains independently validated. Removing redundant main/bundle validation is handled by `praxis-validation.py`, not by treating cached compilation state as proof that tests passed.

## Regression coverage and measurements

- `script/test-ci-build-checkpoint.py` tests owned-main selection, pagination bounds, private-repository guards, complete state and timestamp round trips, checksum/identity mismatches, unsafe paths and links, source/credential protection, and missing compiler state.
- `script/test-bundle-split-modes.py` checks platform/profile isolation, every real bundle's effective incremental setting, restore-only legacy fallback, checkpoint save wiring, and unchanged release/publication gates.
- `incremental_ci.yml` uses **separate seed and restore jobs** on Linux, Windows, and macOS. It builds the real Architect library with the release profile, saves through the production action, downloads on a fresh runner, verifies incremental-file hashes, rebuilds, and reports elapsed time plus fresh/rebuilt Cargo units.
- The on/off compiler comparison remains separate. Neither that small-library comparison nor the checkpoint smoke test is a full-application speed benchmark.

Use a later production build's matched checkpoint identity, archive size, transfer/packing time, bundle duration, and linking time to measure real savings. Large transfers and linking remain costs; no particular full-app speedup is promised without those measurements. Python, Rust, formatting, and platform validation for these changes run in GitHub Actions, not on the user's laptop.
