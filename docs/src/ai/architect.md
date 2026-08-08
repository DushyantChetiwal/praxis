# Architect

Architect is a mode for planning a task as a flowchart before any of it is carried out, and then running that flowchart one step at a time.

The difference from simply asking an agent to "make a plan" is where the plan lives. A plan written into a chat message is a suggestion the model may drift away from. An Architect plan is a graph the agent is driven through: Zed holds the position in it, tells the agent about one step at a time, and decides every branch itself.

## Drafting a plan

Select the **Architect** profile in the Agent Panel and describe what you want done. The agent replies by calling the `draft_plan` tool, which draws the plan on a canvas rather than writing it out as prose.

Open the canvas with the branch icon in the Agent Panel toolbar, or with the `agent: open architect` action.

Each **step** is one meaningful unit of work and carries:

| Field     | Meaning                                                       |
| --------- | ------------------------------------------------------------- |
| **Title** | A short name for the step.                                    |
| **Goal**  | What "done" means for this step.                              |
| **Rules** | Constraints that hold however the step is carried out.        |

Each **connection** says when one step leads to another:

| Condition        | When to use it                                                                     |
| ---------------- | ---------------------------------------------------------------------------------- |
| _(none)_         | The step simply follows.                                                            |
| **Deterministic** | The answer can be checked without judgement, such as a command's exit status.      |
| **LLM-evaluated** | The decision genuinely needs judgement, phrased as a yes-or-no question.           |

Pointing a connection back at an earlier step forms a loop, which is how you express "go back and fix it if the tests fail". Loops are expected; just make sure something can leave the loop.

The Architect profile can read and search your project but cannot change it. Drawing a plan and carrying it out are separate jobs.

## Refining a step

Every step can be argued about in a conversation of its own. Select a step and choose **Discuss** to open a thread dedicated to it.

That thread starts knowing what the main conversation knows, then diverges, so settling one step does not crowd out the context the next step will be settled in. It runs under the **Architect Step** profile, which can read the project and rewrite its own step through the `refine_step` tool, but cannot change the project or any other step.

## Locking

A step you are satisfied with should be **locked**. Locking makes it read-only and is your signal that deliberation on it is over. The agent will not lock a step for you.

A plan cannot run until every step is locked and the canvas reports no problems, such as a connection pointing at a step that does not exist, or a step nothing leads to.

## Running a plan

Press **Run**. The plan is carried out in the conversation that owns it, and the canvas highlights the step currently in progress.

The run proceeds as follows:

1. The step's goal and rules are sent to the agent, and only that step's. The agent is not shown later steps, because a model that can see step 4 tends to start on it while it is still meant to be doing step 3.
2. When the turn finishes, each conditional connection out of that step is put to the agent as a yes-or-no question on its own. The answer is read back by Zed, not acted on by the model.
3. The first condition answered `YES` decides where the run goes next. If every condition is answered `NO`, the run takes the plain unconditional connection out of the step, if there is one. If there is none, the plan is complete.

Because Zed holds the position in the graph, a model cannot quietly decide it has done enough and leave a retry loop early.

Every step runs as an ordinary turn, so tool permissions, sandboxing, and cancellation all behave exactly as they do when you type a message yourself.

### Stopping a run

**Stop** ends the run and cancels the turn it is waiting on. A run is also stopped automatically when a plan is looping without ever reaching an end:

- after 200 steps in total, or
- after any single step has been entered 25 times.

When that happens the agent is told which step kept repeating and asked to explain what would have to change for it to finish, rather than being left to carry on.

### Editing a plan mid-run

A run carries out the plan as it was when you pressed **Run**. Editing the canvas or a step's chat while a run is in flight does not change the run already in progress, so that it is always possible to say afterwards what was actually carried out. Stop the run and start it again to pick up your changes.

## Related settings

Architect adds no settings of its own. The [loop guard](./agent-settings.md#loop-guard), which is separate from the run limits described above, is worth knowing about when running long plans.
