//! Turning a graph into readable positions.
//!
//! A model proposes steps and connections but no coordinates, because asking it
//! to invent pixel positions reliably produces overlapping nodes. Positions are
//! derived here instead, so a plan is legible the moment it appears.
//!
//! The approach is the classic layered one: put each step in a column one past
//! the furthest step that leads to it, then order steps within a column to keep
//! the connections between columns from crossing. Loops are ordinary in a plan
//! ("if the tests fail, edit again"), so the edges that close a cycle are set
//! aside while columns are assigned and drawn as connections back through the
//! layout.

use crate::{ArchitectGraph, NodeId};
use collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A point on the canvas, in unzoomed canvas space, addressing a node's centre.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

impl Position {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };
}

/// Horizontal distance between columns, wide enough for an edge label to sit on
/// the connection without touching either node.
pub const COLUMN_SPACING: f32 = 340.0;
/// Vertical distance between steps sharing a column.
pub const ROW_SPACING: f32 = 168.0;

/// The number of reordering passes. Each pass pulls a step toward the average
/// position of the steps that lead to it; a handful of passes is enough to
/// settle the graphs a plan produces, and it is bounded work either way.
const ORDERING_PASSES: usize = 4;

pub fn layout_positions(graph: &ArchitectGraph) -> HashMap<NodeId, Position> {
    let adjacency = graph.adjacency();
    let node_count = adjacency.len();
    if node_count == 0 {
        return HashMap::default();
    }

    let back_edges = back_edges(&adjacency);
    let columns = assign_columns(&adjacency, &back_edges);
    let rows = order_within_columns(&adjacency, &back_edges, &columns);

    let column_heights = column_heights(&columns, node_count);

    let mut positions = HashMap::default();
    for (ix, node) in graph.nodes.iter().enumerate() {
        let column = columns[ix];
        let height = column_heights[column];
        let row = rows[ix] as f32 - (height.saturating_sub(1) as f32 / 2.0);
        positions.insert(
            node.id.clone(),
            Position {
                x: column as f32 * COLUMN_SPACING,
                y: row * ROW_SPACING,
            },
        );
    }
    positions
}

/// The edges that close a cycle, found by depth-first search: an edge into a
/// node we are still exploring is an edge back into the path we came in on.
pub(crate) fn back_edges(adjacency: &[Vec<usize>]) -> HashSet<(usize, usize)> {
    const UNVISITED: u8 = 0;
    const EXPLORING: u8 = 1;
    const DONE: u8 = 2;

    let mut state = vec![UNVISITED; adjacency.len()];
    let mut back_edges = HashSet::default();

    for start in 0..adjacency.len() {
        if state[start] != UNVISITED {
            continue;
        }
        state[start] = EXPLORING;
        let mut stack = vec![(start, 0usize)];

        while let Some((node, edge_ix)) = stack.pop() {
            let Some(&next) = adjacency[node].get(edge_ix) else {
                state[node] = DONE;
                continue;
            };
            stack.push((node, edge_ix + 1));

            match state[next] {
                EXPLORING => {
                    back_edges.insert((node, next));
                }
                UNVISITED => {
                    state[next] = EXPLORING;
                    stack.push((next, 0));
                }
                _ => {}
            }
        }
    }

    back_edges
}

/// Places each step one column past the furthest step leading into it, so a
/// connection always points forward.
pub(crate) fn assign_columns(
    adjacency: &[Vec<usize>],
    back_edges: &HashSet<(usize, usize)>,
) -> Vec<usize> {
    let node_count = adjacency.len();
    let mut incoming = vec![0usize; node_count];
    for (from, targets) in adjacency.iter().enumerate() {
        for &to in targets {
            if !back_edges.contains(&(from, to)) {
                incoming[to] += 1;
            }
        }
    }

    let mut column = vec![0usize; node_count];
    let mut queue: VecDeque<usize> = (0..node_count).filter(|&ix| incoming[ix] == 0).collect();

    while let Some(node) = queue.pop_front() {
        for &to in &adjacency[node] {
            if back_edges.contains(&(node, to)) {
                continue;
            }
            column[to] = column[to].max(column[node] + 1);
            incoming[to] -= 1;
            if incoming[to] == 0 {
                queue.push_back(to);
            }
        }
    }

    column
}

/// Orders the steps inside each column so connections between columns cross as
/// little as possible, by repeatedly moving each step toward the average
/// position of the steps that lead into it.
fn order_within_columns(
    adjacency: &[Vec<usize>],
    back_edges: &HashSet<(usize, usize)>,
    columns: &[usize],
) -> Vec<usize> {
    let node_count = adjacency.len();

    let mut predecessors = vec![Vec::new(); node_count];
    for (from, targets) in adjacency.iter().enumerate() {
        for &to in targets {
            if !back_edges.contains(&(from, to)) {
                predecessors[to].push(from);
            }
        }
    }

    let column_count = columns.iter().copied().max().unwrap_or(0) + 1;
    let mut grouped: Vec<Vec<usize>> = vec![Vec::new(); column_count];
    for (ix, &column) in columns.iter().enumerate() {
        grouped[column].push(ix);
    }

    let mut row = vec![0usize; node_count];
    for group in &grouped {
        for (position, &node) in group.iter().enumerate() {
            row[node] = position;
        }
    }

    for _ in 0..ORDERING_PASSES {
        for column in 1..column_count {
            let mut group = std::mem::take(&mut grouped[column]);
            // Steps with nothing leading into them keep their place rather than
            // being dragged to the top, which would shuffle unrelated branches.
            group.sort_by(|&left, &right| {
                let left_key = average_row(&predecessors[left], &row).unwrap_or(row[left] as f32);
                let right_key =
                    average_row(&predecessors[right], &row).unwrap_or(row[right] as f32);
                left_key
                    .partial_cmp(&right_key)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for (position, &node) in group.iter().enumerate() {
                row[node] = position;
            }
            grouped[column] = group;
        }
    }

    row
}

fn average_row(nodes: &[usize], row: &[usize]) -> Option<f32> {
    if nodes.is_empty() {
        return None;
    }
    let total: usize = nodes.iter().map(|&node| row[node]).sum();
    Some(total as f32 / nodes.len() as f32)
}

fn column_heights(columns: &[usize], node_count: usize) -> Vec<usize> {
    let column_count = columns.iter().copied().max().unwrap_or(0) + 1;
    let mut heights = vec![0usize; column_count];
    for &column in columns.iter().take(node_count) {
        heights[column] += 1;
    }
    heights
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchitectEdge, ArchitectNode, EdgeCondition};

    fn graph_with(nodes: &[&str], edges: &[(&str, &str)]) -> ArchitectGraph {
        let mut graph = ArchitectGraph::default();
        for id in nodes {
            graph.add_node(ArchitectNode::new(*id, *id));
        }
        for (from, to) in edges {
            graph.connect(*from, *to);
        }
        graph
    }

    fn x_of(positions: &HashMap<NodeId, Position>, id: &str) -> f32 {
        positions.get(&NodeId(id.into())).unwrap().x
    }

    fn y_of(positions: &HashMap<NodeId, Position>, id: &str) -> f32 {
        positions.get(&NodeId(id.into())).unwrap().y
    }

    #[test]
    fn a_chain_runs_left_to_right() {
        let graph = graph_with(&["a", "b", "c"], &[("a", "b"), ("b", "c")]);
        let positions = layout_positions(&graph);

        assert!(x_of(&positions, "a") < x_of(&positions, "b"));
        assert!(x_of(&positions, "b") < x_of(&positions, "c"));
    }

    #[test]
    fn parallel_branches_share_a_column_without_overlapping() {
        let graph = graph_with(
            &["start", "left", "right", "join"],
            &[
                ("start", "left"),
                ("start", "right"),
                ("left", "join"),
                ("right", "join"),
            ],
        );
        let positions = layout_positions(&graph);

        assert_eq!(x_of(&positions, "left"), x_of(&positions, "right"));
        assert_ne!(y_of(&positions, "left"), y_of(&positions, "right"));
        assert!(x_of(&positions, "join") > x_of(&positions, "left"));
    }

    #[test]
    fn a_step_waits_for_the_longest_path_into_it() {
        // `join` must sit past `slow`, not merely past `fast`.
        let graph = graph_with(
            &["start", "fast", "slow_a", "slow_b", "join"],
            &[
                ("start", "fast"),
                ("start", "slow_a"),
                ("slow_a", "slow_b"),
                ("fast", "join"),
                ("slow_b", "join"),
            ],
        );
        let positions = layout_positions(&graph);

        assert!(x_of(&positions, "join") > x_of(&positions, "slow_b"));
        assert!(x_of(&positions, "join") > x_of(&positions, "fast"));
    }

    #[test]
    fn a_loop_still_lays_out_left_to_right() {
        let mut graph = graph_with(
            &["plan", "edit", "test"],
            &[("plan", "edit"), ("edit", "test")],
        );
        graph
            .edges
            .push(ArchitectEdge::new("retry", "test", "edit").with_condition(
                EdgeCondition::LlmEvaluated {
                    question: "Did the tests fail?".into(),
                },
            ));

        let positions = layout_positions(&graph);

        // The loop back to `edit` must not drag it forward past `test`.
        assert!(x_of(&positions, "plan") < x_of(&positions, "edit"));
        assert!(x_of(&positions, "edit") < x_of(&positions, "test"));
    }

    #[test]
    fn a_graph_that_is_entirely_a_cycle_still_gets_positions() {
        let graph = graph_with(&["a", "b", "c"], &[("a", "b"), ("b", "c"), ("c", "a")]);
        let positions = layout_positions(&graph);

        assert_eq!(positions.len(), 3);
        assert!(positions.values().all(|position| position.x.is_finite()));
    }

    #[test]
    fn a_step_that_points_at_itself_does_not_hang() {
        let graph = graph_with(&["a"], &[("a", "a")]);
        let positions = layout_positions(&graph);

        assert_eq!(positions.len(), 1);
    }

    #[test]
    fn an_empty_graph_lays_out_to_nothing() {
        let positions = layout_positions(&ArchitectGraph::default());
        assert!(positions.is_empty());
    }

    #[test]
    fn disconnected_pieces_all_get_placed() {
        let graph = graph_with(&["a", "b", "c", "d"], &[("a", "b"), ("c", "d")]);
        let positions = layout_positions(&graph);

        assert_eq!(positions.len(), 4);
        assert_eq!(x_of(&positions, "a"), x_of(&positions, "c"));
        assert_ne!(y_of(&positions, "a"), y_of(&positions, "c"));
    }

    #[test]
    fn the_same_graph_always_lays_out_the_same_way() {
        let graph = graph_with(
            &["start", "left", "right", "join"],
            &[
                ("start", "left"),
                ("start", "right"),
                ("left", "join"),
                ("right", "join"),
            ],
        );

        let first = layout_positions(&graph);
        for _ in 0..8 {
            assert_eq!(layout_positions(&graph), first);
        }
    }
}
