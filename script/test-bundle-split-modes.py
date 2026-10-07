#!/usr/bin/env python3
"""Run with python3 script/test-bundle-split-modes.py on a Unix CI runner.

Executes both bundle scripts in disposable repositories with mocked build,
platform, signing, and upload tools. Also checks the fork workflow's split-job
wiring without a YAML dependency. No Rust builds or network access occur.
"""

import gzip
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest


REPOSITORY = Path(__file__).resolve().parent.parent
MOCK_COMMAND = r'''
import gzip
import json
import os
from pathlib import Path
import sys

command = Path(sys.argv[0]).name
arguments = sys.argv[1:]
root = Path(os.environ["FIXTURE_ROOT"])
log = root / "commands.jsonl"
previous = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
with log.open("a") as output:
    output.write(json.dumps({
        "command": command,
        "arguments": arguments,
        "bundle": os.environ.get("ZED_BUNDLE"),
        "channel": os.environ.get("ZED_RELEASE_CHANNEL"),
        "version": os.environ.get("RELEASE_VERSION"),
        "bundle_type": os.environ.get("ZED_BUNDLE_TYPE"),
        "incremental": os.environ.get("CARGO_INCREMENTAL"),
    }) + "\n")
if os.environ.get("FAIL_TOOL") == command:
    sys.exit(1)

def value(option):
    return arguments[arguments.index(option) + 1]

def create(path, content=b"binary"):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)

if command == "rustc":
    print("host: " + os.environ["MOCK_HOST"])
elif command == "uname":
    print(os.environ["MOCK_HOST"].split("-")[0])
elif command == "git":
    print("123456789abcdef")
elif command == "get-crate-version":
    print("1.2.3")
elif command == "generate-licenses":
    create(root / "assets/licenses.md", b"licenses")
elif command == "cargo":
    if arguments[:3] == ["-q", "bundle", "--help"]:
        print("cargo-bundle v0.6.1-zed")
    elif "build" in arguments:
        assert arguments[:2] == ["--config", ".cargo/bundle-config.toml"]
        profile = "release" if "--release" in arguments else "debug"
        directory = root / os.environ.get("CARGO_TARGET_DIR", "target") / value("--target") / profile
        packages = [arguments[index + 1] for index, argument in enumerate(arguments) if argument == "--package"]
        assert packages in (["zed", "cli"], ["remote_server"])
        for package in packages:
            create(directory / package)
    elif arguments[0] == "bundle":
        assert "[package.metadata.bundle]" in Path("Cargo.toml").read_text()
        profile = "release" if "--release" in arguments else "debug"
        app = root / "target" / value("--target") / profile / "Mock.app"
        (app / "Contents/MacOS").mkdir(parents=True)
        (app / "Contents/Resources").mkdir()
        print(app)
    else:
        raise AssertionError("Unexpected cargo invocation: " + repr(arguments))
elif command == "dsymutil":
    binary = Path(arguments[-1])
    assert binary.is_file(), binary
    create(str(binary) + ".dwarf", b"symbols")
elif command == "strip":
    binary = Path(arguments[-1])
    assert binary.is_file(), binary
    if binary.name != "cli":
        assert Path(str(binary) + ".dwarf").is_file()
    binary.write_bytes(binary.read_bytes() + b"-stripped")
elif command == "llvm-objcopy":
    if arguments[0] == "--only-keep-debug":
        assert Path(arguments[1]).is_file()
        create(arguments[2], b"symbols")
    else:
        binary = Path(arguments[-1])
        assert binary.is_file(), binary
        if binary.name != "cli":
            assert Path(str(binary) + ".dbg").is_file()
        binary.write_bytes(binary.read_bytes() + b"-stripped")
elif command == "sentry-cli":
    assert arguments[:8] == ["debug-files", "upload", "--include-sources", "--wait", "-p", "zed", "-o", "zed-dev"]
    for name in arguments[8:]:
        binary = Path(name)
        assert binary.is_file(), binary
        assert b"stripped" not in binary.read_bytes()
    attempts = sum(entry["command"] == command for entry in previous)
    if attempts < int(os.environ.get("SENTRY_FAILURES", "0")):
        sys.exit(1)
elif command == "ldd":
    binary = Path(arguments[0])
    assert binary.is_file(), binary
    if binary.name == "zed":
        print("libstdc++.so.6 => " + str(root / "libstdc++.so.6") + " (0x123)")
    elif os.environ.get("MOCK_SSL"):
        print("libssl.so => /lib/libssl.so (0x123)")
    else:
        print("statically linked")
elif command == "gzip":
    assert arguments[:3] == ["-f", "--stdout", "--best"]
    sys.stdout.buffer.write(gzip.compress(Path(arguments[-1]).read_bytes()))
elif command == "tar":
    if "-xvz" in arguments:
        create(Path(value("-C")) / "bin/git")
    else:
        assert arguments[0] == "-czvf"
        create(arguments[1], b"desktop archive")
elif command == "codesign":
    binary = Path(arguments[-2])
    assert binary.exists(), binary
    if binary.is_file():
        binary.write_bytes(binary.read_bytes() + b"-signed")
elif command == "hdiutil":
    create(arguments[-1], b"desktop dmg")
elif command == "xcode-select":
    print(root / "xcode")
elif command == "envsubst":
    print(sys.stdin.read())
elif command not in {"rustup", "curl", "npm", "dmg-license", "security", "notarytool", "stapler", "open"}:
    raise AssertionError("Unexpected command: " + command)
'''


class BundleSplitModes(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="bundle-split-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for directory in (
            "script/lib", "crates/zed/resources", "crates/zed/contents/stable",
            "docs/brand", "assets", "target/release", "bin", "tmp", "xcode/usr/bin",
        ):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        for name in ("bundle-mac", "bundle-linux"):
            shutil.copyfile(REPOSITORY / "script" / name, self.root / "script" / name)
        (self.root / "script/lib/blob-store.sh").write_text("")
        (self.root / "crates/zed/RELEASE_CHANNEL").write_text("dev\n")
        (self.root / "crates/zed/Cargo.toml").write_text("[package.metadata.bundle-dev]\n")
        for name in (
            "app-icon-dev.png", "app-icon-dev@2x.png", "app-icon.png", "app-icon@2x.png",
            "Document.icns", "zed.entitlements", "zed.desktop.in",
        ):
            (self.root / "crates/zed/resources" / name).write_text("fixture\n")
        (self.root / "crates/zed/contents/stable/embedded.provisionprofile").write_text("profile")
        (self.root / "docs/brand/praxis-app-icon.svg").write_text("icon")
        (self.root / "libstdc++.so.6").write_text("library")
        driver = self.root / "bin/mock-command"
        driver.write_text("#!" + sys.executable + "\n" + MOCK_COMMAND)
        driver.chmod(0o755)
        for command in (
            "cargo", "rustc", "rustup", "git", "uname", "dsymutil", "strip",
            "llvm-objcopy", "sentry-cli", "ldd", "gzip", "tar", "codesign",
            "hdiutil", "xcode-select", "envsubst", "curl", "npm", "dmg-license",
            "security", "open",
        ):
            (self.root / "bin" / command).symlink_to(driver)
        for command in ("generate-licenses", "get-crate-version"):
            (self.root / "script" / command).symlink_to(driver)
        for command in ("notarytool", "stapler"):
            (self.root / "xcode/usr/bin" / command).symlink_to(driver)
        # The production script deliberately uses an absolute codesign path.
        (self.root / "bash-env").write_text(
            "function /usr/bin/codesign { "
            + shlex.quote(str(self.root / "bin/codesign")) + ' "$@"; }\n'
        )

    def run_bundle(self, platform, *arguments, environment=None, succeeds=True):
        env = os.environ.copy()
        for name in (
            "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR", "REMOTE_SERVER_TARGET",
            "MACOS_CERTIFICATE", "MACOS_CERTIFICATE_PASSWORD", "MACOS_SIGNING_KEY",
            "APPLE_NOTARIZATION_KEY", "APPLE_NOTARIZATION_KEY_ID", "APPLE_NOTARIZATION_ISSUER_ID",
            "ZED_RELEASE_CHANNEL", "RELEASE_VERSION", "ZED_BUNDLE_TYPE", "FAIL_TOOL",
            "SENTRY_FAILURES", "MOCK_SSL",
        ):
            env.pop(name, None)
        env.update({
            "PATH": str(self.root / "bin") + os.pathsep + env["PATH"],
            "BASH_ENV": str(self.root / "bash-env"),
            "FIXTURE_ROOT": str(self.root),
            "TMPDIR": str(self.root / "tmp"),
            "MOCK_HOST": "aarch64-apple-darwin" if platform == "mac" else "x86_64-unknown-linux-gnu",
            "SENTRY_AUTH_TOKEN": "mock-token",
            "CC": "clang",
        })
        env.update(environment or {})
        (self.root / "commands.jsonl").write_text("")
        result = subprocess.run(
            ["bash", "script/bundle-" + platform, *arguments],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode == 0, succeeds, result.stdout + result.stderr)
        log = self.root / "commands.jsonl"
        self.commands = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return result

    def calls(self, command):
        return [entry for entry in self.commands if entry["command"] == command]

    def assert_mode(self, desktop, remote):
        builds = [entry for entry in self.calls("cargo") if "build" in entry["arguments"]]
        packages = [
            [arguments[index + 1] for index, argument in enumerate(arguments) if argument == "--package"]
            for arguments in (entry["arguments"] for entry in builds)
        ]
        self.assertEqual(packages, ([["zed", "cli"]] if desktop else []) + ([["remote_server"]] if remote else []))
        for entry in builds:
            self.assertEqual(entry["bundle"], "true")
            self.assertEqual(entry["arguments"][:2], ["--config", ".cargo/bundle-config.toml"])
            if "--features" not in entry["arguments"]:
                self.assertIn("--release", entry["arguments"])
        uploads = self.calls("sentry-cli")
        self.assertTrue(uploads)
        uploaded = " ".join(uploads[-1]["arguments"][8:])
        self.assertEqual("/zed" in uploaded, desktop)
        self.assertEqual("/remote_server" in uploaded, remote)
        self.assertEqual(len(self.calls("gzip")), int(remote))
        if not remote:
            for command in ("dsymutil", "strip", "llvm-objcopy", "codesign", "ldd"):
                self.assertNotIn("remote_server", json.dumps(self.calls(command)))
        if not desktop:
            for command in ("hdiutil", "npm", "dmg-license", "curl", "tar", "open"):
                self.assertEqual(self.calls(command), [])
            self.assertFalse(any("bundle" in entry["arguments"] or "install" in entry["arguments"] for entry in self.calls("cargo")))

    def test_release_cargo_invocations_keep_incremental_enabled(self):
        for platform in ("mac", "linux"):
            for mode in ("--desktop-only", "--remote-server-only"):
                with self.subTest(platform=platform, mode=mode):
                    self.run_bundle(platform, mode, environment={"CARGO_INCREMENTAL": "1"})
                    self.assertTrue(self.calls("cargo"))
                    self.assertEqual({entry["incremental"] for entry in self.calls("cargo")}, {"1"})
                    self.assert_mode(desktop=mode == "--desktop-only", remote=mode == "--remote-server-only")

    def test_mac_default_combined(self):
        self.run_bundle("mac")
        self.assert_mode(desktop=True, remote=True)
        self.assertTrue((self.root / "target/aarch64-apple-darwin/release/Praxis-aarch64.dmg").is_file())
        self.assertEqual(gzip.decompress((self.root / "target/zed-remote-server-macos-aarch64.gz").read_bytes()), b"binary-stripped")
        self.assertEqual((self.root / "crates/zed/Cargo.toml").read_text(), "[package.metadata.bundle-dev]\n")

    def test_mac_desktop_only(self):
        self.run_bundle("mac", "--desktop-only", "x86_64-apple-darwin")
        self.assert_mode(desktop=True, remote=False)
        self.assertTrue((self.root / "target/x86_64-apple-darwin/release/Praxis-x86_64.dmg").is_file())
        self.assertFalse(list((self.root / "target").glob("zed-remote-server*")))

    def test_mac_remote_only_signed(self):
        self.run_bundle("mac", "--remote-server-only", "x86_64-apple-darwin", environment={
            "MACOS_CERTIFICATE": "Y2VydGlmaWNhdGU=", "MACOS_CERTIFICATE_PASSWORD": "mock",
            "APPLE_NOTARIZATION_KEY": "mock", "APPLE_NOTARIZATION_KEY_ID": "mock",
            "APPLE_NOTARIZATION_ISSUER_ID": "mock",
        })
        self.assert_mode(desktop=False, remote=True)
        artifact = self.root / "target/zed-remote-server-macos-x86_64.gz"
        self.assertEqual(gzip.decompress(artifact.read_bytes()), b"binary-stripped-signed")
        self.assertEqual(len(self.calls("codesign")), 1)
        self.assertEqual(self.calls("codesign")[0]["arguments"][:-2], [
            "--deep", "--force", "--timestamp", "--options", "runtime", "--entitlements",
            "crates/zed/resources/zed.entitlements", "--sign", "Zed Industries, Inc.",
        ])
        self.assertIn("delete-keychain", self.calls("security")[-1]["arguments"])
        self.assertEqual(self.calls("cargo")[0]["channel"], "dev")

    def test_mac_debug_remote_uses_debug_binary(self):
        self.run_bundle("mac", "-d", "--remote-server-only")
        self.assert_mode(desktop=False, remote=True)
        build = self.calls("cargo")[0]["arguments"]
        self.assertNotIn("--release", build)
        self.assertIn("debug-embed", build)
        self.assertEqual(self.calls("strip"), [])
        self.assertEqual(gzip.decompress((self.root / "target/zed-remote-server-macos-aarch64.gz").read_bytes()), b"binary")

    def test_mac_debug_desktop_preserves_short_options(self):
        self.run_bundle("mac", "-do", "--desktop-only")
        self.assert_mode(desktop=True, remote=False)
        self.assertEqual(len(self.calls("open")), 1)
        self.assertEqual(self.calls("hdiutil"), [])
        self.assertEqual(self.calls("strip"), [])

    def test_mac_stable_desktop_metadata(self):
        (self.root / "crates/zed/RELEASE_CHANNEL").write_text("stable\n")
        (self.root / "crates/zed/Cargo.toml").write_text("[package.metadata.bundle-stable]\n")
        self.run_bundle("mac", "--desktop-only")
        self.assertEqual(self.calls("cargo")[0]["channel"], "stable")
        self.assertTrue((self.root / "target/aarch64-apple-darwin/release/Zed-aarch64.dmg").is_file())
        self.assertEqual(self.calls("dmg-license")[0]["arguments"][0], "script/terms/terms.json")

    def test_linux_default_combined(self):
        self.run_bundle("linux")
        self.assert_mode(desktop=True, remote=True)
        self.assertTrue((self.root / "target/release/praxis-linux-x86_64.tar.gz").is_file())
        self.assertEqual(gzip.decompress((self.root / "target/zed-remote-server-linux-x86_64.gz").read_bytes()), b"binary-stripped")

    def test_linux_desktop_only_flatpak(self):
        self.run_bundle("linux", "--desktop-only", "--flatpak")
        self.assert_mode(desktop=True, remote=False)
        self.assertEqual(self.calls("rustup"), [])
        self.assertEqual(self.calls("cargo")[0]["bundle_type"], "flatpak")
        self.assertEqual(self.calls("cargo")[0]["version"], "1.2.3")
        self.assertTrue((self.root / "target/release/praxis-linux-x86_64.tar.gz").is_file())
        self.assertFalse(list((self.root / "target").glob("zed-remote-server*")))

    def test_linux_remote_only_custom_target_directory(self):
        self.run_bundle("linux", "--remote-server-only", environment={
            "CARGO_TARGET_DIR": "custom-target", "REMOTE_SERVER_TARGET": "aarch64-unknown-linux-gnu",
            "MOCK_HOST": "aarch64-unknown-linux-gnu",
        })
        self.assert_mode(desktop=False, remote=True)
        self.assertEqual(self.calls("cargo")[0]["version"], "1.2.3")
        self.assertIn("aarch64-unknown-linux-gnu", self.calls("cargo")[0]["arguments"])
        self.assertEqual(gzip.decompress((self.root / "custom-target/zed-remote-server-linux-aarch64.gz").read_bytes()), b"binary-stripped")

    def test_linux_musl_dependency_check(self):
        result = self.run_bundle("linux", "--remote-server-only", environment={"MOCK_SSL": "1"}, succeeds=False)
        self.assertIn("still depends on libssl or libcrypto", result.stdout)
        self.assertEqual(self.calls("gzip"), [])

    def test_mac_sentry_retries(self):
        self.run_bundle("mac", "--remote-server-only", environment={"SENTRY_FAILURES": "2"})
        self.assertEqual(len(self.calls("sentry-cli")), 3)
        self.assertEqual(len(self.calls("dsymutil")), 1)

    def test_mac_sentry_failure_is_fatal(self):
        self.run_bundle("mac", "--remote-server-only", environment={"FAIL_TOOL": "sentry-cli"}, succeeds=False)
        self.assertEqual(len(self.calls("sentry-cli")), 3)
        self.assertEqual(self.calls("gzip"), [])

    def test_linux_sentry_failure_preserves_existing_nonfatal_policy(self):
        self.run_bundle("linux", "--remote-server-only", environment={"FAIL_TOOL": "sentry-cli"})
        self.assertEqual(len(self.calls("sentry-cli")), 3)
        self.assertEqual(len(self.calls("gzip")), 1)

    def test_mac_extraction_failure_is_fatal(self):
        self.run_bundle("mac", "--remote-server-only", environment={"FAIL_TOOL": "dsymutil"}, succeeds=False)
        self.assertEqual(self.calls("strip"), [])
        self.assertEqual(self.calls("gzip"), [])

    def test_symbols_retained_without_sentry_token(self):
        for platform, extension, target in (
            ("mac", ".dwarf", "aarch64-apple-darwin"),
            ("linux", ".dbg", "x86_64-unknown-linux-musl"),
        ):
            with self.subTest(platform=platform):
                self.run_bundle(platform, "--remote-server-only", environment={"SENTRY_AUTH_TOKEN": ""})
                self.assertEqual(self.calls("sentry-cli"), [])
                self.assertTrue((self.root / "target" / target / "release" / ("remote_server" + extension)).is_file())

    def test_conflicting_modes_fail_before_build(self):
        for platform in ("mac", "linux"):
            with self.subTest(platform=platform):
                result = self.run_bundle(platform, "--desktop-only", "--remote-server-only", succeeds=False)
                self.assertIn("mutually exclusive", result.stderr)
                self.assertEqual(self.calls("cargo"), [])

    def test_mac_remote_rejects_desktop_actions(self):
        for option in ("-i", "-o"):
            with self.subTest(option=option):
                result = self.run_bundle("mac", "--remote-server-only", option, succeeds=False)
                self.assertIn("require a desktop build", result.stderr)
                self.assertEqual(self.calls("cargo"), [])

    def test_help_does_not_build(self):
        for platform in ("mac", "linux"):
            with self.subTest(platform=platform):
                result = self.run_bundle(platform, "-h")
                self.assertIn("--desktop-only", result.stdout)
                self.assertIn("--remote-server-only", result.stdout)
                self.assertEqual(self.calls("cargo"), [])


def workflow_fields(lines, indentation):
    """Read immediate plain mapping keys at one known indentation, not YAML.

    Nested text is retained for explicit traversal by the workflow tests. Flow
    mappings, quoted keys, aliases, and other YAML constructs are not supported.
    """
    fields = {}
    current = None
    for line in lines:
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if "\t" in line[:len(line) - len(line.lstrip())]:
            raise AssertionError("Workflow indentation must use spaces")
        depth = len(line) - len(line.lstrip(" "))
        if depth == indentation:
            name, separator, value = line.strip().partition(":")
            if not separator or not name.replace("-", "_").isidentifier():
                raise AssertionError("Unsupported workflow mapping: " + line)
            if name in fields:
                raise AssertionError("Duplicate workflow key: " + name)
            fields[name] = (value.strip(), [])
            current = name
        elif depth > indentation and current is not None:
            fields[current][1].append(line)
        else:
            raise AssertionError("Unexpected workflow indentation: " + line)
    return fields


def workflow_value(fields, name):
    value, body = fields[name]
    if value in ("|", ">", ">-"):
        return " ".join(line.strip() for line in body)
    if body:
        raise AssertionError("Expected a workflow scalar: " + name)
    return value


class IncrementalCacheIdentity(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("incremental_cache", REPOSITORY / "script/ci-incremental-cache.py")
        cls.helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.helper)

    def setUp(self):
        self.environment = {
            "RUNNER_OS": "Windows", "RUNNER_ARCH": "X64", "ImageVersion": "fixture-1",
            "PRAXIS_CARGO_PROFILE": "release",
        }

    def prefix(self, name="bundle-windows-desktop-release", compiler="rustc fixture", recipe="recipe-1", **environment):
        return self.helper.cache_prefix(name, compiler, recipe, {**self.environment, **environment})

    def test_incompatible_compilers_targets_and_profiles_never_share_a_prefix(self):
        baseline = self.prefix()
        for changes in (
            {"name": "bundle-windows-server-release"},
            {"name": "bundle-windows-desktop-test"},
            {"compiler": "rustc next"}, {"recipe": "changed-lock-or-build-flags"},
            {"RUNNER_OS": "Linux"}, {"RUNNER_ARCH": "ARM64"},
            {"ImageVersion": "fixture-2"}, {"PRAXIS_CARGO_PROFILE": "praxis-test"},
            {"RUSTFLAGS": "-C target-cpu=native"}, {"SDKROOT": "different-sdk"},
        ):
            with self.subTest(changes=changes):
                self.assertNotEqual(baseline, self.prefix(**changes))
        self.assertEqual(baseline, self.prefix(GITHUB_RUN_ID="999", GH_TOKEN="not-a-cache-input"))

    def test_each_build_can_save_new_artifacts_and_restore_the_previous_prefix(self):
        prefix = self.prefix()
        keys = {
            self.helper.cache_key(prefix, "a" * 40, "10", "1"),
            self.helper.cache_key(prefix, "b" * 40, "11", "1"),
            self.helper.cache_key(prefix, "a" * 40, "10", "2"),
        }
        self.assertEqual(len(keys), 3)
        self.assertTrue(all(key.startswith(prefix + "-") and len(key) < 512 for key in keys))
        with self.assertRaises(ValueError):
            self.prefix(name="bad\noutput=injected")
        with self.assertRaises(ValueError):
            self.helper.cache_key(prefix, "main", "10", "1")

    def test_cache_paths_preserve_incremental_and_workspace_state_without_packaging_outputs(self):
        paths = self.helper.cache_paths("/fixture/cargo")
        self.assertIn("/fixture/cargo/registry", paths)
        self.assertIn("/fixture/cargo/git", paths)
        for directory in ("incremental", ".fingerprint", "deps", "build"):
            self.assertIn("target/**/" + directory, paths)
        self.assertNotIn("target", paths)
        self.assertNotIn("/fixture/cargo", paths)
        self.assertFalse(any("credentials" in path or path.endswith((".exe", ".zip", ".dmg", ".tar.gz")) for path in paths))

    def test_checkpoints_replace_oversized_caches_and_legacy_restore_cannot_prune_them(self):
        action = (REPOSITORY / ".github/actions/rust-build-cache/action.yml").read_text(encoding="utf-8")
        save = (REPOSITORY / ".github/actions/save-rust-build-cache/action.yml").read_text(encoding="utf-8")
        self.assertIn("uses: actions/download-artifact@v8", action)
        self.assertNotIn("uses: actions/cache/", action + save)
        self.assertIn('save-if: "false"', action)
        self.assertIn("steps.unpack.outputs.restored != 'true'", action)
        self.assertLess(action.index("uses: Swatinem/rust-cache@v2"), action.index("Enable incremental compilation after state restoration"))
        self.assertIn('output.write("CARGO_INCREMENTAL=1\\n")', action)
        self.assertIn("uses: actions/upload-artifact@v7", save)
        self.assertIn("if: github.event.repository.visibility == 'public'", save)
        self.assertIn("retention-days: 7", save)
        self.assertIn("compression-level: 0", save)
        self.assertFalse(self.prefix().startswith("praxis-"))


class BundleWorkflowWiring(unittest.TestCase):
    PRODUCERS = (
        "macos", "macos_remote_server", "windows", "windows_remote_server",
        "linux", "linux_remote_server",
    )
    PINNED_SHA = "${{ needs.prepare.outputs.source_sha }}"
    WINDOWS_PROFILE = "${{ inputs.test_build && 'praxis-test' || 'release' }}"

    @classmethod
    def setUpClass(cls):
        workflow = REPOSITORY / ".github/workflows/bundle_fork.yml"
        cls.document = workflow_fields(workflow.read_text(encoding="utf-8").splitlines(), 0)
        cls.jobs = {
            name: workflow_fields(body, 4)
            for name, (value, body) in workflow_fields(cls.document["jobs"][1], 2).items()
        }
        dispatch = workflow_fields(cls.document["on"][1], 2)["workflow_dispatch"]
        inputs = workflow_fields(dispatch[1], 4)["inputs"]
        cls.inputs = {
            name: workflow_fields(body, 8)
            for name, (value, body) in workflow_fields(inputs[1], 6).items()
        }

    def needs(self, job):
        value, body = self.jobs[job]["needs"]
        if value:
            self.assertFalse(body)
            self.assertTrue(value.replace("-", "_").isidentifier(), value)
            return {value}
        names = []
        for line in body:
            self.assertTrue(line.startswith("      - "), line)
            name = line[8:]
            self.assertTrue(name.replace("-", "_").isidentifier(), name)
            names.append(name)
        self.assertEqual(len(names), len(set(names)), "Duplicate needs in " + job)
        return set(names)

    def steps(self, job):
        value, body = self.jobs[job]["steps"]
        self.assertEqual(value, "")
        blocks = []
        for line in body:
            if line.startswith("      - "):
                blocks.append(["        " + line[8:]])
            else:
                self.assertTrue(blocks and line.startswith("        "), line)
                blocks[-1].append(line)
        return [workflow_fields(block, 8) for block in blocks]

    def action_steps(self, job, action):
        return [
            step for step in self.steps(job)
            if "uses" in step and workflow_value(step, "uses").startswith(action + "@")
        ]

    def runs(self, job):
        return [workflow_value(step, "run") for step in self.steps(job) if "run" in step]

    def environment(self, job):
        return workflow_fields(self.jobs[job]["env"][1], 6)

    def assert_expression(self, actual, expected):
        self.assertEqual("".join(actual.split()), "".join(expected.split()))

    def test_producers_are_independent_after_validation(self):
        for job in self.PRODUCERS:
            with self.subTest(job=job):
                # In particular, neither native server waits for its desktop,
                # and Windows collects Linux's artifact without a job-level wait.
                self.assertEqual(self.needs(job), {"prepare", "validated"})
        self.assertTrue(workflow_value(self.jobs["macos_remote_server"], "runs-on").startswith("macos-"))
        self.assertTrue(workflow_value(self.jobs["windows_remote_server"], "runs-on").startswith("windows-"))

    def test_every_producer_checks_out_the_prepared_commit(self):
        prepare = self.jobs["prepare"]
        outputs = workflow_fields(prepare["outputs"][1], 6)
        self.assertEqual(workflow_value(outputs, "source_sha"), "${{ steps.source.outputs.sha }}")
        source_steps = [step for step in self.steps("prepare") if step.get("id", (None,))[0] == "source"]
        self.assertEqual(len(source_steps), 1)
        self.assertEqual(
            workflow_value(source_steps[0], "run"),
            'echo "sha=$(git rev-parse HEAD)" >> "${GITHUB_OUTPUT}"',
        )
        for job in (*self.PRODUCERS, "publish"):
            with self.subTest(job=job):
                checkouts = self.action_steps(job, "actions/checkout")
                self.assertEqual(len(checkouts), 1)
                options = workflow_fields(checkouts[0]["with"][1], 10)
                self.assertEqual(workflow_value(options, "ref"), self.PINNED_SHA)
                self.assertNotIn("repository", options)
        quality_options = workflow_fields(self.jobs["quality"]["with"][1], 6)
        self.assertEqual(workflow_value(quality_options, "source_ref"), self.PINNED_SHA)

    def test_bundle_commands_select_exactly_one_mode(self):
        expected = {
            "macos": "./script/bundle-mac --desktop-only ${{ matrix.target }}",
            "macos_remote_server": "./script/bundle-mac --remote-server-only ${{ matrix.target }}",
            "linux": "./script/bundle-linux --desktop-only",
            "linux_remote_server": "./script/bundle-linux --remote-server-only",
            "windows": "./script/bundle-windows.ps1 -DesktopOnly",
            "windows_remote_server": "./script/bundle-windows.ps1 -RemoteServerOnly",
        }
        for job, command in expected.items():
            with self.subTest(job=job):
                commands = [run for run in self.runs(job) if "script/bundle-" in run]
                self.assertEqual(commands, [command])
        remote_macos = " ".join(self.runs("macos_remote_server"))
        self.assertNotRegex(remote_macos, r"cargo[- ]bundle|\bnpm\b|dmg-license")
        self.assertEqual(self.action_steps("macos_remote_server", "actions/setup-node"), [])

    def test_full_release_defaults_and_profiles_are_preserved(self):
        for name, default, kind in (
            ("source_ref", "main", "string"),
            ("build_macos", "true", "boolean"),
            ("build_windows", "true", "boolean"),
            ("build_linux", "true", "boolean"),
            ("test_build", "false", "boolean"),
        ):
            with self.subTest(input=name):
                self.assertEqual(workflow_value(self.inputs[name], "default"), default)
                self.assertEqual(workflow_value(self.inputs[name], "type"), kind)
        environments = [workflow_fields(self.document.get("env", ("", []))[1], 2)]
        for job in self.PRODUCERS:
            environment = self.environment(job)
            with self.subTest(job=job):
                self.assertEqual(workflow_value(environment, "CARGO_INCREMENTAL"), '"1"')
                if job.startswith("windows"):
                    self.assertEqual(workflow_value(environment, "PRAXIS_CARGO_PROFILE"), self.WINDOWS_PROFILE)
                else:
                    self.assertNotIn("PRAXIS_CARGO_PROFILE", environment)
                self.assertNotRegex(" ".join(self.runs(job)), r"--profile\b|release-fast|CARGO_PROFILE_|RUSTFLAGS")
            environments.append(environment)
            for step in self.steps(job):
                step_environment = workflow_fields(step.get("env", ("", []))[1], 10)
                self.assertNotIn("PRAXIS_CARGO_PROFILE", step_environment)
                environments.append(step_environment)
        self.assertNotIn("PRAXIS_CARGO_PROFILE", environments[0])
        for environment in environments:
            self.assertFalse(any(name.startswith("CARGO_PROFILE_") for name in environment), environment)
            self.assertTrue({"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"}.isdisjoint(environment), environment)

    def test_real_bundle_steps_override_cache_defaults_and_save_complete_build_state(self):
        cache_names = set()
        for job in self.PRODUCERS:
            with self.subTest(job=job):
                steps = self.steps(job)
                restore = [step for step in steps if step.get("uses", (None,))[0] == "./.github/actions/rust-build-cache"]
                self.assertEqual(len(restore), 1)
                self.assertEqual(workflow_value(restore[0], "id"), "build-cache")
                options = workflow_fields(restore[0]["with"][1], 10)
                name = workflow_value(options, "cache-name")
                self.assertNotIn(name, cache_names)
                cache_names.add(name)
                self.assertEqual(name, workflow_value(options, "legacy-key"))
                builds = [step for step in steps if "run" in step and "script/bundle-" in workflow_value(step, "run")]
                self.assertEqual(len(builds), 1)
                self.assertNotIn("continue-on-error", builds[0])
                environment = workflow_fields(builds[0]["env"][1], 10)
                self.assertEqual(workflow_value(environment, "CARGO_INCREMENTAL"), '"1"')
                saves = [step for step in steps if step.get("uses", (None,))[0] == "./.github/actions/save-rust-build-cache"]
                self.assertEqual(len(saves), 1)
                options = workflow_fields(saves[0]["with"][1], 10)
                self.assertEqual(workflow_value(options, "key"), "${{ steps.build-cache.outputs.primary-key }}")
                self.assertNotIn("path", options)
                self.assertEqual(self.action_steps(job, "actions/cache/save"), [])
                self.assertLess(steps.index(restore[0]), steps.index(builds[0]))
                self.assertLess(steps.index(builds[0]), steps.index(saves[0]))
                self.assertIn("success()", workflow_value(saves[0], "if"))
                self.assertEqual(self.action_steps(job, "Swatinem/rust-cache"), [])

    def test_checkpoint_archives_are_not_downloaded_as_release_assets(self):
        import fnmatch
        key = "rust-build-state-v2-bundle-linux-example"
        for step in self.action_steps("publish", "actions/download-artifact"):
            options = workflow_fields(step["with"][1], 10)
            selection = workflow_value(options, "name" if "name" in options else "pattern")
            self.assertFalse(fnmatch.fnmatch(key, selection), selection)

    def test_linux_remote_has_one_producer_and_no_desktop_compile(self):
        invocations = []
        uploaders = []
        for job in self.jobs:
            if "steps" not in self.jobs[job]:
                continue
            for run in self.runs(job):
                if "script/bundle-linux" in run:
                    invocations.append((job, run))
                self.assertNotIn("script/build-linux-remote-server", run)
                self.assertNotRegex(run, r"cargo\b.*(?:--package(?:=|\s+)|-p\s+)remote_server\b")
            for step in self.action_steps(job, "actions/upload-artifact"):
                options = workflow_fields(step["with"][1], 10)
                name = workflow_value(options, "name")
                path = workflow_value(options, "path")
                if name == "praxis-remote-server-linux-x86_64" or "zed-remote-server-linux-" in path:
                    uploaders.append(job)
                    self.assertEqual(name, "praxis-remote-server-linux-x86_64")
                    self.assertEqual(path, "target/zed-remote-server-linux-x86_64.gz")
                    self.assertEqual(workflow_value(options, "if-no-files-found"), "error")
        self.assertCountEqual(invocations, [
            ("linux", "./script/bundle-linux --desktop-only"),
            ("linux_remote_server", "./script/bundle-linux --remote-server-only"),
        ])
        self.assertEqual(uploaders, ["linux_remote_server"])
        self.assertNotRegex(" ".join(self.runs("linux")), r"remote_server|remote-server")

    def test_windows_receives_the_existing_same_run_linux_artifact(self):
        environment = self.environment("windows")
        self.assertEqual(
            workflow_value(environment, "PRAXIS_LINUX_REMOTE_SERVER_ARTIFACT"),
            "${{ !inputs.test_build && 'praxis-remote-server-linux-x86_64' || '' }}",
        )
        self.assertEqual(workflow_value(environment, "GH_TOKEN"), "${{ github.token }}")
        permissions = workflow_fields(self.jobs["windows"]["permissions"][1], 6)
        self.assertEqual(workflow_value(permissions, "actions"), "read")
        # The script uses GitHub's current-run context. Keep that context and
        # the job-level artifact name intact rather than adding another download.
        self.assertEqual(self.action_steps("windows", "actions/download-artifact"), [])
        protected = {
            "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_REPOSITORY",
            "PRAXIS_LINUX_REMOTE_SERVER_ARTIFACT",
        }
        self.assertTrue(protected.isdisjoint(workflow_fields(self.document.get("env", ("", []))[1], 2)))
        self.assertTrue((protected - {"PRAXIS_LINUX_REMOTE_SERVER_ARTIFACT"}).isdisjoint(environment))
        for step in self.steps("windows"):
            self.assertTrue(protected.isdisjoint(workflow_fields(step.get("env", ("", []))[1], 10)))
        self.assertNotRegex(" ".join(self.runs("windows")), r"GITHUB_RUN_ID|GITHUB_RUN_ATTEMPT|GITHUB_REPOSITORY|PRAXIS_LINUX_REMOTE_SERVER_ARTIFACT|gh\s+run\s+download")

    def test_quality_reuse_requires_prepares_verified_true_output(self):
        outputs = workflow_fields(self.jobs["prepare"]["outputs"][1], 6)
        self.assertEqual(workflow_value(outputs, "validated"), "${{ steps.validated.outputs.validated }}")
        receipt_steps = [step for step in self.steps("prepare") if step.get("id", (None,))[0] == "validated"]
        self.assertEqual(len(receipt_steps), 1)
        receipt = receipt_steps[0]
        environment = workflow_fields(receipt["env"][1], 10)
        self.assertEqual(workflow_value(environment, "SOURCE_SHA"), "${{ steps.source.outputs.sha }}")
        receipt_command = workflow_value(receipt, "run")
        self.assertIn("python script/praxis-validation.py check", receipt_command)
        self.assertIn('--source-sha "${SOURCE_SHA}"', receipt_command)
        self.assertIn('|| echo "validated=false" >> "${GITHUB_OUTPUT}"', receipt_command)
        self.assertIn('else echo "No validation receipt helper in this source; running full quality" echo "validated=false"', receipt_command)
        self.assertNotIn("validated=true", receipt_command)
        self.assertEqual(self.needs("quality"), {"prepare"})
        self.assertEqual(workflow_value(self.jobs["quality"], "uses"), "./.github/workflows/architect_quality.yml")
        self.assert_expression(workflow_value(self.jobs["quality"], "if"), "needs.prepare.outputs.validated != 'true'")
        self.assertEqual(self.needs("validated"), {"prepare", "quality"})
        self.assert_expression(workflow_value(self.jobs["validated"], "if"), """
            !cancelled() && needs.prepare.result == 'success' &&
            (needs.quality.result == 'success' ||
             (needs.quality.result == 'skipped' && needs.prepare.outputs.validated == 'true'))
        """)
        selectors = {
            "macos": "inputs.build_macos && !inputs.test_build",
            "macos_remote_server": "inputs.build_macos && !inputs.test_build",
            "linux": "inputs.build_linux && !inputs.test_build",
            "linux_remote_server": "(inputs.build_linux || inputs.build_windows) && !inputs.test_build",
            "windows": "inputs.build_windows",
            "windows_remote_server": "inputs.build_windows",
        }
        for job, selector in selectors.items():
            with self.subTest(job=job):
                self.assert_expression(
                    workflow_value(self.jobs[job], "if"),
                    "!cancelled() && needs.validated.result == 'success' && " + selector,
                )

    def test_publish_requires_every_producer_to_succeed(self):
        self.assertEqual(self.needs("publish"), {"prepare", *self.PRODUCERS})
        self.assert_expression(workflow_value(self.jobs["publish"], "if"), " && ".join([
            "always()", "!inputs.test_build", "inputs.build_macos",
            "inputs.build_windows", "inputs.build_linux",
            *("needs." + job + ".result == 'success'" for job in self.PRODUCERS),
        ]))
        downloads = []
        for step in self.action_steps("publish", "actions/download-artifact"):
            options = workflow_fields(step["with"][1], 10)
            self.assertEqual(workflow_value(options, "path"), "release-assets")
            self.assertNotIn("run-id", options)
            self.assertNotIn("repository", options)
            selector = "name" if "name" in options else "pattern"
            downloads.append((selector, workflow_value(options, selector)))
            if selector == "pattern":
                self.assertEqual(workflow_value(options, "merge-multiple"), "true")
        self.assertCountEqual(downloads, [
            ("name", "praxis-windows-x86_64-installer"),
            ("pattern", "praxis-macos-*-dmg"),
            ("name", "praxis-linux-x86_64-archive"),
            ("pattern", "praxis-remote-server-*"),
        ])


if __name__ == "__main__":
    unittest.main()
