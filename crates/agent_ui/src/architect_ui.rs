//! The Architect canvas: a flowchart of the steps the agent will take.
//!
//! Edges are painted on a `gpui::canvas` layer underneath the nodes, while the
//! nodes themselves are ordinary absolutely-positioned elements on top. Doing
//! it the other way round would cost either curves (if edges were elements) or
//! text layout, hover and buttons inside nodes (if nodes were painted), so the
//! two halves are drawn the way each is best drawn.

mod geometry;
mod inspector;
mod rendering;
mod run;

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use agent::Thread;
use architect::{
    ArchitectGraph, ArchitectNode, EdgeCondition, EdgeId, GraphMutationError, GraphProblem, NodeId,
    NodePath, Position,
};
use editor::Editor;
use git_ui::git_panel::GitPanel;
use gpui::{
    AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent,
    Subscription, WeakEntity, Window, point, px,
};
use project_panel::ProjectPanel;
use terminal_view::terminal_panel::TerminalPanel;
use workspace::{Workspace, ZoomIn, ZoomOut, dock::DockPosition, item::WeakItemHandle};

use crate::AgentPanel;
use geometry::{
    EDGE_HIT_TOLERANCE, EXPANDED_NODE_HEIGHT, EXPANDED_NODE_WIDTH, EdgeCurve, NODE_HEIGHT,
    NODE_WIDTH, snap,
};
use inspector::{EdgeInspector, InspectorTab, NodeInspector};

/// How many children an expanded step shows before it stops and says how many
/// are left. Past this the cards are too narrow to read.
const EXPANDED_CHILD_LIMIT: usize = 4;

const MIN_ZOOM: f32 = 0.35;
const MAX_ZOOM: f32 = 2.0;
const SCROLL_LINE_HEIGHT: f32 = 20.0;
const WORKSPACE_MODE_KEY_PREFIX: &str = "architect-workspace-mode";

/// Below this, node text would be an unreadable smear, so nodes show their
/// title alone and let the shape of the graph do the talking.
const DETAIL_ZOOM_THRESHOLD: f32 = 0.62;

#[derive(Clone, Debug, PartialEq)]
enum Selection {
    Node(NodeId),
    Edge(EdgeId),
}

enum Interaction {
    None,
    /// Dragging the canvas itself.
    Panning {
        last: Point<Pixels>,
    },
    /// Dragging a node. The grab offset keeps the node from jumping so its
    /// centre snaps to the cursor.
    DraggingNode {
        id: NodeId,
        grab: Point<f32>,
    },
    /// Dragging a new connection out of a node's handle.
    Connecting {
        from: NodeId,
        at: Point<Pixels>,
    },
}

impl Interaction {
    fn is_idle(&self) -> bool {
        matches!(self, Interaction::None)
    }
}

/// Identifies the canvas's notifications so a new one replaces the last rather
/// than stacking up behind it.
struct ArchitectNotice;

#[derive(Clone, Debug)]
pub(super) struct ArchitectActivityEntry {
    pub path: Option<NodePath>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchitectWorkspaceMode {
    Architect,
    Code,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct ArchitectWorkspaceState {
    version: u8,
    mode: ArchitectWorkspaceMode,
    outline_width: f32,
    inspector_width: f32,
}

impl Default for ArchitectWorkspaceState {
    fn default() -> Self {
        Self {
            version: 1,
            mode: ArchitectWorkspaceMode::Architect,
            outline_width: 226.0,
            inspector_width: 348.0,
        }
    }
}

impl ArchitectWorkspaceState {
    fn sanitize(mut self) -> Self {
        self.version = 1;
        if !self.outline_width.is_finite() {
            self.outline_width = Self::default().outline_width;
        }
        if !self.inspector_width.is_finite() {
            self.inspector_width = Self::default().inspector_width;
        }
        self.outline_width = self.outline_width.clamp(184.0, 320.0);
        self.inspector_width = self.inspector_width.clamp(288.0, 480.0);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DockSnapshot {
    position: DockPosition,
    open: bool,
    active_panel_index: Option<usize>,
    active_panel_name: Option<&'static str>,
}

pub struct ArchitectPane {
    thread: Entity<Thread>,
    workspace: WeakEntity<Workspace>,
    mode: ArchitectWorkspaceMode,
    previous_code_item: Option<Box<dyn WeakItemHandle>>,
    code_docks: Vec<DockSnapshot>,
    focus_handle: FocusHandle,
    last_code_focus: Option<FocusHandle>,
    last_architect_focus: Option<FocusHandle>,
    transient_return_focus: Option<FocusHandle>,
    /// The canvas layer reports its bounds during paint; everything that
    /// converts between screen and canvas space needs them.
    viewport: Rc<Cell<Option<Bounds<Pixels>>>>,
    pan: Point<Pixels>,
    zoom: f32,
    selection: Option<Selection>,
    inspector: Option<NodeInspector>,
    edge_inspector: Option<EdgeInspector>,
    interaction: Interaction,
    hovered_node: Option<NodeId>,
    /// Which plan the canvas is showing. Empty is the top-level plan; each id
    /// appended is a step whose own plan has been opened.
    focus: NodePath,
    /// Steps showing their sub-plan inside themselves, rather than only as a
    /// count. One level: a child of an expanded step is drawn as a plain card
    /// even if it has a plan of its own, and drilling in is how you go deeper.
    expanded: HashSet<NodeId>,
    /// Which inspector concern is showing.
    inspector_tab: InspectorTab,
    outline_drawer_open: bool,
    inspector_drawer_open: bool,
    plan_conversation_open: bool,
    outline_width: Pixels,
    inspector_width: Pixels,
    search_editor: Entity<Editor>,
    activity: Vec<ArchitectActivityEntry>,
    /// Set while a run is being started, so the toolbar can show it before the
    /// thread has been told. The run itself belongs to the thread.
    run_starting: Cell<bool>,

    _thread_subscription: Subscription,
    _search_subscription: Subscription,
}

impl ArchitectPane {
    fn new(
        thread: Entity<Thread>,
        workspace: WeakEntity<Workspace>,
        previous_code_item: Option<Box<dyn WeakItemHandle>>,
        code_docks: Vec<DockSnapshot>,
        last_code_focus: Option<FocusHandle>,
        outline_width: Pixels,
        inspector_width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&thread, |_, _, cx| {
            cx.emit(workspace::item::ItemEvent::UpdateTab);
            cx.notify();
        });
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search steps", window, cx);
            editor
        });
        let search_subscription = cx.observe(&search_editor, |_, _, cx| cx.notify());
        Self {
            thread,
            workspace,
            mode: ArchitectWorkspaceMode::Architect,
            previous_code_item,
            code_docks,
            focus_handle: cx.focus_handle(),
            last_code_focus,
            last_architect_focus: None,
            transient_return_focus: None,
            viewport: Rc::new(Cell::new(None)),
            pan: point(px(0.0), px(0.0)),
            zoom: 1.0,
            selection: None,
            inspector: None,
            edge_inspector: None,
            interaction: Interaction::None,
            hovered_node: None,
            focus: NodePath::default(),
            expanded: HashSet::default(),
            inspector_tab: InspectorTab::Details,
            outline_drawer_open: false,
            inspector_drawer_open: false,
            plan_conversation_open: false,
            outline_width,
            inspector_width,
            search_editor,
            activity: Vec::new(),
            run_starting: Cell::new(false),
            _thread_subscription: subscription,
            _search_subscription: search_subscription,
        }
    }

    pub fn mode(&self) -> ArchitectWorkspaceMode {
        self.mode
    }

    pub fn owns_thread(&self, thread: &Entity<Thread>) -> bool {
        &self.thread == thread
    }

    fn workspace_mode_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or_else(|| workspace.session_id())
            .map(|id| format!("{WORKSPACE_MODE_KEY_PREFIX}:{id}"))
    }

    fn state_from_persisted_value(key: &str, value: Option<&str>) -> ArchitectWorkspaceState {
        match value {
            Some("code") => ArchitectWorkspaceState {
                mode: ArchitectWorkspaceMode::Code,
                ..ArchitectWorkspaceState::default()
            },
            Some("architect") | None => ArchitectWorkspaceState::default(),
            Some(value) => match serde_json::from_str::<ArchitectWorkspaceState>(value) {
                Ok(state) => state.sanitize(),
                Err(error) => {
                    log::warn!("Ignoring invalid Architect workspace state for {key}: {error:#}");
                    ArchitectWorkspaceState::default()
                }
            },
        }
    }

    fn persisted_state(workspace: &Workspace, cx: &gpui::App) -> ArchitectWorkspaceState {
        let Some(key) = Self::workspace_mode_key(workspace) else {
            return ArchitectWorkspaceState::default();
        };
        match db::kvp::KeyValueStore::global(cx).read_kvp(&key) {
            Ok(value) => Self::state_from_persisted_value(&key, value.as_deref()),
            Err(error) => {
                log::error!("Could not read Architect workspace state for {key}: {error:#}");
                ArchitectWorkspaceState::default()
            }
        }
    }

    pub fn persisted_mode(workspace: &Workspace, cx: &gpui::App) -> ArchitectWorkspaceMode {
        Self::persisted_state(workspace, cx).mode
    }

    fn persist_state(workspace: &Workspace, state: ArchitectWorkspaceState, cx: &mut gpui::App) {
        Self::persist_state_for_key(Self::workspace_mode_key(workspace), state, cx);
    }

    fn persist_state_for_key(
        key: Option<String>,
        state: ArchitectWorkspaceState,
        cx: &mut gpui::App,
    ) {
        let Some(key) = key else {
            return;
        };
        let value = match serde_json::to_string(&state.sanitize()) {
            Ok(value) => value,
            Err(error) => {
                log::error!("Could not serialize Architect workspace state: {error:#}");
                return;
            }
        };
        let store = db::kvp::KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move { store.write_kvp(key, value).await });
    }

    fn persist_mode(workspace: &Workspace, mode: ArchitectWorkspaceMode, cx: &mut gpui::App) {
        let mut state = Self::persisted_state(workspace, cx);
        state.mode = mode;
        Self::persist_state(workspace, state, cx);
    }

    fn persist_layout(&self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let state = ArchitectWorkspaceState {
            version: 1,
            mode: self.mode,
            outline_width: f32::from(self.outline_width),
            inspector_width: f32::from(self.inspector_width),
        };
        let key = Self::workspace_mode_key(workspace.read(cx));
        Self::persist_state_for_key(key, state, cx);
    }

    fn resize_outline(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.outline_width = px((f32::from(self.outline_width) + delta).clamp(184.0, 320.0));
        self.persist_layout(cx);
        cx.notify();
    }

    fn resize_inspector(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.inspector_width = px((f32::from(self.inspector_width) + delta).clamp(288.0, 480.0));
        self.persist_layout(cx);
        cx.notify();
    }

    fn capture_docks(workspace: &Workspace, cx: &Context<Workspace>) -> Vec<DockSnapshot> {
        workspace
            .all_docks()
            .into_iter()
            .map(|dock| {
                let dock = dock.read(cx);
                DockSnapshot {
                    position: dock.position(),
                    open: dock.is_open(),
                    active_panel_index: dock.active_panel_index(),
                    active_panel_name: dock.active_panel().map(|panel| panel.persistent_name()),
                }
            })
            .collect()
    }

    fn hide_code_docks(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        for dock in workspace.all_docks() {
            dock.update(cx, |dock, cx| dock.set_open(false, window, cx));
        }
    }

    fn restore_code_docks(
        workspace: &mut Workspace,
        snapshots: &[DockSnapshot],
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        for snapshot in snapshots {
            let dock = workspace.dock_at_position(snapshot.position);
            dock.update(cx, |dock, cx| {
                let active_index = snapshot
                    .active_panel_name
                    .and_then(|name| dock.panel_index_for_persistent_name(name, cx))
                    .or(snapshot.active_panel_index)
                    .filter(|index| *index < dock.panels_len());
                if let Some(index) = active_index {
                    dock.activate_panel(index, window, cx);
                }
                dock.set_open(snapshot.open, window, cx);
            });
        }
    }

    fn replace_thread(&mut self, thread: Entity<Thread>, cx: &mut Context<Self>) {
        if self.thread == thread {
            return;
        }

        self.thread = thread.clone();
        self._thread_subscription = cx.observe(&thread, |_, _, cx| {
            cx.emit(workspace::item::ItemEvent::UpdateTab);
            cx.notify();
        });
        self.focus = NodePath::default();
        self.selection = None;
        self.inspector = None;
        self.edge_inspector = None;
        self.interaction = Interaction::None;
        self.hovered_node = None;
        self.expanded.clear();
        self.activity.clear();
        self.inspector_tab = InspectorTab::Details;
        self.outline_drawer_open = false;
        self.inspector_drawer_open = false;
        self.plan_conversation_open = false;
        self.last_architect_focus = None;
        self.transient_return_focus = None;
        self.pan = point(px(0.0), px(0.0));
        self.zoom = 1.0;
        cx.notify();
    }

    /// Activates the workspace-owned Architect surface for this plan thread.
    /// Existing Code items and panels remain alive and are restored on return.
    pub fn open(
        thread: Entity<Thread>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let persisted_state = Self::persisted_state(workspace, cx);
        let existing = workspace.item_of_type::<ArchitectPane>(cx);
        let already_active = workspace
            .active_item_as::<ArchitectPane>(cx)
            .is_some_and(|active| existing.as_ref() == Some(&active));

        let previous_code_item = (!already_active)
            .then(|| workspace.active_item(cx))
            .flatten()
            .filter(|item| item.downcast::<ArchitectPane>().is_none())
            .map(|item| item.downgrade_item());
        let last_code_focus = (!already_active).then(|| window.focused(cx)).flatten();
        let code_docks = (!already_active).then(|| Self::capture_docks(workspace, cx));

        Self::hide_code_docks(workspace, window, cx);

        let status_bar = workspace.status_bar().clone();
        if status_bar
            .read(cx)
            .item_of_type::<rendering::ArchitectStatusItem>()
            .is_none()
        {
            let status_item = cx.new(|_| rendering::ArchitectStatusItem::default());
            status_bar.update(cx, |status_bar, cx| {
                status_bar.add_left_item(status_item, window, cx);
            });
        }

        if let Some(existing) = existing {
            existing.update(cx, |architect, cx| {
                architect.replace_thread(thread, cx);
                architect.mode = ArchitectWorkspaceMode::Architect;
                if let Some(previous_code_item) = previous_code_item {
                    architect.previous_code_item = Some(previous_code_item);
                }
                if let Some(code_docks) = code_docks {
                    architect.code_docks = code_docks;
                }
                if let Some(last_code_focus) = last_code_focus {
                    architect.last_code_focus = Some(last_code_focus);
                }
            });
            workspace.activate_item(&existing, true, true, window, cx);
            let focus = existing
                .read(cx)
                .last_architect_focus
                .clone()
                .unwrap_or_else(|| existing.read(cx).focus_handle.clone());
            focus.focus(window, cx);
        } else {
            let workspace_handle = cx.weak_entity();
            let architect = cx.new(|cx| {
                ArchitectPane::new(
                    thread,
                    workspace_handle,
                    previous_code_item,
                    code_docks.unwrap_or_default(),
                    last_code_focus,
                    px(persisted_state.outline_width),
                    px(persisted_state.inspector_width),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(architect), None, true, window, cx);
        }

        workspace
            .active_pane()
            .update(cx, |pane, cx| pane.zoom_in(&ZoomIn, window, cx));
        Self::persist_mode(workspace, ArchitectWorkspaceMode::Architect, cx);
    }

    pub(super) fn arrange_code_panels(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(project_panel) = workspace.panel::<ProjectPanel>(cx) {
            workspace.relocate_panel(&project_panel, DockPosition::Left, window, cx);
        }
        if let Some(git_panel) = workspace.panel::<GitPanel>(cx) {
            workspace.relocate_panel(&git_panel, DockPosition::Left, window, cx);
        }
        if let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) {
            workspace.relocate_panel(&terminal_panel, DockPosition::Bottom, window, cx);
        }
        if let Some(agent_panel) = workspace.panel::<AgentPanel>(cx) {
            workspace.relocate_panel(&agent_panel, DockPosition::Right, window, cx);
            if let Some(conversation_view) =
                agent_panel.read(cx).active_conversation_view().cloned()
            {
                conversation_view.update(cx, |conversation_view, cx| {
                    conversation_view.return_to_root_thread(cx);
                });
            }
        }
    }

    fn restore_code_surface(
        workspace: &mut Workspace,
        previous_code_item: Option<Box<dyn workspace::item::ItemHandle>>,
        code_docks: Vec<DockSnapshot>,
        last_code_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace
            .active_pane()
            .update(cx, |pane, cx| pane.zoom_out(&ZoomOut, window, cx));
        Self::arrange_code_panels(workspace, window, cx);

        let code_item = previous_code_item.or_else(|| {
            workspace
                .items(cx)
                .find(|item| item.downcast::<ArchitectPane>().is_none())
                .map(|item| item.boxed_clone())
        });
        if let Some(code_item) = code_item {
            workspace.activate_item(code_item.as_ref(), true, true, window, cx);
        }

        Self::restore_code_docks(workspace, &code_docks, window, cx);
        if let Some(last_code_focus) = last_code_focus {
            last_code_focus.focus(window, cx);
        }
        Self::persist_mode(workspace, ArchitectWorkspaceMode::Code, cx);
    }

    pub fn activate_code(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(architect) = workspace.item_of_type::<ArchitectPane>(cx) else {
            return;
        };
        let (previous_code_item, code_docks, last_code_focus) =
            architect.update(cx, |architect, cx| {
                architect.mode = ArchitectWorkspaceMode::Code;
                cx.notify();
                (
                    architect
                        .previous_code_item
                        .as_ref()
                        .and_then(|item| item.upgrade()),
                    architect.code_docks.clone(),
                    architect.last_code_focus.clone(),
                )
            });
        Self::restore_code_surface(
            workspace,
            previous_code_item,
            code_docks,
            last_code_focus,
            window,
            cx,
        );
    }

    fn remember_transient_focus(&mut self, window: &Window, cx: &Context<Self>) {
        if self.transient_return_focus.is_none() && self.focus_handle.contains_focused(window, cx) {
            self.transient_return_focus = window.focused(cx);
        }
    }

    fn restore_transient_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.transient_return_focus
            .take()
            .unwrap_or_else(|| self.focus_handle.clone())
            .focus(window, cx);
    }

    fn open_outline_drawer(
        &mut self,
        focus_search: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_transient_focus(window, cx);
        self.outline_drawer_open = true;
        if focus_search {
            self.search_editor.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn open_inspector_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_transient_focus(window, cx);
        self.inspector_drawer_open = true;
        cx.notify();
    }

    fn close_outline_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.outline_drawer_open = false;
        self.restore_transient_focus(window, cx);
        cx.notify();
    }

    fn close_inspector_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.inspector_drawer_open = false;
        self.restore_transient_focus(window, cx);
        cx.notify();
    }

    fn close_plan_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.plan_conversation_open = false;
        self.restore_transient_focus(window, cx);
        cx.notify();
    }

    fn request_code_mode(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                Self::activate_code(workspace, window, cx);
            });
        });
    }

    fn restore_after_discard(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != ArchitectWorkspaceMode::Architect {
            return;
        }
        let workspace = self.workspace.clone();
        let previous_code_item = self
            .previous_code_item
            .as_ref()
            .and_then(|item| item.upgrade());
        let code_docks = self.code_docks.clone();
        let last_code_focus = self.last_code_focus.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                Self::restore_code_surface(
                    workspace,
                    previous_code_item,
                    code_docks,
                    last_code_focus,
                    window,
                    cx,
                );
            });
        });
    }

    /// The whole plan, regardless of which level is being looked at. Validating
    /// and running are properties of the plan, not of the current view.
    fn root_graph<'a>(&self, cx: &'a Context<Self>) -> Option<&'a ArchitectGraph> {
        self.thread.read(cx).architect_graph()
    }

    /// The plan the canvas is showing, which is the top-level one until a step
    /// has been opened.
    fn graph<'a>(&self, cx: &'a Context<Self>) -> Option<&'a ArchitectGraph> {
        self.root_graph(cx)?.graph_at(&self.focus)
    }

    /// Edits the plan being shown. An edit made while inside a step's plan
    /// belongs to that plan, not to the one containing it.
    fn edit_graph(&mut self, edit: impl FnOnce(&mut ArchitectGraph), cx: &mut Context<Self>) {
        let focus = self.focus.clone();
        self.edit_checked(
            move |root| root.mutate_graph_at(&focus, edit).map(|_| ()),
            cx,
        );
    }

    fn edit_checked(
        &mut self,
        edit: impl FnOnce(&mut ArchitectGraph) -> Result<(), GraphMutationError>,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self
            .thread
            .update(cx, |thread, cx| thread.update_architect_graph(edit, cx));
        match result {
            Some(Ok(())) => {
                cx.notify();
                true
            }
            Some(Err(error)) => {
                self.report(format!("That change was refused: {error}"), cx);
                false
            }
            None => {
                self.report(
                    "The plan is no longer available. Return to its root conversation or open another plan."
                        .to_string(),
                    cx,
                );
                false
            }
        }
    }

    fn edit_node(
        &mut self,
        id: NodeId,
        edit: impl FnOnce(&mut ArchitectNode),
        cx: &mut Context<Self>,
    ) -> bool {
        let path = self.focus.child(id);
        self.edit_checked(
            move |graph| graph.mutate_node_at(&path, edit).map(|_| ()),
            cx,
        )
    }

    /// Opens a step's own plan, giving it an empty one if it has none yet.
    fn drill_into(&mut self, id: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus.depth() >= architect::MAX_PLAN_DEPTH {
            self.report(
                format!(
                    "Plans cannot nest more than {} deep. Flatten this part of the plan instead.",
                    architect::MAX_PLAN_DEPTH
                ),
                cx,
            );
            return;
        }

        let locked = self
            .graph(cx)
            .and_then(|graph| graph.node(&id))
            .is_some_and(|node| node.locked);
        let existing = self
            .graph(cx)
            .and_then(|graph| graph.node(&id))
            .is_some_and(ArchitectNode::has_subplan);

        // A locked step is settled, and giving it a plan would reopen it by the
        // back door. Looking inside one it already has is fine.
        if !existing && locked {
            self.report(
                "This step is locked. Unlock it before breaking it into steps.".to_string(),
                cx,
            );
            return;
        }

        if !existing {
            let nested_path = self.focus.child(id.clone());
            let id = id.clone();
            self.edit_graph(
                move |graph| {
                    graph.subplan_mut(&id);
                },
                cx,
            );
            self.record_activity(Some(nested_path), "Created a nested plan", cx);
        }

        self.focus = self.focus.child(id);
        self.selection = None;
        self.inspector = None;
        self.edge_inspector = None;
        self.interaction = Interaction::None;
        self.hovered_node = None;
        self.zoom_to_fit(cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Goes back out to the plan containing the one being shown. The step just
    /// left is selected, so leaving does not lose your place.
    fn drill_out(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(left) = self.focus.0.pop() else {
            return false;
        };
        self.interaction = Interaction::None;
        self.hovered_node = None;
        self.zoom_to_fit(cx);
        self.set_selection(Some(Selection::Node(left)), window, cx);
        cx.notify();
        true
    }

    /// Jumps straight to a level from the breadcrumb.
    fn focus_depth(&mut self, depth: usize, window: &mut Window, cx: &mut Context<Self>) {
        if depth >= self.focus.depth() {
            return;
        }
        self.focus = NodePath(self.focus.0[..depth].to_vec());
        self.selection = None;
        self.inspector = None;
        self.edge_inspector = None;
        self.interaction = Interaction::None;
        self.hovered_node = None;
        self.zoom_to_fit(cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
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

    /// How big a step is drawn, in canvas units. A step showing its sub-plan
    /// inside itself needs room for it, and everything that hit-tests or lays
    /// out a step has to agree about that.
    fn node_size(&self, node: &ArchitectNode) -> (f32, f32) {
        if self.expanded.contains(&node.id) && node.has_subplan() {
            (EXPANDED_NODE_WIDTH, EXPANDED_NODE_HEIGHT)
        } else {
            (NODE_WIDTH, NODE_HEIGHT)
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
                let (width, height) = self.node_size(node);
                (canvas.x - position.x).abs() <= width / 2.0
                    && (canvas.y - position.y).abs() <= height / 2.0
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
            if distance <= tolerance && closest.as_ref().is_none_or(|(best, _)| distance < *best) {
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

        let node_bounds: Vec<(Position, f32, f32)> = graph
            .nodes
            .iter()
            .filter_map(|node| {
                node.position.map(|position| {
                    let (width, height) = self.node_size(node);
                    (position, width, height)
                })
            })
            .collect();
        if node_bounds.is_empty() {
            return;
        }

        let min_x = node_bounds
            .iter()
            .map(|(position, width, _)| position.x - width / 2.0)
            .fold(f32::MAX, f32::min);
        let max_x = node_bounds
            .iter()
            .map(|(position, width, _)| position.x + width / 2.0)
            .fold(f32::MIN, f32::max);
        let min_y = node_bounds
            .iter()
            .map(|(position, _, height)| position.y - height / 2.0)
            .fold(f32::MAX, f32::min);
        let max_y = node_bounds
            .iter()
            .map(|(position, _, height)| position.y + height / 2.0)
            .fold(f32::MIN, f32::max);

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
                let path = self.focus.child(id);
                let activity_path = path.clone();
                if self.edit_checked(move |graph| graph.remove_node_at(&path), cx) {
                    self.record_activity(
                        Some(activity_path),
                        "Deleted this step from the plan",
                        cx,
                    );
                    self.set_selection(None, window, cx);
                }
            }
            Selection::Edge(id) => {
                let graph_path = self.focus.clone();
                let activity_path = graph_path.clone();
                if self.edit_checked(move |graph| graph.disconnect_at(&graph_path, &id), cx) {
                    self.record_activity(
                        Some(activity_path),
                        "Deleted a connection from this plan",
                        cx,
                    );
                    self.set_selection(None, window, cx);
                }
            }
        }
    }

    fn record_activity(
        &mut self,
        path: Option<NodePath>,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        const MAX_ACTIVITY_ENTRIES: usize = 100;
        if self.activity.len() >= MAX_ACTIVITY_ENTRIES {
            let remove_count = self.activity.len() + 1 - MAX_ACTIVITY_ENTRIES;
            self.activity.drain(..remove_count);
        }
        self.activity.push(ArchitectActivityEntry {
            path,
            message: message.into(),
        });
        cx.notify();
    }

    /// Tells the user something the canvas cannot show in place.
    fn report(&self, message: String, cx: &mut Context<Self>) {
        log::warn!("Architect: {message}");
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                workspace::Toast::new(
                    workspace::notifications::NotificationId::unique::<ArchitectNotice>(),
                    message,
                ),
                cx,
            );
        });
    }

    fn open_plan_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_transient_focus(window, cx);
        self.plan_conversation_open = true;
        self.inspector_drawer_open = true;
        self.record_activity(None, "Opened the overall plan conversation", cx);
        self.focus_handle.focus(window, cx);
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
        let path = self.focus.child(id.clone());
        self.record_activity(Some(path), "Added this step to the plan", cx);
        self.set_selection(Some(Selection::Node(id)), window, cx);
    }

    fn tidy_up(&mut self, cx: &mut Context<Self>) {
        self.edit_graph(|graph| graph.relayout(), cx);
        self.record_activity(Some(self.focus.clone()), "Tidied the graph layout", cx);
        self.zoom_to_fit(cx);
    }

    fn problem_selection(problem: &GraphProblem) -> Selection {
        match problem {
            GraphProblem::DuplicateNode(id)
            | GraphProblem::Unreachable(id)
            | GraphProblem::Unlocked(id)
            | GraphProblem::InSubplan { node: id, .. } => Selection::Node(id.clone()),
            GraphProblem::DanglingEdge { edge, .. } | GraphProblem::EmptyCondition(edge) => {
                Selection::Edge(edge.clone())
            }
        }
    }

    fn review_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.graph(cx).map(|graph| {
            let problems = graph.blocking_problems();
            let selection = problems.first().map(Self::problem_selection).or_else(|| {
                graph
                    .execution_order()
                    .first()
                    .cloned()
                    .map(Selection::Node)
            });
            (selection, problems.len())
        });
        let (selection, problem_count) = review.unwrap_or_default();
        self.record_activity(
            None,
            if problem_count == 0 {
                "Reviewed the plan: ready"
            } else {
                "Reviewed the plan: issues need attention"
            },
            cx,
        );
        self.set_selection(selection, window, cx);
    }

    fn select_adjacent_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(order) = self.graph(cx).map(ArchitectGraph::execution_order) else {
            return;
        };
        if order.is_empty() {
            return;
        }
        let selected = match &self.selection {
            Some(Selection::Node(id)) => order.iter().position(|candidate| candidate == id),
            _ => None,
        };
        let index = match (selected, forward) {
            (Some(index), true) => (index + 1).min(order.len().saturating_sub(1)),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => order.len().saturating_sub(1),
        };
        if let Some(id) = order.get(index).cloned() {
            self.set_selection(Some(Selection::Node(id)), window, cx);
        }
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

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                self.edit_node(id, move |node| node.position = Some(position), cx);
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
                let from_path = self.focus.child(from);
                let activity_path = self.focus.child(to.clone());
                if self.edit_checked(
                    move |graph| {
                        graph
                            .connect_from_at(&from_path, to, EdgeCondition::Always)
                            .map(|_| ())
                    },
                    cx,
                ) {
                    self.record_activity(
                        Some(activity_path),
                        "Connected this step to a predecessor",
                        cx,
                    );
                }
            }
        }
        self.interaction = Interaction::None;
        cx.notify();
    }

    /// Selecting a step, then selecting it again, opens its plan. Double-click
    /// is the gesture people already try on a box that looks like it contains
    /// something.
    fn handle_node_click(
        &mut self,
        id: NodeId,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            self.drill_into(id, window, cx);
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let canvas_focused = self.focus_handle.is_focused(window);
        match event.keystroke.key.as_str() {
            "delete" | "backspace" if canvas_focused => self.delete_selection(window, cx),
            "down" | "right" if canvas_focused => self.select_adjacent_step(true, window, cx),
            "up" | "left" if canvas_focused => self.select_adjacent_step(false, window, cx),
            "enter" if canvas_focused => {
                if self.selection.is_some() {
                    self.open_inspector_drawer(window, cx);
                }
            }
            // Escape closes the innermost transient surface before changing
            // navigation state, without ever leaving Architect mode.
            "escape" => {
                self.interaction = Interaction::None;
                if self.plan_conversation_open {
                    self.close_plan_conversation(window, cx);
                } else if self.inspector_drawer_open {
                    self.close_inspector_drawer(window, cx);
                } else if self.outline_drawer_open {
                    self.close_outline_drawer(window, cx);
                } else if self.selection.is_some() {
                    self.set_selection(None, window, cx);
                } else {
                    self.drill_out(window, cx);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, rc::Rc};

    use acp_thread::AgentConnection as _;
    use fs::Fs as _;
    use gpui::{Modifiers, MouseMoveEvent, Task, TestAppContext, VisualTestContext, size};
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path_list::PathList;
    use workspace::{MultiWorkspace, Panel as _, item::test::TestItem};

    use super::*;
    use crate::conversation_view::tests::init_test;

    #[test]
    fn architect_workspace_state_is_backward_compatible_and_bounded() {
        let state = ArchitectWorkspaceState {
            version: 1,
            mode: ArchitectWorkspaceMode::Code,
            outline_width: 20.0,
            inspector_width: f32::INFINITY,
        }
        .sanitize();
        assert_eq!(state.mode, ArchitectWorkspaceMode::Code);
        assert_eq!(state.outline_width, 184.0);
        assert_eq!(state.inspector_width, 348.0);

        let serialized = serde_json::to_string(&state).expect("state should serialize");
        let restored =
            ArchitectPane::state_from_persisted_value("test-workspace", Some(&serialized));
        assert_eq!(restored.mode, ArchitectWorkspaceMode::Code);

        let partial =
            ArchitectPane::state_from_persisted_value("test-workspace", Some(r#"{"mode":"code"}"#));
        assert_eq!(partial.outline_width, 226.0);
        assert_eq!(partial.inspector_width, 348.0);
        assert_eq!(
            ArchitectPane::state_from_persisted_value("test-workspace", Some("code")).mode,
            ArchitectWorkspaceMode::Code
        );
        assert_eq!(
            ArchitectPane::state_from_persisted_value("test-workspace", Some("architect")).mode,
            ArchitectWorkspaceMode::Architect
        );
        assert_eq!(
            ArchitectPane::state_from_persisted_value("test-workspace", Some("obsolete")).mode,
            ArchitectWorkspaceMode::Architect
        );
    }

    #[gpui::test]
    async fn architect_workspace_state_survives_the_persistence_boundary(cx: &mut TestAppContext) {
        init_test(cx);
        let key = "architect-workspace-mode:restart-test".to_string();
        let expected = ArchitectWorkspaceState {
            version: 1,
            mode: ArchitectWorkspaceMode::Code,
            outline_width: 272.0,
            inspector_width: 416.0,
        };

        cx.update(|cx| {
            ArchitectPane::persist_state_for_key(Some(key.clone()), expected, cx);
        });
        cx.run_until_parked();

        let value = cx
            .update(|cx| db::kvp::KeyValueStore::global(cx).read_kvp(&key))
            .expect("persisted workspace state should be readable")
            .expect("persisted workspace state should exist");
        let restored = ArchitectPane::state_from_persisted_value(&key, Some(&value));
        assert_eq!(restored.mode, ArchitectWorkspaceMode::Code);
        assert_eq!(restored.outline_width, 272.0);
        assert_eq!(restored.inspector_width, 416.0);
    }

    #[gpui::test]
    async fn canvas_edits_nested_navigation_and_locking_follow_graph_rules(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/", json!({ "a": {} })).await;
        let project = Project::test(fs.clone(), [Path::new("/a")], cx).await;
        let thread_store = cx.update(|cx| agent::ThreadStore::global(cx));
        let native_agent = cx.update(|cx| {
            agent::NativeAgent::new(thread_store, agent::Templates::new(), fs.clone(), cx)
        });
        let connection = Rc::new(agent::NativeAgentConnection(native_agent));
        let acp_thread = cx
            .update(|cx| {
                connection.clone().new_session(
                    project.clone(),
                    PathList::new(&[Path::new("/a")]),
                    cx,
                )
            })
            .await
            .expect("the Architect test session should open");
        let session_id = acp_thread.read_with(cx, |thread, _| thread.session_id().clone());
        let thread = cx
            .update(|cx| connection.thread(&session_id, cx))
            .expect("the native thread should exist");

        let parent = NodeId::from("parent");
        let child = NodeId::from("child");
        let target = NodeId::from("target");
        let mut nested = ArchitectGraph::default();
        let mut child_node = ArchitectNode::new(child.clone(), "Child");
        child_node.position = Some(Position { x: 0.0, y: 0.0 });
        nested.add_node(child_node);
        let mut graph = ArchitectGraph::default();
        let mut parent_node = ArchitectNode::new(parent.clone(), "Parent");
        parent_node.position = Some(Position { x: 0.0, y: 0.0 });
        parent_node.subplan = Some(Box::new(nested));
        graph.add_node(parent_node);
        let mut target_node = ArchitectNode::new(target.clone(), "Target");
        target_node.position = Some(Position { x: 400.0, y: 0.0 });
        graph.add_node(target_node);
        thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        workspace.update_in(cx, |workspace, window, cx| {
            let agent_panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(agent_panel, window, cx);
        });
        let pane = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| {
                ArchitectPane::new(
                    thread.clone(),
                    workspace,
                    None,
                    Vec::new(),
                    None,
                    px(226.0),
                    px(348.0),
                    window,
                    cx,
                )
            })
        });

        pane.update_in(cx, |pane, window, cx| {
            pane.interaction = Interaction::DraggingNode {
                id: parent.clone(),
                grab: point(0.0, 0.0),
            };
            pane.handle_mouse_move(
                &MouseMoveEvent {
                    position: point(px(83.0), px(39.0)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: Modifiers::default(),
                },
                window,
                cx,
            );
            assert_eq!(
                pane.root_graph(cx)
                    .and_then(|graph| graph.node(&parent))
                    .and_then(|node| node.position),
                Some(Position { x: 80.0, y: 40.0 })
            );

            pane.interaction = Interaction::Connecting {
                from: parent.clone(),
                at: point(px(80.0), px(40.0)),
            };
            pane.handle_mouse_up(
                &MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(400.0), px(0.0)),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                },
                window,
                cx,
            );
            let edge = pane
                .root_graph(cx)
                .and_then(|graph| graph.edges.first())
                .expect("dragging a connector onto a step should create an edge")
                .id
                .clone();
            pane.selection = Some(Selection::Edge(edge));
            pane.delete_selection(window, cx);
            assert!(pane.root_graph(cx).unwrap().edges.is_empty());

            pane.interaction = Interaction::Connecting {
                from: parent.clone(),
                at: point(px(80.0), px(40.0)),
            };
            pane.handle_mouse_up(
                &MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(400.0), px(0.0)),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                },
                window,
                cx,
            );
            pane.selection = Some(Selection::Node(target.clone()));
            pane.delete_selection(window, cx);
            let graph = pane.root_graph(cx).unwrap();
            assert!(graph.node(&target).is_none());
            assert!(graph.edges.is_empty(), "deleting a step removes its edges");

            pane.toggle_lock(parent.clone(), window, cx);
            assert!(!pane.root_graph(cx).unwrap().node(&parent).unwrap().locked);
            pane.drill_into(parent.clone(), window, cx);
            assert_eq!(pane.focus, NodePath::root(parent.clone()));
            pane.toggle_lock(child.clone(), window, cx);
            assert!(pane.graph(cx).unwrap().node(&child).unwrap().locked);
            assert!(pane.drill_out(window, cx));
            assert_eq!(pane.selection, Some(Selection::Node(parent.clone())));
            pane.toggle_lock(parent.clone(), window, cx);
            assert!(pane.root_graph(cx).unwrap().node(&parent).unwrap().locked);

            pane.drill_into(parent.clone(), window, cx);
            pane.focus_depth(0, window, cx);
            assert_eq!(pane.focus, NodePath::default());
        });

        thread.update(cx, |thread, cx| {
            thread.start_architect_run(
                NodePath::root(parent.clone()),
                "Parent".into(),
                Task::ready(()),
                cx,
            );
        });
        drop(pane);

        let reopened = workspace.update_in(cx, |_workspace, window, cx| {
            let workspace = cx.weak_entity();
            cx.new(|cx| {
                ArchitectPane::new(
                    thread.clone(),
                    workspace,
                    None,
                    Vec::new(),
                    None,
                    px(226.0),
                    px(348.0),
                    window,
                    cx,
                )
            })
        });
        reopened.read_with(cx, |pane, cx| {
            assert!(
                pane.is_running(cx),
                "closing the canvas must not stop its run"
            );
            assert_eq!(pane.running_node(cx), Some(&parent));
        });
        reopened.update(cx, |pane, cx| pane.stop_run(cx));
        reopened.read_with(cx, |pane, cx| {
            assert!(
                !pane.is_running(cx),
                "the reopened canvas must be able to cancel"
            );
        });

        let code_item = workspace.update_in(cx, |workspace, window, cx| {
            let code_item = cx.new(TestItem::new);
            workspace.add_item_to_active_pane(Box::new(code_item.clone()), None, true, window, cx);
            code_item
        });
        let architect = workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::open(thread.clone(), workspace, window, cx);
            workspace
                .item_of_type::<ArchitectPane>(cx)
                .expect("Architect should be retained by the workspace")
        });
        let active_pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        let project_panel =
            workspace.read_with(cx, |workspace, cx| workspace.panel::<ProjectPanel>(cx));
        let git_panel = workspace.read_with(cx, |workspace, cx| workspace.panel::<GitPanel>(cx));
        let terminal_panel =
            workspace.read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx));
        let agent_panel =
            workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
        let status_item = workspace.read_with(cx, |workspace, cx| {
            workspace
                .status_bar()
                .read(cx)
                .item_of_type::<rendering::ArchitectStatusItem>()
                .expect("Architect should register one native status item")
        });
        let agent_panel_entity = agent_panel
            .as_ref()
            .expect("the native Agent panel should exist")
            .clone();
        agent_panel_entity.update_in(cx, |agent_panel, window, cx| {
            agent_panel.activate_draft(false, crate::AgentThreadSource::AgentPanel, window, cx);
        });
        cx.run_until_parked();
        let agent_conversation = agent_panel_entity.read_with(cx, |agent_panel, _| {
            agent_panel
                .active_conversation_view()
                .expect("the Agent panel should retain an active conversation")
                .clone()
        });
        let root_conversation = agent_conversation.read_with(cx, |conversation, _| {
            conversation
                .root_thread_view()
                .expect("the active Agent conversation should have a root thread")
        });

        cx.simulate_resize(size(px(1500.0), px(900.0)));
        architect.update_in(cx, |architect, window, cx| {
            architect.selection = None;
            architect.pan = point(px(0.0), px(0.0));
            architect.focus_handle.focus(window, cx);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        for selector in [
            "architect-pane",
            "architect-plan-header",
            "architect-outline",
            "architect-graph-workspace",
            "architect-canvas",
            "architect-overview-inspector",
            "architect-status-context",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "wide Architect layout should render {selector}"
            );
        }

        cx.simulate_keystrokes("right");
        architect.read_with(cx, |architect, _| {
            assert_eq!(architect.selection, Some(Selection::Node(parent.clone())));
        });
        cx.simulate_keystrokes("enter");
        architect.read_with(cx, |architect, _| {
            assert!(architect.inspector_drawer_open);
        });
        cx.simulate_keystrokes("escape escape");
        architect.read_with(cx, |architect, _| {
            assert!(!architect.inspector_drawer_open);
            assert!(architect.selection.is_none());
        });

        let search_focus =
            architect.read_with(cx, |architect, cx| architect.search_editor.focus_handle(cx));
        let code_focus = code_item.read_with(cx, |code_item, cx| code_item.focus_handle(cx));
        architect.update_in(cx, |architect, window, cx| {
            architect.selection = Some(Selection::Node(parent.clone()));
            architect.pan = point(px(72.0), px(-24.0));
            search_focus.focus(window, cx);
            cx.notify();
        });
        cx.update(|window, _| {
            assert!(
                search_focus.is_focused(window),
                "the outline search should receive focus before switching"
            );
        });

        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::activate_code(workspace, window, cx);
            assert_eq!(
                workspace.active_item_as::<TestItem>(cx),
                Some(code_item.clone()),
                "Code mode should restore the exact native item"
            );
            assert!(
                code_focus.is_focused(window),
                "Code should restore the native item's focus"
            );
        });
        architect.read_with(cx, |architect, _| {
            assert_eq!(architect.mode(), ArchitectWorkspaceMode::Code);
            assert_eq!(architect.selection, Some(Selection::Node(parent.clone())));
            assert_eq!(architect.pan, point(px(72.0), px(-24.0)));
        });

        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::open(thread.clone(), workspace, window, cx);
            assert_eq!(
                workspace.item_of_type::<ArchitectPane>(cx),
                Some(architect.clone()),
                "switching back should reuse the retained Architect entity"
            );
            assert!(
                search_focus.is_focused(window),
                "Architect should restore its independently remembered focus"
            );
        });
        architect.read_with(cx, |architect, _| {
            assert_eq!(architect.mode(), ArchitectWorkspaceMode::Architect);
            assert_eq!(architect.selection, Some(Selection::Node(parent.clone())));
            assert_eq!(architect.pan, point(px(72.0), px(-24.0)));
        });

        for _ in 0..3 {
            workspace.update_in(cx, |workspace, window, cx| {
                ArchitectPane::activate_code(workspace, window, cx);
                assert_eq!(workspace.active_pane(), &active_pane);
                ArchitectPane::open(thread.clone(), workspace, window, cx);
                assert_eq!(
                    workspace.item_of_type::<ArchitectPane>(cx),
                    Some(architect.clone())
                );
                assert_eq!(
                    workspace
                        .status_bar()
                        .read(cx)
                        .item_of_type::<rendering::ArchitectStatusItem>(),
                    Some(status_item.clone()),
                    "switching must not register duplicate status items"
                );
            });
        }
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panel::<ProjectPanel>(cx), project_panel);
            assert_eq!(workspace.panel::<GitPanel>(cx), git_panel);
            assert_eq!(workspace.panel::<TerminalPanel>(cx), terminal_panel);
            assert_eq!(workspace.panel::<AgentPanel>(cx), agent_panel);
        });
        assert_eq!(
            agent_panel_entity.read_with(cx, |agent_panel, _| {
                agent_panel.active_conversation_view().cloned()
            }),
            Some(agent_conversation.clone()),
            "switching should retain the exact native Agent conversation"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::activate_code(workspace, window, cx);
            workspace.open_panel::<ProjectPanel>(window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("project-panel").is_some(),
            "Code should render the native Project panel"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("git_panel").is_some(),
            "Code should render the native Git panel as the alternate left tab"
        );
        let git_panel_focus = git_panel
            .as_ref()
            .expect("the native Git panel should exist")
            .read_with(cx, |git_panel, cx| git_panel.activation_focus_handle(cx));
        cx.update(|window, cx| git_panel_focus.focus(window, cx));
        cx.update(|window, _| {
            assert!(
                git_panel_focus.is_focused(window),
                "the native Git panel should own focus before switching"
            );
        });
        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::open(thread.clone(), workspace, window, cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::activate_code(workspace, window, cx);
            assert!(
                git_panel_focus.is_focused(window),
                "Code should restore focus to the exact native panel control"
            );
            ArchitectPane::open(thread.clone(), workspace, window, cx);
        });

        let base_graph = thread
            .read_with(cx, |thread, _| thread.architect_graph().cloned())
            .expect("the nested test graph should remain available");
        cx.simulate_resize(size(px(1500.0), px(900.0)));
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(Some(Selection::Node(parent.clone())), window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-step-inspector").is_some(),
            "selecting a step should render the contextual inspector"
        );
        let locked_title_editor = architect.read_with(cx, |architect, _| {
            architect
                .inspector
                .as_ref()
                .expect("the selected step should have an inspector")
                .title
                .clone()
        });
        assert!(
            locked_title_editor.read_with(cx, |editor, cx| editor.read_only(cx)),
            "settled steps should expose read-only inspector editors"
        );

        architect.update_in(cx, |architect, window, cx| {
            architect.discuss_node(parent.clone(), window, cx);
        });
        cx.run_until_parked();
        let step_session = thread.read_with(cx, |thread, _| {
            thread
                .architect_graph()
                .and_then(|graph| graph.node(&parent))
                .and_then(|node| node.chat.clone())
                .expect("opening a selected-step conversation should persist its session")
        });
        let step_conversation = agent_conversation.read_with(cx, |conversation, _| {
            conversation
                .thread_view(&step_session)
                .expect("the selected-step conversation should remain loaded")
        });
        assert_eq!(
            agent_conversation.read_with(cx, |conversation, _| {
                conversation.active_thread().cloned()
            }),
            Some(root_conversation.clone()),
            "opening a step conversation must not navigate the Agent panel away from the root plan"
        );
        let step_composer =
            step_conversation.read_with(cx, |conversation, _| conversation.message_editor.clone());
        assert!(
            step_composer
                .read_with(cx, |composer, cx| composer.text(cx))
                .contains("We are settling one step of the plan"),
            "a new step conversation should keep its editable seeded draft"
        );
        step_composer.update_in(cx, |composer, window, cx| {
            composer.set_text("Keep this selected-step draft", window, cx);
        });
        architect.update_in(cx, |architect, window, cx| {
            architect.discuss_node(parent.clone(), window, cx);
        });
        assert_eq!(
            thread.read_with(cx, |thread, _| {
                thread
                    .architect_graph()
                    .and_then(|graph| graph.node(&parent))
                    .and_then(|node| node.chat.clone())
            }),
            Some(step_session.clone()),
            "reopening a step conversation should reuse its persisted session"
        );
        assert_eq!(
            agent_conversation.read_with(cx, |conversation, _| {
                conversation.thread_view(&step_session)
            }),
            Some(step_conversation.clone()),
            "reopening a step conversation should reuse its live view"
        );
        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::activate_code(workspace, window, cx);
            ArchitectPane::open(thread.clone(), workspace, window, cx);
        });
        assert_eq!(
            step_composer.read_with(cx, |composer, cx| composer.text(cx)),
            "Keep this selected-step draft",
            "mode switching must not discard an unsent selected-step draft"
        );
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-step-conversation").is_some(),
            "the selected-step conversation state should render"
        );

        architect.update_in(cx, |architect, window, cx| {
            architect.open_plan_conversation(window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-plan-conversation-inspector")
                .is_some(),
            "the root plan conversation should have an explicit visual state"
        );
        architect.update_in(cx, |architect, window, cx| {
            architect.close_plan_conversation(window, cx);
        });

        let edge_target = NodeId::from("edge-target");
        let edge = {
            let mut graph = base_graph.clone();
            let mut target_node = ArchitectNode::new(edge_target.clone(), "Edge target");
            target_node.position = Some(Position { x: 400.0, y: 0.0 });
            graph.add_node(target_node);
            let edge = graph.connect(parent.clone(), edge_target.clone());
            thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));
            edge
        };
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(Some(Selection::Edge(edge)), window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-edge-inspector").is_some(),
            "selecting a connection should render its inspector"
        );
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(Some(Selection::Node(edge_target.clone())), window, cx);
        });
        let rapid_title_editor = architect.read_with(cx, |architect, _| {
            architect
                .inspector
                .as_ref()
                .expect("rapid selection should build the target inspector")
                .title
                .clone()
        });
        rapid_title_editor.update_in(cx, |editor, window, cx| {
            editor.set_text("Rapidly selected target", window, cx);
        });
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(Some(Selection::Node(parent.clone())), window, cx);
        });
        thread.read_with(cx, |thread, _| {
            let graph = thread
                .architect_graph()
                .expect("the rapid-selection graph should remain available");
            assert_eq!(
                graph.node(&edge_target).map(|node| node.title.as_str()),
                Some("Rapidly selected target")
            );
            assert_eq!(
                graph.node(&parent).map(|node| node.title.as_str()),
                Some("Parent"),
                "a stale inspector must not write into the newly selected step"
            );
        });

        thread.update(cx, |thread, cx| {
            thread.set_architect_graph(Some(base_graph.clone()), cx)
        });
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(Some(Selection::Node(parent.clone())), window, cx);
            architect.drill_into(parent.clone(), window, cx);
            assert_eq!(architect.focus, NodePath::root(parent.clone()));
            assert!(architect.drill_out(window, cx));
        });

        thread.update(cx, |thread, cx| {
            thread.start_architect_run(
                NodePath::root(parent.clone()),
                "Parent".into(),
                Task::ready(()),
                cx,
            );
            thread.set_architect_run_remote_workflow_url(
                Some("https://example.com/workflows/architect-run".into()),
                cx,
            );
            thread.note_architect_run_position(
                NodePath::root(parent.clone()),
                "Parent".into(),
                1,
                1,
                cx,
            );
            thread.finish_architect_run_step(Some("Validated the selected step output".into()), cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("architect-run-bar").is_some());
        assert!(cx.debug_bounds("architect-run-source").is_some());
        assert!(cx.debug_bounds("architect-run-output").is_some());
        assert!(
            cx.debug_bounds("architect-open-workflow").is_some(),
            "a remote run should expose exactly one workflow action"
        );
        thread.update(cx, |thread, cx| {
            thread.finish_architect_run(architect::RunOutcome::StepLimit { steps: 1 }, cx)
        });
        thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.architect_run().and_then(|run| run.outcome.as_ref()),
                Some(&architect::RunOutcome::StepLimit { steps: 1 })
            );
        });
        thread.update(cx, |thread, cx| {
            thread.start_architect_run(
                NodePath::root(parent.clone()),
                "Parent".into(),
                Task::ready(()),
                cx,
            );
            thread.finish_architect_run(architect::RunOutcome::Completed, cx);
        });
        thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.architect_run().and_then(|run| run.outcome.as_ref()),
                Some(&architect::RunOutcome::Completed)
            );
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-open-workflow").is_none(),
            "local runs must not render a dead workflow action"
        );

        cx.simulate_resize(size(px(1200.0), px(800.0)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("architect-outline").is_some());
        assert!(cx.debug_bounds("architect-step-inspector").is_some());
        assert!(cx.debug_bounds("architect-outline-drawer").is_none());
        assert!(cx.debug_bounds("architect-inspector-drawer").is_none());

        cx.simulate_resize(size(px(900.0), px(760.0)));
        architect.update_in(cx, |architect, window, cx| {
            architect.focus_handle.focus(window, cx);
            architect.open_inspector_drawer(window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-inspector-drawer").is_some(),
            "narrow layouts should overlay the inspector"
        );
        architect.update_in(cx, |architect, window, cx| {
            architect.close_inspector_drawer(window, cx);
            assert!(architect.focus_handle.is_focused(window));
        });

        cx.simulate_resize(size(px(680.0), px(700.0)));
        architect.update_in(cx, |architect, window, cx| {
            architect.focus_handle.focus(window, cx);
            architect.open_outline_drawer(true, window, cx);
            assert!(architect.search_editor.focus_handle(cx).is_focused(window));
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-outline-drawer").is_some(),
            "compact layouts should overlay plan navigation"
        );
        architect.update_in(cx, |architect, window, cx| {
            architect.close_outline_drawer(window, cx);
            assert!(architect.focus_handle.is_focused(window));
        });

        let positioned_node = |id: &str, title: &str, x: f32, y: f32| {
            let mut node = ArchitectNode::new(NodeId::from(id), title);
            node.position = Some(Position { x, y });
            node
        };
        let mut small = ArchitectGraph::default();
        small.add_node(positioned_node("small", "Small plan", 0.0, 0.0));

        let mut branching = ArchitectGraph::default();
        branching.add_node(positioned_node("branch-root", "Branch root", 0.0, 0.0));
        branching.add_node(positioned_node("branch-left", "Left path", -280.0, 260.0));
        branching.add_node(positioned_node("branch-right", "Right path", 280.0, 260.0));
        branching.connect("branch-root", "branch-left");
        branching.connect("branch-root", "branch-right");

        let mut cyclic = ArchitectGraph::default();
        cyclic.add_node(positioned_node("cycle-a", "Cycle A", -180.0, 0.0));
        cyclic.add_node(positioned_node("cycle-b", "Cycle B", 180.0, 0.0));
        cyclic.connect("cycle-a", "cycle-b");
        cyclic.connect("cycle-b", "cycle-a");

        cx.simulate_resize(size(px(1500.0), px(900.0)));
        for graph in [small, branching, cyclic, base_graph.clone()] {
            thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));
            architect.update_in(cx, |architect, window, cx| {
                architect.set_selection(None, window, cx);
                architect.focus = NodePath::default();
                architect.pan = point(px(0.0), px(0.0));
                architect.zoom = 1.0;
                cx.notify();
            });
            cx.run_until_parked();
            assert!(cx.debug_bounds("architect-canvas").is_some());
        }

        let mut large = ArchitectGraph::default();
        for index in 0..200 {
            large.add_node(positioned_node(
                &format!("large-{index}"),
                &format!(
                    "Large plan step {index} · 架構驗證 · a deliberately long localized title"
                ),
                index as f32 * 1000.0,
                0.0,
            ));
        }
        thread.update(cx, |thread, cx| thread.set_architect_graph(Some(large), cx));
        architect.update(cx, |architect, cx| {
            architect.pan = point(px(0.0), px(0.0));
            architect.zoom = 1.0;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-node-0").is_some(),
            "the visible portion of a large plan should render"
        );
        assert!(
            cx.debug_bounds("architect-node-199").is_none(),
            "far-off nodes should be culled"
        );
        architect.update(cx, |architect, cx| {
            architect.pan = point(px(-1000.0), px(0.0));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("architect-node-1").is_some(),
            "panning should reveal the next culled region"
        );
        architect.update(cx, |architect, cx| {
            architect.zoom = 0.5;
            architect.pan = point(px(-500.0), px(0.0));
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("architect-node-1").is_some());

        thread.update(cx, |thread, cx| {
            thread.start_architect_run(
                NodePath::root(NodeId::from("large-1")),
                "Large plan step 1".into(),
                Task::ready(()),
                cx,
            );
            thread.note_architect_run_position(
                NodePath::root(NodeId::from("large-1")),
                "Large plan step 1".into(),
                1,
                1,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("architect-run-bar").is_some());
        thread.update(cx, |thread, cx| thread.stop_architect_run(cx));

        thread.update(cx, |thread, cx| thread.set_architect_graph(None, cx));
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(None, window, cx);
            architect.focus_handle.focus(window, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("architect-empty-state").is_some());
        assert!(cx.debug_bounds("architect-canvas").is_none());
        cx.simulate_keystrokes("right delete");
        architect.read_with(cx, |architect, _| {
            assert!(architect.selection.is_none());
        });

        let replacement_acp_thread = cx
            .update(|_window, cx| {
                connection.clone().new_session(
                    project.clone(),
                    PathList::new(&[Path::new("/a")]),
                    cx,
                )
            })
            .await
            .expect("a replacement Architect session should open");
        let replacement_session_id =
            replacement_acp_thread.read_with(cx, |thread, _| thread.session_id().clone());
        let replacement_thread = cx
            .update(|_window, cx| connection.thread(&replacement_session_id, cx))
            .expect("the replacement native thread should exist");
        let mut replacement_graph = ArchitectGraph::default();
        replacement_graph.add_node(positioned_node(
            "replacement",
            "Replacement thread step",
            0.0,
            0.0,
        ));
        replacement_thread.update(cx, |thread, cx| {
            thread.set_architect_graph(Some(replacement_graph), cx)
        });
        fs.remove_dir(
            Path::new("/a"),
            fs::RemoveOptions {
                recursive: true,
                ignore_if_not_exists: false,
            },
        )
        .await
        .expect("the fake project directory should be removable");
        architect.update(cx, |architect, cx| {
            architect.selection = Some(Selection::Node(parent.clone()));
            architect.focus = NodePath::root(parent.clone());
            architect.pan = point(px(80.0), px(40.0));
            architect.activity.push(ArchitectActivityEntry {
                path: Some(NodePath::root(parent.clone())),
                message: "Stale activity".to_string(),
            });
            cx.notify();
        });
        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::open(replacement_thread.clone(), workspace, window, cx);
        });
        architect.read_with(cx, |architect, _| {
            assert_eq!(architect.thread, replacement_thread);
            assert!(architect.selection.is_none());
            assert_eq!(architect.focus, NodePath::default());
            assert_eq!(architect.pan, point(px(0.0), px(0.0)));
            assert!(architect.activity.is_empty());
        });

        active_pane.update_in(cx, |pane, window, cx| {
            pane.remove_item(architect.entity_id(), false, false, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.item_of_type::<ArchitectPane>(cx).is_none());
            assert_eq!(
                workspace.active_item_as::<TestItem>(cx),
                Some(code_item.clone()),
                "closing Architect should restore Code without destroying it"
            );
        });

        let reopened_architect = workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::open(replacement_thread, workspace, window, cx);
            workspace
                .item_of_type::<ArchitectPane>(cx)
                .expect("Architect should reopen after being closed")
        });
        assert_ne!(reopened_architect.entity_id(), architect.entity_id());
    }
}
