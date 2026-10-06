# Incremental release-build caching

`bundle_fork.yml` enables `CARGO_INCREMENTAL=1` for the real Windows, macOS, and Linux desktop and remote-server builds, including the Linux server packaged for WSL. The actual bundle steps set it explicitly so a cache action cannot silently turn it off. The existing release/test profiles, ThinLTO, codegen-unit settings, platform separation, source-validation gate, and publication requirements are unchanged.

## Cache lifecycle

1. The local `rust-build-cache` action identifies the actual compiler, runner OS/architecture/image, build recipe, relevant compiler environment, and platform/desktop-or-server/profile namespace.
2. GitHub's maintained `actions/cache/restore` restores the newest compatible cache. Its paths include Cargo registry/git data and the target `build`, `.fingerprint`, `deps`, and `incremental` directories. Workspace intermediates are retained, not just downloaded dependencies. Cargo credentials and top-level installers/archives are excluded.
3. On a miss, the old dependency cache is tried under its original non-incremental environment. That fallback uses `Swatinem/rust-cache` with `save-if: false`: its normal save cleanup would delete incremental directories. Incremental mode is restored afterward and overridden again on the bundle step itself.
4. After the bundle command, `actions/cache/save` records the new intermediates under an immutable key containing the checked-out SHA, run ID, and attempt. This advances the cache even when manifests have not changed. Failed builds may preserve reusable compilation work, but cache success never substitutes for a successful build or quality check. Cache-service failures are nonfatal; compilation failures remain fatal.

The same compiler/recipe prefix is required for restoration. A compiler, target/profile namespace, lockfile/manifest, bundle configuration/script, or runner-image change starts a separate cache. Incremental compiler fingerprints still determine which results are reusable after source or environment changes. To invalidate all build caches deliberately, increment the cache-format version in `ci-incremental-cache.py`.

No paid cache capacity is configured. GitHub's existing quota and eviction rules apply; an evicted cache causes a cold build. Full workspace artifacts use more disk and transfer bandwidth than dependency-only caches, so cache transfer and linking can offset compilation savings. Unchanged optimization settings are more important than promising a particular speedup.

## Validation and measurements

- `script/test-bundle-split-modes.py` checks cache-key compatibility, excluded paths, restore-only fallback, every bundle job's effective incremental setting, and unchanged release gates. Its mocked Unix bundle executions check the environment passed to Cargo.
- `script/test-bundle-windows-modes.ps1` checks the Cargo environment for Windows desktop and server invocations without building locally.
- `incremental_ci.yml` builds the real Architect library with the release profile on Linux, Windows, and macOS, saves the production cache paths, removes only the generated fixture, restores the cache, verifies incremental-file hashes, and rebuilds. This is a same-runner archive round trip, not a full-app speed benchmark.
- The existing on/off comparison remains separate. Its earlier small-edit medians (4.105 s to 1.619 s for tests, 2.668 s to 1.525 s for Clippy) must not be extrapolated to a complete installer build.

Use the build job's cache summary, restore/save timings, bundle-step duration, and artifact sizes to assess actual subsequent release runs. The first incremental build may be no faster. Changing this PR does not alter a build already running from an older committed workflow.
