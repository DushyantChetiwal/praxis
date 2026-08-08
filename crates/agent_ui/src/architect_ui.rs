//! The Architect canvas: a flowchart of the steps the agent will take.
//!
//! Edges are painted on a `gpui::canvas` layer underneath the nodes, while the
//! nodes themselves are ordinary absolutely-positioned elements on top. Doing
//! it the other way round would cost either curves (if edges were elements) or
//! text layout, hover and buttons inside nodes (if nodes were painted), so the
//! two halves are drawn the way each is best drawn.

use std::cell::Cell;
use std::rc::Rc;

use agent::Thread;
use agent_client_protocol::schema::v1 as acp;
use architect::{
    ArchitectGraph, ArchitectNode, EdgeCondition, EdgeId, GraphProblem, NodeId, Position,
};
use editor::{Editor, EditorEvent};
use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels,
    Point, Render, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, WeakEntity, Window,
    canvas, div, point, px,
};
use ui::{TintColor, Tooltip, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::AgentPanel;

/// Node size in canvas units. The layout's spacing is chosen around these, so
/// the two have to agree.
const NODE_WIDTH: f32 = 248.0;
const NODE_HEIGHT: f32 = 104.0;

const MIN_ZOOM: f32 = 0.35;
const MAX_ZOOM: f32 = 2.0;
const SCROLL_LINE_HEIGHT: f32 = 20.0;

/// Below this, node text would be an unreadable smear, so nodes show their
/// title alone and let the shape of the graph do the talking.
const DETAIL_ZOOM_THRESHOLD: f32 = 0.62;

/// Dragged nodes settle onto this grid, which is what keeps a hand-arranged
/// graph looking arranged rather than approximate.
const GRID_SNAP: f32 = 8.0;

/// How close a click has to be to an edge to select it, in screen pixels.
const EDGE_HIT_TOLERANCE: f32 = 9.0;

#[derive(Clone, Debug, PartialEq)]
enum Selection {
    Node(NodeId),
    Edge(EdgeId),
}

enum Interaction {
    None,
    /// Dragging the canvas itself.
    Panning { last: Point<Pixels> },
    /// Dragging a node. The grab offset keeps the node from jumping so its
    /// centre snaps to the cursor.
    DraggingNode { id: NodeId, grab: Point<f32> },
    /// Dragging a new connection out of a node's handle.
    Connecting { from: NodeId, at: Point<Pixels> },
}

impl Interaction {
    fn is_idle(&self) -> bool {
        matches!(self, Interaction::None)
    }
}

/// The editors backing the inspector for the selected step. They are rebuilt
/// whenever the selection changes, which is also what keeps their contents from
/// drifting away from the graph.
struct NodeInspector {
    node: NodeId,
    title: Entity<Editor>,
    goal: Entity<Editor>,
    new_rule: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

pub struct ArchitectPane {
    thread: Entity<Thread>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// The canvas layer reports its bounds during paint; everything that
    /// converts between screen and canvas space needs them.
    viewport: Rc<Cell<Option<Bounds<Pixels>>>>,
    pan: Point<Pixels>,
    zoom: f32,
    selection: Option<Selection>,
    inspector: Option<NodeInspector>,
    interaction: Interaction,
    hovered_node: Option<NodeId>,
    _thread_subscription: Subscription,
}

impl ArchitectPane {
    pub fn new(
        thread: Entity<Thread>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&thread, |_, _, cx| cx.notify());
        Self {
            thread,
            workspace,
            focus_handle: cx.focus_handle(),
            viewport: Rc::new(Cell::new(None)),
            pan: point(px(0.0), px(0.0)),
            zoom: 1.0,
            selection: None,
            inspector: None,
            interaction: Interaction::None,
            hovered_node: None,
            _thread_subscription: subscription,
        }
    }

    /// Opens the canvas for a thread, reusing the tab if it is already showing
    /// that thread's plan.
    pub fn open(
        thread: Entity<Thread>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace
            .active_pane()
            .read(cx)
            .items()
            .filter_map(|item| item.downcast::<ArchitectPane>())
            .find(|pane| pane.read(cx).thread == thread);

        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let handle = cx.weak_entity();
        let pane = cx.new(|cx| ArchitectPane::new(thread, handle, cx));
        workspace.add_item_to_active_pane(Box::new(pane), None, true, window, cx);
    }

    fn graph<'a>(&self, cx: &'a Context<Self>) -> Option<&'a ArchitectGraph> {
        self.thread.read(cx).architect_graph()
    }

    fn edit_graph(&mut self, edit: impl FnOnce(&mut ArchitectGraph), cx: &mut Context<Self>) {
        self.thread.update(cx, |thread, cx| {
            thread.update_architect_graph(edit, cx);
        });
        cx.notify();
    }

    fn set_selection(
        &mut self,
        selection: Option<Selection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected_node = match &selection {
            Some(Selection::Node(id)) => Some(id.clone()),
            _ => None,
        };

        if self.inspector.as_ref().map(|inspector| &inspector.node) != selected_node.as_ref() {
            self.inspector = selected_node
                .and_then(|id| self.graph(cx).and_then(|graph| graph.node(&id)).cloned())
                .map(|node| self.build_inspector(node, window, cx));
        }

        self.selection = selection;
        cx.notify();
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
        let goal = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_placeholder_text("What does this step have to accomplish?", window, cx);
            editor.set_text(node.intent.clone(), window, cx);
            editor.set_read_only(node.locked);
            editor
        });
        let new_rule = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Add a rule and press enter", window, cx);
            editor
        });

        let id_for_title = node.id.clone();
        let id_for_goal = node.id.clone();
        let subscriptions = vec![
            cx.subscribe(&title, move |this, editor, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    let id = id_for_title.clone();
                    this.edit_graph(
                        move |graph| {
                            if let Some(node) = graph.node_mut(&id) {
                                node.title = text;
                            }
                        },
                        cx,
                    );
                }
            }),
            cx.subscribe(&goal, move |this, editor, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let text = editor.read(cx).text(cx);
                    let id = id_for_goal.clone();
                    this.edit_graph(
                        move |graph| {
                            if let Some(node) = graph.node_mut(&id) {
                                node.intent = text;
                            }
                        },
                        cx,
                    );
                }
            }),
        ];

        NodeInspector {
            node: node.id,
            title,
            goal,
            new_rule,
            _subscriptions: subscriptions,
        }
    }

    // -- Coordinate conversion ------------------------------------------------
    //
    // Canvas space is what the graph stores and what the layout produces; it
    // does not move when the user pans or zooms. Screen space is where the
    // mouse lives.

    fn origin(&self) -> Point<Pixels> {
        let Some(bounds) = self.viewport.get() else {
            return point(px(0.0), px(0.0));
        };
        point(
            bounds.origin.x + bounds.size.width / 2.0 + self.pan.x,
            bounds.origin.y + bounds.size.height / 2.0 + self.pan.y,
        )
    }

    fn to_screen(&self, position: Position) -> Point<Pixels> {
        let origin = self.origin();
        point(
            origin.x + px(position.x * self.zoom),
            origin.y + px(position.y * self.zoom),
        )
    }

    fn to_canvas(&self, screen: Point<Pixels>) -> Position {
        let origin = self.origin();
        Position {
            x: f32::from(screen.x - origin.x) / self.zoom,
            y: f32::from(screen.y - origin.y) / self.zoom,
        }
    }

    fn node_at(&self, screen: Point<Pixels>, cx: &Context<Self>) -> Option<NodeId> {
        let canvas = self.to_canvas(screen);
        let graph = self.graph(cx)?;
        graph
            .nodes
            .iter()
            .rev()
            .find(|node| {
                let Some(position) = node.position else {
                    return false;
                };
                (canvas.x - position.x).abs() <= NODE_WIDTH / 2.0
                    && (canvas.y - position.y).abs() <= NODE_HEIGHT / 2.0
            })
            .map(|node| node.id.clone())
    }

    fn edge_at(&self, screen: Point<Pixels>, cx: &Context<Self>) -> Option<EdgeId> {
        let graph = self.graph(cx)?;
        let tolerance = EDGE_HIT_TOLERANCE / self.zoom;
        let canvas = self.to_canvas(screen);

        let mut closest: Option<(f32, EdgeId)> = None;
        for edge in &graph.edges {
            let (Some(from), Some(to)) = (
                graph.node(&edge.from).and_then(|node| node.position),
                graph.node(&edge.to).and_then(|node| node.position),
            ) else {
                continue;
            };

            let curve = EdgeCurve::between(from, to);
            let distance = curve.distance_to(canvas);
            if distance <= tolerance
                && closest
                    .as_ref()
                    .is_none_or(|(best, _)| distance < *best)
            {
                closest = Some((distance, edge.id.clone()));
            }
        }
        closest.map(|(_, id)| id)
    }

    // -- View controls --------------------------------------------------------

    fn set_zoom(&mut self, zoom: f32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let previous = self.zoom;
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);

        // Keep whatever is under the cursor exactly where it is, so zooming
        // feels like moving closer rather than like the graph sliding away.
        if let Some((anchor, bounds)) = anchor.zip(self.viewport.get()) {
            let centre = point(
                anchor.x - bounds.origin.x - bounds.size.width / 2.0,
                anchor.y - bounds.origin.y - bounds.size.height / 2.0,
            );
            let offset = centre - self.pan;
            let ratio = self.zoom / previous;
            self.pan += offset * (1.0 - ratio);
        }

        cx.notify();
    }

    /// Frames the whole graph, which is the only reliable way back when someone
    /// has panned into empty space.
    fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        let Some(bounds) = self.viewport.get() else {
            return;
        };
        let Some(graph) = self.graph(cx) else {
            return;
        };

        let positions: Vec<Position> = graph.nodes.iter().filter_map(|node| node.position).collect();
        if positions.is_empty() {
            return;
        }

        let min_x = positions.iter().map(|p| p.x).fold(f32::MAX, f32::min) - NODE_WIDTH / 2.0;
        let max_x = positions.iter().map(|p| p.x).fold(f32::MIN, f32::max) + NODE_WIDTH / 2.0;
        let min_y = positions.iter().map(|p| p.y).fold(f32::MAX, f32::min) - NODE_HEIGHT / 2.0;
        let max_y = positions.iter().map(|p| p.y).fold(f32::MIN, f32::max) + NODE_HEIGHT / 2.0;

        const MARGIN: f32 = 64.0;
        let width = (max_x - min_x).max(1.0);
        let height = (max_y - min_y).max(1.0);
        let zoom = ((f32::from(bounds.size.width) - MARGIN) / width)
            .min((f32::from(bounds.size.height) - MARGIN) / height)
            .clamp(MIN_ZOOM, 1.0);

        self.zoom = zoom;
        let centre_x = (min_x + max_x) / 2.0;
        let centre_y = (min_y + max_y) / 2.0;
        self.pan = point(px(-centre_x * zoom), px(-centre_y * zoom));
        cx.notify();
    }

    // -- Editing --------------------------------------------------------------

    fn delete_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        match selection {
            Selection::Node(id) => {
                // A locked step has been settled deliberately; deleting it by a
                // stray keypress would throw that away silently.
                if self
                    .graph(cx)
                    .and_then(|graph| graph.node(&id))
                    .is_some_and(|node| node.locked)
                {
                    return;
                }
                self.set_selection(None, window, cx);
                self.edit_graph(|graph| graph.remove_node(&id), cx);
            }
            Selection::Edge(id) => {
                self.set_selection(None, window, cx);
                self.edit_graph(|graph| graph.disconnect(&id), cx);
            }
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

        self.edit_graph(
            move |graph| {
                if let Some(node) = graph.node_mut(&id) {
                    node.rules.push(rule);
                }
            },
            cx,
        );
    }

    fn remove_rule(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(inspector) = &self.inspector else {
            return;
        };
        let id = inspector.node.clone();
        self.edit_graph(
            move |graph| {
                if let Some(node) = graph.node_mut(&id)
                    && index < node.rules.len()
                {
                    node.rules.remove(index);
                }
            },
            cx,
        );
    }

    /// Hands a prompt to the conversation that owns this plan, and shows it.
    fn send_to_plan_chat(&self, prompt: String, submit: bool, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        workspace.update(cx, |workspace, cx| {
            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                return;
            };
            let Some(conversation_view) = panel.read(cx).active_conversation_view().cloned() else {
                return;
            };

            // The plan is the main thread's to run, so make sure a step's
            // conversation is not what receives this.
            conversation_view.update(cx, |conversation_view, cx| {
                conversation_view.return_to_root_thread(cx);
            });
            let Some(thread_view) = conversation_view.read(cx).root_thread_view() else {
                return;
            };

            workspace.focus_panel::<AgentPanel>(window, cx);
            thread_view.update(cx, |thread_view, cx| {
                thread_view.message_editor.update(cx, |editor, cx| {
                    editor.set_message(
                        vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                        window,
                        cx,
                    );
                });
                if submit {
                    thread_view.send(window, cx);
                } else {
                    thread_view
                        .message_editor
                        .focus_handle(cx)
                        .focus(window, cx);
                }
            });
        });
    }

    /// Opens the step's own conversation in the agent panel.
    ///
    /// Each step gets a thread of its own, inheriting the main conversation so
    /// it knows what the plan is for, then diverging. That divergence is the
    /// point: settling this step cannot crowd out the context the next step will
    /// be settled in.
    fn discuss_node(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.graph(cx).and_then(|graph| graph.node(&id)).cloned() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let is_first_visit = node.chat.is_none();
        let session_id = workspace.update(cx, |workspace, cx| {
            let panel = workspace.panel::<AgentPanel>(cx)?;
            let conversation_view = panel.read(cx).active_conversation_view()?.clone();
            workspace.focus_panel::<AgentPanel>(window, cx);
            conversation_view.update(cx, |conversation_view, cx| {
                conversation_view.open_architect_step_thread(
                    node.id.clone(),
                    node.title.clone().into(),
                    node.chat.clone(),
                    window,
                    cx,
                )
            })
        });

        let Some(session_id) = session_id else {
            log::error!("Architect: could not open a conversation for this step");
            return;
        };

        if node.chat.as_ref() != Some(&session_id) {
            let session_id = session_id.clone();
            self.edit_graph(
                move |graph| {
                    if let Some(node) = graph.node_mut(&id) {
                        node.chat = Some(session_id);
                    }
                },
                cx,
            );
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
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

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

        workspace.update(cx, |workspace, cx| {
            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                return;
            };
            let Some(conversation_view) = panel.read(cx).active_conversation_view().cloned() else {
                return;
            };
            let Some(thread_view) = conversation_view.read(cx).thread_view(session_id) else {
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
        });
    }

    /// Compiles the locked plan into an ordered spec and hands it to the chat.
    ///
    /// This is the seam a real executor would replace: today the agent is told
    /// the plan in the order the graph implies, with every condition and loop
    /// spelled out, rather than being driven through the graph node by node.
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(graph) = self.graph(cx) else {
            return;
        };
        let spec = match architect::compile_spec(graph) {
            Ok(spec) => spec,
            Err(problems) => {
                log::warn!(
                    "Architect: refusing to run a plan that is not ready: {}",
                    problems
                        .iter()
                        .map(|problem| problem.to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                return;
            }
        };

        self.send_to_plan_chat(spec, true, window, cx);
    }

    fn toggle_lock(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        // Pressing the button also pressed the node underneath it, which began
        // a drag that no mouse-up is going to end.
        self.interaction = Interaction::None;

        let locked = self
            .graph(cx)
            .and_then(|graph| graph.node(&id))
            .is_some_and(|node| node.locked);
        self.edit_graph(|graph| graph.set_locked(&id, !locked), cx);
        self.refresh_inspector(window, cx);
    }

    fn set_all_locked(&mut self, locked: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_graph(
            |graph| {
                for node in &mut graph.nodes {
                    node.locked = locked;
                }
            },
            cx,
        );
        self.refresh_inspector(window, cx);
    }

    /// Rebuilds the inspector so its fields match the step again, which is what
    /// makes them go read-only the moment a step is locked.
    fn refresh_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.inspector.as_ref().map(|inspector| inspector.node.clone()) else {
            return;
        };
        let Some(node) = self.graph(cx).and_then(|graph| graph.node(&id)).cloned() else {
            self.inspector = None;
            return;
        };
        self.inspector = Some(self.build_inspector(node, window, cx));
        cx.notify();
    }

    fn add_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // New steps land in the middle of the view rather than at the origin,
        // so one appears where the user is looking.
        let centre = self.to_canvas(self.origin());
        let id = NodeId(format!("step-{}", uuid::Uuid::new_v4().simple()));
        let mut node = ArchitectNode::new(id.clone(), "New step");
        node.position = Some(centre);

        self.edit_graph(
            move |graph| {
                graph.add_node(node);
            },
            cx,
        );
        self.set_selection(Some(Selection::Node(id)), window, cx);
    }

    fn tidy_up(&mut self, cx: &mut Context<Self>) {
        self.edit_graph(|graph| graph.relayout(), cx);
        self.zoom_to_fit(cx);
    }

    // -- Input ----------------------------------------------------------------

    fn handle_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.modifiers.control || event.modifiers.platform {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_HEIGHT,
            };
            let factor = if delta > 0.0 {
                1.0 + delta.abs() * 0.006
            } else {
                1.0 / (1.0 + delta.abs() * 0.006)
            };
            self.set_zoom(self.zoom * factor, Some(event.position), cx);
        } else {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels,
                ScrollDelta::Lines(lines) => lines.map(|value| px(value * SCROLL_LINE_HEIGHT)),
            };
            self.pan += delta;
            cx.notify();
        }
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        self.focus_handle.focus(window, cx);

        // Nodes handle their own presses, so reaching here means empty canvas
        // or an edge.
        let selection = self.edge_at(event.position, cx).map(Selection::Edge);
        self.set_selection(selection, window, cx);

        self.interaction = Interaction::Panning {
            last: event.position,
        };
        cx.notify();
    }

    fn handle_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        match &mut self.interaction {
            Interaction::None => {
                let hovered = self.node_at(event.position, cx);
                if hovered != self.hovered_node {
                    self.hovered_node = hovered;
                    cx.notify();
                }
            }
            Interaction::Panning { last } => {
                let delta = event.position - *last;
                *last = event.position;
                self.pan += delta;
                cx.notify();
            }
            Interaction::DraggingNode { id, grab } => {
                let id = id.clone();
                let grab = *grab;
                let canvas = self.to_canvas(event.position);
                let position = Position {
                    x: snap(canvas.x - grab.x),
                    y: snap(canvas.y - grab.y),
                };
                self.edit_graph(
                    move |graph| {
                        if let Some(node) = graph.node_mut(&id) {
                            node.position = Some(position);
                        }
                    },
                    cx,
                );
            }
            Interaction::Connecting { at, .. } => {
                *at = event.position;
                cx.notify();
            }
        }
    }

    fn handle_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Interaction::Connecting { from, .. } = &self.interaction {
            let from = from.clone();
            if let Some(to) = self.node_at(event.position, cx)
                && to != from
            {
                self.edit_graph(move |graph| _ = graph.connect(from, to), cx);
            }
        }
        self.interaction = Interaction::None;
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The inspector's editors have focus of their own, so a keypress that
        // reaches the canvas is one meant for the canvas.
        match event.keystroke.key.as_str() {
            "delete" | "backspace" => self.delete_selection(window, cx),
            "escape" => {
                self.set_selection(None, window, cx);
                self.interaction = Interaction::None;
            }
            _ => {}
        }
    }

    // -- Rendering ------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let graph = self.graph(cx);
        let step_count = graph.map_or(0, |graph| graph.nodes.len());
        let locked_count = graph.map_or(0, |graph| {
            graph.nodes.iter().filter(|node| node.locked).count()
        });
        let problems: Vec<GraphProblem> = graph.map(|graph| graph.problems()).unwrap_or_default();
        let all_locked = step_count > 0 && locked_count == step_count;
        let ready_to_run = all_locked && problems.is_empty();
        let run_tooltip = if ready_to_run {
            "Hand the plan to the chat and start work"
        } else if step_count == 0 {
            "There is no plan yet"
        } else if !problems.is_empty() {
            "Fix the problems with the plan first"
        } else {
            "Lock every step first"
        };

        h_flex()
            .w_full()
            .flex_none()
            .px_3()
            .py_2()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().toolbar_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Architect").size(LabelSize::Small))
                    .child(
                        Label::new(match step_count {
                            0 => "no steps".to_string(),
                            1 => "1 step".to_string(),
                            _ => format!("{locked_count} of {step_count} steps locked"),
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when(!problems.is_empty(), |this| {
                        let summary: SharedString = problems
                            .iter()
                            .map(|problem| problem.to_string())
                            .collect::<Vec<_>>()
                            .join("\n")
                            .into();
                        this.child(
                            h_flex()
                                .id("architect-problems")
                                .gap_1()
                                .child(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::XSmall)
                                        .color(Color::Warning),
                                )
                                .child(
                                    Label::new(match problems.len() {
                                        1 => "1 problem".to_string(),
                                        count => format!("{count} problems"),
                                    })
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                                )
                                .tooltip(Tooltip::text(summary)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("architect-add-step", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Add a step"))
                            .on_click(cx.listener(|this, _, window, cx| this.add_step(window, cx))),
                    )
                    .child(
                        IconButton::new("architect-tidy", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Tidy up the layout"))
                            .on_click(cx.listener(|this, _, _, cx| this.tidy_up(cx))),
                    )
                    .child(
                        IconButton::new("architect-fit", IconName::Maximize)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Fit to view"))
                            .on_click(cx.listener(|this, _, _, cx| this.zoom_to_fit(cx))),
                    )
                    .child(
                        Button::new(
                            "architect-lock-all",
                            if all_locked { "Unlock All" } else { "Lock All" },
                        )
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Lock).size(IconSize::XSmall))
                        .disabled(step_count == 0)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_all_locked(!all_locked, window, cx)
                        })),
                    )
                    .child(
                        Button::new("architect-run", "Run")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::XSmall))
                            .disabled(!ready_to_run)
                            .tooltip(Tooltip::text(run_tooltip))
                            .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
                    ),
            )
            .into_any()
    }

    /// The panel beside the canvas where a single step is settled: its goal and
    /// rules edited directly, or talked through in the chat.
    fn render_inspector(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let inspector = self.inspector.as_ref()?;
        let node = self.graph(cx)?.node(&inspector.node)?.clone();

        let title_editor = inspector.title.clone();
        let goal_editor = inspector.goal.clone();
        let rule_editor = inspector.new_rule.clone();
        let id = node.id.clone();
        let lock_id = node.id.clone();
        let discuss_id = node.id.clone();
        let locked = node.locked;
        let has_chat = node.chat.is_some();

        let field = |label: &'static str| {
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(Color::Muted)
        };

        Some(
            v_flex()
                .w(px(340.0))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().panel_background)
                .child(
                    h_flex()
                        .w_full()
                        .px_3()
                        .py_2()
                        .justify_between()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new(if locked { "Locked Step" } else { "Step" })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("architect-inspector-lock", if locked { "Unlock" } else { "Lock" })
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Lock).size(IconSize::XSmall))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.toggle_lock(lock_id.clone(), window, cx);
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .id("architect-inspector-body")
                        .flex_1()
                        .overflow_y_scroll()
                        .p_3()
                        .gap_3()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(field("Name"))
                                .child(
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
                        .child(
                            v_flex()
                                .gap_1()
                                .child(field("Goal"))
                                .child(
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
                                                .on_click(cx.listener(
                                                    move |this, _, _, cx| this.remove_rule(ix, cx),
                                                )),
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
                                                IconButton::new("architect-rule-add", IconName::Plus)
                                                    .icon_size(IconSize::Small)
                                                    .tooltip(Tooltip::text("Add this rule"))
                                                    .on_click(cx.listener(move |this, _, window, cx| {
                                                        let rule = rule_editor.read(cx).text(cx);
                                                        this.add_rule(rule, window, cx);
                                                    })),
                                            ),
                                    )
                                }),
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(field("Deliberation"))
                                .child(
                                    Button::new(
                                        "architect-discuss",
                                        if has_chat {
                                            "Back to This Step's Chat"
                                        } else {
                                            "Discuss This Step"
                                        },
                                    )
                                    .full_width()
                                    .label_size(LabelSize::Small)
                                    .start_icon(Icon::new(IconName::Sparkle).size(IconSize::XSmall))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.discuss_node(discuss_id.clone(), window, cx);
                                    })),
                                )
                                .child(
                                    Label::new(if has_chat {
                                        "This step has its own conversation. Reopening returns to \
                                         where you left it."
                                    } else {
                                        "Opens a conversation for this step alone. It starts \
                                         knowing everything the main chat knows, then stays out of \
                                         the other steps' way."
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        )
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
                .into_any(),
        )
    }

    fn render_empty_state(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(Label::new("No plan yet").color(Color::Muted))
            .child(
                div().max_w(px(420.0)).child(
                    Label::new(
                        "Describe what you want built in the chat, in the Architect profile. \
                         The steps will appear here as a flowchart you can rearrange, \
                         discuss one at a time, and lock when you are happy with them.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
            )
            .bg(cx.theme().colors().editor_background)
            .into_any()
    }

    fn render_edges(&self, cx: &Context<Self>) -> AnyElement {
        let Some(graph) = self.graph(cx) else {
            return div().into_any();
        };

        let theme = cx.theme().colors();
        let edge_color = theme.text_muted.opacity(0.55);
        let selected_color = theme.text_accent;
        let pending_color = theme.text_accent.opacity(0.7);

        let mut curves = Vec::new();
        for edge in &graph.edges {
            let (Some(from), Some(to)) = (
                graph.node(&edge.from).and_then(|node| node.position),
                graph.node(&edge.to).and_then(|node| node.position),
            ) else {
                continue;
            };
            let selected = self.selection == Some(Selection::Edge(edge.id.clone()));
            curves.push((
                EdgeCurve::between(from, to),
                if selected { selected_color } else { edge_color },
                selected,
            ));
        }

        let pending = match &self.interaction {
            Interaction::Connecting { from, at } => graph
                .node(from)
                .and_then(|node| node.position)
                .map(|position| (position, *at)),
            _ => None,
        };

        let zoom = self.zoom;
        let viewport = self.viewport.clone();
        let pan = self.pan;
        let entity_id = cx.entity_id();

        canvas(
            move |_, _, _| {},
            move |bounds: Bounds<Pixels>, _, window: &mut Window, cx: &mut App| {
                if viewport.get() != Some(bounds) {
                    viewport.set(Some(bounds));
                    // Everything positioned from these bounds was laid out with
                    // the previous ones, so the frame needs redoing.
                    cx.notify(entity_id);
                }

                let origin = point(
                    bounds.origin.x + bounds.size.width / 2.0 + pan.x,
                    bounds.origin.y + bounds.size.height / 2.0 + pan.y,
                );
                let to_screen = |position: Position| {
                    point(
                        origin.x + px(position.x * zoom),
                        origin.y + px(position.y * zoom),
                    )
                };

                window.paint_layer(bounds, |window| {
                    for (curve, color, selected) in &curves {
                        let width = px(if *selected { 2.4 } else { 1.6 });
                        paint_curve(curve, &to_screen, width, *color, window);
                    }

                    if let Some((from, at)) = pending {
                        let start = to_screen(Position {
                            x: from.x + NODE_WIDTH / 2.0,
                            y: from.y,
                        });
                        let mut builder = PathBuilder::stroke(px(2.0));
                        builder.move_to(start);
                        builder.cubic_bezier_to(
                            at,
                            point(start.x + (at.x - start.x) / 2.0, start.y),
                            point(start.x + (at.x - start.x) / 2.0, at.y),
                        );
                        if let Ok(path) = builder.build() {
                            window.paint_path(path, pending_color);
                        }
                    }
                });
            },
        )
        .absolute()
        .size_full()
        .into_any()
    }

    fn render_edge_labels(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(graph) = self.graph(cx) else {
            return Vec::new();
        };
        if self.zoom < DETAIL_ZOOM_THRESHOLD {
            return Vec::new();
        }
        let Some(bounds) = self.viewport.get() else {
            return Vec::new();
        };

        graph
            .edges
            .iter()
            .enumerate()
            .filter_map(|(ix, edge)| {
                let label = edge.condition.label()?;
                let from = graph.node(&edge.from).and_then(|node| node.position)?;
                let to = graph.node(&edge.to).and_then(|node| node.position)?;

                let midpoint = EdgeCurve::between(from, to).midpoint();
                let screen = self.to_screen(midpoint);
                // Absolute children are positioned within the container, so the
                // window origin has to come back out.
                let left = screen.x - bounds.origin.x;
                let top = screen.y - bounds.origin.y;

                let judged = matches!(edge.condition, EdgeCondition::LlmEvaluated { .. });
                let (badge, badge_color) = if judged {
                    ("model decides", Color::Warning)
                } else {
                    ("if", Color::Muted)
                };

                Some(
                    h_flex()
                        .absolute()
                        .left(left - px(90.0))
                        .top(top - px(11.0))
                        .w(px(180.0))
                        .justify_center()
                        .child(
                            h_flex()
                                .id(("architect-edge-label", ix))
                                .gap_1()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .bg(cx.theme().colors().elevated_surface_background)
                                .child(
                                    Label::new(badge)
                                        .size(LabelSize::XSmall)
                                        .color(badge_color),
                                )
                                .child(
                                    Label::new(truncate(label, 32))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Default),
                                )
                                .tooltip(Tooltip::text(label.to_string())),
                        )
                        .into_any(),
                )
            })
            .collect()
    }

    fn render_nodes(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(bounds) = self.viewport.get() else {
            return Vec::new();
        };

        // The nodes are copied out before rendering: building elements needs
        // mutable access to the context, which is where the graph is read from.
        let Some((nodes, invalid)) = self.graph(cx).map(|graph| {
            let invalid: Vec<NodeId> = graph
                .problems()
                .iter()
                .filter_map(|problem| match problem {
                    GraphProblem::Unreachable(id) => Some(id.clone()),
                    _ => None,
                })
                .collect();
            (graph.nodes.clone(), invalid)
        }) else {
            return Vec::new();
        };

        nodes
            .into_iter()
            .enumerate()
            .filter_map(|(ix, node)| {
                let position = node.position?;
                let screen = self.to_screen(position);
                let width = px(NODE_WIDTH * self.zoom);
                let height = px(NODE_HEIGHT * self.zoom);
                let left = screen.x - bounds.origin.x - width / 2.0;
                let top = screen.y - bounds.origin.y - height / 2.0;
                let is_invalid = invalid.contains(&node.id);

                Some(
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(width)
                        .h(height)
                        .child(self.render_node(ix, node, is_invalid, cx))
                        .into_any(),
                )
            })
            .collect()
    }

    fn render_node(
        &self,
        ix: usize,
        node: ArchitectNode,
        invalid: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors().clone();
        let error_border = cx.theme().status().error_border;
        let selected = self.selection == Some(Selection::Node(node.id.clone()));
        let hovered = self.hovered_node.as_ref() == Some(&node.id);
        let detailed = self.zoom >= DETAIL_ZOOM_THRESHOLD;

        let border_color = if invalid {
            error_border
        } else if selected {
            theme.border_focused
        } else if node.locked {
            theme.border
        } else {
            theme.border_variant
        };

        let background = if node.locked {
            theme.elevated_surface_background
        } else {
            theme.surface_background
        };

        let id = node.id.clone();
        let lock_id = node.id.clone();
        let connect_id = node.id.clone();

        div()
            .id(("architect-node", ix))
            .size_full()
            .relative()
            .cursor(CursorStyle::OpenHand)
            .rounded_lg()
            .border_2()
            .border_color(border_color)
            .bg(background)
            .when(hovered && !selected, |this| {
                this.border_color(theme.border_selected)
            })
            .when(!node.locked, |this| this.border_dashed())
            .shadow_sm()
            .text_size(px((13.0 * self.zoom).clamp(9.0, 15.0)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    this.set_selection(Some(Selection::Node(id.clone())), window, cx);

                    let canvas = this.to_canvas(event.position);
                    let centre = this
                        .graph(cx)
                        .and_then(|graph| graph.node(&id))
                        .and_then(|node| node.position)
                        .unwrap_or(Position::ZERO);
                    this.interaction = Interaction::DraggingNode {
                        id: id.clone(),
                        grab: point(canvas.x - centre.x, canvas.y - centre.y),
                    };
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .size_full()
                    .p_2()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .justify_between()
                            .child(
                                Label::new(node.title.clone())
                                    .size(if detailed {
                                        LabelSize::Default
                                    } else {
                                        LabelSize::Small
                                    })
                                    .truncate(),
                            )
                            .child(
                                IconButton::new(
                                    ("architect-node-lock", ix),
                                    if node.locked {
                                        IconName::Lock
                                    } else {
                                        IconName::Pencil
                                    },
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(if node.locked {
                                    Color::Accent
                                } else {
                                    Color::Muted
                                })
                                .tooltip(Tooltip::text(if node.locked {
                                    "Locked. Click to reopen for changes"
                                } else {
                                    "Draft. Click to lock"
                                }))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.toggle_lock(lock_id.clone(), window, cx);
                                })),
                            ),
                    )
                    .when(detailed, |this| {
                        this.child(
                            div().flex_1().overflow_hidden().child(
                                Label::new(if node.intent.trim().is_empty() {
                                    "No goal set yet".to_string()
                                } else {
                                    node.intent.clone()
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(match node.rules.len() {
                                        0 => "no rules".to_string(),
                                        1 => "1 rule".to_string(),
                                        count => format!("{count} rules"),
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                )
                                .when(invalid, |this| {
                                    this.child(
                                        Label::new("unreachable")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Error),
                                    )
                                }),
                        )
                    }),
            )
            .child(
                // The connection handle. Dragging from here is how a step is
                // wired to the next one.
                div()
                    .id(("architect-node-handle", ix))
                    .absolute()
                    .right(px(-6.0))
                    .top_1_2()
                    .size(px(12.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(if hovered {
                        theme.text_accent
                    } else {
                        theme.element_background
                    })
                    .cursor(CursorStyle::PointingHand)
                    .tooltip(Tooltip::text("Drag to connect to another step"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.interaction = Interaction::Connecting {
                                from: connect_id.clone(),
                                at: event.position,
                            };
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .into_any()
    }
}

fn snap(value: f32) -> f32 {
    (value / GRID_SNAP).round() * GRID_SNAP
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// A connection between two steps, in canvas space.
///
/// A connection that runs backwards is a loop, and drawing it the same way as a
/// forward one would hide it underneath the steps it passes. Those bow out
/// below the graph instead, where they read as a loop at a glance.
struct EdgeCurve {
    start: Position,
    control_a: Position,
    control_b: Position,
    end: Position,
}

impl EdgeCurve {
    fn between(from: Position, to: Position) -> Self {
        let backwards = to.x <= from.x + NODE_WIDTH / 2.0;

        if backwards {
            let bow = NODE_HEIGHT * 1.15;
            let start = Position {
                x: from.x,
                y: from.y + NODE_HEIGHT / 2.0,
            };
            let end = Position {
                x: to.x,
                y: to.y + NODE_HEIGHT / 2.0,
            };
            Self {
                start,
                control_a: Position {
                    x: start.x,
                    y: start.y + bow,
                },
                control_b: Position {
                    x: end.x,
                    y: end.y + bow,
                },
                end,
            }
        } else {
            let start = Position {
                x: from.x + NODE_WIDTH / 2.0,
                y: from.y,
            };
            let end = Position {
                x: to.x - NODE_WIDTH / 2.0,
                y: to.y,
            };
            let reach = ((end.x - start.x) * 0.5).max(48.0);
            Self {
                start,
                control_a: Position {
                    x: start.x + reach,
                    y: start.y,
                },
                control_b: Position {
                    x: end.x - reach,
                    y: end.y,
                },
                end,
            }
        }
    }

    fn at(&self, t: f32) -> Position {
        let inverse = 1.0 - t;
        let a = inverse * inverse * inverse;
        let b = 3.0 * inverse * inverse * t;
        let c = 3.0 * inverse * t * t;
        let d = t * t * t;
        Position {
            x: a * self.start.x + b * self.control_a.x + c * self.control_b.x + d * self.end.x,
            y: a * self.start.y + b * self.control_a.y + c * self.control_b.y + d * self.end.y,
        }
    }

    fn midpoint(&self) -> Position {
        self.at(0.5)
    }

    /// Distance from a point to the curve, approximated by sampling. Exact
    /// bezier distance is not worth solving for a click test.
    fn distance_to(&self, position: Position) -> f32 {
        const SAMPLES: usize = 32;
        (0..=SAMPLES)
            .map(|step| {
                let sample = self.at(step as f32 / SAMPLES as f32);
                let dx = sample.x - position.x;
                let dy = sample.y - position.y;
                (dx * dx + dy * dy).sqrt()
            })
            .fold(f32::MAX, f32::min)
    }
}

fn paint_curve(
    curve: &EdgeCurve,
    to_screen: &impl Fn(Position) -> Point<Pixels>,
    width: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let start = to_screen(curve.start);
    let end = to_screen(curve.end);

    let mut builder = PathBuilder::stroke(width);
    builder.move_to(start);
    builder.cubic_bezier_to(end, to_screen(curve.control_a), to_screen(curve.control_b));
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }

    // The arrowhead points along the curve as it arrives, which is what tells
    // you which way a loop runs.
    let approach = to_screen(curve.at(0.94));
    let dx = f32::from(end.x - approach.x);
    let dy = f32::from(end.y - approach.y);
    let length = (dx * dx + dy * dy).sqrt();
    if length < 0.001 {
        return;
    }
    let (ux, uy) = (dx / length, dy / length);
    let size = 8.0;

    let base = point(end.x - px(ux * size), end.y - px(uy * size));
    let mut head = PathBuilder::fill();
    head.move_to(end);
    head.line_to(point(base.x - px(uy * size * 0.5), base.y + px(ux * size * 0.5)));
    head.line_to(point(base.x + px(uy * size * 0.5), base.y - px(ux * size * 0.5)));
    head.close();
    if let Ok(path) = head.build() {
        window.paint_path(path, color);
    }
}

impl Render for ArchitectPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_plan = self.graph(cx).is_some_and(|graph| !graph.is_empty());

        let toolbar = self.render_toolbar(cx);
        let edges = self.render_edges(cx);
        let edge_labels = self.render_edge_labels(cx);
        let nodes = self.render_nodes(cx);
        let inspector = self.render_inspector(cx);

        v_flex()
            .key_context("ArchitectPane")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(toolbar)
            .child(
                h_flex()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .child(if has_plan {
                        div()
                            .id("architect-canvas")
                            .relative()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .cursor(if self.interaction.is_idle() {
                                CursorStyle::Arrow
                            } else {
                                CursorStyle::ClosedHand
                            })
                            .on_scroll_wheel(cx.listener(Self::handle_scroll))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
                            .on_mouse_move(cx.listener(Self::handle_mouse_move))
                            .child(edges)
                            .children(edge_labels)
                            .children(nodes)
                            .into_any()
                    } else {
                        div()
                            .flex_1()
                            .h_full()
                            .child(self.render_empty_state(cx))
                            .into_any()
                    })
                    .children(inspector),
            )
    }
}

impl Focusable for ArchitectPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ArchitectPane {}

impl Item for ArchitectPane {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Architect".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Architect Canvas Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
