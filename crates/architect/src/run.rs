//! Driving a finished plan one step at a time, through nested plans and loops.
//!
//! Compiling a plan into a spec (see [`crate::compile_spec`]) hands the whole
//! graph to the model at once and trusts it to follow the control flow. That
//! trust is the weak point: a model reading "go back to step 2 if the tests
//! fail" may quietly decide it has done enough. This module keeps the control
//! flow out of the model's hands. The run holds the position in the graph, the
//! model is told about one step at a time, and every branch is a question put
//! to it in isolation with the answer read back here.
//!
//! A step that contains a plan of its own is not carried out directly: the run
//! descends into it, and the step is finished when its plan is. Which is why
//! position is a stack of frames rather than a single node.
//!
//! Nothing here talks to a model or a UI. It is a state machine: the caller asks
//! what to do next, does it, and reports what happened. That keeps the part that
//! is easy to get wrong testable without a model in the loop.

use crate::{ArchitectGraph, ArchitectNode, EdgeCondition, EdgeId, GraphProblem, NodeId, NodePath};
use collections::HashMap;
use std::fmt::{self, Display, Write};

/// How many steps a run may take before it is stopped, counted across every
/// level. A plan with loops has no natural length, so the run needs a ceiling.
pub const MAX_RUN_STEPS: usize = 200;

/// How many times a single step may be entered before the run is stopped. A
/// tighter bound on the failure that actually happens: a retry loop whose exit
/// condition is never satisfied.
pub const MAX_NODE_VISITS: usize = 25;

/// How deeply plans may nest before a run refuses to descend further.
pub const MAX_PLAN_DEPTH: usize = 5;

/// Why a run would not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunRefusal {
    NothingToRun,
    NotReady(Vec<GraphProblem>),
}

impl Display for RunRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunRefusal::NothingToRun => write!(formatter, "there are no steps to run"),
            RunRefusal::NotReady(problems) => {
                let described: Vec<String> =
                    problems.iter().map(|problem| problem.to_string()).collect();
                write!(formatter, "{}", described.join("; "))
            }
        }
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    StepLimit { steps: usize },
    NodeLimit { node: NodeId, visits: usize },
    DepthLimit { node: NodeId },
    Cancelled,
}

impl RunOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, RunOutcome::Completed)
    }

    /// A sentence describing the end of the run, for the transcript.
    pub fn describe(&self, graph: &ArchitectGraph) -> String {
        let title = |id: &NodeId| {
            find_title(graph, id).unwrap_or_else(|| id.0.clone())
        };
        match self {
            RunOutcome::Completed => "The plan is complete; every step has been carried out.".into(),
            RunOutcome::StepLimit { steps } => format!(
                "The run was stopped after {steps} steps, which means the plan is looping without \
                 ever reaching an end. Do not carry on; say which step kept repeating and what \
                 would have to change for it to finish."
            ),
            RunOutcome::NodeLimit { node, visits } => format!(
                "The run was stopped because \"{}\" was entered {visits} times without its exit \
                 condition ever being met. Do not carry on; say what kept failing and what would \
                 have to change for it to pass.",
                title(node)
            ),
            RunOutcome::DepthLimit { node } => format!(
                "The run was stopped because plans nested more than {MAX_PLAN_DEPTH} deep at \
                 \"{}\". Flatten that part of the plan before running it again.",
                title(node)
            ),
            RunOutcome::Cancelled => "The run was stopped.".into(),
        }
    }
}

fn find_title(graph: &ArchitectGraph, id: &NodeId) -> Option<String> {
    if let Some(node) = graph.node(id) {
        return Some(node.title.clone());
    }
    graph
        .nodes
        .iter()
        .filter_map(|node| node.subplan())
        .find_map(|subplan| find_title(subplan, id))
}

/// One way out of the step the run is sitting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub edge: EdgeId,
    pub to: NodeId,
    pub condition: EdgeCondition,
}

/// What the caller should do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Carry out this step, then report it with [`PlanRun::finish_step`].
    Run(NodePath),
    /// Put this question to the agent, then report the answer with
    /// [`PlanRun::answer`].
    Ask(Branch),
    /// The run is over.
    Done(RunOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Working,
    Deciding {
        remaining: Vec<Branch>,
        fallback: Option<NodeId>,
    },
}

/// A position within one plan. A run holds a stack of these, one per level.
#[derive(Debug)]
struct Frame {
    /// The steps walked to reach this plan; empty for the top-level plan.
    parents: Vec<NodeId>,
    current: NodeId,
    phase: Phase,
    visits: HashMap<NodeId, usize>,
}

impl Frame {
    fn path(&self) -> NodePath {
        let mut ids = self.parents.clone();
        ids.push(self.current.clone());
        NodePath(ids)
    }

    fn graph_path(&self) -> NodePath {
        NodePath(self.parents.clone())
    }
}

/// A position in a plan, and the history of how it got there.
#[derive(Debug)]
pub struct PlanRun {
    stack: Vec<Frame>,
    /// Every step entered, at any depth, in order and including repeats.
    history: Vec<NodePath>,
    outcome: Option<RunOutcome>,
}

impl PlanRun {
    /// Starts a run at the plan's entry point, or explains why it cannot.
    pub fn start(graph: &ArchitectGraph) -> Result<Self, RunRefusal> {
        if graph.is_empty() {
            return Err(RunRefusal::NothingToRun);
        }
        let problems = graph.blocking_problems();
        if !problems.is_empty() {
            return Err(RunRefusal::NotReady(problems));
        }

        let entry = entry_of(graph).ok_or(RunRefusal::NothingToRun)?;
        let mut run = Self {
            stack: Vec::new(),
            history: Vec::new(),
            outcome: None,
        };
        run.push_frame(Vec::new(), entry);
        // The entry step may itself be a plan, so descend before handing back.
        run.descend_while_nested(graph);
        Ok(run)
    }

    fn push_frame(&mut self, parents: Vec<NodeId>, current: NodeId) {
        let mut visits = HashMap::default();
        visits.insert(current.clone(), 1);
        let frame = Frame {
            parents,
            current,
            phase: Phase::Working,
            visits,
        };
        self.history.push(frame.path());
        self.stack.push(frame);
    }

    /// The step the run is on, as a full path.
    pub fn current(&self) -> NodePath {
        self.stack.last().map(Frame::path).unwrap_or_default()
    }

    /// The steps entered, at any depth, in order.
    pub fn history(&self) -> &[NodePath] {
        &self.history
    }

    pub fn steps_taken(&self) -> usize {
        self.history.len()
    }

    /// How many times the current frame has entered a step. A step reached
    /// again by a loop is on attempt 2.
    pub fn attempt(&self, id: &NodeId) -> usize {
        self.stack
            .last()
            .and_then(|frame| frame.visits.get(id).copied())
            .unwrap_or(1)
    }

    /// Every step currently in progress, outermost first. The breadcrumb during
    /// a run: "Build API › Write handlers › Validate".
    pub fn active_path(&self) -> Vec<NodeId> {
        self.stack
            .last()
            .map(|frame| frame.path().0)
            .unwrap_or_default()
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn outcome(&self) -> Option<&RunOutcome> {
        self.outcome.as_ref()
    }

    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn cancel(&mut self) {
        if !self.is_finished() {
            self.outcome = Some(RunOutcome::Cancelled);
        }
    }

    /// What to do next, given where the run currently is.
    pub fn decide(&mut self, graph: &ArchitectGraph) -> Decision {
        if let Some(outcome) = &self.outcome {
            return Decision::Done(outcome.clone());
        }
        let Some(frame) = self.stack.last() else {
            return self.finish(RunOutcome::Completed);
        };
        match &frame.phase {
            Phase::Working => Decision::Run(frame.path()),
            Phase::Deciding { .. } => self.next_question(graph),
        }
    }

    /// Reports that the current step has been carried out, moving the run on to
    /// deciding which way to leave it.
    pub fn finish_step(&mut self, graph: &ArchitectGraph) -> Decision {
        if self.is_finished() {
            return self.decide(graph);
        }
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };

        let graph_path = frame.graph_path();
        let current = frame.current.clone();
        let Some(local) = graph.graph_at(&graph_path) else {
            return self.finish(RunOutcome::Completed);
        };

        let mut remaining = Vec::new();
        let mut fallback = None;
        for edge in local.edges_from(&current) {
            // A connection to a step that is not there cannot be followed. The
            // plan could not have been started with one, but a step's own chat
            // can rewrite routing while a run is in flight.
            if local.node(&edge.to).is_none() {
                continue;
            }
            if edge.condition.is_always() {
                // The first plain connection is the "otherwise" branch. Later
                // ones are unreachable by this policy, and silently taking one
                // would make the graph mean something other than it looks.
                fallback.get_or_insert_with(|| edge.to.clone());
            } else {
                remaining.push(Branch {
                    edge: edge.id.clone(),
                    to: edge.to.clone(),
                    condition: edge.condition.clone(),
                });
            }
        }

        frame.phase = Phase::Deciding {
            remaining,
            fallback,
        };
        self.next_question(graph)
    }

    /// Reports the answer to the question [`Decision::Ask`] posed.
    pub fn answer(&mut self, graph: &ArchitectGraph, taken: bool) -> Decision {
        if self.is_finished() {
            return self.decide(graph);
        }
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };
        let Phase::Deciding {
            remaining,
            fallback,
        } = &mut frame.phase
        else {
            return self.decide(graph);
        };
        if remaining.is_empty() {
            return self.next_question(graph);
        }

        let branch = remaining.remove(0);
        if taken {
            return self.enter(branch.to, graph);
        }
        let fallback = fallback.clone();
        if remaining.is_empty() {
            return match fallback {
                Some(to) => self.enter(to, graph),
                None => self.leave_frame(graph),
            };
        }
        self.next_question(graph)
    }

    fn next_question(&mut self, graph: &ArchitectGraph) -> Decision {
        let Some(frame) = self.stack.last() else {
            return self.finish(RunOutcome::Completed);
        };
        let Phase::Deciding {
            remaining,
            fallback,
        } = &frame.phase
        else {
            return self.decide(graph);
        };
        if let Some(branch) = remaining.first() {
            return Decision::Ask(branch.clone());
        }
        match fallback.clone() {
            Some(to) => self.enter(to, graph),
            None => self.leave_frame(graph),
        }
    }

    /// Moves onto a step within the current plan, descending into it if it is
    /// itself a plan.
    fn enter(&mut self, id: NodeId, graph: &ArchitectGraph) -> Decision {
        if self.history.len() >= MAX_RUN_STEPS {
            return self.finish(RunOutcome::StepLimit {
                steps: self.history.len(),
            });
        }
        let Some(frame) = self.stack.last_mut() else {
            return self.finish(RunOutcome::Completed);
        };

        let visits = frame.visits.entry(id.clone()).or_insert(0);
        *visits += 1;
        if *visits > MAX_NODE_VISITS {
            let visits = *visits;
            return self.finish(RunOutcome::NodeLimit { node: id, visits });
        }

        frame.current = id;
        frame.phase = Phase::Working;
        self.history.push(frame.path());
        self.descend_while_nested(graph);
        self.decide(graph)
    }

    /// A step that contains a plan is not carried out itself; the run descends
    /// into it. Repeated, because the first step of that plan may nest too.
    fn descend_while_nested(&mut self, graph: &ArchitectGraph) {
        loop {
            let Some(frame) = self.stack.last() else {
                return;
            };
            let path = frame.path();
            let Some(node) = graph.node_at(&path) else {
                return;
            };
            let Some(subplan) = node.subplan() else {
                return;
            };
            if self.stack.len() >= MAX_PLAN_DEPTH {
                self.finish(RunOutcome::DepthLimit {
                    node: node.id.clone(),
                });
                return;
            }
            let Some(entry) = entry_of(subplan) else {
                return;
            };
            self.push_frame(path.0, entry);
        }
    }

    /// The current plan has run out of steps. If it is a nested plan, the step
    /// that contained it is now finished, and the level above carries on.
    fn leave_frame(&mut self, graph: &ArchitectGraph) -> Decision {
        self.stack.pop();
        if self.stack.is_empty() {
            return self.finish(RunOutcome::Completed);
        }
        self.finish_step(graph)
    }

    fn finish(&mut self, outcome: RunOutcome) -> Decision {
        self.outcome = Some(outcome.clone());
        Decision::Done(outcome)
    }
}

/// Where a plan begins. Several entry points are possible and nothing in the
/// graph ranks them, so node order decides: it is the order the steps were
/// drafted in, and it makes the choice reproducible.
fn entry_of(graph: &ArchitectGraph) -> Option<NodeId> {
    graph
        .roots()
        .into_iter()
        .min_by_key(|id| graph.node_index(id).unwrap_or(usize::MAX))
}

/// The brief handed to the agent for one step.
///
/// Only this step is described, plus what the steps feeding into it reported.
/// The agent is not shown the rest of the plan: a model that can see step 4
/// tends to start on it while it is still meant to be doing step 3, and the
/// run, not the model, decides what comes next.
pub fn step_prompt(
    graph: &ArchitectGraph,
    path: &NodePath,
    step_number: usize,
    attempt: usize,
) -> String {
    let Some(node) = graph.node_at(path) else {
        return format!("Step {step_number} is missing from the plan; stop and say so.");
    };
    let local = path
        .parent()
        .and_then(|parent| graph.graph_at(&parent))
        .or_else(|| graph.graph_at(&NodePath::default()))
        .unwrap_or(graph);

    let mut prompt = String::new();

    if path.depth() > 1 {
        let ancestry: Vec<String> = path.0[..path.0.len() - 1]
            .iter()
            .map(|id| find_title(graph, id).unwrap_or_else(|| id.0.clone()))
            .collect();
        let _ = writeln!(prompt, "You are inside: {}.\n", ancestry.join(" › "));
    }

    let _ = writeln!(prompt, "## Step {step_number}: {}", node.title);

    let handed_on = incoming_summaries(local, node);
    if !handed_on.is_empty() {
        prompt.push_str("\nWhat earlier steps reported:\n");
        for (title, summary) in handed_on {
            let _ = writeln!(prompt, "- {title}: {summary}");
        }
    }

    if attempt > 1
        && let Some(result) = &node.result
    {
        let _ = write!(
            prompt,
            "\nYou have been here before. On attempt {}, you reported:\n{}\n\nThat did not settle \
             it, which is why you are back. Do not repeat what you already tried.\n",
            result.attempt.max(1),
            result.summary.trim()
        );
    }

    if !node.intent.trim().is_empty() {
        let _ = write!(prompt, "\nGoal: {}\n", node.intent.trim());
    }

    if !node.rules.is_empty() {
        prompt.push_str("\nRules, which hold however you carry this out:\n");
        for rule in &node.rules {
            let _ = writeln!(prompt, "- {rule}");
        }
    }

    if !node.capture.trim().is_empty() {
        let _ = write!(
            prompt,
            "\nCapture in your summary: {}\n",
            node.capture.trim()
        );
    }

    prompt.push_str(
        "\nCarry out this step now, and only this step. Do not start any later step: what happens \
         next is decided once this one is done. When it is finished, report what you did as your \
         summary.\n",
    );
    prompt
}

/// What the steps leading into this one reported, plus anything pinned.
///
/// Pinned steps are included wherever they are in the plan, which is the whole
/// point of pinning: a decision taken early that everything after it depends on
/// should not fall out of view once it is no longer a direct predecessor.
fn incoming_summaries<'a>(
    local: &'a ArchitectGraph,
    node: &ArchitectNode,
) -> Vec<(&'a str, &'a str)> {
    // Pinned first, so a standing decision is read before the detail of
    // whatever happened to run immediately before this step.
    let mut candidates: Vec<&'a ArchitectNode> = local.pinned_nodes().collect();
    for edge in local.edges_into(&node.id) {
        if let Some(source) = local.node(&edge.from) {
            candidates.push(source);
        }
    }

    let mut seen: Vec<&NodeId> = Vec::new();
    let mut out: Vec<(&'a str, &'a str)> = Vec::new();
    for candidate in candidates {
        if candidate.id == node.id || seen.contains(&&candidate.id) {
            continue;
        }
        let Some(result) = &candidate.result else {
            continue;
        };
        if result.summary.trim().is_empty() {
            continue;
        }
        seen.push(&candidate.id);
        out.push((candidate.title.as_str(), result.summary.trim()));
    }
    out
}

/// The question that decides whether a branch is taken.
///
/// The answer is parsed from the reply, so the shape of the reply is not
/// optional; asking for the verdict first also stops a model reasoning its way
/// into an answer it would not have given up front.
pub fn branch_prompt(graph: &ArchitectGraph, branch: &Branch) -> String {
    let target = find_title(graph, &branch.to).unwrap_or_else(|| branch.to.0.clone());

    let question = match &branch.condition {
        EdgeCondition::Always => "Should the plan continue?".to_string(),
        EdgeCondition::Deterministic { expression } => format!(
            "Check whether this is true right now: {expression}\n\nCheck it, rather than \
             recalling what was true earlier."
        ),
        EdgeCondition::LlmEvaluated { question } => format!("Answer this question: {question}"),
    };

    format!(
        "{question}\n\nAnswering YES means the plan moves on to \"{target}\". Reply with YES or NO \
         on the first line by itself, then one sentence saying why.\n"
    )
}

/// Reads a yes-or-no verdict out of a reply.
///
/// Only the opening of the reply is considered. A model that has answered "NO"
/// and then explains what a "yes" would have looked like must not be read as
/// having said yes, and the first word is the only part of a free-text reply
/// whose meaning is unambiguous.
pub fn parse_verdict(reply: &str) -> Option<bool> {
    for line in reply.lines() {
        let line = line.trim().trim_start_matches(['*', '#', '-', '>', ' ']);
        if line.is_empty() {
            continue;
        }
        let word: String = line
            .chars()
            .take_while(|character| character.is_ascii_alphabetic())
            .collect();
        return match word.to_ascii_lowercase().as_str() {
            "yes" | "true" => Some(true),
            "no" | "false" => Some(false),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchitectEdge, StepResult};

    fn lock_deeply(graph: &mut ArchitectGraph) {
        for node in &mut graph.nodes {
            node.locked = true;
            if let Some(subplan) = node.subplan.as_deref_mut() {
                lock_deeply(subplan);
            }
        }
    }

    /// plan → edit → test, with test looping back to edit when it fails.
    fn locked_graph() -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        let mut plan = ArchitectNode::new("plan", "Plan the change");
        plan.intent = "Decide which files need to change".into();
        plan.rules = vec!["Do not edit anything yet".into()];
        plan.capture = "Which files you intend to change".into();
        graph.add_node(plan);
        graph.add_node(ArchitectNode::new("edit", "Make the edit"));
        graph.add_node(ArchitectNode::new("test", "Run the tests"));
        graph.connect("plan", "edit");
        graph.connect("edit", "test");
        graph.edges.push(
            ArchitectEdge::new("retry", "test", "edit").with_condition(EdgeCondition::LlmEvaluated {
                question: "Did the tests fail?".into(),
            }),
        );
        lock_deeply(&mut graph);
        graph
    }

    fn path(ids: &[&str]) -> NodePath {
        NodePath(ids.iter().map(|id| NodeId((*id).into())).collect())
    }

    #[test]
    fn a_run_starts_at_the_step_nothing_leads_to() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.current(), path(&["plan"]));
        assert_eq!(run.decide(&graph), Decision::Run(path(&["plan"])));
    }

    #[test]
    fn an_unconditional_connection_is_followed_without_asking_anything() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["edit"])));
    }

    #[test]
    fn answering_yes_takes_the_branch_back_round_the_loop() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);
        run.finish_step(&graph);
        run.finish_step(&graph);
        assert_eq!(run.answer(&graph, true), Decision::Run(path(&["edit"])));
        assert_eq!(run.attempt(&NodeId("edit".into())), 2);
    }

    #[test]
    fn answering_no_with_nowhere_else_to_go_completes_the_plan() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);
        run.finish_step(&graph);
        run.finish_step(&graph);
        assert_eq!(
            run.answer(&graph, false),
            Decision::Done(RunOutcome::Completed)
        );
    }

    #[test]
    fn a_plain_connection_is_taken_when_every_condition_says_no() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("check", "Check the build"));
        graph.add_node(ArchitectNode::new("fix", "Fix the build"));
        graph.add_node(ArchitectNode::new("ship", "Ship it"));
        graph.edges.push(
            ArchitectEdge::new("broken", "check", "fix").with_condition(
                EdgeCondition::Deterministic {
                    expression: "the build failed".into(),
                },
            ),
        );
        graph.connect("check", "ship");
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        assert!(matches!(run.finish_step(&graph), Decision::Ask(_)));
        assert_eq!(run.answer(&graph, false), Decision::Run(path(&["ship"])));
    }

    // ── nesting ───────────────────────────────────────────────────────────

    fn nested_graph() -> ArchitectGraph {
        let mut inner = ArchitectGraph::default();
        inner.add_node(ArchitectNode::new("parse", "Parse body"));
        inner.add_node(ArchitectNode::new("respond", "Respond"));
        inner.connect("parse", "respond");

        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("schema", "Define schema"));
        let mut handlers = ArchitectNode::new("handlers", "Write handlers");
        handlers.subplan = Some(Box::new(inner));
        graph.add_node(handlers);
        graph.add_node(ArchitectNode::new("ship", "Ship it"));
        graph.connect("schema", "handlers");
        graph.connect("handlers", "ship");
        lock_deeply(&mut graph);
        graph
    }

    #[test]
    fn entering_a_step_that_contains_a_plan_descends_into_it() {
        let graph = nested_graph();
        let mut run = PlanRun::start(&graph).unwrap();

        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["handlers", "parse"])));
        assert_eq!(run.depth(), 2, "the run should be inside the sub-plan");
    }

    #[test]
    fn finishing_a_sub_plan_finishes_the_step_that_contained_it() {
        let graph = nested_graph();
        let mut run = PlanRun::start(&graph).unwrap();

        run.finish_step(&graph); // schema -> handlers, descends to parse
        run.finish_step(&graph); // parse -> respond
        assert_eq!(run.current(), path(&["handlers", "respond"]));

        // Respond is the last of the sub-plan, so finishing it should pop back
        // out and carry on with the step after the one that contained it.
        assert_eq!(run.finish_step(&graph), Decision::Run(path(&["ship"])));
        assert_eq!(run.depth(), 1);
    }

    #[test]
    fn the_breadcrumb_shows_every_level_in_progress() {
        let graph = nested_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);

        assert_eq!(
            run.active_path(),
            vec![NodeId("handlers".into()), NodeId("parse".into())]
        );
    }

    #[test]
    fn a_plan_nested_too_deep_is_refused_rather_than_run() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("leaf", "Leaf"));
        for depth in 0..MAX_PLAN_DEPTH + 1 {
            let mut outer = ArchitectGraph::default();
            let mut node = ArchitectNode::new(format!("level-{depth}"), format!("Level {depth}"));
            node.subplan = Some(Box::new(graph));
            outer.add_node(node);
            graph = outer;
        }
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        assert!(
            matches!(
                run.decide(&graph),
                Decision::Done(RunOutcome::DepthLimit { .. })
            ),
            "a plan nested past the limit should stop the run"
        );
    }

    #[test]
    fn a_sub_plan_with_an_unlocked_step_blocks_the_whole_run() {
        let mut graph = nested_graph();
        graph
            .node_mut(&NodeId("handlers".into()))
            .unwrap()
            .subplan
            .as_deref_mut()
            .unwrap()
            .set_locked(&NodeId("parse".into()), false);

        let refusal = PlanRun::start(&graph).unwrap_err();
        let RunRefusal::NotReady(problems) = refusal else {
            panic!("expected the unlocked nested step to be reported");
        };
        assert!(
            problems
                .iter()
                .any(|problem| matches!(problem, GraphProblem::InSubplan { .. })),
            "got {problems:?}"
        );
    }

    #[test]
    fn a_step_cannot_be_locked_while_its_sub_plan_is_still_open() {
        let mut graph = nested_graph();
        assert!(graph.can_lock(&NodeId("handlers".into())));

        graph
            .node_mut(&NodeId("handlers".into()))
            .unwrap()
            .subplan
            .as_deref_mut()
            .unwrap()
            .set_locked(&NodeId("parse".into()), false);
        assert!(!graph.can_lock(&NodeId("handlers".into())));
    }

    // ── handoff ───────────────────────────────────────────────────────────

    fn with_result(graph: &mut ArchitectGraph, id: &str, summary: &str, attempt: usize) {
        graph.node_mut(&NodeId(id.into())).unwrap().result = Some(StepResult {
            summary: summary.into(),
            attempt,
        });
    }

    #[test]
    fn a_step_is_told_what_the_steps_feeding_it_reported() {
        let mut graph = locked_graph();
        with_result(&mut graph, "plan", "Will change calc.py only.", 1);

        let prompt = step_prompt(&graph, &path(&["edit"]), 2, 1);
        assert!(prompt.contains("What earlier steps reported"));
        assert!(prompt.contains("Plan the change: Will change calc.py only."));
    }

    #[test]
    fn a_step_is_not_told_what_unrelated_steps_reported() {
        let mut graph = locked_graph();
        with_result(&mut graph, "test", "Tests failed.", 1);

        // `test` does not lead into `edit` except by the retry loop, which does
        // feed it — so use `plan`, which nothing leads into.
        let prompt = step_prompt(&graph, &path(&["plan"]), 1, 1);
        assert!(
            !prompt.contains("Tests failed."),
            "a step should not receive summaries from steps that do not feed it:\n{prompt}"
        );
    }

    #[test]
    fn a_pinned_step_is_told_to_every_later_step() {
        let mut graph = locked_graph();
        graph.node_mut(&NodeId("plan".into())).unwrap().pinned = true;
        with_result(&mut graph, "plan", "Decided to keep the public API.", 1);

        let prompt = step_prompt(&graph, &path(&["test"]), 3, 1);
        assert!(
            prompt.contains("Decided to keep the public API."),
            "a pinned step should reach steps it does not directly feed:\n{prompt}"
        );
    }

    #[test]
    fn coming_round_a_loop_reminds_the_step_what_it_already_tried() {
        let mut graph = locked_graph();
        with_result(&mut graph, "edit", "Widened the clock skew.", 1);

        let prompt = step_prompt(&graph, &path(&["edit"]), 4, 2);
        assert!(prompt.contains("You have been here before"));
        assert!(prompt.contains("Widened the clock skew."));
        assert!(prompt.contains("Do not repeat what you already tried"));
    }

    #[test]
    fn a_first_attempt_is_not_told_about_a_previous_one() {
        let mut graph = locked_graph();
        with_result(&mut graph, "edit", "Widened the clock skew.", 1);

        let prompt = step_prompt(&graph, &path(&["edit"]), 2, 1);
        assert!(!prompt.contains("You have been here before"), "{prompt}");
    }

    #[test]
    fn a_step_states_what_its_summary_must_capture() {
        let graph = locked_graph();
        let prompt = step_prompt(&graph, &path(&["plan"]), 1, 1);
        assert!(prompt.contains("Capture in your summary: Which files you intend to change"));
    }

    #[test]
    fn a_nested_step_is_told_which_step_it_is_inside() {
        let graph = nested_graph();
        let prompt = step_prompt(&graph, &path(&["handlers", "parse"]), 2, 1);
        assert!(
            prompt.contains("You are inside: Write handlers"),
            "{prompt}"
        );
    }

    #[test]
    fn a_step_prompt_never_mentions_later_steps() {
        let graph = locked_graph();
        let prompt = step_prompt(&graph, &path(&["plan"]), 1, 1);
        assert!(!prompt.contains("Run the tests"), "{prompt}");
    }

    #[test]
    fn steps_that_lead_somewhere_without_saying_what_they_hand_on_are_reported() {
        let graph = locked_graph();
        let missing = graph.steps_without_capture();
        assert!(missing.contains(&NodeId("edit".into())));
        assert!(
            !missing.contains(&NodeId("plan".into())),
            "plan states a capture, so it should not be reported"
        );
    }

    // ── budgets and endings ───────────────────────────────────────────────

    #[test]
    fn a_loop_that_never_exits_is_stopped_rather_than_run_forever() {
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("work", "Work"));
        graph.edges.push(
            ArchitectEdge::new("again", "work", "work").with_condition(EdgeCondition::LlmEvaluated {
                question: "Again?".into(),
            }),
        );
        lock_deeply(&mut graph);

        let mut run = PlanRun::start(&graph).unwrap();
        let outcome = loop {
            match run.finish_step(&graph) {
                Decision::Ask(_) => match run.answer(&graph, true) {
                    Decision::Done(outcome) => break outcome,
                    _ => continue,
                },
                Decision::Done(outcome) => break outcome,
                Decision::Run(_) => continue,
            }
        };
        assert_eq!(
            outcome,
            RunOutcome::NodeLimit {
                node: NodeId("work".into()),
                visits: MAX_NODE_VISITS + 1,
            }
        );
    }

    #[test]
    fn an_empty_plan_refuses_to_run() {
        assert_eq!(
            PlanRun::start(&ArchitectGraph::default()).unwrap_err(),
            RunRefusal::NothingToRun
        );
    }

    #[test]
    fn cancelling_ends_the_run_wherever_it_had_got_to() {
        let graph = locked_graph();
        let mut run = PlanRun::start(&graph).unwrap();
        run.finish_step(&graph);
        run.cancel();
        assert_eq!(run.decide(&graph), Decision::Done(RunOutcome::Cancelled));
    }

    #[test]
    fn an_outcome_describes_itself_using_the_step_title() {
        let graph = nested_graph();
        let described = RunOutcome::NodeLimit {
            node: NodeId("parse".into()),
            visits: 25,
        }
        .describe(&graph);
        assert!(
            described.contains("Parse body"),
            "a nested step should still be named: {described}"
        );
    }

    // ── verdicts ──────────────────────────────────────────────────────────

    #[test]
    fn a_verdict_is_read_from_the_first_word_of_the_reply() {
        assert_eq!(parse_verdict("YES\nThe tests failed."), Some(true));
        assert_eq!(parse_verdict("no, everything passed"), Some(false));
        assert_eq!(parse_verdict("**YES** - it failed"), Some(true));
    }

    #[test]
    fn a_no_that_goes_on_to_mention_yes_is_still_a_no() {
        assert_eq!(
            parse_verdict("NO\nIf they had failed the answer would be yes."),
            Some(false)
        );
    }

    #[test]
    fn a_reply_that_does_not_start_with_a_verdict_is_not_guessed_at() {
        assert_eq!(parse_verdict("I think the tests failed, so yes"), None);
        assert_eq!(parse_verdict(""), None);
    }
}
