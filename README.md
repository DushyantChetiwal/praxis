> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

<p align="center">
  <img src="docs/brand/praxis-banner.png" alt="Praxis — detailed and deterministic planning" width="420">
</p>

# Praxis

**Plan systems. Ship code.**

Praxis is a plan-driven software workspace for detailed and deterministic
planning. It is an independent product built on the open-source
[Zed](https://github.com/zed-industries/zed) editor and is not affiliated with
or endorsed by Zed Industries.

## Architect View

Instead of treating an agent as a one-turn command box, Praxis makes planning a
primary view. Work is drafted as a graph of explicit steps, constraints,
dependencies, conditions, loops, and expected hand-offs. Individual steps can
be discussed and refined before the locked plan is executed.

Editor View is the familiar editor: project tree, editor, terminal, Git panel,
and the overall plan conversation. Switch between the two with the Architect
View button in the Agent panel, the Editor View button on the canvas, or
`agent: toggle architect view`; opening a file from Architect View switches to
Editor View with that file in front. Native threads have three modes: Plan
researches with reviewed read capabilities and asks you to approve a plan
before anything changes, as in Zed; Architect drafts the plan on the canvas
with the same read-only boundary; and Build enables approved mutation and
execution tools.

The main product additions live in:

- `crates/architect` for the plan graph, compiler, persistence, and runner
- `crates/agent_ui/src/architect_ui/` for Architect View
- `crates/agent` and `crates/agent_ui` for tools and execution integration

See [`docs/src/ai/architect-portfolio.md`](./docs/src/ai/architect-portfolio.md)
for the architecture and reproducible demo.

## Praxis Remote

Follow and steer the agent from your phone: chat, answer permission prompts,
switch modes, run a plan, and read project files. Open **Praxis Remote…** in
Praxis and sign in with GitHub, sign in to the native Android app with the same
account, and pair the phone by comparing a six-digit code and clicking Allow on
the computer. There is nothing else to set up: the two talk through a secret
gist in your account, end-to-end encrypted, so nothing on your computer listens
on the network and GitHub only sees encrypted data. The APK is published in the
releases tagged `praxis-remote-android-…`. See
[`docs/src/ai/praxis-remote.md`](./docs/src/ai/praxis-remote.md) for setup and
the security model, and
[`docs/src/ai/praxis-remote-protocol.md`](./docs/src/ai/praxis-remote-protocol.md)
for the protocol. The app's source is in [`remote-android/`](./remote-android/).

## Installation and updates

Unsigned Windows, macOS, and Linux builds are published through this
repository's GitHub releases:

- Windows x86_64: `Praxis-x86_64.exe`
- macOS Apple silicon: `Praxis-aarch64.dmg`
- macOS Intel: `Praxis-x86_64.dmg`
- Linux x86_64: `praxis-linux-x86_64.tar.gz`

Download the latest build from [Releases](https://github.com/DushyantChetiwal/praxis/releases/latest).

Installed **Praxis Dev** builds poll the repository's signed-hash update
manifest and update through the in-app updater. Praxis uses its own application,
installer, process, registry, bundle, and user-data identities, so it can remain
installed beside Stable Zed without sharing or deleting Stable Zed data.

## Upstream maintenance

A scheduled GitHub workflow merges `zed-industries/zed` into a temporary
staging branch, validates the exact merge with the Praxis quality workflow, and
promotes only a green result. Provider updates, editor improvements, security
fixes, and other upstream internals therefore continue to flow into Praxis.
Merge conflicts or quality failures stop promotion for manual resolution.

## Development

`main` is the product branch. Changes land through pull requests, and every
push and pull request to `main` runs the Architect quality workflow.

Praxis retains Zed's internal crate names and most source identifiers to keep
upstream merges reviewable. Platform builds and Rust validation run in GitHub
Actions rather than on contributor workstations.

The upstream platform build documentation remains applicable:

- [macOS](./docs/src/development/macos.md)
- [Linux](./docs/src/development/linux.md)
- [Windows](./docs/src/development/windows.md)

See [CONTRIBUTING.md](./CONTRIBUTING.md) before submitting changes.

## Licensing and attribution

Praxis is derived from Zed, which is developed by Zed Industries, Inc. Zed and
its associated marks belong to their respective owners. This repository keeps
the upstream copyright, license, and attribution notices.

The source is licensed primarily under GPL-3.0-or-later, with Apache-2.0
components where marked. Third-party dependency notices are generated with
[`cargo-about`](https://github.com/EmbarkStudios/cargo-about).
