<p align="center">
  <img src="docs/brand/praxis-banner.png" alt="Praxis — detailed and deterministic planning" width="420">
</p>

# Praxis

**Plan systems. Ship code.**

Praxis is a plan-driven software workspace for detailed and deterministic
planning. It is an independent product built on the open-source
[Zed](https://github.com/zed-industries/zed) editor and is not affiliated with
or endorsed by Zed Industries.

## Architect workspace

Instead of treating an agent as a one-turn command box, Praxis makes planning a
primary workspace. Work is drafted as a graph of explicit steps, constraints,
dependencies, conditions, loops, and expected hand-offs. Individual steps can
be discussed and refined before the locked plan is executed.

The alternate Code workspace retains the project tree, editor, terminal, Git
panel, and overall plan conversation. Plan mode limits the agent to reviewed
read capabilities, including shell-free Git and GitHub/GitLab pull-request
inspection, while Build mode enables approved mutation and execution tools.

The main product additions live in:

- `crates/architect` for the plan graph, compiler, persistence, and runner
- `crates/agent_ui/src/architect_ui/` for the Architect workspace
- `crates/agent` and `crates/agent_ui` for tools and execution integration

See [`docs/src/ai/architect-portfolio.md`](https://github.com/DushyantChetiwal/praxis/blob/Enhanced_Agents/docs/src/ai/architect-portfolio.md)
for the architecture and reproducible demo.

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

Product source lives on the [`Enhanced_Agents`](https://github.com/DushyantChetiwal/praxis/tree/Enhanced_Agents) branch. `main` carries the landing page and release workflows.


Praxis retains Zed's internal crate names and most source identifiers to keep
upstream merges reviewable. Platform builds and Rust validation run in GitHub
Actions rather than on contributor workstations.

The upstream platform build documentation remains applicable:

- [macOS](https://github.com/DushyantChetiwal/praxis/blob/Enhanced_Agents/docs/src/development/macos.md)
- [Linux](https://github.com/DushyantChetiwal/praxis/blob/Enhanced_Agents/docs/src/development/linux.md)
- [Windows](https://github.com/DushyantChetiwal/praxis/blob/Enhanced_Agents/docs/src/development/windows.md)

See [CONTRIBUTING.md](https://github.com/DushyantChetiwal/praxis/blob/Enhanced_Agents/CONTRIBUTING.md) before submitting changes.

## Licensing and attribution

Praxis is derived from Zed, which is developed by Zed Industries, Inc. Zed and
its associated marks belong to their respective owners. This repository keeps
the upstream copyright, license, and attribution notices.

The source is licensed primarily under GPL-3.0-or-later, with Apache-2.0
components where marked. Third-party dependency notices are generated with
[`cargo-about`](https://github.com/EmbarkStudios/cargo-about).
