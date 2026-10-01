use std::{
    cell::RefCell,
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};

use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, ensure};
use architect::{ArchitectGraph, GraphEdit, GraphEditPreview, NodePath, preview_graph_edits};
use gpui::{App, SharedString, Task, WeakEntity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::architect_run_tool::main_thread;
use crate::{
    AgentTool, ArchitectRunToolOutput, Thread, ToolCallEventStream,
    ToolCapability, ToolInput, ToolPermissionContext, apply_architect_graph_update,
    architect_run_readiness, preview_architect_graph_update,
};

const MAX_OUTPUT_BYTES: usize = 32_768;
const MAX_PREVIEWS: usize = 8;
const MAX_PREVIEW_BYTES: usize = 1_048_576;
const PREVIEW_LIFETIME: Duration = Duration::from_secs(300);

/// Inspect the authoritative plan as paginated records: graphs with full-path
/// entries, nodes with their own fields (never flattened child briefs), and
/// edges with full-path endpoints. Execution readiness is attached to nodes.
/// Follow next_offset; restart at zero if revision changes. No execution occurs.
/// Pages contain at most 20 records and 32768 serialized bytes. A single record
/// too large to fit returns an error, never silently truncates authoritative fields.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectArchitectPlanToolInput {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "page_limit")]
    pub limit: usize,
    /// Optional revision from the preceding page; refuses mixed-revision reads.
    #[serde(default)]
    pub revision: Option<u64>,
}

fn page_limit() -> usize {
    10
}

pub struct InspectArchitectPlanTool {
    thread: WeakEntity<Thread>,
}

impl InspectArchitectPlanTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self { thread }
    }
}

fn graph_records(
    graph: &ArchitectGraph,
    parent: &NodePath,
    readiness: &Value,
    records: &mut Vec<Value>,
) -> Result<()> {
    records.push(json!({
        "kind": "graph", "parent": parent,
        "entries": graph.roots().into_iter().map(|id| parent.child(id)).collect::<Vec<_>>(),
        "node_count": graph.nodes.len(), "edge_count": graph.edges.len(),
    }));
    for node in &graph.nodes {
        let path = parent.child(node.id.clone());
        let mut own = node.clone();
        own.subplan = None;
        let mut fields = serde_json::to_value(own)?;
        fields.as_object_mut().context("Expected node fields.")?.remove("subplan");
        records.push(json!({
            "kind": "node", "node_path": path, "fields": fields,
            "has_subplan": node.subplan.is_some(),
            "model": node.model,
            "model_source": if node.model.is_some() { "step_override" } else { "plan_inheritance" },
            "execution": readiness["steps"].as_array().and_then(|steps| steps.iter().find(|step| step["path"] == json!(path))),
        }));
        if let Some(subplan) = &node.subplan {
            graph_records(subplan, &path, readiness, records)?;
        }
    }
    for edge in &graph.edges {
        records.push(json!({
            "kind": "edge", "parent": parent, "fields": edge,
            "from_path": parent.child(edge.from.clone()), "to_path": parent.child(edge.to.clone()),
        }));
    }
    Ok(())
}

fn checked_output(value: Value) -> Result<Value> {
    ensure!(serde_json::to_vec(&json!({"data": &value}))?.len() <= MAX_OUTPUT_BYTES,
        "The response exceeds 32768 bytes. Use a smaller, targeted edit or inspection page.");
    Ok(value)
}

fn record_page(records: &[Value], offset: usize, limit: usize, revision: u64) -> Result<Value> {
    ensure!((1..=20).contains(&limit), "limit must be between 1 and 20.");
    ensure!(offset <= records.len(), "offset is out of range.");
    let mut end = offset.saturating_add(limit).min(records.len());
    loop {
        let page = json!({
            "revision": revision, "records": &records[offset..end], "total_records": records.len(),
            "next_offset": (end < records.len()).then_some(end),
        });
        match checked_output(page) {
            Ok(page) => return Ok(page),
            Err(error) if end <= offset + 1 => return Err(error),
            Err(_) => end -= 1,
        }
    }
}

impl AgentTool for InspectArchitectPlanTool {
    type Input = InspectArchitectPlanToolInput;
    type Output = ArchitectRunToolOutput;
    const NAME: &'static str = "inspect_architect_plan";

    fn capability() -> ToolCapability {
        ToolCapability::ReadOnly
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }
    fn initial_title(&self, _input: Result<Self::Input, Value>, _cx: &mut App) -> SharedString {
        "Inspect Architect plan".into()
    }
    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _events: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let result = async {
                let input = input.recv().await?;
                cx.update(|cx| {
                    let thread = main_thread(&self.thread, cx)?;
                    let owner = thread.read(cx);
                    ensure!(input.revision.is_none_or(|revision| revision == owner.architect_revision()),
                        "The plan changed. Restart inspection at offset zero.");
                    let graph = owner.architect_graph().context("There is no plan to inspect.")?;
                    let mut records = Vec::new();
                    graph_records(graph, &NodePath::default(), &architect_run_readiness(owner), &mut records)?;
                    record_page(&records, input.offset, input.limit, owner.architect_revision())
                })
            }
            .await;
            result.map(|data| ArchitectRunToolOutput::Success { data })
                .map_err(ArchitectRunToolOutput::error)
        })
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchitectPlanEditAction {
    #[default]
    Preview,
    Apply,
}

/// Preview targeted topology edits without mutating the plan (default action).
/// To apply, send only action=apply and the exact preview_token returned by a
/// successful preview. Tokens expire after five minutes and are single-use.
/// Apply always checks permission settings, even in Architect mode. Plan mode
/// cannot mutate. Stop execution before applying; unsupported checkpoint rebases
/// are refused. Execution edits reopen affected locks; this tool never approves
/// locks, starts execution, or silently clears checkpoints. Impact includes
/// changes from the frozen checkpoint, so a move on a diverged live graph can
/// invalidate execution. Missing runtime impact fields prevent issuing a token.
/// Rebases requiring reconstruction support flat acyclic plans only and cannot
/// infer retained conditional verdicts or discard oversized checkpoint history.
/// Unchanged topology can preserve existing lanes when the runtime permits it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArchitectPlanToolInput {
    #[serde(default)]
    pub action: ArchitectPlanEditAction,
    /// Sequential GraphEdit operations, at most 100. Only for preview.
    #[serde(default)]
    pub edits: Vec<GraphEdit>,
    /// Only for apply. Bound to the graph, revision, run ID, event cursor, and expiry.
    #[serde(default)]
    pub preview_token: Option<String>,
}

impl EditArchitectPlanToolInput {
    fn validate(&self) -> Result<()> {
        match self.action {
            ArchitectPlanEditAction::Preview => {
                ensure!(self.preview_token.is_none(), "Preview does not accept a token.");
                ensure!((1..=100).contains(&self.edits.len()), "Preview requires 1 to 100 edits.");
                ensure!(serde_json::to_vec(&self.edits)?.len() <= MAX_PREVIEW_BYTES,
                    "The edit batch exceeds the one-MiB input limit.");
            }
            ArchitectPlanEditAction::Apply => {
                ensure!(self.edits.is_empty(), "Apply accepts only a preview_token, never new edits.");
                Uuid::parse_str(self.preview_token.as_deref().context("Apply requires a preview_token.")?)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RuntimeImpact {
    changed_steps: Vec<NodePath>,
    invalidated_paths: Vec<NodePath>,
    affected_locks: Vec<NodePath>,
    retained_results: Vec<NodePath>,
    requires_review: bool,
    ready_to_run: bool,
    can_apply: bool,
}

impl RuntimeImpact {
    fn read(runtime: &Value, preview: &GraphEditPreview) -> Result<Self> {
        // No defaults: an absent field is unknown impact, never an empty set.
        let mut impact: Self = serde_json::from_value(runtime.clone())
            .context("Runtime impact is incomplete; changed_steps, invalidated_paths, affected_locks, retained_results, requires_review, ready_to_run, and can_apply are required. Preview again after the runtime exposes them.")?;
        ensure!(impact.changed_steps.iter().chain(&impact.invalidated_paths)
            .chain(&impact.affected_locks).chain(&impact.retained_results)
            .all(|path| !path.is_empty()),
            "Runtime impact contains an empty step path. No apply token can be issued.");
        impact.changed_steps = impact.changed_steps.into_iter()
            .chain(preview.changed_steps.iter().cloned())
            .collect::<BTreeSet<_>>().into_iter().collect();
        impact.invalidated_paths = impact.invalidated_paths.into_iter()
            .chain(preview.invalidated_steps.iter().cloned())
            .collect::<BTreeSet<_>>().into_iter().collect();
        impact.affected_locks = impact.affected_locks.into_iter()
            .chain(preview.affected_locks.iter().cloned())
            .collect::<BTreeSet<_>>().into_iter().collect();
        ensure!(impact.retained_results.iter().all(|path| !impact.invalidated_paths.contains(path)),
            "Runtime impact both retains and invalidates a result. Preview again after the runtime is consistent.");
        Ok(impact)
    }
}

struct StoredPreview {
    token: Uuid,
    source: ArchitectGraph,
    revision: u64,
    run_id: Option<Uuid>,
    event_sequence: u64,
    expires: Instant,
    preview: GraphEditPreview,
    runtime: Value,
    impact: RuntimeImpact,
}

impl StoredPreview {
    fn check(
        &self,
        graph: Option<&ArchitectGraph>,
        revision: u64,
        run_id: Option<Uuid>,
        event_sequence: u64,
        now: Instant,
    ) -> Result<()> {
        ensure!(now < self.expires, "Preview expired. Preview the edit again.");
        ensure!(graph == Some(&self.source) && revision == self.revision
            && run_id == self.run_id && event_sequence == self.event_sequence,
            "The plan or run changed. Preview the edit again before applying.");
        Ok(())
    }

    fn check_runtime(&self, runtime: &Value) -> Result<()> {
        let impact = RuntimeImpact::read(runtime, &self.preview)?;
        ensure!(runtime == &self.runtime && impact == self.impact,
            "Runtime impact changed after preview. Preview the edit again before applying.");
        ensure!(self.preview.is_valid && impact.can_apply,
            "The edit cannot be applied safely: {}. Preview it again.", runtime["reason"]);
        Ok(())
    }

    fn permission_title(&self) -> String {
        format!(
            "Apply Architect preview {}: {} changed steps, {} affected locks, {} invalidated step checkpoints, {} retained results; requires review: {}",
            self.token,
            self.impact.changed_steps.len(),
            self.impact.affected_locks.len(),
            self.impact.invalidated_paths.len(),
            self.impact.retained_results.len(),
            self.impact.requires_review,
        )
    }
}

pub struct EditArchitectPlanTool {
    thread: WeakEntity<Thread>,
    previews: RefCell<Vec<StoredPreview>>,
}

impl EditArchitectPlanTool {
    pub fn new(thread: WeakEntity<Thread>) -> Self {
        Self {
            thread,
            previews: RefCell::new(Vec::new()),
        }
    }

    fn preview(&self, input: &EditArchitectPlanToolInput, cx: &App) -> Result<Value> {
        let thread = main_thread(&self.thread, cx)?;
        let owner = thread.read(cx);
        let source = owner.architect_graph().context("There is no plan to edit.")?;
        ensure!(serde_json::to_vec(source)?.len() <= MAX_PREVIEW_BYTES,
            "The source graph exceeds the one-MiB preview storage limit.");
        let preview = preview_graph_edits(source, &input.edits)?;
        ensure!(serde_json::to_vec(&preview)?.len() <= MAX_PREVIEW_BYTES,
            "The candidate exceeds the one-MiB preview storage limit.");
        let runtime = preview_architect_graph_update(owner, &preview.graph, &preview.invalidated_steps);
        let impact = RuntimeImpact::read(&runtime, &preview);
        let can_apply = preview.is_valid && impact.as_ref().is_ok_and(|impact| impact.can_apply);
        let token = can_apply.then(Uuid::new_v4);
        let disclosed = impact.as_ref().ok();
        let output = checked_output(json!({
            "preview_token": token, "expires_in_ms": token.map(|_| PREVIEW_LIFETIME.as_millis()),
            "revision": owner.architect_revision(), "run_id": owner.architect_run().map(|run| run.id()),
            "event_sequence": owner.architect_event_sequence(),
            "changed_steps": disclosed.map(|impact| &impact.changed_steps),
            "affected_locks": disclosed.map(|impact| &impact.affected_locks),
            "invalidated_steps": disclosed.map(|impact| &impact.invalidated_paths),
            "retained_results": disclosed.map(|impact| &impact.retained_results),
            "requires_review": disclosed.map(|impact| impact.requires_review),
            "routing_changes": preview.routing_changes,
            "problems": preview.problems, "is_valid": preview.is_valid,
            "ready_to_run": disclosed.map(|impact| impact.ready_to_run),
            "requires_approval": true,
            "can_apply": can_apply, "runtime": runtime,
            "impact_error": impact.as_ref().err().map(|error| format!("{error:#}")),
            "note": "Invalidations and affected locks include runtime checkpoint impact, not just the live-graph edit. Null impact fields mean unknown, not zero; no token is issued. Apply never approves reopened locks, starts a run, or silently discards a checkpoint.",
        }))?;
        if let Some(token) = token {
            let now = Instant::now();
            let mut previews = self.previews.borrow_mut();
            previews.retain(|preview| preview.expires > now);
            if previews.len() >= MAX_PREVIEWS {
                previews.remove(0);
            }
            previews.push(StoredPreview {
                token, source: source.clone(), revision: owner.architect_revision(),
                run_id: owner.architect_run().map(|run| run.id()),
                event_sequence: owner.architect_event_sequence(),
                expires: now + PREVIEW_LIFETIME, preview,
                runtime, impact: impact?,
            });
        }
        Ok(output)
    }

    fn check_apply(&self, stored: &StoredPreview, cx: &App) -> Result<gpui::Entity<Thread>> {
        let thread = main_thread(&self.thread, cx)?;
        let owner = thread.read(cx);
        ensure!(Self::capability().is_allowed_in(owner.session_mode()),
            "Plan mode cannot apply edits. Ask the user to switch to Architect or Build.");
        stored.check(
            owner.architect_graph(),
            owner.architect_revision(),
            owner.architect_run().map(|run| run.id()),
            owner.architect_event_sequence(),
            Instant::now(),
        )?;
        let runtime = preview_architect_graph_update(owner, &stored.preview.graph, &stored.preview.invalidated_steps);
        stored.check_runtime(&runtime)?;
        Ok(thread)
    }
}

impl AgentTool for EditArchitectPlanTool {
    type Input = EditArchitectPlanToolInput;
    type Output = ArchitectRunToolOutput;
    const NAME: &'static str = "edit_architect_plan";

    fn capability() -> ToolCapability {
        ToolCapability::ConversationMutation
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Edit
    }
    fn initial_title(&self, input: Result<Self::Input, Value>, _cx: &mut App) -> SharedString {
        if input.is_ok_and(|input| input.action == ArchitectPlanEditAction::Apply) {
            "Apply Architect plan edit".into()
        } else {
            "Preview Architect plan edit".into()
        }
    }
    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        events: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let result = async {
                let input = input.recv().await?;
                input.validate()?;
                if input.action == ArchitectPlanEditAction::Preview {
                    return cx.update(|cx| self.preview(&input, cx));
                }
                let token = Uuid::parse_str(input.preview_token.as_deref().context("Missing preview token.")?)?;
                // Consume before awaiting approval: concurrent calls cannot reuse an approval.
                let stored = {
                    let mut previews = self.previews.borrow_mut();
                    let index = previews.iter().position(|preview| preview.token == token)
                        .context("Unknown or consumed preview token. Preview the edit again.")?;
                    previews.remove(index)
                };
                cx.update(|cx| self.check_apply(&stored, cx))?;
                cx.update(|cx| events.authorize(
                    stored.permission_title(),
                    ToolPermissionContext::new(Self::NAME, vec!["apply".into()]), cx,
                )).await?;
                cx.update(|cx| {
                    let thread = self.check_apply(&stored, cx)?;
                    apply_architect_graph_update(&thread, stored.preview.graph, &stored.preview.invalidated_steps, cx)?;
                    Ok(json!({"applied": true, "revision": thread.read(cx).architect_revision(),
                        "note": "Review and explicitly approve reopened locks before execution."}))
                })
            }
            .await;
            result.map(|data| ArchitectRunToolOutput::Success { data })
                .map_err(ArchitectRunToolOutput::error)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use architect::ArchitectNode;
    use crate::SessionMode;
    use crate::tools::architect_run_tool::architect_tool_test_session;
    use gpui::{Entity, TestAppContext};
    use settings::{Settings as _, ToolPermissionMode};

    fn plan(thread: &Entity<Thread>, cx: &mut TestAppContext) -> ArchitectGraph {
        let mut step = ArchitectNode::new("step", "Step");
        step.locked = true;
        let graph = ArchitectGraph { nodes: vec![step], edges: vec![] };
        thread.update(cx, |thread, cx| {
            thread.set_session_mode(SessionMode::Architect, cx);
            thread.set_architect_graph(Some(graph.clone()), cx);
        });
        graph
    }

    fn permission(mode: ToolPermissionMode, cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings = agent_settings::AgentSettings::get_global(cx).clone();
            settings.tool_permissions.tools.insert(EditArchitectPlanTool::NAME.into(), agent_settings::ToolRules {
                default: Some(mode), always_allow: vec![], always_deny: vec![],
                always_confirm: vec![], invalid_patterns: vec![],
            });
            agent_settings::AgentSettings::override_global(settings, cx);
        });
    }

    fn runtime_impact_fixture() -> Value {
        json!({
            "changed_steps": [],
            "invalidated_paths": [],
            "affected_locks": [],
            "retained_results": [],
            "requires_review": false,
            "ready_to_run": true,
            "can_apply": true,
        })
    }

    #[test]
    fn runtime_impact_is_required_and_never_defaults_to_zero() {
        let preview = preview_graph_edits(&ArchitectGraph::default(), &[]).unwrap();
        let runtime = runtime_impact_fixture();
        for field in [
            "changed_steps", "invalidated_paths", "affected_locks", "retained_results",
            "requires_review", "ready_to_run", "can_apply",
        ] {
            let mut missing = runtime.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(RuntimeImpact::read(&missing, &preview).is_err(), "{field}");
            let mut malformed = runtime.clone();
            malformed[field] = Value::Null;
            assert!(RuntimeImpact::read(&malformed, &preview).is_err(), "{field}");
        }
    }

    #[test]
    fn disclosed_impact_unions_runtime_and_live_graph_paths() {
        let mut graph = ArchitectGraph::default();
        let mut node = ArchitectNode::new("step", "Step");
        node.locked = true;
        graph.nodes.push(node);
        let preview = preview_graph_edits(&graph, &[GraphEdit::RemoveNode {
            path: NodePath::root("step".into()),
        }]).unwrap();
        let mut runtime = runtime_impact_fixture();
        runtime["changed_steps"] = json!([["outer", "frozen"]]);
        runtime["invalidated_paths"] = json!([["outer", "frozen"], ["step"]]);
        runtime["affected_locks"] = json!([["outer"], ["outer", "frozen"]]);
        runtime["requires_review"] = json!(true);
        runtime["ready_to_run"] = json!(false);
        let impact = RuntimeImpact::read(&runtime, &preview).unwrap();
        assert_eq!(json!(impact.changed_steps), json!([["outer", "frozen"], ["step"]]));
        assert_eq!(json!(impact.invalidated_paths), json!([["outer", "frozen"], ["step"]]));
        assert_eq!(json!(impact.affected_locks), json!([["outer"], ["outer", "frozen"], ["step"]]));
        runtime["retained_results"] = json!([["step"]]);
        assert!(RuntimeImpact::read(&runtime, &preview).is_err());
    }

    #[test]
    fn stored_preview_rejects_any_runtime_impact_drift() {
        let graph = ArchitectGraph::default();
        let preview = preview_graph_edits(&graph, &[]).unwrap();
        let runtime = runtime_impact_fixture();
        let stored = StoredPreview {
            token: Uuid::new_v4(), source: graph, revision: 0, run_id: None,
            event_sequence: 0, expires: Instant::now() + PREVIEW_LIFETIME,
            impact: RuntimeImpact::read(&runtime, &preview).unwrap(),
            preview, runtime: runtime.clone(),
        };
        stored.check_runtime(&runtime).unwrap();
        for (field, changed) in [
            ("changed_steps", json!([["step"]])),
            ("invalidated_paths", json!([["step"]])),
            ("affected_locks", json!([["step"]])),
            ("retained_results", json!([["step"]])),
            ("requires_review", json!(true)),
            ("ready_to_run", json!(false)),
            ("can_apply", json!(false)),
            ("requires_restart", json!(true)),
            ("checkpoint_error", json!("Checkpoint exceeds the save budget")),
        ] {
            let mut current = runtime.clone();
            current[field] = changed;
            assert!(stored.check_runtime(&current).is_err(), "{field}");
        }
    }

    fn move_input() -> EditArchitectPlanToolInput {
        serde_json::from_value(json!({"edits": [{
            "kind": "move_node", "path": ["step"], "position": {"x": 12.0, "y": 24.0}
        }]})).unwrap()
    }

    fn apply_input(preview: &Value) -> ToolInput<EditArchitectPlanToolInput> {
        ToolInput::ready(json!({"action": "apply", "preview_token": preview["preview_token"]}))
    }

    #[gpui::test]
    async fn preview_is_pure_and_apply_honors_denial_in_architect_mode(cx: &mut TestAppContext) {
        let (_connection, _agent, thread, _coordinator) = architect_tool_test_session(cx).await;
        let graph = plan(&thread, cx);
        permission(ToolPermissionMode::Deny, cx);
        let tool = Arc::new(EditArchitectPlanTool::new(thread.downgrade()));
        let revision = thread.read_with(cx, |thread, _| thread.architect_revision());
        let (events, _receiver) = ToolCallEventStream::test();
        let result = cx.update(|cx| tool.clone().run(ToolInput::resolved(move_input()), events, cx)).await.unwrap();
        let ArchitectRunToolOutput::Success { data: preview } = result else { panic!("expected preview"); };
        assert_eq!(preview["can_apply"], true);
        assert_eq!(preview["invalidated_steps"], json!([]));
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.architect_revision(), revision);
            assert_eq!(thread.architect_graph(), Some(&graph));
            assert!(thread.architect_run().is_none());
        });
        let (events, _receiver) = ToolCallEventStream::test();
        assert!(cx.update(|cx| tool.clone().run(apply_input(&preview), events, cx)).await.is_err());
        thread.read_with(cx, |thread, _| assert_eq!(thread.architect_graph(), Some(&graph)));
        assert!(tool.previews.borrow().is_empty(), "denied token is consumed");
    }

    #[gpui::test]
    async fn topology_preview_never_approves_reopened_locks(cx: &mut TestAppContext) {
        let (_connection, _agent, thread, _coordinator) = architect_tool_test_session(cx).await;
        plan(&thread, cx);
        permission(ToolPermissionMode::Allow, cx);
        let tool = Arc::new(EditArchitectPlanTool::new(thread.downgrade()));
        let input: EditArchitectPlanToolInput = serde_json::from_value(json!({"edits": [
            {"kind": "insert_node", "parent": [], "node": {"id": "next", "title": "Next"}},
            {"kind": "insert_edge", "parent": [], "edge": {"id": "route", "from": "step", "to": "next"}}
        ]})).unwrap();
        let preview = cx.update(|cx| tool.preview(&input, cx)).unwrap();
        assert_eq!(preview["ready_to_run"], false);
        assert!(preview["affected_locks"].as_array().unwrap().contains(&json!(["step"])));
        assert_eq!(preview["can_apply"], true);
        assert_eq!(preview["requires_review"], true);
        assert!(preview["preview_token"].is_string());
        let (events, _receiver) = ToolCallEventStream::test();
        assert!(cx.update(|cx| tool.run(apply_input(&preview), events, cx)).await.is_ok());
        thread.read_with(cx, |thread, _| {
            let graph = thread.architect_graph().unwrap();
            assert_eq!(graph.nodes.len(), 2);
            assert!(graph.nodes.iter().all(|node| !node.locked));
            assert!(thread.architect_run().is_none());
        });
    }

    #[gpui::test]
    async fn preview_storage_is_bounded_and_active_runs_cannot_apply(cx: &mut TestAppContext) {
        let (_connection, _agent, thread, _coordinator) = architect_tool_test_session(cx).await;
        plan(&thread, cx);
        let tool = EditArchitectPlanTool::new(thread.downgrade());
        let first = cx.update(|cx| tool.preview(&move_input(), cx)).unwrap();
        for _ in 0..MAX_PREVIEWS {
            cx.update(|cx| tool.preview(&move_input(), cx)).unwrap();
        }
        assert_eq!(tool.previews.borrow().len(), MAX_PREVIEWS);
        assert!(!tool.previews.borrow().iter().any(|stored| json!(stored.token) == first["preview_token"]));
        thread.update(cx, |thread, cx| {
            thread.start_architect_run(NodePath::root("step".into()), "Step".into(), Task::ready(()), cx);
        });
        let preview = cx.update(|cx| tool.preview(&move_input(), cx)).unwrap();
        assert_eq!(preview["can_apply"], false);
        assert!(preview["preview_token"].is_null());
        assert!(preview["runtime"]["reason"].as_str().unwrap().contains("Stop the run"));
    }

    #[gpui::test]
    async fn stopped_run_move_discloses_frozen_impact_and_rechecks_it_after_approval(cx: &mut TestAppContext) {
        let (_connection, _agent, thread, coordinator) = architect_tool_test_session(cx).await;
        let mut graph = plan(&thread, cx);
        let mut other = ArchitectNode::new("other", "Other");
        other.locked = true;
        graph.nodes.push(other);
        cx.update(|cx| {
            thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph.clone()), cx));
            crate::start_architect_run(thread.clone(), coordinator.clone(), graph, cx).unwrap();
            crate::stop_architect_run(&thread, None, cx);
        });
        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(|graph| {
                graph.node_at_mut(&NodePath::root("step".into())).unwrap().intent = "Changed on the live canvas".into();
            }, cx);
        });
        permission(ToolPermissionMode::Confirm, cx);
        let tool = Arc::new(EditArchitectPlanTool::new(thread.downgrade()));
        let preview = cx.update(|cx| tool.preview(&move_input(), cx)).unwrap();
        assert_eq!(preview["can_apply"], true);
        assert_eq!(preview["invalidated_steps"], json!([["step"]]));
        assert_eq!(preview["affected_locks"], json!([["step"]]));
        assert_eq!(preview["retained_results"], json!([]));
        assert_eq!(preview["requires_review"], true);
        assert_eq!(preview["ready_to_run"], false);
        {
            let stored = tool.previews.borrow();
            let stored = stored.first().unwrap();
            assert!(stored.preview.invalidated_steps.is_empty(), "the canvas move alone has no execution impact");
            assert!(stored.preview.affected_locks.is_empty());
        }
        let (events, mut receiver) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(apply_input(&preview), events, cx));
        let authorization = receiver.expect_authorization().await;
        let title = authorization.tool_call.fields.title.as_deref().unwrap();
        assert!(title.contains("1 affected locks"));
        assert!(title.contains("1 invalidated step checkpoints"));
        assert!(title.contains("0 retained results"));
        assert!(title.contains("requires review: true"));
        let (live, revision, sequence, run_id, control) = thread.read_with(cx, |thread, _| {
            let run = thread.architect_run().unwrap();
            assert!(!run.is_running());
            (
                thread.architect_graph().unwrap().clone(), thread.architect_revision(),
                thread.architect_event_sequence(), run.id(), run.control().unwrap().clone(),
            )
        });
        // Change only the frozen execution state, leaving all root bindings intact.
        // This specifically exercises the post-authorization impact comparison.
        let mut checkpoint = control.borrow().checkpoint();
        let mut frozen: ArchitectGraph = serde_json::from_value(checkpoint["state"]["graph"].clone()).unwrap();
        frozen.node_at_mut(&NodePath::root("other".into())).unwrap().intent = "Additional frozen change".into();
        checkpoint["state"]["graph"] = json!(frozen);
        *control.borrow_mut() = crate::architect_runner::RunState::from_checkpoint(checkpoint).unwrap();
        let before = control.borrow().checkpoint();
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.architect_graph(), Some(&live));
            assert_eq!(thread.architect_revision(), revision);
            assert_eq!(thread.architect_event_sequence(), sequence);
            assert_eq!(thread.architect_run().unwrap().id(), run_id);
            let candidate = preview_graph_edits(&live, &move_input().edits).unwrap();
            let runtime = preview_architect_graph_update(thread, &candidate.graph, &candidate.invalidated_steps);
            assert!(runtime["invalidated_paths"].as_array().unwrap().contains(&json!(["other"])));
        });
        authorization.response.send(acp_thread::SelectedPermissionOutcome::new(
            acp::PermissionOptionId::new("allow"), acp::PermissionOptionKind::AllowOnce,
        )).unwrap();
        let ArchitectRunToolOutput::Error { error } = task.await.unwrap_err() else { panic!("expected stale impact error"); };
        assert!(error.contains("Runtime impact changed"), "{error}");
        assert_eq!(control.borrow().checkpoint(), before);
        thread.read_with(cx, |thread, _| {
            assert_eq!(thread.architect_graph(), Some(&live));
            assert_eq!(thread.architect_revision(), revision);
            assert_eq!(thread.architect_event_sequence(), sequence);
        });
    }

    #[gpui::test]
    async fn apply_rechecks_graph_and_mode_after_permission(cx: &mut TestAppContext) {
        let (_connection, _agent, thread, _coordinator) = architect_tool_test_session(cx).await;
        permission(ToolPermissionMode::Confirm, cx);
        for change_mode in [false, true] {
            let graph = plan(&thread, cx);
            let tool = Arc::new(EditArchitectPlanTool::new(thread.downgrade()));
            let preview = cx.update(|cx| tool.preview(&move_input(), cx)).unwrap();
            let (events, mut receiver) = ToolCallEventStream::test();
            let task = cx.update(|cx| tool.run(apply_input(&preview), events, cx));
            let authorization = receiver.expect_authorization().await;
            assert_eq!(authorization.context.as_ref().unwrap().tool_name, EditArchitectPlanTool::NAME);
            thread.update(cx, |thread, cx| {
                if change_mode {
                    thread.set_session_mode(SessionMode::Plan, cx);
                } else {
                    let mut changed = graph.clone();
                    changed.nodes.first_mut().unwrap().title = "Changed during approval".into();
                    thread.set_architect_graph(Some(changed), cx);
                }
            });
            authorization.response.send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"), acp::PermissionOptionKind::AllowOnce,
            )).unwrap();
            assert!(task.await.is_err());
            thread.read_with(cx, |thread, _| {
                assert!(thread.architect_graph().unwrap().nodes.first().unwrap().position.is_none());
                assert!(thread.architect_run().is_none());
            });
        }
    }

    #[gpui::test]
    async fn approved_apply_changes_only_preview_and_titles_never_read_owner(cx: &mut TestAppContext) {
        let (_connection, _agent, thread, _coordinator) = architect_tool_test_session(cx).await;
        plan(&thread, cx);
        permission(ToolPermissionMode::Allow, cx);
        let tool = Arc::new(EditArchitectPlanTool::new(thread.downgrade()));
        let inspect = InspectArchitectPlanTool::new(thread.downgrade());
        thread.update(cx, |_, cx| {
            assert_eq!(tool.initial_title(Err(Value::Null), cx), "Preview Architect plan edit");
            assert_eq!(inspect.initial_title(Err(Value::Null), cx), "Inspect Architect plan");
        });
        let preview = cx.update(|cx| tool.preview(&move_input(), cx)).unwrap();
        let (events, _receiver) = ToolCallEventStream::test();
        assert!(cx.update(|cx| tool.clone().run(apply_input(&preview), events, cx)).await.is_ok());
        thread.read_with(cx, |thread, _| {
            let step = thread.architect_graph().unwrap().nodes.first().unwrap();
            assert_eq!(step.position.unwrap().x, 12.0);
            assert!(step.locked);
            assert!(thread.architect_run().is_none());
        });
        let (events, _receiver) = ToolCallEventStream::test();
        assert!(cx.update(|cx| tool.run(apply_input(&preview), events, cx)).await.is_err());
    }

    #[test]
    fn inspection_keeps_nested_own_fields_and_paths() {
        let mut parent = ArchitectNode::new("outer", "Parent");
        parent.intent = "Parent's own goal".into();
        let mut child = ArchitectNode::new("step", "Child");
        child.intent = "Child goal".into();
        child.locked = true;
        parent.subplan = Some(Box::new(ArchitectGraph { nodes: vec![child], edges: vec![] }));
        let graph = ArchitectGraph { nodes: vec![parent], edges: vec![] };
        let mut records = Vec::new();
        graph_records(&graph, &NodePath::default(), &json!({}), &mut records).unwrap();
        assert_eq!(records[1]["fields"]["intent"], "Parent's own goal");
        assert!(records[1]["fields"].get("subplan").is_none());
        assert_eq!(records[2]["entries"], json!([["outer", "step"]]));
        assert_eq!(records[3]["node_path"], json!(["outer", "step"]));
        assert_eq!(records[3]["fields"]["locked"], true);
    }

    #[test]
    fn preview_tokens_bind_source_revision_run_and_expiry() {
        let graph = ArchitectGraph::default();
        let now = Instant::now();
        let stored = StoredPreview {
            token: Uuid::new_v4(), source: graph.clone(), revision: 4,
            run_id: Some(Uuid::new_v4()), event_sequence: 10, expires: now + PREVIEW_LIFETIME,
            preview: preview_graph_edits(&graph, &[]).unwrap(),
            runtime: runtime_impact_fixture(),
            impact: serde_json::from_value(runtime_impact_fixture()).unwrap(),
        };
        assert!(stored.check(Some(&graph), 4, stored.run_id, 10, now).is_ok());
        assert!(stored.check(Some(&graph), 4, stored.run_id, 11, now).is_err());
        assert!(stored.check(Some(&graph), 5, stored.run_id, 10, now).is_err());
        assert!(stored.check(Some(&graph), 4, Some(Uuid::new_v4()), 10, now).is_err());
        assert!(stored.check(Some(&graph), 4, stored.run_id, 10, stored.expires).is_err());
        let changed = ArchitectGraph { nodes: vec![ArchitectNode::new("new", "New")], edges: vec![] };
        assert!(stored.check(Some(&changed), 4, stored.run_id, 10, now).is_err());
    }

    #[test]
    fn apply_cannot_smuggle_edits_and_preview_is_default() {
        let preview: EditArchitectPlanToolInput = serde_json::from_value(json!({
            "edits": [{"kind": "remove_node", "path": ["outer", "step"]}]
        })).unwrap();
        assert_eq!(preview.action, ArchitectPlanEditAction::Preview);
        preview.validate().unwrap();
        let mut apply = preview;
        apply.action = ArchitectPlanEditAction::Apply;
        apply.preview_token = Some(Uuid::new_v4().to_string());
        assert!(apply.validate().is_err());
        apply.edits.clear();
        apply.validate().unwrap();
        assert_eq!(InspectArchitectPlanTool::capability(), ToolCapability::ReadOnly);
        assert!(!EditArchitectPlanTool::capability().is_allowed_in(SessionMode::Plan));
        assert!(EditArchitectPlanTool::capability().is_allowed_in(SessionMode::Architect));
    }

    #[test]
    fn graph_pages_count_json_escaping_and_never_truncate_paths() {
        let record = json!({"node_path": ["outer", "step"], "goal": "\0".repeat(1024)});
        let records = vec![record.clone(); 20];
        let page = record_page(&records, 0, 20, 1).unwrap();
        let count = page["records"].as_array().unwrap().len();
        assert!(count < 20);
        assert_eq!(page["next_offset"], count);
        assert_eq!(page["records"][0], record);
        assert!(record_page(&[json!({"node_path": ["x".repeat(MAX_OUTPUT_BYTES)]})], 0, 1, 0).is_err());
    }
}
