//! A plan seen as a checklist.
//!
//! The checklist above the message box and the canvas are two views of one
//! plan: the checklist lists the plan's top-level steps in the order they run,
//! and a checklist the agent writes edits the plan.

use crate::{ArchitectGraph, ArchitectNode, NodeId};
use collections::HashMap;

/// A step's name as a checklist entry matches it: without case, and without
/// differences in spacing.
fn checklist_key(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// An id for a step a checklist adds, readable in the plan's outline.
fn checklist_step_id(title: &str) -> NodeId {
    const MAX_ID_LENGTH: usize = 40;
    let mut id = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            id.push(character.to_ascii_lowercase());
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
        if id.len() >= MAX_ID_LENGTH {
            break;
        }
    }
    let id = id.trim_end_matches('-');
    NodeId(if id.is_empty() { "step" } else { id }.to_string())
}

impl ArchitectNode {
    /// Whether anything was decided about this step beyond its name, on the
    /// canvas or in a draft: a goal, rules, files, a model, a nested plan, its
    /// own conversation, a result, or a lock. A checklist cannot see any of
    /// that, so it never removes such a step.
    pub fn has_detail(&self) -> bool {
        self.locked
            || self.pinned
            || self.model.is_some()
            || !self.responsibility.trim().is_empty()
            || !self.intent.trim().is_empty()
            || !self.rules.is_empty()
            || !self.capture.trim().is_empty()
            || self
                .file_surface
                .as_ref()
                .is_some_and(|files| !files.is_empty())
            || self.chat.is_some()
            || self.subplan.is_some()
            || self.result.is_some()
    }
}

impl ArchitectGraph {
    /// Whether the plan is a single line of steps, each simply followed by the
    /// next: no branches, conditions, loops or steps standing apart.
    pub fn is_single_line(&self) -> bool {
        if self.nodes.is_empty() {
            return self.edges.is_empty();
        }
        if self.edges.len() + 1 != self.nodes.len() {
            return false;
        }
        let mut incoming: HashMap<&NodeId, usize> = HashMap::default();
        let mut outgoing: HashMap<&NodeId, usize> = HashMap::default();
        for edge in &self.edges {
            let plain = edge.condition.is_always() && edge.max_repeats.is_none();
            if !plain
                || edge.from == edge.to
                || self.node(&edge.from).is_none()
                || self.node(&edge.to).is_none()
            {
                return false;
            }
            *outgoing.entry(&edge.from).or_default() += 1;
            *incoming.entry(&edge.to).or_default() += 1;
        }
        if incoming.values().chain(outgoing.values()).any(|count| *count > 1) {
            return false;
        }
        let mut entries = self
            .nodes
            .iter()
            .filter(|node| !incoming.contains_key(&node.id));
        let (Some(entry), None) = (entries.next(), entries.next()) else {
            return false;
        };
        let mut visited = 1;
        let mut current = &entry.id;
        while let Some(edge) = self.edges_from(current).next() {
            visited += 1;
            if visited > self.nodes.len() {
                return false;
            }
            current = &edge.to;
        }
        visited == self.nodes.len()
    }

    /// Whether the plan is nothing more than a checklist: a single line of
    /// steps with nothing decided about any of them beyond its name.
    pub fn is_plain_checklist(&self) -> bool {
        self.is_single_line() && !self.nodes.iter().any(ArchitectNode::has_detail)
    }

    /// Makes the plan's top-level steps match a checklist, and returns, for
    /// each entry, the step it names.
    ///
    /// - An entry names the step with the same title, compared without case.
    ///   An entry that names no step adds one.
    /// - In a plan that is a single line of steps, steps the checklist no
    ///   longer lists are removed unless they carry detail (see
    ///   [`ArchitectNode::has_detail`]), and the steps follow the checklist's
    ///   order, with any it keeps but does not list after the rest.
    /// - A plan with branches keeps its shape and every step. New steps run
    ///   after everything else in it.
    pub fn sync_checklist(&mut self, titles: &[String]) -> Vec<NodeId> {
        let single_line = self.is_single_line();
        let mut unlisted = self.execution_order();
        let mut ids = Vec::with_capacity(titles.len());
        let mut added = Vec::new();
        for title in titles {
            let key = checklist_key(title);
            let matched = unlisted.iter().position(|id| {
                self.node(id)
                    .is_some_and(|node| checklist_key(&node.title) == key)
            });
            let id = match matched {
                Some(index) => unlisted.remove(index),
                None => {
                    let id =
                        self.add_node(ArchitectNode::new(checklist_step_id(title), title.trim()));
                    added.push(id.clone());
                    id
                }
            };
            ids.push(id);
        }

        if single_line {
            unlisted.retain(|id| {
                let keep = self.node(id).is_some_and(ArchitectNode::has_detail);
                if !keep {
                    self.remove_node(id);
                }
                keep
            });
            self.edges.clear();
            let order: Vec<NodeId> = ids.iter().chain(&unlisted).cloned().collect();
            for pair in order.windows(2) {
                self.connect(pair[0].clone(), pair[1].clone());
            }
        } else if let Some(first) = added.first() {
            let ends: Vec<NodeId> = self
                .nodes
                .iter()
                .filter(|node| {
                    !added.contains(&node.id) && self.edges_from(&node.id).next().is_none()
                })
                .map(|node| node.id.clone())
                .collect();
            let ends = if ends.is_empty() {
                // Every step leads somewhere, as in a plan ending in a loop:
                // follow the step that runs last.
                self.execution_order()
                    .into_iter()
                    .rfind(|id| !added.contains(id))
                    .into_iter()
                    .collect()
            } else {
                ends
            };
            for end in ends {
                self.connect(end, first.clone());
            }
            for pair in added.windows(2) {
                self.connect(pair[0].clone(), pair[1].clone());
            }
        }
        self.place_unpositioned_nodes();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EdgeCondition;

    fn titles(titles: &[&str]) -> Vec<String> {
        titles.iter().map(|title| title.to_string()).collect()
    }

    fn step_titles(graph: &ArchitectGraph) -> Vec<String> {
        graph
            .execution_order()
            .iter()
            .filter_map(|id| graph.node(id))
            .map(|node| node.title.clone())
            .collect()
    }

    #[test]
    fn a_checklist_becomes_a_single_line_plan() {
        let mut graph = ArchitectGraph::default();
        let ids =
            graph.sync_checklist(&titles(&["Read the code", "Fix the bug!", "Run tests"]));

        assert_eq!(
            ids,
            vec![
                NodeId::from("read-the-code"),
                NodeId::from("fix-the-bug"),
                NodeId::from("run-tests")
            ]
        );
        assert!(graph.is_single_line());
        assert_eq!(step_titles(&graph), ["Read the code", "Fix the bug!", "Run tests"]);
        assert!(
            graph.nodes.iter().all(|node| node.position.is_some()),
            "the canvas can show the plan as soon as it exists"
        );
        assert!(graph.problems().is_empty(), "{:?}", graph.problems());
    }

    #[test]
    fn a_single_line_plan_follows_the_checklist() {
        let mut graph = ArchitectGraph::default();
        graph.sync_checklist(&titles(&["First", "Second", "Third"]));
        let first = graph.node(&NodeId::from("first")).unwrap().position;

        // Reworded, reordered and trimmed, with one step added.
        let ids = graph.sync_checklist(&titles(&["  third ", "First", "New step"]));

        assert_eq!(
            ids,
            vec![
                NodeId::from("third"),
                NodeId::from("first"),
                NodeId::from("new-step")
            ]
        );
        assert_eq!(step_titles(&graph), ["Third", "First", "New step"]);
        assert!(graph.node(&NodeId::from("second")).is_none());
        assert!(graph.is_single_line());
        assert_eq!(
            graph.node(&NodeId::from("first")).unwrap().position,
            first,
            "steps the checklist keeps stay where they were drawn"
        );
    }

    #[test]
    fn steps_with_detail_survive_a_checklist_that_drops_them() {
        let mut graph = ArchitectGraph::default();
        graph.sync_checklist(&titles(&["Plan", "Build", "Ship"]));
        graph.node_mut(&NodeId::from("build")).unwrap().intent = "Compile it".into();

        graph.sync_checklist(&titles(&["Ship", "Plan"]));

        assert_eq!(step_titles(&graph), ["Ship", "Plan", "Build"]);
        assert!(graph.is_single_line());
    }

    #[test]
    fn a_plan_with_branches_keeps_its_shape() {
        let mut graph = ArchitectGraph::default();
        for id in ["start", "left", "right"] {
            graph.add_node(ArchitectNode::new(id, id));
        }
        graph.connect("start", "left");
        graph.connect_with(
            "start",
            "right",
            EdgeCondition::Objective {
                statement: "tests failed".into(),
            },
        );
        assert!(!graph.is_single_line());

        let ids = graph.sync_checklist(&titles(&["LEFT", "Wrap up"]));

        assert_eq!(ids, vec![NodeId::from("left"), NodeId::from("wrap-up")]);
        assert_eq!(graph.nodes.len(), 4, "unlisted steps of a branched plan stay");
        let into_new: Vec<&str> = graph
            .edges_into(&NodeId::from("wrap-up"))
            .map(|edge| edge.from.0.as_str())
            .collect();
        assert_eq!(into_new, ["left", "right"], "a new step follows every branch");
    }

    #[test]
    fn repeated_titles_name_different_steps() {
        let mut graph = ArchitectGraph::default();
        let ids = graph.sync_checklist(&titles(&["Check", "Check"]));
        assert_eq!(ids, vec![NodeId::from("check"), NodeId::from("check-2")]);
        assert_eq!(graph.sync_checklist(&titles(&["Check", "Check"])), ids);
    }

    #[test]
    fn an_empty_checklist_empties_a_plan_of_plain_steps() {
        let mut graph = ArchitectGraph::default();
        graph.sync_checklist(&titles(&["One", "Two"]));
        graph.sync_checklist(&[]);
        assert!(graph.is_empty());
        assert!(graph.edges.is_empty());
    }

    #[test]
    fn only_a_line_of_bare_steps_is_a_plain_checklist() {
        let mut graph = ArchitectGraph::default();
        assert!(graph.is_plain_checklist(), "no plan is no more than a checklist");
        graph.sync_checklist(&titles(&["Read", "Write"]));
        assert!(graph.is_plain_checklist());

        graph.node_mut(&NodeId::from("write")).unwrap().locked = true;
        assert!(!graph.is_plain_checklist(), "a settled step was decided on");
        graph.node_mut(&NodeId::from("write")).unwrap().locked = false;

        graph.add_node(ArchitectNode::new("apart", "Apart"));
        assert!(!graph.is_plain_checklist(), "a step standing apart is a shape");
    }

    #[test]
    fn single_line_detection() {
        let mut graph = ArchitectGraph::default();
        assert!(graph.is_single_line());
        for id in ["a", "b", "c"] {
            graph.add_node(ArchitectNode::new(id, id));
        }
        assert!(!graph.is_single_line(), "steps standing apart are not a line");
        graph.connect("a", "b");
        graph.connect("b", "c");
        assert!(graph.is_single_line());
        graph.connect("c", "a");
        assert!(!graph.is_single_line(), "a loop is not a line");
    }
}
