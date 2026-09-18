# Architect Architecture and Demo

This guide explains how Architect is put together, what its controls do and do
not guarantee, how the fork validates it, and how to demonstrate it from a
clean repository.

## Design {#design}

Architect separates planning state from chat prose. The model proposes a graph,
the application owns that graph, and the runner exposes one step at a time.

```text
user request
    -> native agent thread and Plan/Build mode
    -> Architect graph (steps, connections, locks, nested plans)
    -> canvas and per-step refinement chats
    -> bounded application-owned PlanRun state machine
    -> one Build-mode turn per step
    -> captured summary and application-selected next connection
```

The main responsibilities are split across three packages:

- `crates/architect` contains the UI-independent graph, validation, layout,
  compilation, branch evaluation, nesting limits, and run limits.
- `crates/agent` connects the graph to native agent threads. It owns session
  mode, filters built-in tools, builds step prompts, records summaries, and
  advances the run.
- `crates/agent_ui` renders the canvas, inspector, step chat, controls, and run
  status inside Zed.

A step carries a goal, rules, a capture contract, lock state, and optional
nested plan. A connection is unconditional or model-mediated. The legacy
`deterministic` variant records an objective fact, but the built-in runner still
asks the model to evaluate that fact from the completed step summary; it does
not execute a free-form expression. The runner, not the model, applies the
verdict and chooses the next connection.

## Capability boundary {#capability-boundary}

Every native tool declares one capability:

- `ReadOnly` reads local project or conversation state.
- `ConversationMutation` changes only conversation-owned planning state.
- `ExternalRead` performs a non-mutating external lookup.
- `ProjectMutation` changes project state.
- `ExternalMutation` may change an external system.
- `ArbitraryExecution` runs commands or delegates unrestricted execution.

Plan mode permits `ReadOnly`, `ConversationMutation`, and `ExternalRead`. The
default for a newly added tool is `ProjectMutation`, so an unreviewed tool fails
closed. This is
enforced when the enabled tool set is built, again when each completion request
is serialized, and again at invocation time. A model cannot invoke a terminal
from an earlier tool snapshot after the thread changes from Build to Plan.

The built-in research surface includes file and symbol reads, diagnostics,
fetch, web search, and shell-free Git tools for status, diffs, branches, remotes,
and commit inspection. `pull_request` performs strict HTTPS GET-only inspection
of GitHub pull requests and GitLab merge requests, including optional files,
conversation, reviews, and checks. Hosted review reads use bounded pagination,
a shared response budget, cancellable requests, and preserve the summary when an
optional section fails. Terminal, sibling threads, and subagents are
classified as `ArbitraryExecution` and are unavailable in Plan mode.

MCP tools are admitted only when their annotation explicitly sets
`readOnlyHint: true`. An open-world read is classified as `ExternalRead`; a
local read is `ReadOnly`. Missing or false annotations are classified as
`ExternalMutation` and withheld. This is deliberately conservative because a
tool name or description is not a security boundary.

The effective set can still be narrower. Provider support, the active tool
profile, feature flags, restricted-project rules, network grants, and sandbox
configuration are applied in addition to Plan mode. Plan-state tools such as
`draft_plan` and `refine_step` may update the conversation's proposed plan, but
cannot change project files or external systems. External ACP agents advertise
and enforce their own modes and capabilities.

This boundary prevents native agent tools from mutating the project during Plan
mode. It does not freeze the filesystem: the user, another process, or another
thread can still change files while a plan is being drafted.

## Tradeoffs {#tradeoffs}

- **Application-owned control flow:** A graph is inspectable and the runner can
  enforce its current step. The cost is more state and validation than a prose
  checklist.
- **One step of context at a time:** The model is less likely to work ahead. The
  capture field must carry enough information for downstream steps.
- **Explicit locks:** A plan cannot run until deliberation is complete. This adds
  friction, but preserves reviewed steps when the graph is redrafted.
- **Conditional loops:** Retry behavior is visible and bounded. Branch decisions
  that require judgment still depend on model output.
- **Fail-closed Plan tools:** Repository-aware plans remain useful through
  structured Git and hosting reads. Arbitrary shell execution is deferred until
  Build mode, and unannotated MCP tools are excluded rather than guessed safe.

## GitHub-only validation {#github-only-validation}

`.github/workflows/architect_quality.yml` runs quality checks on GitHub-hosted
Ubuntu runners. It uses no Zed private runner labels or private service secrets.
Rust dependencies and build artifacts are cached, and concurrency cancels a
superseded run for the same branch or pull request.

The workflow checks:

1. Rust formatting and the repository's Prettier checks.
2. The full `architect` package unit-test suite.
3. Capability, MCP annotation, structured Git/PR, and nested-refinement tests.
4. Targeted native-agent tests for mode selection and Plan-mode filtering.
5. Targeted `agent_ui` tests under `architect_ui`.
6. Clippy for `architect`, `agent`, and `agent_ui`, including their targets and
   features through `script/clippy`.

A green Actions run is the validation record. The workflow's presence alone is
not evidence that a revision passed.

The workflow is maintained on the default `main` branch under the fork's
workflow-only policy. Product code remains on `Enhanced_Agents`. Push and pull
request runs validate the event revision; a manual dispatch defaults to the
head of `Enhanced_Agents` and accepts an explicit source ref for a historical
or diagnostic run.

> **Note:** GitHub evaluates push workflows from the pushed branch and manual
> workflows from the default branch. Keep the workflow file synchronized between
> the workflow-only `main` branch and `Enhanced_Agents` without merging product
> source into `main`.

## Reproducible demo {#reproducible-demo}

Use a disposable repository with at least one committed file and one uncommitted
change. Install the latest unsigned Zed Dev build from this fork's GitHub
release; no local Zed compilation is required.

1. Open the repository and start a native-agent thread in **Plan** mode.
2. Ask the agent to inspect the working tree and draft a plan for the pending
   change. If a public GitHub PR or GitLab MR is relevant, include its browser
   URL and ask for its files and checks.
3. Confirm that the model can call `git_status`, `git_diff`, `git_branches`,
   `git_remotes`, `git_show`, and `pull_request`, while `terminal`, `edit_file`,
   `spawn_agent`, and `create_thread` are absent from its offered tools.
4. Ask for a plan with two disconnected root components. The run preview and
   unit tests demonstrate that both components execute in graph order rather
   than silently dropping the second root.
5. Break one step into a nested plan. Reuse the same child id under two different
   parent steps, open the second child's chat, and refine it. Only the child at
   that complete path changes.
6. Lock the containing step before its nested steps. Architect refuses the lock.
   Lock the children first, then the parent. Attempting to edit a locked child,
   its outgoing route, or its containing plan is refused.
7. Run the plan. The thread switches to Build mode, exposes one leaf step at a
   time, records each `complete_step` summary, and passes the composed nested-plan
   handoff to the successor. A disconnected second root begins only after the
   first component ends.

For the automated evidence, dispatch **Architect quality** from the Actions tab
with `source_ref` set to the commit being demonstrated. The run is expected to
show separate formatting, graph-test, agent/canvas-test, and Clippy jobs. Link
the immutable Actions run in a portfolio or review; do not describe an unrun
workflow as passing validation.

## Maintenance and distribution {#maintenance-and-distribution}

The fork keeps product source on `Enhanced_Agents` and workflow definitions on
the workflow-only default branch. `sync_upstream.yml` runs weekly and can also be
dispatched manually. It fetches `zed-industries/zed`, merges `upstream/main`,
runs package checks on GitHub, pushes only a successful merge, and dispatches
`bundle_fork.yml` when a rebuild is requested.

`bundle_fork.yml` builds Windows and macOS installers on GitHub-hosted runners,
publishes unsigned assets, and writes per-platform manifests containing the
version, download URL, and SHA-256 digest. The manifest base URL is compiled into
the Dev build. That opt-in makes the normally non-updating Dev channel poll the
fork's latest GitHub release and use Zed's existing verified update path.

Upstream merge conflicts still require review. Automation should stop on a
conflict rather than choosing a resolution that could erase either upstream
behavior or the Architect changes.

## Current limitations {#current-limitations}

- Git status comes from Zed's repository snapshot, so it reflects the latest
  completed background scan rather than forcing a new scan.
- `git_show` returns commit metadata and changed-file names; it does not recreate
  an arbitrary commit patch. `git_diff` covers the working tree, staged index,
  and merge-base comparisons.
- Hosted review inspection currently accepts public `github.com` and
  `gitlab.com` URLs. Private repositories require `GITHUB_TOKEN` or
  `GITLAB_TOKEN` in Zed's environment; self-hosted forges are rejected rather
  than guessed.
- MCP read-only annotations are declarations made by the server. Plan mode
  enforces the declaration boundary but cannot prove that a dishonest server
  implemented its tool without side effects.
- `deterministic` conditions are model-mediated in the built-in runner until a
  typed evaluator exists. The documentation and API expose that behavior
  explicitly instead of implying a free-form string is safely executable.
- Fork releases are unsigned, so operating-system trust prompts are expected.
