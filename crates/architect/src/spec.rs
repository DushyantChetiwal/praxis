//! Rendering a finished graph as prose.
//!
//! This flattens a plan into an ordered list of steps with every condition
//! stated in full, so that a graph can be read by anything that cannot see the
//! canvas. Plans are not run this way: [`crate::PlanRun`] drives them a step at
//! a time and keeps the control flow out of the model's hands. What this
//! produces is a description of a plan, not an instruction to carry one out.

use crate::{ArchitectGraph, EdgeCondition, GraphProblem, NodeId, layout};
use std::fmt::Write;

/// Compiles a graph into an ordered spec, or reports why it is not ready.
///
/// Every step has to be locked first: an unlocked step is one the user is still
/// deliberating, and running a half-argued plan is how a plan stops being worth
/// making.
pub fn compile_spec(graph: &ArchitectGraph) -> Result<String, Vec<GraphProblem>> {
    let problems = graph.blocking_problems();
    if !problems.is_empty() {
        return Err(problems);
    }
    if graph.nodes.is_empty() {
        return Err(vec![]);
    }

    let order = execution_order(graph);
    let step_numbers: Vec<(NodeId, usize)> = order
        .iter()
        .enumerate()
        .map(|(position, &ix)| (graph.nodes[ix].id.clone(), position + 1))
        .collect();
    let step_number = |id: &NodeId| {
        step_numbers
            .iter()
            .find(|(node_id, _)| node_id == id)
            .map(|(_, number)| *number)
    };

    let mut spec = String::new();
    spec.push_str(
        "Follow this plan. Each step states what it must accomplish and the rules it must \
         honour. Work through the steps in order, and take a connection only when its condition \
         holds.\n",
    );

    for (position, &ix) in order.iter().enumerate() {
        let node = &graph.nodes[ix];
        let number = position + 1;

        write!(spec, "\n## Step {number}: {}\n", node.title).ok();

        if let Some(model) = &node.model {
            spec.push_str(&format!("\nModel: {}/{}\n", model.provider, model.model));
        } else {
            spec.push_str("\nModel: inherit plan\n");
        }

        spec.push_str(&format!(
            "\nExisting-file surface: {}\n",
            node.file_surface_description()
        ));

        if !node.responsibility.trim().is_empty() {
            spec.push_str("\nResponsibility: ");
            spec.push_str(node.responsibility.trim());
            spec.push('\n');
        }

        if !node.intent.trim().is_empty() {
            write!(spec, "\nGoal: {}\n", node.intent.trim()).ok();
        }

        if !node.rules.is_empty() {
            spec.push_str("\nRules:\n");
            for rule in &node.rules {
                writeln!(spec, "- {rule}").ok();
            }
        }

        if let Some(subplan) = node.subplan() {
            spec.push_str("\nNested plan:\n\n");
            for line in compile_spec(subplan)?.lines() {
                spec.push_str("> ");
                spec.push_str(line);
                spec.push('\n');
            }
        }

        let outgoing: Vec<_> = graph.edges_from(&node.id).collect();
        if outgoing.is_empty() {
            spec.push_str("\nThis is a final step; when it is done, the plan is complete.\n");
            continue;
        }

        spec.push_str("\nThen:\n");
        for edge in outgoing {
            let target = graph
                .node(&edge.to)
                .map(|node| node.title.as_str())
                .unwrap_or(edge.to.0.as_str());
            let target_number = step_number(&edge.to);

            let destination = match target_number {
                Some(target_number) if target_number <= number => {
                    format!("go back to step {target_number} ({target})")
                }
                Some(target_number) => format!("go to step {target_number} ({target})"),
                None => format!("go to {target}"),
            };
            let destination = match edge.max_repeats {
                Some(limit) => format!("{destination} (at most {limit} times in total)"),
                None => destination,
            };

            match &edge.condition {
                EdgeCondition::Always => writeln!(spec, "- {destination}.").ok(),
                EdgeCondition::Objective { statement } => {
                    writeln!(spec, "- If {statement}, {destination}.").ok()
                }
                EdgeCondition::LlmEvaluated { question } => writeln!(
                    spec,
                    "- Judge for yourself: if the answer to \"{question}\" is yes, {destination}.",
                )
                .ok(),
            };
        }
    }

    Ok(spec)
}

/// The order the steps run in: every step comes after the steps that lead into
/// it. Edges that close a loop are excluded from the ordering, since a loop by
/// definition cannot be honoured by a linear sequence; they are reported in the
/// spec as jumps instead.
fn execution_order(graph: &ArchitectGraph) -> Vec<usize> {
    let adjacency = graph.adjacency();
    let back_edges = layout::back_edges(&adjacency);
    let columns = layout::assign_columns(&adjacency, &back_edges);

    let mut order: Vec<usize> = (0..graph.nodes.len()).collect();
    order.sort_by_key(|&ix| (columns[ix], ix));
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchitectEdge, ArchitectNode};

    fn locked_graph() -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();

        let mut plan = ArchitectNode::new("plan", "Plan the change");
        plan.intent = "Decide which files need to change".into();
        plan.rules = vec!["Do not edit anything yet".into()];
        graph.add_node(plan);

        let mut edit = ArchitectNode::new("edit", "Make the edit");
        edit.intent = "Apply the change".into();
        graph.add_node(edit);

        graph.add_node(ArchitectNode::new("test", "Run the tests"));

        graph.connect("plan", "edit");
        graph.connect("edit", "test");
        graph
            .edges
            .push(ArchitectEdge::new("retry", "test", "edit").with_condition(
                EdgeCondition::LlmEvaluated {
                    question: "Did the tests fail?".into(),
                },
            ));

        let ids: Vec<NodeId> = graph.nodes.iter().map(|node| node.id.clone()).collect();
        for id in &ids {
            graph.set_locked(id, true);
        }
        graph
    }

    #[test]
    fn compiles_steps_in_dependency_order() {
        let spec = compile_spec(&locked_graph()).unwrap();

        let plan_at = spec.find("Plan the change").unwrap();
        let edit_at = spec.find("Make the edit").unwrap();
        let test_at = spec.find("Run the tests").unwrap();

        assert!(plan_at < edit_at, "plan should come before edit:\n{spec}");
        assert!(edit_at < test_at, "edit should come before test:\n{spec}");
    }

    #[test]
    fn spec_includes_explicit_and_inherited_models_in_nested_plans() {
        let mut graph = locked_graph();
        let mut nested = ArchitectGraph::default();
        let mut child = ArchitectNode::new("child", "Nested work");
        child.model = Some(crate::StepModel {
            provider: "test-provider".into(),
            model: "test-model".into(),
        });
        child.locked = true;
        nested.add_node(child);
        graph
            .node_mut(&"edit".into())
            .expect("edit step should exist")
            .subplan = Some(Box::new(nested));

        let spec = compile_spec(&graph).expect("locked plan should compile");
        assert!(spec.contains("Model: inherit plan"));
        assert!(spec.contains("Model: test-provider/test-model"));
        assert!(spec.contains("Nested work"));
    }

    #[test]
    fn spec_carries_explicit_existing_file_surfaces() {
        let mut graph = locked_graph();
        graph.node_mut(&"plan".into()).expect("plan").file_surface =
            Some(vec!["worktree/src/plan.rs".into()]);
        let spec = compile_spec(&graph).expect("spec");
        assert!(spec.contains("Existing-file surface: [\"worktree/src/plan.rs\"]"));
        assert!(spec.contains("[] — no existing files anticipated"));
        graph.node_mut(&"plan".into()).expect("plan").file_surface = None;
        assert!(compile_spec(&graph).is_err());
    }

    #[test]
    fn states_intent_and_rules() {
        let spec = compile_spec(&locked_graph()).unwrap();

        assert!(spec.contains("Goal: Decide which files need to change"));
        assert!(spec.contains("- Do not edit anything yet"));
    }

    #[test]
    fn a_loop_is_written_as_a_jump_backwards() {
        let spec = compile_spec(&locked_graph()).unwrap();

        assert!(
            spec.contains("Did the tests fail?"),
            "the loop condition should survive into the spec:\n{spec}"
        );
        assert!(
            spec.contains("go back to step 2"),
            "the loop should read as a jump back:\n{spec}"
        );
    }

    #[test]
    fn the_last_step_says_the_plan_is_done() {
        let mut graph = locked_graph();
        graph.disconnect(&"retry".into());

        let spec = compile_spec(&graph).unwrap();
        assert!(spec.contains("the plan is complete"), "{spec}");
    }

    #[test]
    fn refuses_a_graph_with_an_unlocked_step() {
        let mut graph = locked_graph();
        graph.set_locked(&"edit".into(), false);

        let problems = compile_spec(&graph).unwrap_err();
        assert!(
            problems.contains(&GraphProblem::Unlocked(NodeId("edit".into()))),
            "expected the unlocked step to be reported, got {problems:?}"
        );
    }

    #[test]
    fn refuses_a_structurally_broken_graph() {
        let mut graph = locked_graph();
        graph.connect("test", "missing");

        let problems = compile_spec(&graph).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|problem| matches!(problem, GraphProblem::DanglingEdge { .. })),
            "expected a dangling edge, got {problems:?}"
        );
    }

    #[test]
    fn refuses_an_empty_graph() {
        assert!(compile_spec(&ArchitectGraph::default()).is_err());
    }
}
