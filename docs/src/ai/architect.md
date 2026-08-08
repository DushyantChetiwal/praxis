# Architect

Architect is a mode for planning a task as a flowchart before any of it is carried out, and then running that flowchart one step at a time.

The difference from simply asking an agent to "make a plan" is where the plan lives. A plan written into a chat message is a suggestion the model may drift away from. An Architect plan is a graph the agent is driven through: Zed holds the position in it, tells the agent about one step at a time, and decides every branch itself.

## Plan and Build modes

The agent works in one of two modes, shown next to the message editor:

| Mode                | Can change your project | Purpose                 |
| ------------------- | ----------------------- | ----------------------- |
| **Build** (default) | Yes                     | Carrying work out.      |
| **Plan**            | **No**                  | Working out what to do. |

You do not have to switch modes yourself. When the agent decides a task is worth planning, it draws a plan, and drawing a plan is what puts the thread into Plan mode. Pressing **Run** puts it back into Build.

In Plan mode the tools that change your project — `edit_file`, `write_file`, `terminal`, `delete_path` and the rest — are withheld rather than merely discouraged, so a model cannot start building halfway through drafting. You can switch modes by hand at any time from the mode selector.

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

| Condition         | When to use it                                                                |
| ----------------- | ----------------------------------------------------------------------------- |
| _(none)_          | The step simply follows.                                                      |
| **Deterministic** | The answer can be checked without judgement, such as a command's exit status. |
| **LLM-evaluated** | The decision genuinely needs judgement, phrased as a yes-or-no question.      |

Pointing a connection back at an earlier step forms a loop, which is how you express "go back and fix it if the tests fail". Loops are expected; just make sure something can leave the loop.

The Architect profile can read and search your project but cannot change it. Drawing a plan and carrying it out are separate jobs.

## Steps inside steps

A step can contain a plan of its own, for work that is one step at this level but several once you look closely. Running such a step runs the plan inside it, and the step is done when that plan is.

- **Double-click a step** on the canvas to open its plan, or use **Break Into Steps** in the inspector. If the step has no plan yet, an empty one is created.
- A breadcrumb along the top names the trail back out. **Escape** backs out one layer at a time: first the selection, then the plan you are looking at, then the canvas itself.
- A step containing a plan **cannot be locked** until every step inside it is locked.
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

## Locking

A step you are satisfied with should be **locked**. Locking makes it read-only and is your signal that deliberation on it is over. The agent will not lock a step for you.

A plan cannot run until every step is locked and the canvas reports no problems, such as a connection pointing at a step that does not exist, or a step nothing leads to.

## Running a plan

Press **Run**. The plan is carried out in the conversation that owns it, and the canvas highlights the step currently in progress.

The run proceeds as follows:

1. The step's goal, rules and capture are sent to the agent, along with the summaries of the steps feeding into it. The agent is not shown later steps, because a model that can see step 4 tends to start on it while it is still meant to be doing step 3.
2. If the step contains a plan of its own, the run descends into it and works through it before the step counts as done.
3. When the turn finishes, each conditional connection out of that step is put to the agent as a yes-or-no question on its own. The answer is read back by Zed, not acted on by the model.
4. The first condition answered `YES` decides where the run goes next. If every condition is answered `NO`, the run takes the plain unconditional connection out of the step, if there is one. If there is none, the plan is complete.

Because Zed holds the position in the graph, a model cannot quietly decide it has done enough and leave a retry loop early.

Every step runs as an ordinary turn, so tool permissions, sandboxing, and cancellation all behave exactly as they do when you type a message yourself.

### Stopping a run

**Stop** ends the run and cancels the turn it is waiting on. A run is also stopped automatically when a plan is looping without ever reaching an end:

- after 200 steps in total, counting every level, or
- after any single step has been entered 25 times, or
- if plans turn out to nest more than 5 deep.

When that happens the agent is told which step kept repeating and asked to explain what would have to change for it to finish, rather than being left to carry on.

### Editing a plan mid-run

A run carries out the plan as it was when you pressed **Run**. Editing the canvas or a step's chat while a run is in flight does not change the run already in progress, so that it is always possible to say afterwards what was actually carried out. Stop the run and start it again to pick up your changes.

## Related settings

Architect adds no settings of its own, and is not a profile: planning is a mode of an ordinary thread. The [loop guard](./agent-settings.md#loop-guard), which is separate from the run limits described above, is worth knowing about when running long plans.
