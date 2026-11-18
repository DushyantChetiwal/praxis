use std::collections::HashMap;

use acp_thread::ThreadStatus;
use agent::{
    ArchitectRun, apply_architect_graph_update, architect_run_readiness, resume_architect_run_at,
    start_architect_run_from, update_architect_step,
};
use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result, anyhow};
use architect::{ArchitectGraph, GraphEdit, NodeId, NodePath, preview_graph_edits};
use gpui::App;
use serde_json::{Value, json};
use workspace::Workspace;

use super::{
    FILE_JSON_BUDGET, agent_panel, architect, conversation_view, required, root_thread_view,
    status_name, transcript_entry, truncate,
};

const MAX_SUBAGENTS: usize = 30;
const SUMMARY_PREVIEW_LIMIT: usize = 240;

pub(super) fn inspect(workspace: &gpui::Entity<Workspace>, cx: &App) -> Result<Value> {
    let view = conversation_view(workspace, cx)?;
    let owner = view.read(cx).as_native_thread(cx);
    let root_view = root_thread_view(workspace, cx)?;
    let root_acp = root_view.read(cx).thread.read(cx);

    let subagents: Vec<Value> = root_acp
        .subagents()
        .take(MAX_SUBAGENTS)
        .map(|subagent| {
            let sub = subagent.read(cx);
            let entries = sub.entries();
            let preview = entries
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, entry)| transcript_entry(index, entry, false, cx))
                .unwrap_or(Value::Null);
            let title = sub
                .title()
                .map(|title| truncate(&title, 120))
                .unwrap_or_else(|| "Sub-agent".to_string());
            let status = sub.status();
            json!({
                "session_id": sub.session_id().0.as_ref(),
                "title": title,
                "status": status_name(status),
                "total_entries": entries.len(),
                "can_cancel": status != ThreadStatus::Idle,
                "preview": preview,
            })
        })
        .collect();

    let Some(owner) = owner else {
        return Ok(fit_plan_payload(
            json!({
                "has_plan": false,
                "revision": 0,
                "running": false,
                "paused": false,
                "can_resume": false,
                "outcome": Value::Null,
                "nodes": [],
                "edges": [],
                "subagents": subagents,
            }),
            FILE_JSON_BUDGET,
        ));
    };

    let thread = owner.read(cx);
    let graph = thread.architect_status_graph();
    let run = thread.architect_run();
    let revision = thread.architect_revision();
    let running = run.is_some_and(ArchitectRun::is_running);
    let paused = run.is_some_and(ArchitectRun::is_paused);
    let can_resume = run.is_some_and(ArchitectRun::can_resume);
    let outcome = run
        .and_then(|run| run.outcome.as_ref())
        .map(|outcome| format!("{outcome:?}"));

    let Some(graph) = graph else {
        return Ok(fit_plan_payload(
            json!({
                "has_plan": false,
                "revision": revision,
                "running": running,
                "paused": paused,
                "can_resume": can_resume,
                "outcome": outcome,
                "nodes": [],
                "edges": [],
                "subagents": subagents,
            }),
            FILE_JSON_BUDGET,
        ));
    };

    let readiness = architect_run_readiness(thread);
    let mut readiness_by_path: HashMap<Vec<String>, (String, String)> = HashMap::new();
    if let Some(steps) = readiness.get("steps").and_then(Value::as_array) {
        for step in steps {
            if let Some(path) = parse_json_path(step.get("path")) {
                let status = step
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("waiting")
                    .to_string();
                let reason = step
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                readiness_by_path.insert(path, (status, reason));
            }
        }
    }

    let mut history_by_path: HashMap<Vec<String>, (Option<String>, Option<String>, usize)> =
        HashMap::new();
    if let Some(run) = run {
        for step in run.history() {
            let path: Vec<String> = step.path.0.iter().map(|id| id.0.clone()).collect();
            let session_id = step
                .session_id
                .as_ref()
                .map(|id| id.0.as_ref().to_string());
            let summary = step
                .summary
                .as_ref()
                .map(|summary| truncate(summary.as_ref(), SUMMARY_PREVIEW_LIMIT));
            history_by_path.insert(path, (session_id, summary, step.attempt));
        }
    }

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    collect_graph(
        graph,
        &[],
        &readiness_by_path,
        &history_by_path,
        &mut nodes,
        &mut edges,
    );

    Ok(fit_plan_payload(
        json!({
            "has_plan": true,
            "revision": revision,
            "running": running,
            "paused": paused,
            "can_resume": can_resume,
            "outcome": outcome,
            "nodes": nodes,
            "edges": edges,
            "subagents": subagents,
        }),
        FILE_JSON_BUDGET,
    ))
}

pub(super) fn action(
    workspace: &gpui::Entity<Workspace>,
    args: &Value,
    window: &mut gpui::Window,
    cx: &mut App,
) -> Result<Value> {
    let action_name = required(args, "action")?;
    if matches!(action_name, "run" | "pause" | "resume" | "stop") {
        return architect(workspace, action_name, window, cx);
    }

    if action_name == "cancel_subagent" {
        let session_id = acp::SessionId::new(required(args, "session_id")?.to_string());
        let root_view = root_thread_view(workspace, cx)?;
        let subagent = root_view
            .read(cx)
            .thread
            .read(cx)
            .subagent(&session_id)
            .cloned()
            .context("that sub-agent is no longer active")?;
        subagent
            .update(cx, |sub, cx| sub.cancel(cx))
            .detach_and_log_err(cx);
        return Ok(json!({ "ok_action": "cancel_subagent" }));
    }

    let panel = agent_panel(workspace, cx)?;
    let thread = panel
        .read(cx)
        .active_native_agent_thread(cx)
        .context("the conversation has no plan")?;
    let acp_thread = panel
        .read(cx)
        .active_agent_thread(cx)
        .context("the conversation has not loaded yet")?;

    match action_name {
        "retry_step" => {
            let path = parse_node_path(args.get("path"))?;
            let can_resume = thread
                .read(cx)
                .architect_run()
                .is_some_and(ArchitectRun::can_resume);
            if can_resume {
                resume_architect_run_at(thread, acp_thread, path, cx)
                    .map_err(|error| anyhow!("{error}"))?;
            } else {
                let graph = thread
                    .read(cx)
                    .architect_graph()
                    .cloned()
                    .context("the conversation has no plan")?;
                start_architect_run_from(thread, acp_thread, graph, path, cx)
                    .map_err(|error| anyhow!("{error}"))?;
            }
            Ok(json!({ "ok_action": "retry_step" }))
        }
        "edit_step" => {
            let path = parse_node_path(args.get("path"))?;
            let intent = args
                .get("intent")
                .and_then(Value::as_str)
                .map(str::to_string);
            let rules = args.get("rules").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
            let capture = args
                .get("capture")
                .and_then(Value::as_str)
                .map(str::to_string);
            update_architect_step(&thread, &path, intent, rules, capture, cx)?;
            Ok(json!({ "ok_action": "edit_step" }))
        }
        "skip_step" => {
            let path = parse_node_path(args.get("path"))?;
            let graph = thread
                .read(cx)
                .architect_graph()
                .cloned()
                .context("the conversation has no plan")?;
            let preview = preview_graph_edits(&graph, &[GraphEdit::RemoveNode { path }])
                .map_err(|error| anyhow!("{error}"))?;
            apply_architect_graph_update(&thread, preview.graph, &preview.invalidated_steps, cx)?;
            Ok(json!({ "ok_action": "skip_step" }))
        }
        other => Err(anyhow!("Praxis does not know the plan action {other:?}")),
    }
}

fn collect_graph(
    graph: &ArchitectGraph,
    prefix: &[String],
    readiness: &HashMap<Vec<String>, (String, String)>,
    history: &HashMap<Vec<String>, (Option<String>, Option<String>, usize)>,
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
) {
    for node in &graph.nodes {
        let mut path = prefix.to_vec();
        path.push(node.id.0.clone());

        let (status, reason) = readiness
            .get(&path)
            .cloned()
            .unwrap_or_else(|| ("waiting".to_string(), String::new()));

        let (hist_session, hist_summary, hist_attempt) =
            history.get(&path).cloned().unwrap_or((None, None, 0));

        let session_id = hist_session
            .or_else(|| node.chat.as_ref().map(|chat| chat.0.as_ref().to_string()));
        let summary = hist_summary.or_else(|| {
            node.result
                .as_ref()
                .map(|result| truncate(&result.summary, SUMMARY_PREVIEW_LIMIT))
        });
        let attempt = if hist_attempt > 0 {
            hist_attempt
        } else {
            node.result.as_ref().map_or(0, |result| result.attempt)
        };

        let model = node.model.as_ref().map(|model| {
            json!({
                "provider": model.provider,
                "model": model.model,
            })
        });

        nodes.push(json!({
            "path": path,
            "id": node.id.0,
            "title": node.title,
            "responsibility": node.responsibility,
            "intent": node.intent,
            "rules": node.rules,
            "capture": node.capture,
            "locked": node.locked,
            "status": status,
            "reason": reason,
            "model": model,
            "session_id": session_id,
            "summary": summary,
            "attempt": attempt,
        }));

        if let Some(subplan) = node.subplan() {
            collect_graph(subplan, &path, readiness, history, nodes, edges);
        }
    }

    for edge in &graph.edges {
        let mut from = prefix.to_vec();
        from.push(edge.from.0.clone());
        let mut to = prefix.to_vec();
        to.push(edge.to.0.clone());
        edges.push(json!({
            "from": from,
            "to": to,
            "condition": edge.condition.label(),
        }));
    }
}

fn parse_json_path(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    let mut path = Vec::with_capacity(items.len());
    for item in items {
        let segment = item.as_str()?.trim();
        if segment.is_empty() {
            return None;
        }
        path.push(segment.to_string());
    }
    (!path.is_empty()).then_some(path)
}

pub(super) fn parse_node_path(value: Option<&Value>) -> Result<NodePath> {
    let segments = parse_json_path(value)
        .context("expected \"path\" as a non-empty array of step ids")?;
    Ok(NodePath(
        segments.into_iter().map(NodeId::from).collect(),
    ))
}

pub(super) fn fit_plan_payload(mut payload: Value, budget: usize) -> Value {
    if payload.to_string().len() <= budget {
        return payload;
    }
    if let Some(subagents) = payload
        .get_mut("subagents")
        .and_then(Value::as_array_mut)
    {
        for subagent in subagents.iter_mut() {
            if let Some(object) = subagent.as_object_mut() {
                object.insert("preview".into(), Value::Null);
            }
        }
        while payload.to_string().len() > budget && !subagents.is_empty() {
            subagents.pop();
        }
    }
    if payload.to_string().len() <= budget {
        return payload;
    }
    if let Some(nodes) = payload.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes.iter_mut() {
            if let Some(object) = node.as_object_mut() {
                object.insert("rules".into(), json!([]));
                object.insert("summary".into(), Value::Null);
                object.insert("responsibility".into(), json!(""));
            }
        }
        while payload.to_string().len() > budget && !nodes.is_empty() {
            nodes.pop();
        }
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use architect::{ArchitectEdge, ArchitectNode, EdgeCondition};

    #[test]
    fn parse_node_path_validates_segments() {
        assert!(parse_node_path(None).is_err());
        assert!(parse_node_path(Some(&json!([]))).is_err());
        assert!(parse_node_path(Some(&json!([""]))).is_err());
        assert!(parse_node_path(Some(&json!([123]))).is_err());

        let parsed = parse_node_path(Some(&json!(["build", "test"]))).expect("valid path");
        assert_eq!(parsed.0, vec![NodeId::from("build"), NodeId::from("test")]);
    }

    #[test]
    fn collect_graph_flattens_nested_nodes_and_edges() {
        let mut parent = ArchitectNode::new("outer", "Outer Step");
        parent.intent = "Run inner steps".into();
        let mut inner = ArchitectGraph::default();
        inner.nodes.push(ArchitectNode::new("inner_a", "Inner A"));
        inner.nodes.push(ArchitectNode::new("inner_b", "Inner B"));
        inner.edges.push(ArchitectEdge {
            id: "e1".into(),
            from: "inner_a".into(),
            to: "inner_b".into(),
            condition: EdgeCondition::Objective {
                statement: "tests passed".into(),
            },
            max_repeats: None,
        });
        parent.subplan = Some(Box::new(inner));

        let mut root = ArchitectGraph::default();
        root.nodes.push(parent);

        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        collect_graph(
            &root,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &mut nodes,
            &mut edges,
        );

        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0]["path"], json!(["outer"]));
        assert_eq!(nodes[1]["path"], json!(["outer", "inner_a"]));
        assert_eq!(nodes[2]["path"], json!(["outer", "inner_b"]));

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], json!(["outer", "inner_a"]));
        assert_eq!(edges[0]["to"], json!(["outer", "inner_b"]));
        assert_eq!(edges[0]["condition"], json!("tests passed"));
    }

    #[test]
    fn fit_plan_payload_respects_budget() {
        let large_nodes: Vec<Value> = (0..100)
            .map(|index| {
                json!({
                    "path": [format!("step_{index}")],
                    "id": format!("step_{index}"),
                    "title": format!("Step {index}"),
                    "responsibility": "x".repeat(200),
                    "intent": "y".repeat(200),
                    "rules": ["z".repeat(200)],
                    "capture": "c",
                    "locked": false,
                    "status": "waiting",
                    "reason": "",
                    "model": null,
                    "session_id": null,
                    "summary": "s".repeat(200),
                    "attempt": 0,
                })
            })
            .collect();
        let payload = json!({
            "has_plan": true,
            "revision": 1,
            "running": false,
            "paused": false,
            "can_resume": false,
            "outcome": null,
            "nodes": large_nodes,
            "edges": [],
            "subagents": [],
        });
        let fitted = fit_plan_payload(payload, 4_000);
        assert!(fitted.to_string().len() <= 4_000);
    }
}
