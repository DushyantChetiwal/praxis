//! The Architect half of the automation channel in `crate::automation`: a
//! description of what the canvas is showing, and the canvas actions an agent
//! testing the real app needs, addressed by step and connection id.

use anyhow::{Context as _, Result, anyhow, bail};
use architect::{EdgeCondition, EdgeId, NodeId, Position};
use gpui::{Context, Window};
use serde_json::{Value, json};

use super::inspector::InspectorTab;
use super::{ArchitectPane, ArchitectWorkspaceMode, HistoryDirection, Selection};

fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("expected a string \"{key}\""))
}

fn number(args: &Value, key: &str) -> Result<f32> {
    args.get(key)
        .and_then(Value::as_f64)
        .map(|value| value as f32)
        .with_context(|| format!("expected a number \"{key}\""))
}

impl ArchitectPane {
    /// What the canvas shows, with each visible step's rectangle in window
    /// coordinates (logical pixels) so a real mouse can be aimed at it.
    pub(crate) fn automation_state(&self, cx: &Context<Self>) -> Value {
        let graph = self.graph(cx);
        let nodes: Vec<Value> = graph
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .map(|node| {
                        let rect = node.position.map(|position| {
                            let centre = self.to_screen(position);
                            let (width, height) = self.node_size(node);
                            let (width, height) = (width * self.zoom, height * self.zoom);
                            json!({
                                "x": f32::from(centre.x) - width / 2.0,
                                "y": f32::from(centre.y) - height / 2.0,
                                "width": width,
                                "height": height,
                            })
                        });
                        json!({
                            "id": node.id.0,
                            "title": node.title,
                            "locked": node.locked,
                            "pinned": node.pinned,
                            "nested_steps": node.subplan().map_or(0, |plan| plan.nodes.len()),
                            "has_result": node.result.is_some(),
                            "rect": rect,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let uses = self.connection_uses(cx);
        let edges: Vec<Value> = graph
            .map(|graph| {
                graph
                    .edges
                    .iter()
                    .map(|edge| {
                        let kind = match edge.condition {
                            EdgeCondition::Always => "always",
                            EdgeCondition::Objective { .. } => "objective",
                            EdgeCondition::LlmEvaluated { .. } => "agent",
                        };
                        json!({
                            "id": edge.id.0,
                            "from": edge.from.0,
                            "to": edge.to.0,
                            "condition": kind,
                            "label": edge.condition.label(),
                            "max_repeats": edge.max_repeats,
                            "taken_in_latest_run": uses
                                .get(&(edge.from.clone(), edge.to.clone()))
                                .copied()
                                .unwrap_or(0),
                            "is_loop": graph.is_loop_edge(edge),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let problems: Vec<String> = graph
            .map(|graph| {
                graph
                    .blocking_problems()
                    .iter()
                    .map(|problem| problem.describe(graph))
                    .collect()
            })
            .unwrap_or_default();
        let root = self.root_graph(cx);
        let thread = self.thread.read(cx);
        let run = thread.architect_run().map(|run| {
            let outcome = run
                .outcome
                .as_ref()
                .zip(root)
                .map(|(outcome, root)| outcome.summary(root));
            json!({
                "running": run.is_running(),
                "step": run.step_number,
                "current": run.current_title.to_string(),
                "outcome": outcome,
            })
        });
        let selection = match &self.selection {
            Some(Selection::Node(id)) => json!({ "node": id.0 }),
            Some(Selection::Edge(id)) => json!({ "edge": id.0 }),
            None => Value::Null,
        };
        let viewport = self.viewport.get().map(|bounds| {
            json!({
                "x": f32::from(bounds.origin.x),
                "y": f32::from(bounds.origin.y),
                "width": f32::from(bounds.size.width),
                "height": f32::from(bounds.size.height),
            })
        });
        let activity: Vec<&str> = self
            .activity
            .iter()
            .rev()
            .take(12)
            .map(|entry| entry.message.as_str())
            .collect();

        json!({
            "mode": match self.mode {
                ArchitectWorkspaceMode::Architect => "architect",
                ArchitectWorkspaceMode::Code => "code",
            },
            "plan_title": thread.title().map(|title| title.to_string()),
            "total_steps": root.map_or(0, architect::ArchitectGraph::step_count_deeply),
            "settled_steps": root.map_or(0, architect::ArchitectGraph::locked_step_count_deeply),
            "focus": self.focus.iter().map(|id| id.0.clone()).collect::<Vec<_>>(),
            "selection": selection,
            "bulk_selection": self.bulk.iter().map(|id| id.0.clone()).collect::<Vec<_>>(),
            "inspector_tab": format!("{:?}", self.inspector_tab),
            "zoom": self.zoom,
            "viewport": viewport,
            "undo": self.undo_stack.len(),
            "redo": self.redo_stack.len(),
            "running": self.is_running(cx),
            "run": run,
            "nodes": nodes,
            "edges": edges,
            "problems": problems,
            "recent_activity": activity,
        })
    }

    /// Performs one canvas action and returns the canvas state afterwards.
    pub(crate) fn automation_command(
        &mut self,
        op: &str,
        args: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value> {
        let node = |args: &Value| text(args, "node").map(|id| NodeId(id.to_string()));
        match op {
            "state" => {}
            "select" => {
                let selection = if let Ok(id) = node(args) {
                    Selection::Node(id)
                } else {
                    Selection::Edge(EdgeId(text(args, "edge")?.to_string()))
                };
                self.select_and_reveal(selection, window, cx);
            }
            "deselect" => self.set_selection(None, window, cx),
            "bulk_select" => {
                let ids = args
                    .get("nodes")
                    .and_then(Value::as_array)
                    .context("expected \"nodes\" to be a list of step ids")?
                    .iter()
                    .map(|id| {
                        id.as_str()
                            .map(|id| NodeId(id.to_string()))
                            .context("expected every step id to be a string")
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.set_bulk_selection(ids, window, cx);
            }
            "select_all" => self.select_all_steps(window, cx),
            "bulk_lock" => self.lock_bulk_selection(true, cx),
            "lock_all" => self.lock_all_steps(window, cx),
            "bulk_unlock" => self.lock_bulk_selection(false, cx),
            "bulk_delete" => self.delete_bulk_selection(window, cx),
            "undo" => self.step_history(HistoryDirection::Undo, window, cx),
            "redo" => self.step_history(HistoryDirection::Redo, window, cx),
            "duplicate" => self.duplicate_selection(window, cx),
            "delete" => self.delete_selection(window, cx),
            "drill" => self.drill_into(node(args)?, window, cx),
            "up" => {
                self.drill_out(window, cx);
            }
            "lock" => self.toggle_lock(node(args)?, window, cx),
            "add_step" => self.add_step(window, cx),
            "tidy" => self.tidy_up(cx),
            "fit" => self.zoom_to_fit(cx),
            "first" => self.select_end_step(false, window, cx),
            "last" => self.select_end_step(true, window, cx),
            "move" => {
                let path = self.focus.child(node(args)?);
                let position = Position {
                    x: number(args, "x")?,
                    y: number(args, "y")?,
                };
                if !self.edit_checked(move |graph| graph.move_node_at(&path, position), cx) {
                    bail!("the step could not be moved");
                }
            }
            "connect" => {
                if self.is_running(cx) {
                    bail!("the plan is running");
                }
                let from = NodeId(text(args, "from")?.to_string());
                let to = NodeId(text(args, "to")?.to_string());
                let from_path = self.focus.child(from.clone());
                let target = to.clone();
                let connected = self.edit_checked(
                    move |graph| {
                        graph
                            .connect_from_at(&from_path, target, EdgeCondition::Always)
                            .map(|_| ())
                    },
                    cx,
                );
                if !connected {
                    bail!("the connection was refused");
                }
                self.select_new_loop(&from, &to, window, cx);
            }
            "repeats" => {
                let edge = EdgeId(text(args, "edge")?.to_string());
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_u64()
                            .and_then(|limit| u32::try_from(limit).ok())
                            .context("expected \"limit\" to be a whole number or null")?,
                    ),
                };
                self.set_edge_max_repeats(edge, limit, cx);
            }
            "condition" => {
                let edge = EdgeId(text(args, "edge")?.to_string());
                let condition = match text(args, "kind")? {
                    "always" => EdgeCondition::Always,
                    "objective" => EdgeCondition::Objective {
                        statement: text(args, "text")?.to_string(),
                    },
                    "agent" => EdgeCondition::LlmEvaluated {
                        question: text(args, "text")?.to_string(),
                    },
                    kind => bail!("unknown condition kind {kind:?}"),
                };
                if !self.set_edge_condition(edge, condition, cx) {
                    bail!("the condition was refused");
                }
            }
            "tab" => {
                self.inspector_tab = match text(args, "name")? {
                    "details" => InspectorTab::Details,
                    "conversation" => InspectorTab::Conversation,
                    "activity" => InspectorTab::Activity,
                    name => bail!("unknown inspector tab {name:?}"),
                };
                cx.notify();
            }
            "code" | "editor" => self.request_code_mode(window, cx),
            "run" => self.run(cx),
            "stop" => self.stop_run(cx),
            op => return Err(anyhow!("unknown Architect op {op:?}")),
        }
        // The state reports where steps are drawn so a real mouse can be aimed
        // at them, which only holds once the view has stopped moving.
        self.settle_camera(cx);
        Ok(self.automation_state(cx))
    }
}
