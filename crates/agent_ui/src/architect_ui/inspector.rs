use acp_thread::AcpThread;
use agent_client_protocol::schema::v1 as acp;
use architect::{ArchitectEdge, ArchitectGraph, ArchitectNode, EdgeCondition, EdgeId, NodeId};
use editor::{Editor, EditorEvent};
use gpui::{App, Context, Entity, Focusable, SharedString, Subscription, Window, div, px};
use ui::{TintColor, Tooltip, prelude::*};

use crate::AgentPanel;

use super::rendering::truncate;
use super::{ArchitectPane, Interaction, Selection};

/// The inspector shows one step, either as fields or as the conversation about
/// it. The conversation lives here rather than in the agent panel so that the
/// panel is never navigated away from the plan's own thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InspectorTab {
    Details,
    Conversation,
    Activity,
}

/// The editors backing the inspector for the selected step. They are rebuilt
/// whenever the selection changes, which is also what keeps their contents from
/// drifting away from the graph.
pub(super) struct EdgeInspector {
    edge: EdgeId,
    condition: Entity<Editor>,
    _subscription: Subscription,
}

pub(super) struct NodeInspector {
    node: NodeId,
    title: Entity<Editor>,
    responsibility: Entity<Editor>,
    goal: Entity<Editor>,
    capture: Entity<Editor>,
    new_rule: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

impl ArchitectPane {
    pub(super) fn set_selection(
        &mut self,
        selection: Option<Selection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected_node = match &selection {
            Some(Selection::Node(id)) => Some(id.clone()),
            _ => None,
        };
        let selected_edge = match &selection {
            Some(Selection::Edge(id)) => Some(id.clone()),
            _ => None,
        };

        if self.inspector.as_ref().map(|inspector| &inspector.node) != selected_node.as_ref() {
            self.inspector = selected_node
                .and_then(|id| self.graph(cx).and_then(|graph| graph.node(&id)).cloned())
                .map(|node| self.build_inspector(node, window, cx));
        }
        if self
            .edge_inspector
            .as_ref()
            .map(|inspector| &inspector.edge)
            != selected_edge.as_ref()
        {
            self.edge_inspector = selected_edge
                .and_then(|id| {
                    self.graph(cx)
                        .and_then(|graph| graph.edges.iter().find(|edge| edge.id == id))
                        .cloned()
                })
                .map(|edge| self.build_edge_inspector(edge, window, cx));
        }

        self.selection = selection;
        if self.selection.is_some() && f32::from(window.viewport_size().width) < 1060.0 {
            self.inspector_drawer_open = true;
        }
        cx.notify();
    }

    fn build_edge_inspector(
        &self,
        edge: ArchitectEdge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EdgeInspector {
        let source_locked = self
            .graph(cx)
            .and_then(|graph| graph.node(&edge.from))
            .is_some_and(|node| node.locked);
        let condition = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 5, window, cx);
            editor.set_placeholder_text(
                "Describe when this connection should be taken",
                window,
                cx,
            );
            editor.set_text(edge.condition.label().unwrap_or_default(), window, cx);
            editor.set_read_only(source_locked);
            editor
        });
        let edge_id = edge.id.clone();
        let subscription = cx.subscribe(&condition, move |this, editor, event, cx| {
            if !matches!(event, EditorEvent::BufferEdited) {
                return;
            }
            let text = editor.read(cx).text(cx);
            if text.trim().is_empty() {
                return;
            }
            let current = this
                .graph(cx)
                .and_then(|graph| graph.edges.iter().find(|edge| edge.id == edge_id))
                .map(|edge| edge.condition.clone());
            let condition = match current {
                Some(EdgeCondition::LlmEvaluated { .. }) => {
                    EdgeCondition::LlmEvaluated { question: text }
                }
                _ => EdgeCondition::Objective { statement: text },
            };
            this.set_edge_condition(edge_id.clone(), condition, cx);
        });

        EdgeInspector {
            edge: edge.id,
            condition,
            _subscription: subscription,
        }
    }

    fn build_inspector(
        &self,
        node: ArchitectNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> NodeInspector {
        let title = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(node.title.clone(), window, cx);
            editor.set_read_only(node.locked);
            editor
        });
        let responsibility = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("What area of work does this step own?", window, cx);
            editor.set_text(node.responsibility.clone(), window, cx);
            editor.set_read_only(node.locked);
            editor
        });
        let goal = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_placeholder_text("What does this step have to accomplish?", window, cx);
            editor.set_text(node.intent.clone(), window, cx);
            editor.set_read_only(node.locked);
            editor
        });
        let capture = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 6, window, cx);
            editor.set_placeholder_text(
                "What must this step's summary contain, for the steps after it?",
                window,
                cx,
            );
            editor.set_text(node.capture.clone(), window, cx);
            editor.set_read_only(node.locked);
            editor
        });
        let new_rule = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Add a rule and press enter", window, cx);
            editor
        });

        let id_for_title = node.id.clone();
        let id_for_responsibility = node.id.clone();
        let id_for_goal = node.id.clone();
        let id_for_capture = node.id.clone();
        let subscriptions = vec![
            cx.subscribe(&title, move |this, editor, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    let id = id_for_title.clone();
                    this.edit_node(id, move |node| node.title = text, cx);
                }
            }),
            cx.subscribe(&responsibility, move |this, editor, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    let id = id_for_responsibility.clone();
                    this.edit_node(id, move |node| node.responsibility = text, cx);
                }
            }),
            cx.subscribe(&goal, move |this, editor, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    let id = id_for_goal.clone();
                    this.edit_node(id, move |node| node.intent = text, cx);
                }
            }),
            cx.subscribe(&capture, move |this, editor, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    let id = id_for_capture.clone();
                    this.edit_node(id, move |node| node.capture = text, cx);
                }
            }),
        ];

        NodeInspector {
            node: node.id,
            title,
            responsibility,
            goal,
            capture,
            new_rule,
            _subscriptions: subscriptions,
        }
    }

    fn add_rule(&mut self, rule: String, window: &mut Window, cx: &mut Context<Self>) {
        let rule = rule.trim().to_string();
        if rule.is_empty() {
            return;
        }
        let Some(inspector) = &self.inspector else {
            return;
        };
        let id = inspector.node.clone();
        inspector
            .new_rule
            .update(cx, |editor, cx| editor.set_text("", window, cx));

        self.edit_node(id, move |node| node.rules.push(rule), cx);
    }

    fn remove_rule(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(inspector) = &self.inspector else {
            return;
        };
        let id = inspector.node.clone();
        self.edit_node(
            id,
            move |node| {
                if index < node.rules.len() {
                    node.rules.remove(index);
                }
            },
            cx,
        );
    }

    fn set_edge_condition(
        &mut self,
        id: EdgeId,
        condition: EdgeCondition,
        cx: &mut Context<Self>,
    ) -> bool {
        let focus = self.focus.clone();
        self.edit_checked(
            move |graph| graph.set_edge_condition_at(&focus, &id, condition),
            cx,
        )
    }

    /// Opens the step's own conversation in the inspector, beside the step.
    ///
    /// Each step gets a thread of its own, inheriting the main conversation so
    /// it knows what the plan is for, then diverging. That divergence is the
    /// point: settling this step cannot crowd out the context the next step will
    /// be settled in. The agent panel is deliberately left where it is, showing
    /// the conversation that owns the plan.
    fn discuss_node(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.graph(cx).and_then(|graph| graph.node(&id)).cloned() else {
            return;
        };
        let Some(conversation_view) = self.plan_conversation_view(cx) else {
            return;
        };
        let node_path = self.focus.child(id);

        self.inspector_tab = InspectorTab::Conversation;
        let is_first_visit = node.chat.is_none();
        let session_id = conversation_view.update(cx, |conversation_view, cx| {
            conversation_view.ensure_architect_step_thread(
                node_path.clone(),
                node.title.clone().into(),
                node.chat.clone(),
                window,
                cx,
            )
        });

        let Some(session_id) = session_id else {
            log::error!("Architect: could not open a conversation for this step");
            return;
        };

        if node.chat.as_ref() != Some(&session_id) {
            let session_id = session_id.clone();
            self.thread.update(cx, |thread, cx| {
                thread.update_architect_graph(
                    move |graph| {
                        if let Some(node) = graph.node_at_mut(&node_path) {
                            // Chat identity is persistence metadata, not a
                            // refinement of the settled step itself.
                            node.chat = Some(session_id);
                        }
                    },
                    cx,
                );
            });
        }

        // A brand new step thread has nothing in it, so state the brief it is
        // there to settle. Returning to an existing one must not repeat it.
        if is_first_visit {
            self.seed_step_thread(&node, &session_id, window, cx);
        }
    }

    /// Puts the step's brief in the composer of its own thread, ready to send.
    /// It is left unsent so the brief can be adjusted before the conversation
    /// takes its first turn.
    fn seed_step_thread(
        &self,
        node: &ArchitectNode,
        session_id: &acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let goal = if node.intent.trim().is_empty() {
            "Not decided yet; that is what this conversation is for.".to_string()
        } else {
            node.intent.trim().to_string()
        };
        let rules = if node.rules.is_empty() {
            "None yet.\n".to_string()
        } else {
            node.rules
                .iter()
                .map(|rule| format!("- {rule}\n"))
                .collect()
        };
        let next_steps = self
            .graph(cx)
            .map(|graph| {
                let followers: Vec<String> = graph
                    .edges_from(&node.id)
                    .filter_map(|edge| graph.node(&edge.to))
                    .map(|target| format!("- {} (id: {})", target.title, target.id))
                    .collect();
                if followers.is_empty() {
                    "Nothing follows this step; it is where the plan ends.\n".to_string()
                } else {
                    followers.join("\n") + "\n"
                }
            })
            .unwrap_or_default();

        let prompt = format!(
            "We are settling one step of the plan: \"{title}\".\n\n\
             ## Goal so far\n{goal}\n\n\
             ## Rules so far\n{rules}\n\
             ## What follows this step\n{next_steps}\n\
             Help me pin this down. Ask about anything ambiguous instead of \
             guessing, and question the goal if it is vague or wrong. When we \
             agree, record it with refine_step, including the routing to the \
             steps above once we have settled when each one is taken.\n\n\
             You are not building anything here, and no other step is yours to \
             change; that is the main conversation's job.",
            title = node.title,
        );

        // Resolved by reading, and the borrow is over before the composer is
        // touched. Putting a message into the composer reads the workspace to
        // resolve file mentions, which panics if the workspace is mid-update.
        let Some(thread_view) = self.step_chat_view(session_id, cx) else {
            log::error!("Architect: this step's conversation is not on screen yet");
            return;
        };

        thread_view.update(cx, |thread_view, cx| {
            thread_view.message_editor.update(cx, |editor, cx| {
                editor.set_message(
                    vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                    window,
                    cx,
                );
            });
            thread_view
                .message_editor
                .focus_handle(cx)
                .focus(window, cx);
        });
    }

    /// The agent panel's conversation, found without borrowing the workspace
    /// mutably.
    ///
    /// Everything the canvas wants from the workspace is reachable by reading
    /// it. Holding an update lease instead is a trap: the composer reads the
    /// workspace when a message is put into it, and a read inside an update
    /// panics.
    fn plan_conversation_view(
        &self,
        cx: &App,
    ) -> Option<Entity<crate::conversation_view::ConversationView>> {
        let workspace = self.workspace.upgrade()?;
        let panel = workspace.read(cx).panel::<AgentPanel>(cx)?;
        Some(panel.read(cx).active_conversation_view()?.clone())
    }

    /// The conversation the run drives, which is the one that owns the plan.
    pub(super) fn plan_acp_thread(&self, cx: &App) -> Option<Entity<AcpThread>> {
        let conversation_view = self.plan_conversation_view(cx)?;
        let thread_view = conversation_view.read(cx).root_thread_view()?;
        Some(thread_view.read(cx).thread.clone())
    }

    /// The view for a step's own conversation, once it has been created and
    /// loaded. Rendered inside the inspector rather than in the agent panel.
    fn step_chat_view(
        &self,
        session_id: &acp::SessionId,
        cx: &Context<Self>,
    ) -> Option<Entity<crate::conversation_view::ThreadView>> {
        self.plan_conversation_view(cx)?
            .read(cx)
            .thread_view(session_id)
    }

    pub(super) fn toggle_lock(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        // Pressing the button also pressed the node underneath it, which began
        // a drag that no mouse-up is going to end.
        self.interaction = Interaction::None;

        let Some(graph) = self.graph(cx) else {
            return;
        };
        let locked = graph.node(&id).is_some_and(|node| node.locked);

        // Settling a step that contains a plan would settle that plan too,
        // which is not the user's to do from out here.
        if !locked && !graph.can_lock(&id) {
            self.report(
                "This step contains a plan that is still being argued about. Lock every step \
                 inside it first."
                    .to_string(),
                cx,
            );
            return;
        }

        let path = self.focus.child(id);
        self.edit_checked(move |graph| graph.set_locked_at(&path, !locked), cx);
        self.refresh_inspector(window, cx);
    }

    /// Rebuilds the inspector so its fields match the step again, which is what
    /// makes them go read-only the moment a step is locked.
    fn refresh_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .inspector
            .as_ref()
            .map(|inspector| inspector.node.clone())
        else {
            return;
        };
        let Some(node) = self.graph(cx).and_then(|graph| graph.node(&id)).cloned() else {
            self.inspector = None;
            return;
        };
        self.inspector = Some(self.build_inspector(node, window, cx));
        cx.notify();
    }

    fn render_inspector_resize_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .gap_0p5()
            .child(
                IconButton::new("architect-inspector-narrower", IconName::Dash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Make the inspector narrower"))
                    .on_click(cx.listener(|this, _, _, cx| this.resize_inspector(-16.0, cx))),
            )
            .child(
                IconButton::new("architect-inspector-wider", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Make the inspector wider"))
                    .on_click(cx.listener(|this, _, _, cx| this.resize_inspector(16.0, cx))),
            )
            .into_any()
    }

    fn render_overview_inspector(&self, width: gpui::Pixels, cx: &mut Context<Self>) -> AnyElement {
        let graph = self.graph(cx);
        let step_count = graph.map_or(0, |graph| graph.nodes.len());
        let settled = graph.map_or(0, |graph| {
            graph.nodes.iter().filter(|node| node.locked).count()
        });
        let issues = graph
            .map(ArchitectGraph::blocking_problems)
            .unwrap_or_default();
        let captures = graph
            .map(ArchitectGraph::steps_without_capture)
            .unwrap_or_default();
        let ready = step_count > 0 && issues.is_empty();
        let latest_run = self.thread.read(cx).architect_run().map(|run| {
            (
                run.history().len(),
                if run.is_running() {
                    "in progress"
                } else {
                    "finished"
                },
            )
        });

        div()
            .flex_none()
            .h_full()
            .py_2()
            .pr_2()
            .child(
                v_flex()
                    .w(width)
                    .h_full()
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .shadow_md()
                    .bg(cx.theme().colors().panel_background)
                    .child(
                        v_flex()
                            .gap_1()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .child(Label::new("Plan Overview").size(LabelSize::Default))
                                    .child(self.render_inspector_resize_controls(cx)),
                            )
                            .child(
                                Label::new(if ready {
                                    "Ready for execution"
                                } else {
                                    "Review the plan before execution"
                                })
                                .size(LabelSize::Small)
                                .color(if ready {
                                    Color::Success
                                } else {
                                    Color::Warning
                                }),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_3()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("PLAN HEALTH")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(format!(
                                            "{step_count} steps · {settled} settled · {} draft",
                                            step_count.saturating_sub(settled)
                                        ))
                                        .size(LabelSize::Small),
                                    )
                                    .child(
                                        Label::new(format!(
                                            "{} blocking issues · {} incomplete handoffs",
                                            issues.len(),
                                            captures.len()
                                        ))
                                        .size(LabelSize::Small)
                                        .color(if issues.is_empty() && captures.is_empty() {
                                            Color::Muted
                                        } else {
                                            Color::Warning
                                        }),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("HOW TO USE ARCHITECT")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(
                                            "Select a step in the outline or graph to edit its goal, constraints, handoff, conversation, and activity. Select a connection to inspect its routing condition.",
                                        )
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    ),
                            )
                            .when_some(latest_run, |this, (visits, state)| {
                                this.child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("LATEST RUN")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Label::new(format!("{visits} visits · {state}"))
                                                .size(LabelSize::Small),
                                        ),
                                )
                            }),
                    ),
            )
            .into_any()
    }

    fn render_edge_inspector(
        &self,
        id: &EdgeId,
        width: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let edge = self
            .graph(cx)
            .and_then(|graph| graph.edges.iter().find(|edge| &edge.id == id))
            .cloned();
        let Some(edge) = edge else {
            return self.render_overview_inspector(width, cx);
        };
        let graph = self.graph(cx);
        let source = graph
            .and_then(|graph| graph.node(&edge.from))
            .map(|node| node.title.clone())
            .unwrap_or_else(|| edge.from.0.clone());
        let target = graph
            .and_then(|graph| graph.node(&edge.to))
            .map(|node| node.title.clone())
            .unwrap_or_else(|| edge.to.0.clone());
        let source_locked = graph
            .and_then(|graph| graph.node(&edge.from))
            .is_some_and(|node| node.locked);
        let Some(condition_editor) = self
            .edge_inspector
            .as_ref()
            .filter(|inspector| inspector.edge == edge.id)
            .map(|inspector| inspector.condition.clone())
        else {
            return self.render_overview_inspector(width, cx);
        };
        let observed_editor = condition_editor.clone();
        let model_editor = condition_editor.clone();
        let always_id = edge.id.clone();
        let observed_id = edge.id.clone();
        let model_id = edge.id.clone();
        let delete_id = edge.id.clone();
        let is_always = matches!(edge.condition, EdgeCondition::Always);
        let is_objective = matches!(edge.condition, EdgeCondition::Objective { .. });
        let is_model = matches!(edge.condition, EdgeCondition::LlmEvaluated { .. });
        let evaluation = if is_always {
            "No evaluation required"
        } else {
            "Evaluated by the model from the completed step summary"
        };

        div()
            .flex_none()
            .h_full()
            .py_2()
            .pr_2()
            .child(
                v_flex()
                    .w(width)
                    .h_full()
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .shadow_md()
                    .bg(cx.theme().colors().panel_background)
                    .child(
                        v_flex()
                            .gap_1()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .child(Label::new("Connection").size(LabelSize::Default))
                                    .child(self.render_inspector_resize_controls(cx)),
                            )
                            .child(
                                Label::new(format!("{source} → {target}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_3()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("ROUTING CONDITION")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_1()
                                            .child(
                                                Button::new(
                                                    "architect-edge-always",
                                                    "Always",
                                                )
                                                .label_size(LabelSize::XSmall)
                                                .toggle_state(is_always)
                                                .selected_style(ButtonStyle::Tinted(
                                                    TintColor::Accent,
                                                ))
                                                .style(ButtonStyle::Subtle)
                                                .disabled(source_locked)
                                                .on_click(cx.listener(
                                                    move |this, _, _, cx| {
                                                        this.set_edge_condition(
                                                            always_id.clone(),
                                                            EdgeCondition::Always,
                                                            cx,
                                                        );
                                                    },
                                                )),
                                            )
                                            .child(
                                                Button::new(
                                                    "architect-edge-objective",
                                                    "Observed",
                                                )
                                                .label_size(LabelSize::XSmall)
                                                .toggle_state(is_objective)
                                                .selected_style(ButtonStyle::Tinted(
                                                    TintColor::Accent,
                                                ))
                                                .style(ButtonStyle::Subtle)
                                                .disabled(source_locked)
                                                .on_click(cx.listener(
                                                    move |this, _, _, cx| {
                                                        let statement =
                                                            observed_editor.read(cx).text(cx);
                                                        if statement.trim().is_empty() {
                                                            this.report(
                                                                "Describe the observed condition first."
                                                                    .to_string(),
                                                                cx,
                                                            );
                                                            return;
                                                        }
                                                        this.set_edge_condition(
                                                            observed_id.clone(),
                                                            EdgeCondition::Objective { statement },
                                                            cx,
                                                        );
                                                    },
                                                )),
                                            )
                                            .child(
                                                Button::new(
                                                    "architect-edge-model",
                                                    "Model Decides",
                                                )
                                                .label_size(LabelSize::XSmall)
                                                .toggle_state(is_model)
                                                .selected_style(ButtonStyle::Tinted(
                                                    TintColor::Accent,
                                                ))
                                                .style(ButtonStyle::Subtle)
                                                .disabled(source_locked)
                                                .on_click(cx.listener(
                                                    move |this, _, _, cx| {
                                                        let question = model_editor.read(cx).text(cx);
                                                        if question.trim().is_empty() {
                                                            this.report(
                                                                "Write the yes-or-no routing question first."
                                                                    .to_string(),
                                                                cx,
                                                            );
                                                            return;
                                                        }
                                                        this.set_edge_condition(
                                                            model_id.clone(),
                                                            EdgeCondition::LlmEvaluated { question },
                                                            cx,
                                                        );
                                                    },
                                                )),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(cx.theme().colors().border)
                                            .bg(cx.theme().colors().editor_background)
                                            .child(condition_editor),
                                    )
                                    .child(
                                        Label::new(evaluation)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                Button::new("architect-delete-edge", "Delete Connection")
                                    .full_width()
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Subtle)
                                    .start_icon(Icon::new(IconName::Trash).size(IconSize::XSmall))
                                    .disabled(source_locked)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_selection(
                                            Some(Selection::Edge(delete_id.clone())),
                                            window,
                                            cx,
                                        );
                                        this.delete_selection(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any()
    }

    /// The contextual panel beside the graph.
    pub(super) fn render_inspector(
        &self,
        width: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match &self.selection {
            None => return Some(self.render_overview_inspector(width, cx)),
            Some(Selection::Edge(id)) => return Some(self.render_edge_inspector(id, width, cx)),
            Some(Selection::Node(_)) => {}
        }

        let inspector = self.inspector.as_ref()?;
        let node = self.graph(cx)?.node(&inspector.node)?.clone();

        let title_editor = inspector.title.clone();
        let responsibility_editor = inspector.responsibility.clone();
        let goal_editor = inspector.goal.clone();
        let capture_editor = inspector.capture.clone();
        let rule_editor = inspector.new_rule.clone();
        let id = node.id.clone();
        let footer_lock_id = node.id.clone();
        let pin_id = node.id.clone();
        let details_subplan_id = node.id.clone();
        let chat_tab_id = node.id.clone();
        let tab = self.inspector_tab;
        let step_chat = node
            .chat
            .as_ref()
            .and_then(|session_id| self.step_chat_view(session_id, cx));
        let subplan_steps = node.subplan().map_or(0, |subplan| subplan.nodes.len());
        // The steps inside, listed here so a sub-plan can be read without
        // leaving the step that contains it.
        let subplan_titles: Vec<(SharedString, bool)> = node
            .subplan()
            .map(|subplan| {
                subplan
                    .nodes
                    .iter()
                    .map(|step| (SharedString::from(step.title.clone()), step.locked))
                    .collect()
            })
            .unwrap_or_default();
        // Where this step hands off to, and on what terms.
        let leads_to: Vec<(SharedString, SharedString, bool)> = self
            .graph(cx)
            .map(|graph| {
                graph
                    .edges_from(&node.id)
                    .map(|edge| {
                        let target = graph
                            .node(&edge.to)
                            .map(|node| SharedString::from(node.title.clone()))
                            .unwrap_or_else(|| SharedString::from(edge.to.0.clone()));
                        let judged = matches!(edge.condition, EdgeCondition::LlmEvaluated { .. });
                        let condition = match edge.condition.label() {
                            Some(label) => SharedString::from(label.to_string()),
                            None => SharedString::from("always"),
                        };
                        (target, condition, judged)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let node_path = self.focus.child(node.id.clone());
        let run_activity: Vec<(SharedString, usize, u64, bool, Option<SharedString>)> = self
            .thread
            .read(cx)
            .architect_run()
            .map(|run| {
                run.history()
                    .iter()
                    .filter(|step| step.path == node_path)
                    .map(|step| {
                        (
                            step.title.clone(),
                            step.attempt,
                            step.elapsed().as_secs(),
                            step.is_running(),
                            step.summary.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let locked = node.locked;
        let has_chat = node.chat.is_some();
        // A step nothing leads out of has nobody to hand anything to, so an
        // empty capture there is a decision rather than an oversight.
        let hands_on = self
            .graph(cx)
            .is_some_and(|graph| graph.edges_from(&node.id).next().is_some());
        let wants_capture = hands_on && node.capture.trim().is_empty();

        // Uppercase, so a field's name reads as a heading rather than as more
        // of the prose it labels.
        let field = |label: &'static str| {
            Label::new(label.to_uppercase())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
        };

        Some(
            // The inset lives on a full-height wrapper rather than as a margin on
            // the card. The canvas row centres its children, so a card that sized
            // itself would collapse to its contents and leave the chat, and the
            // scrolling bodies, with no height to fill.
            div()
                .flex_none()
                .h_full()
                .py_2()
                .pr_2()
                .child(
            // A card inset from the canvas rather than a wall against it: the
            // inspector is about one step, and butting it against the edge makes
            // it read as part of the window instead.
            v_flex()
                .w(width)
                .h_full()
                .overflow_hidden()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().colors().border)
                .shadow_md()
                .bg(cx.theme().colors().panel_background)
                // The step's own name, so the panel says which step it is
                // without the canvas having to be read alongside it.
                .child(
                    h_flex()
                        .w_full()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .justify_between()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new(node.title.clone())
                                .size(LabelSize::Default)
                                .truncate(),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(if locked {
                                        IconName::Lock
                                    } else {
                                        IconName::Pencil
                                    })
                                    .size(IconSize::XSmall)
                                    .color(if locked {
                                        Color::Success
                                    } else {
                                        Color::Muted
                                    }),
                                )
                                .child(
                                    Label::new(if locked { "Settled" } else { "Draft" })
                                        .size(LabelSize::Small)
                                        .color(if locked {
                                            Color::Success
                                        } else {
                                            Color::Muted
                                        }),
                                )
                                .child(self.render_inspector_resize_controls(cx)),
                        ),
                )
                .child(
                    h_flex()
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Button::new("architect-tab-details", "Details")
                                .label_size(LabelSize::Small)
                                .toggle_state(tab == InspectorTab::Details)
                                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.inspector_tab = InspectorTab::Details;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("architect-tab-chat", "Conversation")
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(if has_chat {
                                        IconName::Sparkle
                                    } else {
                                        IconName::Plus
                                    })
                                    .size(IconSize::XSmall),
                                )
                                .toggle_state(tab == InspectorTab::Conversation)
                                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.discuss_node(chat_tab_id.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new("architect-tab-subplan", "Activity")
                                .label_size(LabelSize::Small)
                                .toggle_state(tab == InspectorTab::Activity)
                                .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                                .style(ButtonStyle::Subtle)
                                .tooltip(Tooltip::text(if subplan_steps > 0 {
                                    "Execution history, nested work, and handoffs"
                                } else {
                                    "Execution history and handoffs for this step"
                                }))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.inspector_tab = InspectorTab::Activity;
                                    cx.notify();
                                })),
                        ),
                )
                .when(tab == InspectorTab::Conversation, |this| {
                    this.child(match step_chat {
                        // `min_h_0` so the conversation can shrink inside the
                        // card instead of growing it and pushing the lock
                        // button out of the clipped edge.
                        Some(view) => div()
                            .flex_1()
                            .w_full()
                            .min_h_0()
                            .overflow_hidden()
                            .child(view)
                            .into_any(),
                        None => v_flex()
                            .flex_1()
                            .p_3()
                            .gap_2()
                            .child(
                                Label::new("Opening this step's conversation…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(
                                    "It starts knowing what the main conversation knows, then \
                                     stays out of the other steps' way.",
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                            .into_any(),
                    })
                })
                .when(tab == InspectorTab::Details, |this| {
                    this.child(
                    v_flex()
                        .id("architect-inspector-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_3()
                        .gap_3()
                        // The name is already the panel's heading, so the field
                        // is only worth its space while it can still be changed.
                        .when(!locked, |this| {
                            this.child(
                                v_flex().gap_1().child(field("Name")).child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(cx.theme().colors().border)
                                        .bg(cx.theme().colors().editor_background)
                                        .child(title_editor),
                                ),
                            )
                        })
                        .child(
                            v_flex()
                                .gap_1()
                                .child(field("Responsibility"))
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(cx.theme().colors().border)
                                        .bg(cx.theme().colors().editor_background)
                                        .child(responsibility_editor),
                                ),
                        )
                        .child(
                            v_flex().gap_1().child(field("Goal")).child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .bg(cx.theme().colors().editor_background)
                                    .child(goal_editor),
                            ),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(field("Rules"))
                                .children(node.rules.iter().enumerate().map(|(ix, rule)| {
                                    h_flex()
                                        .w_full()
                                        .gap_1()
                                        .justify_between()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .bg(cx.theme().colors().element_background)
                                        .child(Label::new(rule.clone()).size(LabelSize::Small))
                                        .when(!locked, |this| {
                                            this.child(
                                                IconButton::new(
                                                    ("architect-rule-remove", ix),
                                                    IconName::Close,
                                                )
                                                .icon_size(IconSize::XSmall)
                                                .tooltip(Tooltip::text("Remove this rule"))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.remove_rule(ix, cx)
                                                })),
                                            )
                                        })
                                }))
                                .when(node.rules.is_empty(), |this| {
                                    this.child(
                                        Label::new("No rules yet.")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
                                .when(!locked, |this| {
                                    this.child(
                                        h_flex()
                                            .w_full()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_sm()
                                                    .border_1()
                                                    .border_color(cx.theme().colors().border)
                                                    .bg(cx.theme().colors().editor_background)
                                                    .child(rule_editor.clone()),
                                            )
                                            .child(
                                                IconButton::new(
                                                    "architect-rule-add",
                                                    IconName::Plus,
                                                )
                                                .icon_size(IconSize::Small)
                                                .tooltip(Tooltip::text("Add this rule"))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        let rule = rule_editor.read(cx).text(cx);
                                                        this.add_rule(rule, window, cx);
                                                    },
                                                )),
                                            ),
                                    )
                                }),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .child(field("Capture In Summary"))
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(
                                                    Label::new(if node.pinned {
                                                        "told to every later step"
                                                    } else if hands_on {
                                                        "feeds the next step"
                                                    } else {
                                                        "nothing follows this step"
                                                    })
                                                    .size(LabelSize::XSmall)
                                                    .color(if node.pinned {
                                                        Color::Accent
                                                    } else {
                                                        Color::Muted
                                                    }),
                                                )
                                                .child(
                                                    IconButton::new(
                                                        "architect-pin-step",
                                                        if node.pinned {
                                                            IconName::StarFilled
                                                        } else {
                                                            IconName::Star
                                                        },
                                                    )
                                                    .icon_size(IconSize::XSmall)
                                                    .icon_color(if node.pinned {
                                                        Color::Accent
                                                    } else {
                                                        Color::Muted
                                                    })
                                                    .disabled(locked)
                                                    .tooltip(Tooltip::text(if node.pinned {
                                                        "Every later step is told this summary. \
                                                         Click to tell only the next ones."
                                                    } else {
                                                        "Only the steps this one leads to are told \
                                                         its summary. Click to tell every later \
                                                         step."
                                                    }))
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            let id = pin_id.clone();
                                                            this.edit_node(
                                                                id,
                                                                |node| node.pinned = !node.pinned,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(if wants_capture {
                                            cx.theme().status().warning_border
                                        } else {
                                            cx.theme().colors().border
                                        })
                                        .bg(cx.theme().colors().editor_background)
                                        .child(capture_editor),
                                )
                                .when(wants_capture, |this| {
                                    this.child(
                                        Label::new(
                                            "This step leads somewhere but says nothing about \
                                             what it hands on. The steps after it see only this \
                                             summary.",
                                        )
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                    )
                                })
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(field("Nested Plan"))
                                .child(
                                    Button::new(
                                        "architect-details-open-subplan",
                                        match subplan_steps {
                                            0 => "Break Into Steps".to_string(),
                                            1 => "Open Nested Plan (1 step)".to_string(),
                                            count => {
                                                format!("Open Nested Plan ({count} steps)")
                                            }
                                        },
                                    )
                                    .full_width()
                                    .label_size(LabelSize::Small)
                                    .start_icon(
                                        Icon::new(IconName::ListTree).size(IconSize::XSmall),
                                    )
                                    .disabled(locked && subplan_steps == 0)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.drill_into(details_subplan_id.clone(), window, cx);
                                    })),
                                )
                                .child(
                                    Label::new(
                                        "Use a nested plan when this responsibility needs several coordinated steps at a closer level.",
                                    )
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        )
                        .when_some(node.result.clone(), |this, result| {
                            this.child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .child(field("Last Result"))
                                            .child(
                                                Label::new(format!(
                                                    "attempt {}",
                                                    result.attempt.max(1)
                                                ))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                            ),
                                    )
                                    .child(
                                        // Tinted, not just outlined: a result is
                                        // the one thing here the step said rather
                                        // than something the user typed.
                                        div()
                                            .px_2()
                                            .py_1()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(cx.theme().status().success_border)
                                            .bg(cx.theme().status().success_background)
                                            .child(
                                                Label::new(result.summary)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                            ),
                                    ),
                            )
                        })
                        .when(!locked, |this| {
                            this.child(
                                Button::new("architect-delete-step", "Delete Step")
                                    .full_width()
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Subtle)
                                    .start_icon(Icon::new(IconName::Trash).size(IconSize::XSmall))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_selection(
                                            Some(Selection::Node(id.clone())),
                                            window,
                                            cx,
                                        );
                                        this.delete_selection(window, cx);
                                    })),
                            )
                        }),
                )
                })
                .when(tab == InspectorTab::Activity, |this| {
                    this.child(
                        v_flex()
                            .id("architect-inspector-subplan")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_3()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .child(field("Run Activity"))
                                            .child(
                                                Label::new("source · plan conversation")
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted),
                                            ),
                                    )
                                    .when(run_activity.is_empty(), |this| {
                                        this.child(
                                            Label::new(
                                                "No execution visits have reached this step yet.",
                                            )
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                        )
                                    })
                                    .children(run_activity.iter().enumerate().map(
                                        |(index, (title, attempt, elapsed, running, summary))| {
                                            v_flex()
                                                .id(("architect-step-activity", index))
                                                .gap_0p5()
                                                .px_2()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(cx.theme().colors().element_background)
                                                .child(
                                                    h_flex()
                                                        .w_full()
                                                        .gap_1()
                                                        .child(
                                                            Icon::new(if *running {
                                                                IconName::PlayFilled
                                                            } else {
                                                                IconName::Check
                                                            })
                                                            .size(IconSize::XSmall)
                                                            .color(if *running {
                                                                Color::Info
                                                            } else {
                                                                Color::Success
                                                            }),
                                                        )
                                                        .child(
                                                            Label::new(title.clone())
                                                                .size(LabelSize::Small)
                                                                .truncate(),
                                                        )
                                                        .child(div().flex_1())
                                                        .child(
                                                            Label::new(format!(
                                                                "attempt {attempt} · {elapsed}s"
                                                            ))
                                                            .size(LabelSize::XSmall)
                                                            .color(Color::Muted),
                                                        ),
                                                )
                                                .when_some(summary.clone(), |this, summary| {
                                                    this.child(
                                                        Label::new(summary)
                                                            .size(LabelSize::XSmall)
                                                            .color(Color::Muted),
                                                    )
                                                })
                                        },
                                    )),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .child(field("Nested Plan"))
                                            .child(
                                                Label::new(match subplan_steps {
                                                    0 => "none yet".to_string(),
                                                    count => {
                                                        let locked_inside = subplan_titles
                                                            .iter()
                                                            .filter(|(_, locked)| *locked)
                                                            .count();
                                                        if locked_inside == count {
                                                            format!("{count} steps · all locked")
                                                        } else {
                                                            format!(
                                                                "{count} steps · {} still open",
                                                                count - locked_inside
                                                            )
                                                        }
                                                    }
                                                })
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                            ),
                                    )
                                    .children(subplan_titles.iter().enumerate().map(
                                        |(ix, (title, step_locked))| {
                                            h_flex()
                                                .w_full()
                                                .gap_1p5()
                                                .px_2()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(cx.theme().colors().element_background)
                                                // A square rather than a number:
                                                // what matters at a glance is
                                                // whether a step is settled.
                                                .child(
                                                    div()
                                                        .size(px(9.0))
                                                        .flex_none()
                                                        .rounded_sm()
                                                        .bg(if *step_locked {
                                                            cx.theme().status().success
                                                        } else {
                                                            cx.theme().colors().text_muted
                                                        }),
                                                )
                                                .child(
                                                    Label::new(title.clone())
                                                        .size(LabelSize::Small)
                                                        .truncate(),
                                                )
                                                .child(div().flex_1())
                                                .child(
                                                    Label::new(if *step_locked {
                                                        "locked"
                                                    } else {
                                                        "draft"
                                                    })
                                                    .size(LabelSize::XSmall)
                                                    .color(if *step_locked {
                                                        Color::Success
                                                    } else {
                                                        Color::Muted
                                                    }),
                                                )
                                                .id(("architect-subplan-step", ix))
                                        },
                                    ))
                                    .child(
                                        Label::new(if subplan_steps > 0 {
                                            "This step is carried out by running the plan inside \
                                             it. It cannot be locked until every step in there is."
                                        } else {
                                            "For work that is one step here but several up close. \
                                             The plan inside runs in place of this step."
                                        })
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    ),
                            )
                            // Where this step hands off to. It is the other half
                            // of the capture field: what is passed, and to whom.
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(field("Leads To"))
                                    .when(leads_to.is_empty(), |this| {
                                        this.child(
                                            Label::new(
                                                "Nothing follows this step. It is where a branch \
                                                 of the plan ends.",
                                            )
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                        )
                                    })
                                    .children(leads_to.iter().enumerate().map(
                                        |(ix, (target, condition, judged))| {
                                            h_flex()
                                                .id(("architect-leads-to", ix))
                                                .w_full()
                                                .gap_1p5()
                                                .px_2()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(cx.theme().colors().element_background)
                                                .child(
                                                    Icon::new(IconName::ArrowRight)
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(target.clone())
                                                        .size(LabelSize::Small)
                                                        .truncate(),
                                                )
                                                .child(div().flex_1())
                                                .child(
                                                    Label::new(truncate(condition, 18))
                                                        .size(LabelSize::XSmall)
                                                        .color(if *judged {
                                                            Color::Warning
                                                        } else {
                                                            Color::Muted
                                                        }),
                                                )
                                                .tooltip(Tooltip::text(condition.clone()))
                                        },
                                    )),
                            ),
                    )
                })
                // The lock is the decision the whole panel is building towards,
                // so it sits where a decision belongs: at the bottom, in full,
                // whichever tab is open.
                .child(
                    div()
                        .w_full()
                        .flex_none()
                        .p_2()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Button::new(
                                "architect-inspector-lock-footer",
                                if locked {
                                    "Locked — unlock to edit"
                                } else {
                                    "Lock this step"
                                },
                            )
                            .full_width()
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(if locked {
                                TintColor::Success
                            } else {
                                TintColor::Accent
                            }))
                            .start_icon(Icon::new(IconName::Lock).size(IconSize::XSmall))
                            .tooltip(Tooltip::text(if locked {
                                "A locked step is settled, and the plan can only run once every \
                                 step is"
                            } else {
                                "Says this step is settled. Every step has to be locked before the \
                                 plan can run."
                            }))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.toggle_lock(footer_lock_id.clone(), window, cx);
                                },
                            )),
                        ),
                ),
                )
                .into_any(),
        )
    }
}
