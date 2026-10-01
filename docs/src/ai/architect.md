# Architect

Architect is a mode for planning a task as a flowchart before any of it is carried out, and then running that flowchart one step at a time.

The difference from simply asking an agent to "make a plan" is where the plan lives. A plan written into a chat message is a suggestion the model may drift away from. An Architect plan is a graph the agent is driven through: Zed holds the position in it, tells the agent about one step at a time, and decides every branch itself.

## Plan, Build and Architect modes

The agent works in one of three modes, shown next to the message editor:

| Mode                | Can change your project | Purpose                                           |
| ------------------- | ----------------------- | ------------------------------------------------- |
| **Build** (default) | Yes                     | Carrying work out.                                |
| **Plan**            | **No**                  | Working out what to do, then asking to go ahead.  |
| **Architect**       | **No**                  | Drawing a plan on the canvas to run step by step. |

**Plan** works like Zed's Plan mode. The agent reads and researches, then presents its plan for approval. Choosing **Start Building** switches the conversation to Build so it can carry the plan out; **Keep Planning** keeps it in Plan to refine it.

You do not have to switch to Architect yourself. When the agent decides a task is worth drawing as a flowchart, it draws a plan, and drawing a plan is what puts the thread into Architect mode. Pressing **Run** puts it back into Build.

In Plan and Architect modes only tools reviewed as local or external reads are available, and Architect adds conversation-owned plan updates. Architect's tools may draft, refine, and report steps, but cannot change project files or external systems. Project mutation, external mutation, terminal execution, subagents, sibling threads, and MCP tools without an explicit read-only annotation are withheld rather than merely discouraged. Shell-free Git and GitHub/GitLab pull-request tools remain available for research. The selected mode is saved with the thread, so reopening a plan does not silently restore Build tools. You can switch modes by hand at any time from the mode selector.

## Drafting a plan

Describe what you want done. If the task warrants it, the agent calls the `draft_plan` tool, which draws the plan on a canvas rather than writing it out as prose. You can also ask for a plan directly.

Open the canvas with the branch icon in the Agent Panel toolbar, or with the `agent: open architect` action. It opens over the workspace rather than in a tab, because a plan belongs to a conversation rather than to your project.

Each **step** is one meaningful unit of work and carries:

| Field       | Meaning                                                                         |
| ----------- | ------------------------------------------------------------------------------- |
| **Title**   | A short name for the step.                                                      |
| **Goal**    | What "done" means for this step.                                                |
| **Rules**   | Constraints that hold however the step is carried out.                          |
| **Capture** | What this step's summary must contain. See [Handing work on](#handing-work-on). |

Each **connection** says when one step leads to another:

| Condition         | When to use it                                                                                                                        |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| _(none)_          | The step simply follows.                                                                                                              |
| **Objective**     | An observable statement such as whether a command succeeded. The built-in runner asks the model to evaluate it from the step summary. |
| **LLM-evaluated** | The decision genuinely needs judgement, phrased as a yes-or-no question.                                                              |

Pointing a connection back at an earlier step forms a loop, which is how you express "go back and fix it if the tests fail". A node can also loop into itself (`from == to`). Use a condition or a positive `max_repeats` and a way out: `max_repeats: 3` allows three edge traversals after the initial visit, not three total visits. Unlimited unconditional loops with no exit are blocked; run safety limits still apply.

Architect mode can read and search your project but cannot change it through native tools. Drawing a plan and carrying it out are separate jobs.

## Existing-file surfaces

Every drafted step must declare `file_surface`: the existing files it anticipates working on, using exact worktree-qualified paths such as `project/src/main.rs`. Include the declaration on parent steps and every nested child. Send explicit `[]` when no existing files are anticipated. Omission is not the same as an empty declaration: older saved steps with no surface remain visible but block execution until reviewed.

Use file paths, not directories or globs. Use `/` separators without absolute paths, `..`, empty or `.` components, or duplicate entries. Keep the file's actual case; concurrency comparisons use normalized, case-insensitive identities. These identities are lexical, not filesystem or symlink resolution. A parent step's effective surface includes its own declaration and every descendant's declaration.

Steps that may run concurrently must have disjoint effective surfaces. If two branches need the same existing file, serialize that work with connections or split it into genuinely disjoint work. The shared join may use those files after the branches finish.

**A surface is planning information, not a write allowlist.** It does not restrict tools from writing other files or creating new ones; ordinary tool permissions still apply. Do not omit anticipated existing files simply to suppress an overlap error.

The native runtime also observes new files and adds them to reachable successors' declarations and containing scopes, including nested successors. Automatic discovery excludes ignored files unless always included; explicitly declared ignored paths are still checked. If this exposes a conflict, further dispatch stops with an actionable error and completed results remain. Inspect, preview a targeted repair, review/relock, then resume; this is a scheduling guard, not write enforcement.

The canvas shows each declaration; its tooltip and the step's **Details** inspector show nested effective surfaces and actionable problems. Invalid or conflicting drafts stay on the canvas for correction, but cannot run. Ask the step chat to update `refine_step.file_surface` (omission preserves the saved declaration; `[]` explicitly clears it; `null` is rejected), or ask the plan chat to preview `edit_architect_plan` with `{"kind":"set_file_surface","path":["step"],"file_surface":["project/src/main.rs"]}`. Surface edits follow existing lock, approval, and checkpoint guards; they are not brief-only `revise_step` updates. Review and relock affected steps before execution.

## Steps inside steps

A step can contain a plan of its own, for work that is one step at this level but several once you look closely. Running such a step runs the plan inside it, and the step is done when that plan is.

- **Double-click a step** on the canvas to open its plan, or use **Break Into Steps** in the inspector. If the step has no plan yet, an empty one is created.
- A breadcrumb along the top names the trail back out. **Escape** backs out one layer at a time: first the selection, then the plan you are looking at, then the canvas itself.
- **Locking** a step that contains a plan locks every step inside it too. Unlocking it reopens only that step.
- Plans nested more than **5** deep are refused rather than run.

## Handing work on {#handing-work-on}

Steps are carried out one at a time, and a step is never shown what happened in the steps before it. It is shown their **summaries** and nothing else.

Each step's **Capture** field says what its summary must contain. It is the contract between that step and everything downstream of it, so name the specifics a later step will need — the file that changed, the command that ran, the exact error output — rather than "what happened".

When a step finishes, the agent reports through the `complete_step` tool. That summary is then shown to:

- the steps this one leads to, and
- **the step itself**, if a loop brings it round again — under "You have been here before", so it does not try the same fix twice.

A step that leads nowhere hands nothing on and needs no capture. A step that _does_ lead somewhere without one is flagged, since it is usually an oversight.

Use the **★** beside Capture to **pin** a step. A pinned step's summary is shown to every later step, not just the ones it leads to directly — for a decision taken early that everything after it depends on.

## Refining a step

Every step can be argued about in a conversation of its own. Select a step and open the **Chat** tab in the inspector.

That conversation appears **beside the step, on the canvas**. The Agent Panel stays on the conversation that owns the plan, so arguing about one step never costs you your place in the main one.

The step's thread starts knowing what the main conversation knows, then diverges, so settling one step does not crowd out the context the next step will be settled in. It can read the project and rewrite its own step through the `refine_step` tool, but cannot change the project or any other step.

## Per-step models

In a step's **Details** inspector, choose **Execution model** before locking it. **Inherit plan model** uses the main conversation's model; an override stores both the provider and model ID. The main agent can include the same selection in `draft_plan`, and a step's refinement chat can change it before locking. Existing plans without overrides continue to inherit the plan model.

An unavailable model fails that step with an actionable error rather than silently choosing a different provider. Correct its model or provider configuration, then resume. Explicit step models require the native Praxis Agent.

## Locking

A step you are satisfied with should be **locked**. Locking makes it read-only and is your signal that deliberation on it is over. The agent will not lock or unlock a step on its own initiative.

You can also ask for it in the main conversation, such as "lock the schema step", "lock everything", or "reopen the tests step so we can change it". The agent changes the locks with the `set_step_locks` tool, the same way the lock button does: locking a step that contains a plan locks everything inside it, and unlocking reopens only the step named. Locks cannot change while a run is in progress or paused.

A plan cannot run until every step is locked and the canvas reports no problems, such as a connection pointing at a step that does not exist, a step nothing leads to, or a missing, invalid, or overlapping parallel file surface.

## Running a plan

Press **Run**. The plan is carried out in the conversation that owns it, and the canvas highlights every step currently in progress.

The run proceeds as follows:

1. The step's goal, rules and capture are sent to the agent, along with the summaries of the steps feeding into it. The agent is not shown later steps, because a model that can see step 4 tends to start on it while it is still meant to be doing step 3.
2. If the step contains a plan of its own, the run descends into it and works through it before the step counts as done.
3. When the turn finishes, each conditional connection out of that step is put to the agent as a yes-or-no question on its own. The answer is read back by Zed, not acted on by the model.
4. The first condition answered `YES` decides where the run goes next. If every condition is answered `NO`, the run takes the plain unconditional connections out of the step: one is simply followed, and several run as [parallel branches](#parallel-branches). If there is none, the plan is complete. A plain connection that loops back with a repeat limit is taken on its own until its repeats are spent.

Because Zed holds the position in the graph, a model cannot quietly decide it has done enough and leave a retry loop early.

Every step runs as an ordinary turn, so tool permissions, sandboxing, and cancellation all behave exactly as they do when you type a message yourself.

### Parallel branches {#parallel-branches}

Independent branches of a structured fork run at the same time, each step in a conversation of its own. Each branch stops before the shared join, which runs once after the branches finish.

When multiple roots or partially overlapping branches feed the same downstream work, the runner uses prerequisite tracking instead. These dependency regions execute ready steps serially: every incoming prerequisite must finish or be explicitly skipped by routing before a successor starts. A step with no selected incoming route is skipped, not executed merely because it is a nominal join. Unsupported cyclic overlaps stop with an explanation rather than executing out of order. `inspect_architect_run` with `readiness: true` reports readiness, routing selections, and waiting or skipped reasons.

The branches share your working tree, so each parallel step is told which steps are running alongside it and asked to keep to its own work, leave alone what those steps own, and avoid commands that act on the whole repository, such as committing or formatting everything, unless its step needs them. If one branch fails, the others are stopped and the run ends with that failure.

Agents other than Zed's own run every step in the plan's conversation, so there the branches run one after another.

### Pausing and resuming

**Pause** starts no new steps and lets the steps already running finish, then waits. **Resume** carries on from exactly where the run paused.

A run that was stopped or that failed can also be resumed, including after an API-key or provider error. Fix the configuration and choose **Resume**: interrupted steps run again as new attempts, while completed branches and their summaries are kept. Failures stay in run status and the affected step conversation; they do not send a new prompt into the main conversation. **Run** always starts again from the beginning with a clean slate. Run checkpoints, events, visit history, and step-conversation links are saved with the thread. After a restart, interrupted execution stays stopped until you explicitly resume. In-flight work may need to be retried; this is not a transaction guaranteeing exactly-once filesystem or external-service effects. If a checkpoint is incompatible with the saved plan, history remains available but resume is refused.

To start part way through, choose **Run From Here** on a step's right-click menu, or the play button in its inspector. The run starts at that step, even inside a nested plan, and the steps before it are not run again: what they last reported is what the later steps are told.

### Stopping a run

**Stop** ends the run and cancels every turn it is waiting on. A run is also stopped automatically when a plan is looping without ever reaching an end:

- after 200 steps in total, counting every level and every branch, or
- after any single step has been entered 25 times, or
- if plans turn out to nest more than 5 deep.

When a limit is reached, the run status records why execution stopped. The main conversation can inspect that outcome without being interrupted by an automatically submitted prompt.

### Editing a plan mid-run

The main conversation has these coordinator tools:

- `inspect_architect_plan` reads the authoritative graph as paginated graph, node, and edge records, including full nested paths, entry points, model overrides, locks, positions, declared and effective file surfaces, surface errors, and execution readiness. Supply the returned revision on later pages to reject mixed-revision reads.
- `inspect_architect_run` reads current or archived runs, paginated visits, saved step conversations, dependency readiness, events, and active tool calls. Visit IDs and full paths disambiguate repeated or nested steps.
- `control_architect_run` can pause, interrupt, resume, change models, or revise pending briefs. `resume_at` selects an unfinished, eligible step from an inactive checkpoint without replaying completed results; unresolved prerequisites are refused. `set_step_models` applies a validated batch atomically without redrawing the graph. Copy each native model's `{provider, model}` object directly from `list_agents_and_models`'s `configuration` field.
- `edit_architect_plan` previews targeted graph edits and applies only the returned preview token after permission approval. Operations insert/remove nodes and edges, reconnect edges, move nodes on the canvas, or replace file surfaces with `set_file_surface`; moves do not reparent nodes.
- `wait_architect_run` subscribes to execution transitions or user-input waits for up to 30 seconds. It returns immediately for available events, failures or stopped execution, or pending user input. Follow its event cursor rather than repeatedly polling. This is an on-demand observer, not a new agent started automatically in the background.

Execution controls require **Build** mode and configured tool permission approval. Topology editing is also available in **Architect** mode but cannot start execution or approve locks. Step agents cannot control sibling steps.

Pending step briefs and models can be changed during execution. Active work must be interrupted first, then revised and resumed. Approved brief revisions preserve locks and checkpoints; completed briefs cannot be rewritten. Model changes affect subsequent visits, never past conversations. Approval is rejected if the plan changes while permission is pending.

### Previewing topology edits

Preview reports changed paths, affected locks, invalidated results/checkpoints, retained results, routing consequences, validation problems, and whether the runtime can safely preserve its checkpoint. Nothing changes during preview. Tokens expire after five minutes, are single-use, and are rejected if the graph or run changes before approval.

Stop execution before applying a topology edit. Execution-affecting changes reopen impacted locks for review and invalidate downstream consumers, including consumers of pinned summaries. Unaffected positions, results, run history, and transcript links remain. Relock reviewed steps before resuming; apply never silently approves them.

Checkpoint rebasing currently supports flat acyclic topology updates and topology-preserving edits that do not invalidate finished nested work. Unsupported nested/cyclic rebases or uncertain retained branch verdicts are refused before mutation. The preview explains when a separate restart is required. Full `draft_plan` replacement refuses active or resumable runs. A changed replacement preserves positions for matching paths and archives the old run, but is not a checkpoint-preserving recovery edit; archived history does not make the replacement resumable from the old checkpoint.

### History and liveness

Completed run histories and child-transcript links remain inspectable across revisions. The event journal keeps the latest 1,024 events and reports cursor gaps. At most 64 full draft revision snapshots are retained; layout movement is coalesced instead of storing a whole graph for every mouse move. Evicting old draft snapshots does not remove run histories or transcripts.

Active-call inspection reports locally observed start and last-update times, status, known wait reasons, and cancellation requests. These are not backend heartbeats: a quiet tool may still be working, and a cancellation request does not prove that a backend process has stopped. Timestamps reconstructed during replay are not original execution times.

## Where the model's tool descriptions live

The model-facing instructions are the Rust `///` comments on tool input types; field comments become parameter descriptions in JSON Schema:

- [`draft_plan_tool.rs`](../../../crates/agent/src/tools/draft_plan_tool.rs): whole-plan creation/replacement, merge rules, and routing examples.
- [`refine_step_tool.rs`](../../../crates/agent/src/tools/refine_step_tool.rs): updates bound to one draft step.
- [`architect_plan_tool.rs`](../../../crates/agent/src/tools/architect_plan_tool.rs): authoritative inspection and targeted preview/apply operations.
- [`architect_run_tool.rs`](../../../crates/agent/src/tools/architect_run_tool.rs): execution inspection and approved brief/model/recovery controls.

[`thread.rs`](../../../crates/agent/src/thread.rs) turns these into model requests: `AgentTool::description()` reads the input type's schema description, `AgentTool::input_schema()` generates its parameter schema, and `Thread::build_completion_request()` includes available tools. Contract tests in the tool files check the descriptions and normalized parameter schemas, including parseable JSON examples, rather than maintaining a second copy here.

## Related settings

Architect adds no settings of its own, and is not a profile: planning is a mode of an ordinary thread. The [loop guard](./agent-settings.md#loop-guard), which is separate from the run limits described above, is worth knowing about when running long plans.
