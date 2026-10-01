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

Pointing a connection back at an earlier step forms a loop, which is how you express "go back and fix it if the tests fail". Loops are expected; just make sure something can leave the loop.

Architect mode can read and search your project but cannot change it through native tools. Drawing a plan and carrying it out are separate jobs.

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

A plan cannot run until every step is locked and the canvas reports no problems, such as a connection pointing at a step that does not exist, or a step nothing leads to.

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

When a step has several plain connections out of it, every branch runs at the same time, each step in a conversation of its own. The branches meet at their join: the step every branch can reach that the slowest branch reaches soonest. Each branch stops short of the join, and once all of them have finished the join runs once, told what every branch reported. Branches that never meet run to their ends, and the plan then carries on as if that level had run out of steps.

The branches share your working tree, so each parallel step is told which steps are running alongside it and asked to keep to its own work, leave alone what those steps own, and avoid commands that act on the whole repository, such as committing or formatting everything, unless its step needs them. If one branch fails, the others are stopped and the run ends with that failure.

Agents other than Zed's own run every step in the plan's conversation, so there the branches run one after another.

### Pausing and resuming

**Pause** starts no new steps and lets the steps already running finish, then waits. **Resume** carries on from exactly where the run paused.

A run that was stopped or that failed can also be resumed, including after an API-key or provider error. Fix the configuration and choose **Resume**: interrupted steps run again as new attempts, while completed branches and their summaries are kept. Failures stay in run status and the affected step conversation; they do not send a new prompt into the main conversation. **Run** always starts again from the beginning with a clean slate. Checkpoints are kept in memory only, so a run cannot be resumed after Praxis restarts.

To start part way through, choose **Run From Here** on a step's right-click menu, or the play button in its inspector. The run starts at that step, even inside a nested plan, and the steps before it are not run again: what they last reported is what the later steps are told.

### Stopping a run

**Stop** ends the run and cancels every turn it is waiting on. A run is also stopped automatically when a plan is looping without ever reaching an end:

- after 200 steps in total, counting every level and every branch, or
- after any single step has been entered 25 times, or
- if plans turn out to nest more than 5 deep.

When a limit is reached, the run status records why execution stopped. The main conversation can inspect that outcome without being interrupted by an automatically submitted prompt.

### Editing a plan mid-run

The main conversation has two coordinator tools:

- `inspect_architect_run` reads status, paginated visit history, and the conversation for a specific step visit, addressed by its full nested path and visit ID. It can inspect both active and finished visits without changing execution.
- `control_architect_run` can pause, interrupt, resume, change a step's model, or revise its goal, rules, and capture requirements. These execution changes require **Build** mode and the configured tool permission approval. Step agents cannot control sibling steps.

Pending step briefs and models can be changed during execution. Active work must be interrupted first, then revised and resumed. Approved brief revisions preserve locks and checkpoints; completed briefs cannot be rewritten. Model changes affect subsequent visits, never past conversations. Approval is rejected if the plan changes while permission is pending.

These tools let you ask the main conversation to monitor and steer the run while it is executing; they do not start an independent, continuously polling coordinator. Structural changes still require redrafting, which clears the checkpoint and requires a new run. Ordinary canvas edits are not a substitute for the coordinator's checkpoint-preserving revision operation.

## Related settings

Architect adds no settings of its own, and is not a profile: planning is a mode of an ordinary thread. The [loop guard](./agent-settings.md#loop-guard), which is separate from the run limits described above, is worth knowing about when running long plans.
