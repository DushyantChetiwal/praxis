//! Pure topology previews. Applying a preview, approving reopened locks, and
//! retiring the reported checkpoints are separate responsibilities of the caller.

use crate::{
    ArchitectEdge, ArchitectGraph, ArchitectNode, COLUMN_SPACING, EdgeId, GraphMutationError,
    GraphProblem, MAX_PLAN_DEPTH, NodeId, NodePath, Position, ROW_SPACING,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

/// Canvas coordinates only; moving never reparents a step or changes its identity.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodePosition {
    pub x: f32,
    pub y: f32,
}

impl From<NodePosition> for Position {
    fn from(position: NodePosition) -> Self {
        Self {
            x: position.x,
            y: position.y,
        }
    }
}

/// `parent: []` addresses the root graph; other parents address an existing
/// subplan, including an empty one. Edge endpoints are IDs local to that graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphEdit {
    InsertNode {
        parent: NodePath,
        node: ArchitectNode,
    },
    /// Incident edges are removed too, and listed in the routing consequences.
    RemoveNode {
        path: NodePath,
    },
    MoveNode {
        path: NodePath,
        position: NodePosition,
    },
    /// Replace the complete existing-file list; tool inputs are resolved before preview.
    SetFileSurface {
        /// Full node path, for example ["outer", "step"], not a file path.
        path: NodePath,
        /// Project-relative files, for example ["src/main.rs", "README.md"].
        /// Use root/path when a multi-root path is ambiguous. [] means no existing
        /// files are anticipated. No absolute paths, directories, globs, or '..'.
        file_surface: Vec<String>,
    },
    InsertEdge {
        parent: NodePath,
        edge: ArchitectEdge,
    },
    RemoveEdge {
        parent: NodePath,
        edge_id: EdgeId,
    },
    ReconnectEdge {
        parent: NodePath,
        edge_id: EdgeId,
        from: NodeId,
        to: NodeId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GraphEditPreview {
    /// Candidate only. The caller must obtain approval and compare the live
    /// graph with the source snapshot before replacing it (including positions).
    pub graph: ArchitectGraph,
    /// Directly changed paths, including layout, runtime state, and edge endpoints.
    pub changed_steps: Vec<NodePath>,
    /// Existing paths locked in either snapshot that are reopened or removed,
    /// including composite parents. Newly inserted nodes are not existing locks.
    pub affected_locks: Vec<NodePath>,
    /// Checkpoint keys to retire, whether or not a result is currently attached.
    /// Removed paths are included; historical run records are never modified.
    pub invalidated_steps: Vec<NodePath>,
    pub routing_changes: Vec<String>,
    /// Existing graph validation, including nested paths and unlocked steps.
    pub problems: Vec<GraphProblem>,
    /// Structural validity alone does not authorize applying or running a plan.
    pub is_valid: bool,
    pub ready_to_run: bool,
    /// Any actual edit needs approval, even when it changes only the layout.
    pub requires_approval: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEditError {
    Mutation(GraphMutationError),
    Problem(GraphProblem),
    DuplicateEdge { parent: NodePath, edge_id: EdgeId },
    DepthLimit { path: NodePath },
    InvalidPosition { path: NodePath },
}

impl fmt::Display for GraphEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mutation(error) => fmt::Display::fmt(error, formatter),
            Self::Problem(problem) => fmt::Display::fmt(problem, formatter),
            Self::DuplicateEdge { parent, edge_id } => write!(
                formatter,
                "duplicate connection {} in graph {}",
                edge_id.0,
                graph_name(parent)
            ),
            Self::DepthLimit { path } => write!(
                formatter,
                "step {path} exceeds the maximum plan depth of {MAX_PLAN_DEPTH}"
            ),
            Self::InvalidPosition { path } => {
                write!(formatter, "step {path} needs finite canvas coordinates")
            }
        }
    }
}

impl std::error::Error for GraphEditError {}

impl From<GraphMutationError> for GraphEditError {
    fn from(error: GraphMutationError) -> Self {
        Self::Mutation(error)
    }
}

/// Applies operations sequentially to a private clone, then computes net impact
/// against the source. Failure never exposes a partially applied graph. Cycles
/// and other execution blockers are reported using the existing GraphProblem
/// semantics: bounded/conditional loops are not arbitrarily rejected.
///
/// This deliberately bypasses mutation-time lock refusals only on the clone.
/// Execution-affecting edits reopen impacted steps for explicit review; they
/// never approve or relock them. No runner or checkpoint history is mutated.
///
/// Before topology operations, automatic positions are materialized recursively
/// from the source layout so surviving nodes do not shift with the new topology.
/// Explicit moves and inserted positions win; new unpositioned nodes use the
/// final layout, nudged vertically only when an occupied layout cell conflicts.
/// The nearest free row wins, preferring downward placement on ties. Materializing
/// positions never invalidates execution. This policy is specific to targeted
/// edits, not arbitrary replacements or draft merging.
pub fn preview_graph_edits(
    graph: &ArchitectGraph,
    operations: &[GraphEdit],
) -> Result<GraphEditPreview, GraphEditError> {
    validate_addressability(graph, &NodePath::default())?;
    let has_topology_operations = operations.iter().any(|operation| {
        !matches!(
            operation,
            GraphEdit::MoveNode { .. } | GraphEdit::SetFileSurface { .. }
        )
    });
    let mut target = graph.clone();
    let positioned_source = has_topology_operations.then(|| {
        materialize_positions(&mut target);
        target.clone()
    });
    for operation in operations {
        apply_operation(&mut target, operation)?;
        validate_addressability(&target, &NodePath::default())?;
    }
    if positioned_source.as_ref() == Some(&target)
        && !operations
            .iter()
            .any(|operation| matches!(operation, GraphEdit::MoveNode { .. }))
    {
        // A cancelled topology edit must not turn implicit positions into a
        // persisted change. An explicit move still records its chosen position.
        return Ok(graph_edit_impact(graph, graph.clone()));
    }
    if has_topology_operations {
        place_inserted_nodes(&mut target, &NodePath::default())?;
    }
    Ok(graph_edit_impact(graph, target))
}

fn materialize_positions(graph: &mut ArchitectGraph) {
    graph.place_unpositioned_nodes();
    for node in &mut graph.nodes {
        if let Some(subplan) = node.subplan.as_deref_mut() {
            materialize_positions(subplan);
        }
    }
}

fn place_inserted_nodes(
    graph: &mut ArchitectGraph,
    parent: &NodePath,
) -> Result<(), GraphEditError> {
    if graph.nodes.iter().any(|node| node.position.is_none()) {
        let defaults = crate::layout_positions(graph);
        // Reserve explicit positions even when their nodes come later in the
        // batch. Only nodes inserted without a position remain unpositioned here.
        let mut occupied: Vec<Position> = graph
            .nodes
            .iter()
            .filter_map(|node| node.position)
            .collect();
        for node in &mut graph.nodes {
            if node.position.is_some() {
                continue;
            }
            let path = parent.child(node.id.clone());
            let mut position = defaults
                .get(&node.id)
                .copied()
                .ok_or_else(|| GraphEditError::InvalidPosition { path: path.clone() })?;
            if occupied
                .iter()
                .any(|other| layout_cells_overlap(position, *other))
            {
                // At a fixed column, the nearest free position must lie on a
                // boundary of an occupied row. This avoids an unbounded search.
                position.y = occupied
                    .iter()
                    .flat_map(|other| [other.y + ROW_SPACING, other.y - ROW_SPACING])
                    .filter(|y| {
                        y.is_finite()
                            && !occupied.iter().any(|other| {
                                layout_cells_overlap(
                                    Position {
                                        x: position.x,
                                        y: *y,
                                    },
                                    *other,
                                )
                            })
                    })
                    .min_by(|left, right| {
                        (left - position.y)
                            .abs()
                            .total_cmp(&(right - position.y).abs())
                            .then_with(|| right.total_cmp(left))
                    })
                    .ok_or(GraphEditError::InvalidPosition { path })?;
            }
            node.position = Some(position);
            occupied.push(position);
        }
    }
    for node in &mut graph.nodes {
        if let Some(subplan) = node.subplan.as_deref_mut() {
            place_inserted_nodes(subplan, &parent.child(node.id.clone()))?;
        }
    }
    Ok(())
}

fn layout_cells_overlap(left: Position, right: Position) -> bool {
    (left.x - right.x).abs() < COLUMN_SPACING && (left.y - right.y).abs() < ROW_SPACING
}

/// Previews replacing one graph snapshot with another without applying it.
/// Both snapshots must have unambiguous paths, supported depths, and finite
/// positions. Execution blockers are reported in the preview's `problems`.
///
/// Result-only and lock-only drift is reported in `changed_steps`, but does not
/// invalidate execution. Unaffected results and locks come from `after`; semantic
/// changes clear results and reopen locks only in the returned clone. Dependency
/// traversal uses both snapshots, including their pinned-summary consumers.
/// Neither input nor external checkpoint history is modified. Unlike targeted
/// edits, replacements keep `after`'s positions verbatim, including `None`.
pub fn preview_graph_replacement(
    before: &ArchitectGraph,
    after: &ArchitectGraph,
) -> Result<GraphEditPreview, GraphEditError> {
    validate_addressability(before, &NodePath::default())?;
    validate_addressability(after, &NodePath::default())?;
    Ok(graph_edit_impact(before, after.clone()))
}

fn graph_edit_impact(graph: &ArchitectGraph, mut target: ArchitectGraph) -> GraphEditPreview {
    let mut before_nodes = BTreeMap::new();
    let mut before_graphs = BTreeMap::new();
    index_graph(
        graph,
        &NodePath::default(),
        &mut before_nodes,
        &mut before_graphs,
    );
    let mut after_nodes = BTreeMap::new();
    let mut after_graphs = BTreeMap::new();
    index_graph(
        &target,
        &NodePath::default(),
        &mut after_nodes,
        &mut after_graphs,
    );

    let mut changed = BTreeSet::new();
    let mut execution_changes = BTreeSet::new();
    for path in before_nodes.keys().chain(after_nodes.keys()) {
        match (before_nodes.get(path), after_nodes.get(path)) {
            (Some(before), Some(after)) => {
                // A nested edit invalidates its composite result, not unrelated
                // children of that composite. Compare each node's own payload.
                let mut before_payload = (**before).clone();
                let mut after_payload = (**after).clone();
                before_payload.subplan = None;
                after_payload.subplan = None;
                before_payload.position = None;
                after_payload.position = None;
                // A frozen run may lag behind live results and review state.
                // Those differences are not changes to what a step executes.
                before_payload.result = None;
                after_payload.result = None;
                before_payload.locked = false;
                after_payload.locked = false;
                if before_payload != after_payload {
                    changed.insert(path.clone());
                    execution_changes.insert(path.clone());
                }
                if before.position != after.position
                    || before.result != after.result
                    || before.locked != after.locked
                {
                    changed.insert(path.clone());
                }
            }
            _ => {
                changed.insert(path.clone());
                execution_changes.insert(path.clone());
            }
        }
    }

    let mut routing_changes = Vec::new();
    let parents: BTreeSet<_> = before_graphs
        .keys()
        .chain(after_graphs.keys())
        .cloned()
        .collect();
    for parent in parents {
        let before = before_graphs.get(&parent).copied();
        let after = after_graphs.get(&parent).copied();
        let before_edges = before
            .map(|graph| graph.edges.as_slice())
            .unwrap_or_default();
        let after_edges = after
            .map(|graph| graph.edges.as_slice())
            .unwrap_or_default();
        let edge_ids: BTreeSet<_> = before_edges
            .iter()
            .chain(after_edges)
            .map(|edge| edge.id.clone())
            .collect();
        for edge_id in edge_ids {
            let old = before_edges.iter().find(|edge| edge.id == edge_id);
            let new = after_edges.iter().find(|edge| edge.id == edge_id);
            if old == new {
                continue;
            }
            routing_changes.push(format!(
                "Graph {}, connection {}: {} -> {}",
                graph_name(&parent),
                edge_id.0,
                describe_route(old),
                describe_route(new)
            ));
            for edge in old.into_iter().chain(new) {
                for endpoint in [&edge.from, &edge.to] {
                    let path = parent.child(endpoint.clone());
                    changed.insert(path.clone());
                    execution_changes.insert(path);
                }
            }
        }
        // Connection order can change branch/loop routing even when every
        // connection keeps the same ID and payload.
        let old_order: Vec<_> = before_edges.iter().map(|edge| &edge.id).collect();
        let new_order: Vec<_> = after_edges.iter().map(|edge| &edge.id).collect();
        if old_order != new_order
            && before_edges.len() == after_edges.len()
            && before_edges.iter().all(|edge| after_edges.contains(edge))
        {
            routing_changes.push(format!(
                "Graph {}: connection order changed",
                graph_name(&parent)
            ));
            for edge in before_edges {
                let path = parent.child(edge.from.clone());
                changed.insert(path.clone());
                execution_changes.insert(path);
            }
        }
        let old_nodes: Vec<_> = before
            .into_iter()
            .flat_map(|graph| graph.nodes.iter().map(|node| &node.id))
            .collect();
        let new_nodes: Vec<_> = after
            .into_iter()
            .flat_map(|graph| graph.nodes.iter().map(|node| &node.id))
            .collect();
        if old_nodes != new_nodes
            && old_nodes.len() == new_nodes.len()
            && old_nodes.iter().all(|id| new_nodes.contains(id))
        {
            routing_changes.push(format!("Graph {}: step order changed", graph_name(&parent)));
            for id in old_nodes {
                let path = parent.child(id.clone());
                changed.insert(path.clone());
                execution_changes.insert(path);
            }
        }
        let old_roots = root_ids(before);
        let new_roots = root_ids(after);
        if old_roots != new_roots {
            routing_changes.push(format!(
                "Graph {}: entry steps changed from {old_roots:?} to {new_roots:?}",
                graph_name(&parent)
            ));
            for id in old_roots.iter().chain(&new_roots) {
                if !old_roots.contains(id) || !new_roots.contains(id) {
                    execution_changes.insert(parent.child(id.clone()));
                }
            }
        }
    }

    let invalidated = invalidation_closure(&execution_changes, &before_graphs, &after_graphs);
    let affected_locks = invalidated
        .iter()
        .filter(|path| {
            before_nodes.get(*path).is_some_and(|node| {
                node.locked || after_nodes.get(*path).is_some_and(|node| node.locked)
            })
        })
        .cloned()
        .collect();
    let requires_approval = graph != &target;
    for path in &invalidated {
        if let Some(node) = target.node_at_mut(path) {
            node.result = None;
            node.locked = false;
        }
    }
    let problems = target.blocking_problems();
    let is_valid = problems.iter().all(only_unlocked);
    let ready_to_run = !target.is_empty() && problems.is_empty();
    GraphEditPreview {
        graph: target,
        changed_steps: changed.into_iter().collect(),
        affected_locks,
        invalidated_steps: invalidated.into_iter().collect(),
        routing_changes,
        problems,
        is_valid,
        ready_to_run,
        requires_approval,
    }
}

fn only_unlocked(problem: &GraphProblem) -> bool {
    match problem {
        GraphProblem::Unlocked(_) => true,
        GraphProblem::InSubplan { problem, .. } => only_unlocked(problem),
        _ => false,
    }
}

fn scoped_problem(parent: &NodePath, mut problem: GraphProblem) -> GraphProblem {
    for node in parent.as_slice().iter().rev() {
        problem = GraphProblem::InSubplan {
            node: node.clone(),
            problem: Box::new(problem),
        };
    }
    problem
}

fn validate_addressability(
    graph: &ArchitectGraph,
    parent: &NodePath,
) -> Result<(), GraphEditError> {
    let mut node_ids = BTreeSet::new();
    for node in &graph.nodes {
        let path = parent.child(node.id.clone());
        if path.depth() > MAX_PLAN_DEPTH {
            return Err(GraphEditError::DepthLimit { path });
        }
        if !node_ids.insert(&node.id) {
            return Err(GraphEditError::Problem(scoped_problem(
                parent,
                GraphProblem::DuplicateNode(node.id.clone()),
            )));
        }
        if node
            .position
            .is_some_and(|position| !position.x.is_finite() || !position.y.is_finite())
        {
            return Err(GraphEditError::InvalidPosition { path });
        }
        if let Some(subplan) = &node.subplan {
            validate_addressability(subplan, &path)?;
        }
    }
    let mut edge_ids = BTreeSet::new();
    for edge in &graph.edges {
        if !edge_ids.insert(&edge.id) {
            return Err(GraphEditError::DuplicateEdge {
                parent: parent.clone(),
                edge_id: edge.id.clone(),
            });
        }
    }
    Ok(())
}

fn graph_at_mut<'a>(
    mut graph: &'a mut ArchitectGraph,
    parent: &NodePath,
) -> Result<&'a mut ArchitectGraph, GraphMutationError> {
    let mut walked = NodePath::default();
    for id in parent.iter() {
        walked = walked.child(id.clone());
        let node = graph
            .node_mut(id)
            .ok_or_else(|| GraphMutationError::NodeNotFound {
                path: walked.clone(),
            })?;
        graph = node
            .subplan
            .as_deref_mut()
            .ok_or_else(|| GraphMutationError::MissingSubplan {
                path: walked.clone(),
            })?;
    }
    Ok(graph)
}

fn check_endpoints(
    graph: &ArchitectGraph,
    parent: &NodePath,
    edge: &ArchitectEdge,
) -> Result<(), GraphEditError> {
    for endpoint in [&edge.from, &edge.to] {
        if graph.node(endpoint).is_none() {
            return Err(GraphEditError::Problem(scoped_problem(
                parent,
                GraphProblem::DanglingEdge {
                    edge: edge.id.clone(),
                    missing: endpoint.clone(),
                },
            )));
        }
    }
    Ok(())
}

fn apply_operation(
    graph: &mut ArchitectGraph,
    operation: &GraphEdit,
) -> Result<(), GraphEditError> {
    match operation {
        GraphEdit::InsertNode { parent, node } => {
            let local = graph_at_mut(graph, parent)?;
            if local.node(&node.id).is_some() {
                return Err(GraphEditError::Problem(scoped_problem(
                    parent,
                    GraphProblem::DuplicateNode(node.id.clone()),
                )));
            }
            local.nodes.push(node.clone());
        }
        GraphEdit::RemoveNode { path } => {
            let id = path.leaf().ok_or(GraphMutationError::EmptyPath)?;
            let parent = path.parent().unwrap_or_default();
            let local = graph_at_mut(graph, &parent)?;
            if local.node(id).is_none() {
                return Err(GraphMutationError::NodeNotFound { path: path.clone() }.into());
            }
            local.remove_node(id);
        }
        GraphEdit::MoveNode { path, position } => {
            if !position.x.is_finite() || !position.y.is_finite() {
                return Err(GraphEditError::InvalidPosition { path: path.clone() });
            }
            graph.move_node_at(path, (*position).into())?;
        }
        GraphEdit::SetFileSurface { path, file_surface } => {
            if path.is_empty() {
                return Err(GraphMutationError::EmptyPath.into());
            }
            let node = graph
                .node_at_mut(path)
                .ok_or_else(|| GraphMutationError::NodeNotFound { path: path.clone() })?;
            node.file_surface = Some(file_surface.clone());
        }
        GraphEdit::InsertEdge { parent, edge } => {
            let local = graph_at_mut(graph, parent)?;
            if local.edges.iter().any(|existing| existing.id == edge.id) {
                return Err(GraphEditError::DuplicateEdge {
                    parent: parent.clone(),
                    edge_id: edge.id.clone(),
                });
            }
            check_endpoints(local, parent, edge)?;
            local.edges.push(edge.clone());
        }
        GraphEdit::RemoveEdge { parent, edge_id } => {
            let local = graph_at_mut(graph, parent)?;
            if !local.edges.iter().any(|edge| &edge.id == edge_id) {
                return Err(GraphMutationError::EdgeNotFound {
                    graph: parent.clone(),
                    edge: edge_id.clone(),
                }
                .into());
            }
            local.disconnect(edge_id);
        }
        GraphEdit::ReconnectEdge {
            parent,
            edge_id,
            from,
            to,
        } => {
            let local = graph_at_mut(graph, parent)?;
            let mut replacement = local
                .edges
                .iter()
                .find(|edge| &edge.id == edge_id)
                .cloned()
                .ok_or_else(|| GraphMutationError::EdgeNotFound {
                    graph: parent.clone(),
                    edge: edge_id.clone(),
                })?;
            replacement.from = from.clone();
            replacement.to = to.clone();
            check_endpoints(local, parent, &replacement)?;
            if let Some(edge) = local.edges.iter_mut().find(|edge| &edge.id == edge_id) {
                *edge = replacement;
            }
        }
    }
    Ok(())
}

fn index_graph<'a>(
    graph: &'a ArchitectGraph,
    parent: &NodePath,
    nodes: &mut BTreeMap<NodePath, &'a ArchitectNode>,
    graphs: &mut BTreeMap<NodePath, &'a ArchitectGraph>,
) {
    graphs.insert(parent.clone(), graph);
    for node in &graph.nodes {
        let path = parent.child(node.id.clone());
        nodes.insert(path.clone(), node);
        if let Some(subplan) = &node.subplan {
            index_graph(subplan, &path, nodes, graphs);
        }
    }
}

fn graph_name(parent: &NodePath) -> String {
    if parent.is_empty() {
        "(root)".into()
    } else {
        parent.to_string()
    }
}

fn describe_route(edge: Option<&ArchitectEdge>) -> String {
    match edge {
        Some(edge) => format!(
            "{} -> {} ({:?}, max_repeats={:?})",
            edge.from, edge.to, edge.condition, edge.max_repeats
        ),
        None => "absent".into(),
    }
}

fn root_ids(graph: Option<&ArchitectGraph>) -> Vec<NodeId> {
    graph.map(ArchitectGraph::roots).unwrap_or_default()
}

fn invalidation_closure(
    seeds: &BTreeSet<NodePath>,
    before: &BTreeMap<NodePath, &ArchitectGraph>,
    after: &BTreeMap<NodePath, &ArchitectGraph>,
) -> BTreeSet<NodePath> {
    let mut queue: VecDeque<_> = seeds.iter().cloned().map(|path| (path, true)).collect();
    let mut invalidated = BTreeSet::new();
    let mut expanded = BTreeSet::new();
    while let Some((path, descend)) = queue.pop_front() {
        let first_visit = invalidated.insert(path.clone());
        let expand_children = descend && expanded.insert(path.clone());
        if !first_visit && !expand_children {
            continue;
        }
        if let Some(parent) = path.parent() {
            // Recompute the composite handoff, but preserve unaffected siblings.
            queue.push_back((parent, false));
        }
        let parent = path.parent().unwrap_or_default();
        let Some(id) = path.leaf() else {
            continue;
        };
        for graphs in [before, after] {
            let Some(local) = graphs.get(&parent) else {
                continue;
            };
            if expand_children {
                if let Some(subplan) = graphs.get(&path) {
                    queue.extend(
                        subplan
                            .nodes
                            .iter()
                            .map(|node| (path.child(node.id.clone()), true)),
                    );
                }
            }
            for edge in local.edges_from(id) {
                queue.push_back((parent.child(edge.to.clone()), true));
            }
            // Pinned summaries feed all steps in their containing graph, not
            // just explicit successors (the same scope as incoming_summaries).
            if local.node(id).is_some_and(|node| node.pinned) {
                queue.extend(
                    local
                        .nodes
                        .iter()
                        .filter(|node| &node.id != id)
                        .map(|node| (parent.child(node.id.clone()), true)),
                );
            }
        }
    }
    invalidated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EdgeCondition, StepResult};

    fn path(ids: &[&str]) -> NodePath {
        NodePath(ids.iter().map(|id| NodeId::from(*id)).collect())
    }

    fn settled_node(id: &str) -> ArchitectNode {
        let mut node = ArchitectNode::new(id, id);
        node.locked = true;
        node.position = Some(Position { x: 13.5, y: -27.0 });
        node.result = Some(StepResult {
            summary: format!("Completed {id}"),
            attempt: 2,
        });
        node.chat = Some(crate::acp::SessionId::new(format!("chat-{id}")));
        node
    }

    fn local_graph() -> ArchitectGraph {
        ArchitectGraph {
            nodes: ["a", "b", "c", "sibling"].map(settled_node).to_vec(),
            edges: vec![
                ArchitectEdge::new("ab", "a", "b"),
                ArchitectEdge::new("bc", "b", "c"),
            ],
        }
    }

    fn nested_graph() -> ArchitectGraph {
        let mut left = settled_node("left");
        left.subplan = Some(Box::new(local_graph()));
        let mut right = settled_node("right");
        right.subplan = Some(Box::new(local_graph()));
        ArchitectGraph {
            nodes: vec![left, right, settled_node("ship")],
            edges: vec![ArchitectEdge::new("ship-left", "left", "ship")],
        }
    }

    fn automatic_graph() -> ArchitectGraph {
        let mut graph = local_graph();
        for node in &mut graph.nodes {
            node.position = None;
        }
        graph
    }

    fn effective_position(graph: &ArchitectGraph, path: &NodePath) -> Position {
        let local = graph
            .graph_at(&path.parent().unwrap_or_default())
            .expect("containing graph");
        let node = graph.node_at(path).expect("positioned node");
        node.position.unwrap_or_else(|| {
            crate::layout_positions(local)
                .get(&node.id)
                .copied()
                .expect("automatic position")
        })
    }

    #[test]
    fn inserting_into_a_chain_nudges_only_the_new_default_position() {
        let mut before = ArchitectGraph {
            nodes: ["a", "c"].map(settled_node).to_vec(),
            edges: vec![ArchitectEdge::new("ac", "a", "c")],
        };
        for node in &mut before.nodes {
            node.position = None;
        }
        let snapshot = before.clone();
        let operations = [
            GraphEdit::InsertNode {
                parent: NodePath::default(),
                node: ArchitectNode::new("b", "B"),
            },
            GraphEdit::ReconnectEdge {
                parent: NodePath::default(),
                edge_id: "ac".into(),
                from: "a".into(),
                to: "b".into(),
            },
            GraphEdit::InsertEdge {
                parent: NodePath::default(),
                edge: ArchitectEdge::new("bc", "b", "c"),
            },
        ];
        let mut unpreserved = before.clone();
        for operation in &operations {
            apply_operation(&mut unpreserved, operation).expect("raw chain edit");
        }
        let frozen_c = Position {
            x: COLUMN_SPACING,
            y: 0.0,
        };
        assert_eq!(effective_position(&before, &path(&["c"])), frozen_c);
        assert_eq!(effective_position(&unpreserved, &path(&["b"])), frozen_c);

        let preview = preview_graph_edits(&before, &operations).expect("chain insertion");
        assert_eq!(
            preview.graph.node(&"a".into()).expect("a").position,
            Some(Position::ZERO)
        );
        assert_eq!(
            preview.graph.node(&"c".into()).expect("c").position,
            Some(frozen_c)
        );
        let inserted_position = Position {
            x: COLUMN_SPACING,
            y: ROW_SPACING,
        };
        assert_eq!(
            preview.graph.node(&"b".into()).expect("b").position,
            Some(inserted_position)
        );
        assert!(!layout_cells_overlap(inserted_position, frozen_c));
        assert_eq!(
            preview.invalidated_steps,
            vec![path(&["a"]), path(&["b"]), path(&["c"])]
        );
        assert_eq!(preview.affected_locks, vec![path(&["a"]), path(&["c"])]);
        assert_eq!(
            preview_graph_edits(&before, &operations).expect("repeat preview"),
            preview
        );
        assert_eq!(before, snapshot);
    }

    #[test]
    fn nested_default_placement_reserves_later_explicit_insertions() {
        let nested = ArchitectGraph {
            nodes: vec![ArchitectNode::new("a", "A"), ArchitectNode::new("c", "C")],
            edges: vec![ArchitectEdge::new("ac", "a", "c")],
        };
        let mut container = settled_node("container");
        container.subplan = Some(Box::new(nested));
        let before = ArchitectGraph {
            nodes: vec![container],
            edges: vec![],
        };
        let snapshot = before.clone();
        let parent = path(&["container"]);
        let explicit_position = Position {
            x: COLUMN_SPACING,
            y: ROW_SPACING,
        };
        let mut explicit = ArchitectNode::new("explicit", "Explicit");
        explicit.position = Some(explicit_position);
        let operations = [
            GraphEdit::InsertNode {
                parent: parent.clone(),
                node: ArchitectNode::new("b", "B"),
            },
            GraphEdit::ReconnectEdge {
                parent: parent.clone(),
                edge_id: "ac".into(),
                from: "a".into(),
                to: "b".into(),
            },
            GraphEdit::InsertEdge {
                parent: parent.clone(),
                edge: ArchitectEdge::new("bc", "b", "c"),
            },
            GraphEdit::InsertNode {
                parent: parent.clone(),
                node: explicit,
            },
        ];
        let preview = preview_graph_edits(&before, &operations).expect("nested insertion");
        let after = preview.graph.graph_at(&parent).expect("nested graph");
        assert_eq!(
            after.node(&"a".into()).expect("a").position,
            Some(Position::ZERO)
        );
        assert_eq!(
            after.node(&"c".into()).expect("c").position,
            Some(Position {
                x: COLUMN_SPACING,
                y: 0.0,
            })
        );
        assert_eq!(
            after.node(&"explicit".into()).expect("explicit").position,
            Some(explicit_position)
        );
        assert_eq!(
            after.node(&"b".into()).expect("b").position,
            Some(Position {
                x: COLUMN_SPACING,
                y: -ROW_SPACING,
            })
        );
        assert_eq!(
            preview
                .graph
                .node(&"container".into())
                .expect("container")
                .position,
            before
                .node(&"container".into())
                .expect("container")
                .position
        );
        assert_eq!(before, snapshot);
    }

    #[test]
    fn new_default_positions_reserve_space_for_each_other_without_touching_existing_state() {
        let before = ArchitectGraph {
            nodes: vec![ArchitectNode {
                position: None,
                ..settled_node("a")
            }],
            edges: vec![],
        };
        let preview = preview_graph_edits(
            &before,
            &[
                GraphEdit::InsertNode {
                    parent: NodePath::default(),
                    node: ArchitectNode::new("b", "B"),
                },
                GraphEdit::InsertNode {
                    parent: NodePath::default(),
                    node: ArchitectNode::new("c", "C"),
                },
            ],
        )
        .expect("two insertions");
        let mut expected_a = settled_node("a");
        expected_a.position = Some(Position::ZERO);
        assert_eq!(preview.graph.node(&"a".into()), Some(&expected_a));
        assert_eq!(
            preview.graph.node(&"b".into()).expect("b").position,
            Some(Position {
                x: 0.0,
                y: ROW_SPACING,
            })
        );
        assert_eq!(
            preview.graph.node(&"c".into()).expect("c").position,
            Some(Position {
                x: 0.0,
                y: ROW_SPACING * 2.0,
            })
        );
        assert_eq!(preview.invalidated_steps, vec![path(&["b"]), path(&["c"])]);
        assert!(preview.affected_locks.is_empty());
    }

    #[test]
    fn inserting_a_node_preserves_effective_positions_and_existing_checkpoints() {
        let before = automatic_graph();
        let snapshot = before.clone();
        let operations = [GraphEdit::InsertNode {
            parent: NodePath::default(),
            node: ArchitectNode::new("new", "New"),
        }];
        let mut unpreserved = before.clone();
        apply_operation(&mut unpreserved, &operations[0]).expect("raw insertion");
        assert_eq!(
            effective_position(&before, &path(&["a"])),
            Position { x: 0.0, y: -98.0 }
        );
        assert_eq!(
            effective_position(&unpreserved, &path(&["a"])),
            Position { x: 0.0, y: -196.0 }
        );
        let preview = preview_graph_edits(&before, &operations).expect("insertion preview");
        for (id, expected_position) in [
            ("a", Position { x: 0.0, y: -98.0 }),
            ("b", Position { x: 392.0, y: 0.0 }),
            ("c", Position { x: 784.0, y: 0.0 }),
            ("sibling", Position { x: 0.0, y: 98.0 }),
        ] {
            let mut expected = before.node(&id.into()).expect("source node").clone();
            expected.position = Some(expected_position);
            assert_eq!(preview.graph.node(&id.into()), Some(&expected));
            assert_eq!(
                effective_position(&preview.graph, &path(&[id])),
                expected_position
            );
        }
        assert_eq!(
            preview
                .graph
                .node(&"new".into())
                .expect("inserted node")
                .position,
            Some(Position { x: 0.0, y: 294.0 })
        );
        assert_eq!(preview.invalidated_steps, vec![path(&["new"])]);
        assert!(preview.affected_locks.is_empty());
        assert_eq!(before, snapshot);

        let replacement = preview_graph_replacement(&before, &unpreserved)
            .expect("unrestricted replacement preview");
        assert!(
            replacement
                .graph
                .nodes
                .iter()
                .all(|node| node.position.is_none())
        );
    }

    #[test]
    fn reconnecting_preserves_automatic_positions_without_invalidating_an_unrelated_node() {
        let before = automatic_graph();
        let snapshot = before.clone();
        let operations = [GraphEdit::ReconnectEdge {
            parent: NodePath::default(),
            edge_id: "ab".into(),
            from: "a".into(),
            to: "c".into(),
        }];
        let mut unpreserved = before.clone();
        apply_operation(&mut unpreserved, &operations[0]).expect("raw reconnect");
        assert_eq!(
            effective_position(&before, &path(&["sibling"])),
            Position { x: 0.0, y: 98.0 }
        );
        assert_eq!(
            effective_position(&unpreserved, &path(&["sibling"])),
            Position { x: 0.0, y: 196.0 }
        );
        assert_eq!(
            effective_position(&unpreserved, &path(&["c"])),
            Position { x: 392.0, y: 0.0 }
        );
        let preview = preview_graph_edits(&before, &operations).expect("reconnect preview");
        for id in ["a", "b", "c", "sibling"] {
            assert_eq!(
                preview
                    .graph
                    .node(&id.into())
                    .expect("surviving node")
                    .position,
                Some(effective_position(&before, &path(&[id])))
            );
        }
        let mut expected_sibling = before.node(&"sibling".into()).expect("sibling").clone();
        expected_sibling.position = Some(Position { x: 0.0, y: 98.0 });
        assert_eq!(
            preview.graph.node(&"sibling".into()),
            Some(&expected_sibling)
        );
        let expected_impact = vec![path(&["a"]), path(&["b"]), path(&["c"])];
        assert_eq!(preview.invalidated_steps, expected_impact);
        assert_eq!(preview.affected_locks, expected_impact);
        assert_eq!(before, snapshot);
    }

    #[test]
    fn nested_layout_freezing_preserves_explicit_positions_and_moves_win() {
        let mut before = automatic_graph();
        let mut nested = automatic_graph();
        nested.node_mut(&"a".into()).expect("nested a").subplan = Some(Box::new(automatic_graph()));
        before.node_mut(&"a".into()).expect("a").subplan = Some(Box::new(nested));
        let explicit_path = path(&["a", "a", "b"]);
        let explicit_position = Position { x: -600.0, y: 47.0 };
        before
            .node_at_mut(&explicit_path)
            .expect("explicit node")
            .position = Some(explicit_position);
        let snapshot = before.clone();
        let moved_path = path(&["a", "a", "a"]);
        let moved_position = NodePosition { x: -42.0, y: 63.0 };
        let inserted_position = Position {
            x: 1234.0,
            y: -56.0,
        };
        let mut inserted = ArchitectNode::new("new", "New");
        inserted.position = Some(inserted_position);
        let operations = [
            GraphEdit::MoveNode {
                path: moved_path.clone(),
                position: moved_position,
            },
            GraphEdit::InsertNode {
                parent: path(&["a", "a"]),
                node: inserted,
            },
            GraphEdit::InsertNode {
                parent: path(&["a", "a"]),
                node: ArchitectNode::new("automatic", "Automatic"),
            },
        ];
        let preview = preview_graph_edits(&before, &operations).expect("nested preview");
        for node_path in [
            path(&["a"]),
            path(&["b"]),
            path(&["c"]),
            path(&["sibling"]),
            path(&["a", "a"]),
            path(&["a", "b"]),
            path(&["a", "c"]),
            path(&["a", "sibling"]),
            explicit_path,
            path(&["a", "a", "c"]),
            path(&["a", "a", "sibling"]),
        ] {
            assert_eq!(
                preview
                    .graph
                    .node_at(&node_path)
                    .expect("surviving node")
                    .position,
                Some(effective_position(&before, &node_path))
            );
        }
        let mut expected_moved = before.node_at(&moved_path).expect("moved node").clone();
        expected_moved.position = Some(moved_position.into());
        assert_eq!(preview.graph.node_at(&moved_path), Some(&expected_moved));
        assert!(!preview.invalidated_steps.contains(&moved_path));
        assert!(!preview.affected_locks.contains(&moved_path));
        assert_eq!(
            preview
                .graph
                .node_at(&path(&["a", "a", "new"]))
                .expect("inserted")
                .position,
            Some(inserted_position)
        );
        assert_eq!(
            preview
                .graph
                .node_at(&path(&["a", "a", "automatic"]))
                .expect("automatic insertion")
                .position,
            Some(Position { x: 0.0, y: 294.0 })
        );
        assert_eq!(before, snapshot);
    }

    #[test]
    fn no_op_edits_do_not_materialize_automatic_positions() {
        let before = automatic_graph();
        for operations in [
            vec![],
            vec![GraphEdit::ReconnectEdge {
                parent: NodePath::default(),
                edge_id: "ab".into(),
                from: "a".into(),
                to: "b".into(),
            }],
            vec![
                GraphEdit::InsertNode {
                    parent: NodePath::default(),
                    node: ArchitectNode::new("temporary", "Temporary"),
                },
                GraphEdit::RemoveNode {
                    path: path(&["temporary"]),
                },
            ],
        ] {
            let preview = preview_graph_edits(&before, &operations).expect("no-op preview");
            assert_eq!(preview.graph, before);
            assert!(preview.changed_steps.is_empty());
            assert!(preview.invalidated_steps.is_empty());
            assert!(preview.affected_locks.is_empty());
            assert!(!preview.requires_approval);
        }
        let preview = preview_graph_edits(
            &before,
            &[GraphEdit::MoveNode {
                path: path(&["a"]),
                position: NodePosition { x: 0.0, y: -98.0 },
            }],
        )
        .expect("explicit move to the automatic position");
        let mut expected = before;
        expected.node_mut(&"a".into()).expect("a").position = Some(Position { x: 0.0, y: -98.0 });
        assert_eq!(preview.graph, expected);
        assert!(preview.invalidated_steps.is_empty());
        assert!(preview.affected_locks.is_empty());
        assert!(preview.requires_approval);
    }

    #[test]
    fn replacement_and_targeted_edits_share_the_same_impact() {
        let before = nested_graph();
        let operations = [
            GraphEdit::RemoveNode {
                path: path(&["left", "b"]),
            },
            GraphEdit::MoveNode {
                path: path(&["right", "a"]),
                position: NodePosition { x: 42.0, y: -19.0 },
            },
        ];
        let mut after = before.clone();
        for operation in &operations {
            apply_operation(&mut after, operation).expect("candidate edit");
        }
        let before_snapshot = before.clone();
        let after_snapshot = after.clone();
        assert_eq!(
            preview_graph_replacement(&before, &after).expect("replacement preview"),
            preview_graph_edits(&before, &operations).expect("targeted preview")
        );
        assert_eq!(before, before_snapshot);
        assert_eq!(after, after_snapshot);
    }

    #[test]
    fn replacement_invalidation_uses_pinned_consumers_in_both_snapshots() {
        for (was_pinned, is_pinned) in [(true, true), (true, false), (false, true)] {
            let mut before = nested_graph();
            let changed_path = path(&["left", "a"]);
            let consumer_path = path(&["left", "sibling"]);
            before
                .node_at_mut(&changed_path)
                .expect("changed node")
                .pinned = was_pinned;
            before.node_at_mut(&consumer_path).expect("consumer").locked = false;
            let mut after = before.clone();
            let changed_node = after.node_at_mut(&changed_path).expect("changed node");
            changed_node.intent = "A different execution contract".into();
            changed_node.pinned = is_pinned;
            after.node_at_mut(&consumer_path).expect("consumer").locked = true;
            let unaffected = after
                .node_at_mut(&path(&["right", "a"]))
                .expect("unaffected");
            unaffected.result = Some(StepResult {
                summary: "New live result unrelated to this edit".into(),
                attempt: 5,
            });
            unaffected.locked = false;
            let before_snapshot = before.clone();
            let after_snapshot = after.clone();
            let preview = preview_graph_replacement(&before, &after).expect("replacement");
            let expected = vec![
                path(&["left"]),
                path(&["left", "a"]),
                path(&["left", "b"]),
                path(&["left", "c"]),
                consumer_path,
                path(&["ship"]),
            ];
            assert_eq!(preview.invalidated_steps, expected);
            assert_eq!(preview.affected_locks, expected);
            for invalidated in &preview.invalidated_steps {
                let node = preview
                    .graph
                    .node_at(invalidated)
                    .expect("invalidated node");
                assert!(node.result.is_none());
                assert!(!node.locked);
            }
            assert_eq!(
                preview.graph.node(&"right".into()),
                after.node(&"right".into())
            );
            assert_eq!(before, before_snapshot);
            assert_eq!(after, after_snapshot);
        }
    }

    #[test]
    fn replacement_result_and_lock_drift_does_not_invalidate_execution() {
        let mut before = nested_graph();
        let changed_path = path(&["left", "a"]);
        before.node_mut(&"left".into()).expect("parent").pinned = true;
        let changed_node = before.node_at_mut(&changed_path).expect("changed node");
        changed_node.pinned = true;
        let old_result = changed_node.result.clone();
        let new_result = Some(StepResult {
            summary: "A new runtime checkpoint".into(),
            attempt: 7,
        });
        let before_snapshot = before.clone();
        for (result, locked) in [
            (None, true),
            (new_result.clone(), true),
            (old_result, false),
            (new_result, false),
        ] {
            let mut after = before.clone();
            let changed_node = after.node_at_mut(&changed_path).expect("changed node");
            changed_node.result = result;
            changed_node.locked = locked;
            let after_snapshot = after.clone();
            let preview = preview_graph_replacement(&before, &after).expect("runtime drift");
            assert_eq!(preview.graph, after);
            assert_eq!(preview.changed_steps, vec![changed_path.clone()]);
            assert!(preview.invalidated_steps.is_empty());
            assert!(preview.affected_locks.is_empty());
            assert!(preview.routing_changes.is_empty());
            assert!(preview.is_valid);
            assert_eq!(preview.ready_to_run, locked);
            assert!(preview.requires_approval);
            assert_eq!(before, before_snapshot);
            assert_eq!(after, after_snapshot);
        }
    }

    #[test]
    fn replacement_validates_target_addressability_before_computing_impact() {
        let before = nested_graph();
        let before_snapshot = before.clone();
        let mut duplicate_node = before.clone();
        duplicate_node.nodes.push(settled_node("left"));
        assert!(matches!(
            preview_graph_replacement(&before, &duplicate_node),
            Err(GraphEditError::Problem(GraphProblem::DuplicateNode(_)))
        ));
        let mut duplicate_edge = before.clone();
        duplicate_edge
            .edges
            .push(ArchitectEdge::new("ship-left", "right", "ship"));
        assert!(matches!(
            preview_graph_replacement(&before, &duplicate_edge),
            Err(GraphEditError::DuplicateEdge { .. })
        ));
        let mut invalid_position = before.clone();
        invalid_position
            .node_mut(&"left".into())
            .expect("left")
            .position = Some(Position {
            x: f32::INFINITY,
            y: 0.0,
        });
        assert!(matches!(
            preview_graph_replacement(&before, &invalid_position),
            Err(GraphEditError::InvalidPosition { .. })
        ));
        let mut too_deep = local_graph();
        for _ in 0..MAX_PLAN_DEPTH {
            let mut parent = settled_node("parent");
            parent.subplan = Some(Box::new(too_deep));
            too_deep = ArchitectGraph {
                nodes: vec![parent],
                edges: vec![],
            };
        }
        assert!(matches!(
            preview_graph_replacement(&before, &too_deep),
            Err(GraphEditError::DepthLimit { .. })
        ));
        assert_eq!(before, before_snapshot);
    }

    #[test]
    fn replacement_reports_execution_blockers_without_applying_the_candidate() {
        let mut before = local_graph();
        // Otherwise sibling is the sole root after closing the cycle, and the
        // validator reports the disconnected cycle as unreachable, not endless.
        before
            .edges
            .push(ArchitectEdge::new("entry", "sibling", "a"));
        assert!(before.blocking_problems().is_empty());
        let before_snapshot = before.clone();
        let mut after = before.clone();
        after.edges.push(ArchitectEdge::new("loop", "c", "a"));
        let after_snapshot = after.clone();
        let preview = preview_graph_replacement(&before, &after).expect("loop preview");
        assert!(
            preview
                .problems
                .iter()
                .any(|problem| matches!(problem, GraphProblem::EndlessLoop(_)))
        );
        assert!(!preview.is_valid);
        assert!(!preview.ready_to_run);
        assert_eq!(before, before_snapshot);
        assert_eq!(after, after_snapshot);
    }

    #[test]
    fn nested_removal_reopens_impact_without_touching_siblings_or_source() {
        let graph = nested_graph();
        let snapshot = graph.clone();
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::RemoveNode {
                path: path(&["left", "b"]),
            }],
        )
        .expect("removal should preview");
        let expected = vec![
            path(&["left"]),
            path(&["left", "a"]),
            path(&["left", "b"]),
            path(&["left", "c"]),
            path(&["ship"]),
        ];
        assert_eq!(graph, snapshot);
        assert_eq!(preview.invalidated_steps, expected);
        assert_eq!(preview.affected_locks, expected);
        assert_eq!(
            preview.changed_steps,
            vec![
                path(&["left", "a"]),
                path(&["left", "b"]),
                path(&["left", "c"]),
            ]
        );
        for retained in [path(&["right"]), path(&["left", "sibling"])] {
            assert_eq!(preview.graph.node_at(&retained), graph.node_at(&retained));
        }
        for invalidated in &preview.invalidated_steps {
            if let Some(node) = preview.graph.node_at(invalidated) {
                assert!(!node.locked);
                assert!(node.result.is_none());
                let original = graph.node_at(invalidated).expect("original node");
                assert_eq!(node.position, original.position);
                assert_eq!(node.chat, original.chat);
            }
        }
        assert_eq!(preview.graph.edges, graph.edges);
        for removed_edge in ["ab", "bc"] {
            assert!(
                preview
                    .routing_changes
                    .iter()
                    .any(|change| { change.contains(removed_edge) && change.contains("absent") })
            );
        }
        assert!(preview.is_valid);
        assert!(!preview.ready_to_run);
        assert!(preview.requires_approval);
        assert!(
            preview
                .problems
                .contains(&GraphProblem::Unlocked("left".into()))
        );
    }

    #[test]
    fn canvas_move_preserves_nested_locks_results_and_routing() {
        let graph = nested_graph();
        let moved = path(&["left", "a"]);
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::MoveNode {
                path: moved.clone(),
                position: NodePosition {
                    x: -100.25,
                    y: 45.0,
                },
            }],
        )
        .expect("locked steps may move");
        let mut expected = graph.clone();
        expected.node_at_mut(&moved).expect("moved node").position = Some(Position {
            x: -100.25,
            y: 45.0,
        });
        assert_eq!(preview.graph, expected);
        assert_eq!(preview.changed_steps, vec![moved]);
        assert!(preview.invalidated_steps.is_empty());
        assert!(preview.affected_locks.is_empty());
        assert!(preview.routing_changes.is_empty());
        assert!(preview.problems.is_empty());
        assert!(preview.ready_to_run);
        assert!(preview.requires_approval);
        assert_ne!(
            preview.graph, graph,
            "position changes must stale a revision snapshot"
        );
    }

    #[test]
    fn reconnect_invalidates_transitively_in_both_graphs_and_preserves_edge_metadata() {
        let mut graph = local_graph();
        graph.nodes.extend([settled_node("d"), settled_node("e")]);
        graph.edges.push(ArchitectEdge::new("de", "d", "e"));
        let edge = graph
            .edges
            .iter_mut()
            .find(|edge| edge.id == EdgeId::from("ab"))
            .expect("edge");
        edge.condition = EdgeCondition::Objective {
            statement: "Tests passed".into(),
        };
        edge.max_repeats = Some(3);
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::ReconnectEdge {
                parent: NodePath::default(),
                edge_id: "ab".into(),
                from: "a".into(),
                to: "d".into(),
            }],
        )
        .expect("reconnect");
        assert_eq!(
            preview.invalidated_steps,
            ["a", "b", "c", "d", "e"].map(|id| path(&[id]))
        );
        assert_eq!(
            preview.graph.node(&"sibling".into()),
            graph.node(&"sibling".into())
        );
        let original = graph.edges.first().expect("original edge");
        let edited = preview.graph.edges.first().expect("edited edge");
        assert_eq!(edited.id, original.id);
        assert_eq!(edited.condition, original.condition);
        assert_eq!(edited.max_repeats, original.max_repeats);
        assert_eq!(edited.to, NodeId::from("d"));
        assert_eq!(preview.graph.edges.get(1..), graph.edges.get(1..));
    }

    #[test]
    fn removal_of_a_composite_includes_all_checkpoint_descendants() {
        let graph = nested_graph();
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::RemoveNode {
                path: path(&["left"]),
            }],
        )
        .expect("remove composite");
        for removed in ["a", "b", "c", "sibling"] {
            assert!(
                preview
                    .invalidated_steps
                    .contains(&path(&["left", removed]))
            );
        }
        assert!(preview.invalidated_steps.contains(&path(&["ship"])));
        assert_eq!(
            preview.graph.node(&"right".into()),
            graph.node(&"right".into())
        );
        assert!(preview.graph.edges.is_empty());
    }

    #[test]
    fn incoming_edits_invalidate_descendants_of_a_downstream_composite() {
        let mut graph = nested_graph();
        graph.nodes.push(settled_node("start"));
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::InsertEdge {
                parent: NodePath::default(),
                edge: ArchitectEdge::new("enter", "start", "left"),
            }],
        )
        .expect("insert edge");
        for child in ["a", "b", "c", "sibling"] {
            assert!(preview.invalidated_steps.contains(&path(&["left", child])));
        }
        assert_eq!(
            preview.graph.node(&"right".into()),
            graph.node(&"right".into())
        );
    }

    #[test]
    fn insert_into_empty_nested_graph_uses_full_path_and_drops_supplied_completion() {
        let mut graph = nested_graph();
        graph.node_mut(&"left".into()).expect("left").subplan = Some(Box::default());
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::InsertNode {
                parent: path(&["left"]),
                node: settled_node("a"),
            }],
        )
        .expect("insert into empty subplan");
        let inserted = preview
            .graph
            .node_at(&path(&["left", "a"]))
            .expect("inserted");
        assert!(!inserted.locked);
        assert!(inserted.result.is_none());
        assert_eq!(inserted.position, settled_node("a").position);
        assert_eq!(
            preview.graph.node(&"right".into()),
            graph.node(&"right".into())
        );
        assert_eq!(
            preview.affected_locks,
            vec![path(&["left"]), path(&["ship"])]
        );
    }

    #[test]
    fn pinned_result_invalidation_includes_implicit_consumers() {
        let mut graph = local_graph();
        graph.node_mut(&"a".into()).expect("a").pinned = true;
        let preview = preview_graph_edits(&graph, &[GraphEdit::RemoveNode { path: path(&["a"]) }])
            .expect("remove pinned");
        assert!(preview.invalidated_steps.contains(&path(&["sibling"])));
        assert!(
            preview
                .graph
                .node(&"sibling".into())
                .expect("sibling")
                .result
                .is_none()
        );
    }

    #[test]
    fn operations_are_atomic_and_reject_ambiguous_or_missing_addresses() {
        let graph = nested_graph();
        let snapshot = graph.clone();
        let failures = vec![
            GraphEdit::InsertNode {
                parent: path(&["left"]),
                node: settled_node("a"),
            },
            GraphEdit::InsertEdge {
                parent: path(&["left"]),
                edge: ArchitectEdge::new("ab", "a", "b"),
            },
            GraphEdit::InsertEdge {
                parent: path(&["left"]),
                edge: ArchitectEdge::new("cross", "a", "ship"),
            },
            GraphEdit::ReconnectEdge {
                parent: path(&["left"]),
                edge_id: "ab".into(),
                from: "missing".into(),
                to: "b".into(),
            },
            GraphEdit::RemoveNode {
                path: NodePath::default(),
            },
            GraphEdit::RemoveNode {
                path: path(&["missing"]),
            },
            GraphEdit::RemoveEdge {
                parent: path(&["left"]),
                edge_id: "missing".into(),
            },
            GraphEdit::InsertNode {
                parent: path(&["ship"]),
                node: settled_node("new"),
            },
        ];
        for failure in failures {
            let operations = [
                GraphEdit::RemoveNode {
                    path: path(&["right"]),
                },
                failure,
            ];
            assert!(preview_graph_edits(&graph, &operations).is_err());
            assert_eq!(graph, snapshot);
        }
    }

    #[test]
    fn coordinates_must_be_finite_even_when_inserting_a_subtree() {
        let graph = nested_graph();
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for position in [
                NodePosition { x: invalid, y: 0.0 },
                NodePosition { x: 0.0, y: invalid },
            ] {
                assert!(matches!(
                    preview_graph_edits(
                        &graph,
                        &[GraphEdit::MoveNode {
                            path: path(&["left", "a"]),
                            position,
                        }]
                    ),
                    Err(GraphEditError::InvalidPosition { .. })
                ));
                let mut node = settled_node("new");
                node.position = Some(position.into());
                assert!(matches!(
                    preview_graph_edits(
                        &graph,
                        &[GraphEdit::InsertNode {
                            parent: path(&["left"]),
                            node,
                        }]
                    ),
                    Err(GraphEditError::InvalidPosition { .. })
                ));
            }
        }
    }

    #[test]
    fn inserted_subtrees_validate_duplicate_ids_and_depth() {
        let mut node = settled_node("container");
        let mut subplan = local_graph();
        subplan.nodes.push(settled_node("a"));
        node.subplan = Some(Box::new(subplan));
        assert!(matches!(
            preview_graph_edits(
                &ArchitectGraph::default(),
                &[GraphEdit::InsertNode {
                    parent: NodePath::default(),
                    node,
                }]
            ),
            Err(GraphEditError::Problem(GraphProblem::InSubplan { .. }))
        ));

        let mut node = settled_node("leaf");
        for _ in 1..MAX_PLAN_DEPTH {
            let mut parent = settled_node("container");
            parent.subplan = Some(Box::new(ArchitectGraph {
                nodes: vec![node],
                edges: vec![],
            }));
            node = parent;
        }
        assert!(
            preview_graph_edits(
                &ArchitectGraph::default(),
                &[GraphEdit::InsertNode {
                    parent: NodePath::default(),
                    node: node.clone(),
                }]
            )
            .is_ok()
        );
        let mut parent = settled_node("too-deep");
        parent.subplan = Some(Box::new(ArchitectGraph {
            nodes: vec![node],
            edges: vec![],
        }));
        assert!(matches!(
            preview_graph_edits(
                &ArchitectGraph::default(),
                &[GraphEdit::InsertNode {
                    parent: NodePath::default(),
                    node: parent,
                }]
            ),
            Err(GraphEditError::DepthLimit { .. })
        ));
    }

    #[test]
    fn previews_use_existing_cycle_and_condition_validation_and_never_fake_readiness() {
        let graph = ArchitectGraph {
            nodes: vec![settled_node("a")],
            edges: vec![],
        };
        let mut edge = ArchitectEdge::new("loop", "a", "a");
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::InsertEdge {
                parent: NodePath::default(),
                edge: edge.clone(),
            }],
        )
        .expect("invalid execution still has a preview");
        assert!(
            preview
                .problems
                .contains(&GraphProblem::EndlessLoop("a".into()))
        );
        assert!(!preview.is_valid);
        assert!(!preview.ready_to_run);
        edge.max_repeats = Some(2);
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::InsertEdge {
                parent: NodePath::default(),
                edge: edge.clone(),
            }],
        )
        .expect("bounded loop");
        assert!(preview.is_valid);
        assert!(!preview.ready_to_run, "editing reopened the lock");
        edge.condition = EdgeCondition::Objective {
            statement: " ".into(),
        };
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::InsertEdge {
                parent: NodePath::default(),
                edge,
            }],
        )
        .expect("blank condition preview");
        assert!(
            preview
                .problems
                .contains(&GraphProblem::EmptyCondition("loop".into()))
        );
        assert!(!preview.is_valid);
        assert!(
            !preview_graph_edits(&ArchitectGraph::default(), &[])
                .expect("empty preview")
                .ready_to_run
        );
    }

    #[test]
    fn net_no_op_preserves_everything_and_needs_no_approval() {
        let graph = local_graph();
        let edge = ArchitectEdge::new("temporary", "a", "c");
        let preview = preview_graph_edits(
            &graph,
            &[
                GraphEdit::InsertEdge {
                    parent: NodePath::default(),
                    edge: edge.clone(),
                },
                GraphEdit::RemoveEdge {
                    parent: NodePath::default(),
                    edge_id: edge.id,
                },
                GraphEdit::ReconnectEdge {
                    parent: NodePath::default(),
                    edge_id: "ab".into(),
                    from: "a".into(),
                    to: "b".into(),
                },
            ],
        )
        .expect("net no-op");
        assert_eq!(preview.graph, graph);
        assert!(preview.changed_steps.is_empty());
        assert!(preview.invalidated_steps.is_empty());
        assert!(preview.affected_locks.is_empty());
        assert!(preview.routing_changes.is_empty());
        assert!(!preview.requires_approval);
        assert!(preview.ready_to_run);
    }

    #[test]
    fn setting_a_nested_file_surface_invalidates_results_and_reopens_affected_locks() {
        let graph = nested_graph();
        let changed = path(&["left", "a"]);
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::SetFileSurface {
                path: changed.clone(),
                file_surface: vec!["worktree/src/a.rs".into()],
            }],
        )
        .expect("surface edit");
        for affected in [
            path(&["left"]),
            changed.clone(),
            path(&["left", "b"]),
            path(&["left", "c"]),
            path(&["ship"]),
        ] {
            assert!(preview.invalidated_steps.contains(&affected));
            assert!(preview.affected_locks.contains(&affected));
            let node = preview.graph.node_at(&affected).expect("affected step");
            assert!(!node.locked);
            assert!(node.result.is_none());
            assert!(graph.node_at(&affected).expect("original").locked);
        }
        for untouched in [
            path(&["left", "sibling"]),
            path(&["right"]),
            path(&["right", "a"]),
        ] {
            assert_eq!(preview.graph.node_at(&untouched), graph.node_at(&untouched));
        }
        assert_eq!(
            preview
                .graph
                .node_at(&changed)
                .expect("changed")
                .file_surface,
            Some(vec!["worktree/src/a.rs".into()])
        );
        assert!(preview.routing_changes.is_empty());
        assert!(preview.requires_approval);
        assert!(preview.is_valid);
        let mut replacement = graph.clone();
        replacement
            .node_at_mut(&changed)
            .expect("changed")
            .file_surface = Some(vec!["worktree/src/a.rs".into()]);
        let replacement = preview_graph_replacement(&graph, &replacement).expect("replacement");
        assert_eq!(replacement.invalidated_steps, preview.invalidated_steps);
        assert_eq!(replacement.affected_locks, preview.affected_locks);
    }

    #[test]
    fn file_surface_edits_retain_conflicts_and_legacy_corrections_for_review() {
        let mut graph = local_graph();
        graph.node_mut(&"a".into()).expect("a").file_surface = None;
        let corrected = preview_graph_edits(
            &graph,
            &[GraphEdit::SetFileSurface {
                path: path(&["a"]),
                file_surface: Vec::new(),
            }],
        )
        .expect("legacy correction");
        assert!(corrected.is_valid);
        assert!(corrected.requires_approval);
        assert!(!corrected.ready_to_run);
        let conflicting = preview_graph_edits(
            &graph,
            &[
                GraphEdit::SetFileSurface {
                    path: path(&["a"]),
                    file_surface: vec!["worktree/a.rs".into()],
                },
                GraphEdit::SetFileSurface {
                    path: path(&["sibling"]),
                    file_surface: vec!["WORKTREE/a.rs".into()],
                },
            ],
        )
        .expect("conflicting draft stays inspectable");
        assert!(!conflicting.is_valid);
        assert!(
            conflicting
                .problems
                .iter()
                .any(|problem| matches!(problem, GraphProblem::FileSurfaceOverlap { .. }))
        );
        assert!(crate::PlanRun::start(&conflicting.graph).is_err());
        assert!(
            preview_graph_edits(
                &graph,
                &[GraphEdit::SetFileSurface {
                    path: NodePath::default(),
                    file_surface: Vec::new(),
                }]
            )
            .is_err()
        );
        assert!(
            preview_graph_edits(
                &graph,
                &[GraphEdit::SetFileSurface {
                    path: path(&["missing"]),
                    file_surface: Vec::new(),
                }]
            )
            .is_err()
        );
    }

    #[test]
    fn file_surface_no_ops_preserve_automatic_positions_and_checkpoints() {
        let graph = automatic_graph();
        let first = graph.nodes.first().expect("first");
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::SetFileSurface {
                path: NodePath::root(first.id.clone()),
                file_surface: Vec::new(),
            }],
        )
        .expect("no op");
        assert_eq!(preview.graph, graph);
        assert!(!preview.requires_approval);
        assert!(preview.invalidated_steps.is_empty());
        let preview = preview_graph_edits(
            &graph,
            &[GraphEdit::SetFileSurface {
                path: NodePath::root(first.id.clone()),
                file_surface: vec!["worktree/first.rs".into()],
            }],
        )
        .expect("changed surface");
        assert!(
            preview
                .graph
                .nodes
                .iter()
                .all(|node| node.position.is_none())
        );
    }

    #[test]
    fn graph_edit_schema_and_json_cover_full_nested_payloads() {
        let mut node = settled_node("parent");
        node.subplan = Some(Box::new(local_graph()));
        let operations = vec![
            GraphEdit::InsertNode {
                parent: NodePath::default(),
                node,
            },
            GraphEdit::MoveNode {
                path: path(&["parent", "a"]),
                position: NodePosition { x: 2.5, y: -4.0 },
            },
            GraphEdit::SetFileSurface {
                path: path(&["parent", "a"]),
                file_surface: vec!["worktree/src/a.rs".into()],
            },
            GraphEdit::InsertEdge {
                parent: path(&["parent"]),
                edge: ArchitectEdge::new("ac", "a", "c"),
            },
            GraphEdit::ReconnectEdge {
                parent: path(&["parent"]),
                edge_id: "ac".into(),
                from: "b".into(),
                to: "c".into(),
            },
            GraphEdit::RemoveEdge {
                parent: path(&["parent"]),
                edge_id: "ac".into(),
            },
            GraphEdit::RemoveNode {
                path: path(&["parent", "sibling"]),
            },
        ];
        let json = serde_json::to_value(&operations).expect("serialize operations");
        assert_eq!(
            serde_json::from_value::<Vec<GraphEdit>>(json).expect("deserialize operations"),
            operations
        );
        let schema = serde_json::to_string(&schemars::schema_for!(GraphEdit)).expect("schema");
        for field in [
            "insert_node",
            "remove_node",
            "move_node",
            "insert_edge",
            "remove_edge",
            "reconnect_edge",
            "set_file_surface",
            "file_surface",
            "position",
            "parent",
            "subplan",
        ] {
            assert!(schema.contains(field), "missing {field}");
        }
        let preview =
            preview_graph_edits(&ArchitectGraph::default(), &operations).expect("preview");
        let json = serde_json::to_value(&preview).expect("serialize preview");
        assert_eq!(
            serde_json::from_value::<GraphEditPreview>(json).expect("deserialize preview"),
            preview
        );
    }
}
