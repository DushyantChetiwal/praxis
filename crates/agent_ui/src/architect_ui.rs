//! The Architect canvas: a flowchart of the steps the agent will take.
//!
//! Edges are painted on a `gpui::canvas` layer underneath the nodes, while the
//! nodes themselves are ordinary absolutely-positioned elements on top. Doing
//! it the other way round would cost either curves (if edges were elements) or
//! text layout, hover and buttons inside nodes (if nodes were painted), so the
//! two halves are drawn the way each is best drawn.

mod automation;
mod bulk;
mod geometry;
mod inspector;
mod rendering;
mod run;

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use agent::Thread;
use architect::{
    ArchitectGraph, ArchitectNode, EdgeCondition, EdgeId, GraphMutationError, GraphProblem, NodeId,
    NodePath, Position,
};
use editor::Editor;
use git_ui::git_panel::GitPanel;
use gpui::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels,
    Point, ScrollDelta, ScrollWheelEvent, Subscription, Task, TaskExt as _, WeakEntity, Window,
    point, px,
};
use project_panel::ProjectPanel;
use settings::Settings as _;
use terminal_view::terminal_panel::TerminalPanel;
use workspace::{
    TabBarSettings, Workspace, ZoomIn, ZoomOut, dock::DockPosition, item::WeakItemHandle,
};

use crate::AgentPanel;
use crate::conversation_view::AcpServerViewEvent;
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
/// How far one press of a zoom button or key zooms.
const ZOOM_STEP: f32 = 1.2;
/// How much zoom a pixel of Ctrl- or Cmd-scrolling is worth. Applied as an
/// exponent, so scrolling back the same distance returns to the same zoom, and
/// a wheel notch is a modest step rather than a lurch.
const SCROLL_ZOOM_RATE: f32 = 0.0025;
const SCROLL_LINE_HEIGHT: f32 = 20.0;
/// How long zooming or moving the view takes to settle.
const CAMERA_ANIMATION: Duration = Duration::from_millis(200);
/// How often a moving view is redrawn: about once a display frame.
const CAMERA_FRAME: Duration = Duration::from_millis(16);
const WORKSPACE_MODE_KEY_PREFIX: &str = "architect-workspace-mode";

/// Below this, node text would be an unreadable smear, so nodes show their
/// title alone and let the shape of the graph do the talking.
const DETAIL_ZOOM_THRESHOLD: f32 = 0.62;

/// How many plan edits can be undone. Plans are small, so whole snapshots are
/// cheap and far simpler to keep correct than inverse operations.
const UNDO_LIMIT: usize = 100;
const UNDO_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd+Z"
} else {
    "Ctrl+Z"
};
const REDO_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd+Shift+Z"
} else {
    "Ctrl+Y"
};
const DUPLICATE_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd+D"
} else {
    "Ctrl+D"
};
const SELECT_ALL_SHORTCUT: &str = if cfg!(target_os = "macos") {
    "Cmd+A"
} else {
    "Ctrl+A"
};
/// How far a duplicated step lands from its original, so both stay visible.
const DUPLICATE_OFFSET: f32 = 40.0;

/// Edits that arrive as a stream, such as a drag or typing, and should undo as
/// one change.
#[derive(Clone, Debug, PartialEq)]
enum UndoGroup {
    Move(NodeId),
    /// Dragging several selected steps together.
    MoveSelection,
    NodeText(NodeId, &'static str),
    Condition(EdgeId),
}

/// A user edit to the plan, kept as the whole plan on either side of it.
struct UndoEntry {
    before: ArchitectGraph,
    after: ArchitectGraph,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryDirection {
    Undo,
    Redo,
}

#[derive(Clone, Debug, PartialEq)]
enum Selection {
    Node(NodeId),
    Edge(EdgeId),
}

/// What part of the canvas is in view: how far it is panned and zoomed.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Camera {
    pan: Point<Pixels>,
    zoom: f32,
}

/// The view gliding from one camera to another.
///
/// Zoom and pan are interpolated together with the same eased progress. The
/// two ends of a zoom about some point agree on where that point is drawn, and
/// a straight line between them keeps it there on every frame in between, so
/// the point under the cursor stays under it without special handling.
struct CameraAnimation {
    from: Camera,
    to: Camera,
    started: Instant,
}

impl CameraAnimation {
    /// Where the view is at `now`, and whether it has arrived.
    fn at(&self, now: Instant) -> (Camera, bool) {
        let elapsed = now.saturating_duration_since(self.started);
        let progress = elapsed.as_secs_f32() / CAMERA_ANIMATION.as_secs_f32();
        if progress >= 1.0 {
            return (self.to, true);
        }
        // Ease out: quick to answer the input, gentle to land.
        let eased = 1.0 - (1.0 - progress).powi(3);
        let camera = Camera {
            pan: self.from.pan + (self.to.pan - self.from.pan) * eased,
            zoom: self.from.zoom + (self.to.zoom - self.from.zoom) * eased,
        };
        (camera, false)
    }
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
    /// Shift-dragging a rectangle over empty canvas to select what it touches.
    Selecting {
        start: Point<Pixels>,
        at: Point<Pixels>,
    },
    /// Dragging the selected steps together. Each keeps its offset from where
    /// it started; a press that never moves selects the pressed step alone.
    DraggingGroup {
        anchor: NodeId,
        origin: Position,
        starts: Vec<(NodeId, Position)>,
        moved: bool,
    },
}

/// Identifies the canvas's notifications so a new one replaces the last rather
/// than stacking up behind it. Confirmations and problems are kept apart, so
/// a confirmation hiding itself never takes a problem with it.
struct ArchitectNotice;
struct ArchitectReport;

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
    window_handle: AnyWindowHandle,
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
    /// What is on screen right now, mid-glide included, so hit-testing always
    /// matches the frame the user is looking at.
    pan: Point<Pixels>,
    zoom: f32,
    /// Set while the view glides to a new zoom or position.
    camera_animation: Option<CameraAnimation>,
    _camera_animation_task: Task<()>,
    selection: Option<Selection>,
    /// Steps selected together, in the order they were added. Only meaningful
    /// with two or more; `selection` is empty meanwhile.
    bulk: Vec<NodeId>,
    /// The outline row a Shift-click selects from: the one last clicked there
    /// without Shift. A selection made anywhere else clears it, and the range
    /// then starts from what that selected.
    outline_anchor: Option<NodeId>,
    inspector: Option<NodeInspector>,
    edge_inspector: Option<EdgeInspector>,
    interaction: Interaction,
    hovered_node: Option<NodeId>,
    /// The step the last left press landed on. The platform counts a double
    /// click by time and place alone, so a press on a menu or the canvas
    /// followed by one on a step must not open that step's plan.
    last_pressed_node: Option<NodeId>,
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
    /// While a run is going, the conversation drawer shows the running step's
    /// own thread unless the user asked for the plan's conversation instead.
    plan_drawer_shows_plan: bool,
    /// A past step visit whose thread the conversation drawer is showing, as
    /// opened from a step's run activity. `None` follows the running step.
    inspected_step_session: Option<agent_client_protocol::schema::v1::SessionId>,
    outline_width: Pixels,
    inspector_width: Pixels,
    search_editor: Entity<Editor>,
    activity: Vec<ArchitectActivityEntry>,
    /// Set while a run is being started, so the toolbar can show it before the
    /// thread has been told. The run itself belongs to the thread.
    run_starting: Cell<bool>,
    /// Numbers each confirmation, so the timer hiding one cannot hide the one
    /// that replaced it.
    notice_count: Cell<usize>,
    undo_stack: Vec<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    /// The group the newest undo entry belongs to, while it can still grow.
    undo_group: Option<UndoGroup>,
    /// The group the next edit belongs to, set by the code making it.
    next_undo_group: Option<UndoGroup>,

    _thread_subscription: Subscription,
    _model_registry_subscription: Subscription,
    _search_subscription: Subscription,
    /// Watches for another item becoming active while Architect View owns the
    /// workspace, so that opening a file switches to Editor View with it.
    _workspace_subscription: Option<Subscription>,
    /// Follows the plan's conversation, so that a step thread asking for its
    /// parent (its Minimize button, or Go Back) brings the drawer back to
    /// the plan's own conversation. Kept with the id of the conversation it
    /// follows, because the Agent panel can switch conversations.
    plan_conversation_subscription: Option<(gpui::EntityId, Subscription)>,
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
        let subscription = cx.observe(&thread, |this, _, cx| {
            // The agent or a run may have removed selected steps.
            this.prune_bulk_selection(cx);
            this.refresh_inspector_if_source_changed(cx);
            cx.emit(workspace::item::ItemEvent::UpdateTab);
            cx.notify();
        });
        let model_registry_subscription = cx.subscribe(
            &language_model::LanguageModelRegistry::global(cx),
            |_, _, _, cx| cx.notify(),
        );
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search steps", window, cx);
            editor
        });
        let search_subscription = cx.observe(&search_editor, |_, _, cx| cx.notify());
        let workspace_subscription = workspace.upgrade().map(|workspace| {
            cx.subscribe_in(
                &workspace,
                window,
                |this, workspace, event: &workspace::Event, window, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        this.switch_to_editor_for_opened_item(workspace, window, cx);
                    }
                },
            )
        });
        Self {
            thread,
            workspace,
            window_handle: window.window_handle(),
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
            camera_animation: None,
            _camera_animation_task: Task::ready(()),
            selection: None,
            bulk: Vec::new(),
            outline_anchor: None,
            inspector: None,
            edge_inspector: None,
            interaction: Interaction::None,
            hovered_node: None,
            last_pressed_node: None,
            focus: NodePath::default(),
            expanded: HashSet::default(),
            inspector_tab: InspectorTab::Details,
            outline_drawer_open: false,
            inspector_drawer_open: false,
            plan_conversation_open: false,
            plan_drawer_shows_plan: false,
            inspected_step_session: None,
            outline_width,
            inspector_width,
            search_editor,
            activity: Vec::new(),
            run_starting: Cell::new(false),
            notice_count: Cell::new(0),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            undo_group: None,
            next_undo_group: None,
            _thread_subscription: subscription,
            _model_registry_subscription: model_registry_subscription,
            _search_subscription: search_subscription,
            _workspace_subscription: workspace_subscription,
            plan_conversation_subscription: None,
        }
    }

    pub fn mode(&self) -> ArchitectWorkspaceMode {
        self.mode
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

    fn set_outline_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        let width = px(f32::from(width).clamp(184.0, 320.0));
        if width != self.outline_width {
            self.outline_width = width;
            cx.notify();
        }
    }

    fn set_inspector_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        let width = px(f32::from(width).clamp(288.0, 480.0));
        if width != self.inspector_width {
            self.inspector_width = width;
            cx.notify();
        }
    }

    fn reset_divider(&mut self, divider: rendering::ArchitectDivider, cx: &mut Context<Self>) {
        let defaults = ArchitectWorkspaceState::default();
        match divider {
            rendering::ArchitectDivider::Outline => {
                self.set_outline_width(px(defaults.outline_width), cx);
            }
            rendering::ArchitectDivider::Inspector => {
                self.set_inspector_width(px(defaults.inspector_width), cx);
            }
        }
        self.persist_layout(cx);
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
            dock.update(cx, |dock, cx| {
                dock.set_opening_enabled(false, window, cx);
            });
        }
    }

    fn restore_code_docks(
        workspace: &mut Workspace,
        snapshots: &[DockSnapshot],
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        for dock in workspace.all_docks() {
            dock.update(cx, |dock, cx| {
                dock.set_opening_enabled(true, window, cx);
            });
        }
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
        self._thread_subscription = cx.observe(&thread, |this, _, cx| {
            this.prune_bulk_selection(cx);
            this.refresh_inspector_if_source_changed(cx);
            cx.emit(workspace::item::ItemEvent::UpdateTab);
            cx.notify();
        });
        self.focus = NodePath::default();
        self.selection = None;
        self.bulk.clear();
        self.outline_anchor = None;
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
        self.inspected_step_session = None;
        self.last_architect_focus = None;
        self.transient_return_focus = None;
        self.pan = point(px(0.0), px(0.0));
        self.zoom = 1.0;
        self.camera_animation = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_group = None;
        self.next_undo_group = None;
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
        let agent_panel = workspace.panel::<AgentPanel>(cx);
        let attached_architect = workspace.item_of_type::<ArchitectPane>(cx);
        let existing = attached_architect.or_else(|| {
            agent_panel
                .as_ref()
                .and_then(|panel| panel.read(cx).retained_architect_pane())
        });
        let existing_is_attached = existing.as_ref().is_some_and(|existing| {
            workspace
                .items(cx)
                .any(|item| item.item_id() == existing.entity_id())
        });
        let already_active = workspace
            .active_item_as::<ArchitectPane>(cx)
            .is_some_and(|active| {
                existing.as_ref() == Some(&active)
                    && active.read(cx).mode() == ArchitectWorkspaceMode::Architect
            });

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

        let architect = if let Some(existing) = existing {
            existing.update(cx, |architect, cx| {
                architect.replace_thread(thread, cx);
                if architect.mode != ArchitectWorkspaceMode::Architect {
                    architect.mode = ArchitectWorkspaceMode::Architect;
                    cx.notify();
                }
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
            if existing_is_attached {
                workspace.activate_item(&existing, true, true, window, cx);
            } else {
                workspace.add_item_to_active_pane(
                    Box::new(existing.clone()),
                    None,
                    true,
                    window,
                    cx,
                );
            }
            let focus = existing
                .read(cx)
                .last_architect_focus
                .clone()
                .unwrap_or_else(|| existing.read(cx).focus_handle.clone());
            focus.focus(window, cx);
            existing
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
            workspace.add_item_to_active_pane(Box::new(architect.clone()), None, true, window, cx);
            architect
        };

        if let Some(agent_panel) = agent_panel {
            agent_panel.update(cx, |agent_panel, _| {
                agent_panel.retain_architect_pane(architect.clone());
            });
        }

        workspace.active_pane().update(cx, |pane, cx| {
            pane.set_should_display_tab_bar(|_, _| false);
            pane.zoom_in(&ZoomIn, window, cx);
            cx.notify();
        });
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

    /// `opened_item`, when given, is something the user opened while Architect
    /// View was showing; it stays active and focused instead of the item that
    /// was active before Architect View opened.
    fn restore_code_surface(
        workspace: &mut Workspace,
        opened_item: Option<Box<dyn workspace::item::ItemHandle>>,
        previous_code_item: Option<Box<dyn workspace::item::ItemHandle>>,
        code_docks: Vec<DockSnapshot>,
        last_code_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.active_pane().update(cx, |pane, cx| {
            pane.set_should_display_tab_bar(|_, cx| TabBarSettings::get_global(cx).show);
            pane.zoom_out(&ZoomOut, window, cx);
            cx.notify();
        });
        Self::arrange_code_panels(workspace, window, cx);

        let opened_focus = opened_item
            .as_ref()
            .map(|opened_item| opened_item.item_focus_handle(cx));
        let code_item = opened_item.or(previous_code_item).or_else(|| {
            workspace
                .items(cx)
                .find(|item| item.downcast::<ArchitectPane>().is_none())
                .map(|item| item.boxed_clone())
        });
        if let Some(code_item) = code_item {
            workspace.activate_item(code_item.as_ref(), true, true, window, cx);
        }

        Self::restore_code_docks(workspace, &code_docks, window, cx);
        if let Some(focus) = opened_focus.or(last_code_focus) {
            focus.focus(window, cx);
        }
        Self::persist_mode(workspace, ArchitectWorkspaceMode::Code, cx);
    }

    pub fn activate_code(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::activate_code_with_item(workspace, None, window, cx);
    }

    fn activate_code_with_item(
        workspace: &mut Workspace,
        opened_item: Option<Box<dyn workspace::item::ItemHandle>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(architect) = workspace.item_of_type::<ArchitectPane>(cx) else {
            return;
        };
        let architect_pane = workspace
            .panes()
            .iter()
            .find(|pane| pane.read(cx).index_for_item(&architect).is_some())
            .cloned();
        let architect_is_retained = workspace.panel::<AgentPanel>(cx).is_some_and(|panel| {
            panel.read(cx).retained_architect_pane().as_ref() == Some(&architect)
        });
        let (previous_code_item, code_docks, last_code_focus) =
            architect.update(cx, |architect, cx| {
                if architect.mode != ArchitectWorkspaceMode::Code {
                    architect.mode = ArchitectWorkspaceMode::Code;
                    cx.notify();
                }
                (
                    architect
                        .previous_code_item
                        .as_ref()
                        .and_then(|item| item.upgrade()),
                    architect.code_docks.clone(),
                    architect.last_code_focus.clone(),
                )
            });
        // An item opened into another pane (a split, say) leaves Architect's
        // own pane zoomed with its tab strip hidden unless it is reset here.
        if let Some(architect_pane) = &architect_pane
            && architect_pane != workspace.active_pane()
        {
            architect_pane.update(cx, |pane, cx| {
                pane.set_should_display_tab_bar(|_, cx| TabBarSettings::get_global(cx).show);
                pane.zoom_out(&ZoomOut, window, cx);
                cx.notify();
            });
        }
        Self::restore_code_surface(
            workspace,
            opened_item,
            previous_code_item,
            code_docks,
            last_code_focus,
            window,
            cx,
        );
        if architect_is_retained && let Some(architect_pane) = architect_pane {
            architect_pane.update(cx, |pane, cx| {
                pane.remove_item(architect.entity_id(), false, false, window, cx);
            });
        }
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
        self.inspected_step_session = None;
        self.restore_transient_focus(window, cx);
        cx.notify();
    }

    /// Opening a file, or anything else, while Architect View owns the
    /// workspace means the user wants to see it. Left alone it would show in
    /// Architect's zoomed pane with no tabs and no panels, so switch to Editor
    /// View and keep it in front.
    fn switch_to_editor_for_opened_item(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != ArchitectWorkspaceMode::Architect {
            return;
        }
        let architect_id = cx.entity().entity_id();
        let opened_item = {
            let workspace = workspace.read(cx);
            // A canvas that is not in the workspace, such as one kept by the
            // Agent panel, has no say over what the workspace shows.
            if !workspace
                .items(cx)
                .any(|item| item.item_id() == architect_id)
            {
                return;
            }
            let Some(opened_item) = workspace
                .active_item(cx)
                .filter(|item| item.item_id() != architect_id)
            else {
                return;
            };
            opened_item.downgrade_item()
        };
        let architect = cx.weak_entity();
        let workspace = workspace.downgrade();
        window.defer(cx, move |window, cx| {
            let still_architect = architect.upgrade().is_some_and(|architect| {
                architect.read(cx).mode == ArchitectWorkspaceMode::Architect
            });
            if !still_architect {
                return;
            }
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            let Some(opened_item) = opened_item.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                Self::activate_code_with_item(workspace, Some(opened_item), window, cx);
            });
        });
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

    /// Closing Architect while it owns the workspace must hand the workspace
    /// back to Code; otherwise the docks stay locked and the tab strip hidden.
    fn restore_after_removal(&self, cx: &mut Context<Self>) {
        if self.mode != ArchitectWorkspaceMode::Architect {
            return;
        }
        let architect = cx.weak_entity();
        let workspace = self.workspace.clone();
        let window_handle = self.window_handle;
        let previous_code_item = self
            .previous_code_item
            .as_ref()
            .and_then(|item| item.upgrade());
        let code_docks = self.code_docks.clone();
        let last_code_focus = self.last_code_focus.clone();
        cx.defer(move |cx| {
            let result = window_handle.update(cx, |_, window, cx| {
                if let Some(architect) = architect.upgrade() {
                    architect.update(cx, |architect, cx| {
                        architect.mode = ArchitectWorkspaceMode::Code;
                        cx.notify();
                    });
                }
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    Self::restore_code_surface(
                        workspace,
                        None,
                        previous_code_item,
                        code_docks,
                        last_code_focus,
                        window,
                        cx,
                    );
                });
            });
            if let Err(error) = result {
                log::debug!("Skipped restoring Code after closing Architect: {error:#}");
            }
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
    fn edit_graph(
        &mut self,
        edit: impl FnOnce(&mut ArchitectGraph),
        cx: &mut Context<Self>,
    ) -> bool {
        let focus = self.focus.clone();
        self.edit_checked(
            move |root| root.mutate_graph_at(&focus, edit).map(|_| ()),
            cx,
        )
    }

    fn edit_checked(
        &mut self,
        edit: impl FnOnce(&mut ArchitectGraph) -> Result<(), GraphMutationError>,
        cx: &mut Context<Self>,
    ) -> bool {
        let group = self.next_undo_group.take();
        let before = self.root_graph(cx).cloned();
        let result = self
            .thread
            .update(cx, |thread, cx| thread.update_architect_graph(edit, cx));
        match result {
            Some(Ok(())) => {
                let after = self.root_graph(cx).cloned();
                if let (Some(before), Some(after)) = (before, after) {
                    self.record_undo(before, after, group);
                }
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

    fn record_undo(
        &mut self,
        before: ArchitectGraph,
        after: ArchitectGraph,
        group: Option<UndoGroup>,
    ) {
        if before == after {
            return;
        }
        self.redo_stack.clear();
        if group.is_some()
            && group == self.undo_group
            && let Some(last) = self.undo_stack.last_mut()
            && last.after == before
        {
            last.after = after;
            return;
        }
        self.undo_group = group;
        if self.undo_stack.len() >= UNDO_LIMIT {
            let excess = self.undo_stack.len() + 1 - UNDO_LIMIT;
            self.undo_stack.drain(..excess);
        }
        self.undo_stack.push(UndoEntry { before, after });
    }

    /// Applies bookkeeping that is not a user edit, such as a step's
    /// conversation id, to every recorded plan so it neither invalidates the
    /// history nor is lost by undoing past it.
    fn update_history(&mut self, mut update: impl FnMut(&mut ArchitectGraph)) {
        for entry in self.undo_stack.iter_mut().chain(self.redo_stack.iter_mut()) {
            update(&mut entry.before);
            update(&mut entry.after);
        }
    }

    fn step_history(
        &mut self,
        direction: HistoryDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_running(cx) {
            self.report(
                "Plan edits cannot be undone or redone while the plan is running.".to_string(),
                cx,
            );
            return;
        }
        let entry = match direction {
            HistoryDirection::Undo => self.undo_stack.pop(),
            HistoryDirection::Redo => self.redo_stack.pop(),
        };
        let Some(entry) = entry else {
            return;
        };
        let (expected, restored) = match direction {
            HistoryDirection::Undo => (&entry.after, &entry.before),
            HistoryDirection::Redo => (&entry.before, &entry.after),
        };
        // Anything else that changed the plan, such as the agent, has made the
        // recorded snapshots stale. Restoring one would silently drop that work.
        if self.root_graph(cx) != Some(expected) {
            self.undo_stack.clear();
            self.redo_stack.clear();
            self.undo_group = None;
            self.report(
                "The plan was changed elsewhere, so earlier edits can no longer be undone."
                    .to_string(),
                cx,
            );
            return;
        }

        let restored = restored.clone();
        self.thread.update(cx, |thread, cx| {
            thread.set_architect_graph(Some(restored), cx)
        });
        self.undo_group = None;
        self.interaction = Interaction::None;
        let message = match direction {
            HistoryDirection::Undo => {
                self.redo_stack.push(entry);
                "Undid a plan edit"
            }
            HistoryDirection::Redo => {
                self.undo_stack.push(entry);
                "Redid a plan edit"
            }
        };
        self.record_activity(None, message, cx);
        self.sync_view_to_graph(window, cx);
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
        if !existing && self.is_running(cx) {
            self.report(
                "Stop the run before breaking a step into steps.".to_string(),
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
        self.bulk.clear();
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
        self.bulk.clear();
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

    fn camera(&self) -> Camera {
        Camera {
            pan: self.pan,
            zoom: self.zoom,
        }
    }

    /// Where the view is heading: the end of its glide, or where it is.
    fn target_camera(&self) -> Camera {
        self.camera_animation
            .as_ref()
            .map_or(self.camera(), |animation| animation.to)
    }

    /// Glides the view to `to`, starting from what is on screen, so a new
    /// target mid-glide carries on from there rather than snapping back.
    fn animate_camera(&mut self, to: Camera, cx: &mut Context<Self>) {
        let to = Camera {
            zoom: to.zoom.clamp(MIN_ZOOM, MAX_ZOOM),
            ..to
        };
        if to == self.camera() {
            self.camera_animation = None;
            cx.notify();
            return;
        }
        let executor = cx.background_executor().clone();
        self.camera_animation = Some(CameraAnimation {
            from: self.camera(),
            to,
            started: executor.now(),
        });
        // The first frame is timed from now rather than from whenever the task
        // first runs, so frames and the animation keep the same clock.
        let mut frame = executor.timer(CAMERA_FRAME);
        self._camera_animation_task = cx.spawn(async move |this, cx| {
            loop {
                frame.await;
                match this.update(cx, |this, cx| this.step_camera_animation(cx)) {
                    Ok(true) => {}
                    // Landed, or the canvas has closed.
                    Ok(false) | Err(_) => break,
                }
                frame = executor.timer(CAMERA_FRAME);
            }
        });
        cx.notify();
    }

    /// Moves the view a frame along its glide, returning whether there is more
    /// of it to come.
    fn step_camera_animation(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(animation) = &self.camera_animation else {
            return false;
        };
        let (camera, arrived) = animation.at(cx.background_executor().now());
        self.pan = camera.pan;
        self.zoom = camera.zoom;
        if arrived {
            self.camera_animation = None;
        }
        cx.notify();
        !arrived
    }

    /// Jumps to the end of any glide, for callers that need the view where it
    /// is going rather than where it is.
    fn settle_camera(&mut self, cx: &mut Context<Self>) {
        if let Some(animation) = self.camera_animation.take() {
            self.pan = animation.to.pan;
            self.zoom = animation.to.zoom;
            cx.notify();
        }
    }

    /// Scrolling and dragging move the view at once, under the hand. A glide
    /// under way is carried along with it, so a zoom still settling lands
    /// where the user has since moved the view.
    fn pan_by(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        self.pan += delta;
        if let Some(animation) = &mut self.camera_animation {
            animation.from.pan += delta;
            animation.to.pan += delta;
        }
        cx.notify();
    }

    /// Zooms to `zoom`, keeping whatever is under `anchor` exactly where it is,
    /// so zooming feels like moving closer rather than like the graph sliding
    /// away. Without an anchor, as for the buttons and keys, the middle of the
    /// view stays put.
    fn set_zoom(&mut self, zoom: f32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let centre =
            anchor
                .zip(self.viewport.get())
                .map_or(point(px(0.0), px(0.0)), |(anchor, bounds)| {
                    point(
                        anchor.x - bounds.origin.x - bounds.size.width / 2.0,
                        anchor.y - bounds.origin.y - bounds.size.height / 2.0,
                    )
                });
        // Measured from what is on screen, not from where a glide was heading,
        // so the point kept still is the one the user sees under the cursor.
        let ratio = zoom / self.zoom;
        let pan = centre - (centre - self.pan) * ratio;
        self.animate_camera(Camera { pan, zoom }, cx);
    }

    /// Zooms by `factor` from wherever the zoom is heading, so quick repeated
    /// presses or scrolls add up instead of each starting over.
    fn zoom_by(&mut self, factor: f32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        self.set_zoom(self.target_camera().zoom * factor, anchor, cx);
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

        let centre_x = (min_x + max_x) / 2.0;
        let centre_y = (min_y + max_y) / 2.0;
        let pan = point(px(-centre_x * zoom), px(-centre_y * zoom));
        self.animate_camera(Camera { pan, zoom }, cx);
    }

    /// Selects something chosen away from the canvas, such as in the outline or
    /// with the keyboard, and brings it into view.
    fn select_and_reveal(
        &mut self,
        selection: Selection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_selection(Some(selection.clone()), window, cx);
        self.reveal(&selection, cx);
    }

    /// Centres a step or connection that is not wholly on screen. One already
    /// in view stays where it is, so the canvas never moves under the pointer.
    fn reveal(&mut self, selection: &Selection, cx: &mut Context<Self>) {
        let Some(bounds) = self.viewport.get() else {
            return;
        };
        let Some(graph) = self.graph(cx) else {
            return;
        };
        let target = match selection {
            Selection::Node(id) => graph.node(id).and_then(|node| {
                let (width, height) = self.node_size(node);
                node.position.map(|position| (position, width, height))
            }),
            Selection::Edge(id) => {
                graph
                    .edges
                    .iter()
                    .find(|edge| &edge.id == id)
                    .and_then(|edge| {
                        let from = graph.node(&edge.from)?.position?;
                        let to = graph.node(&edge.to)?.position?;
                        Some((EdgeCurve::between(from, to).midpoint(), 0.0, 0.0))
                    })
            }
        };
        let Some((centre, width, height)) = target else {
            return;
        };
        // Judged by where the view is heading, so something already on its way
        // into view is not sent after again.
        let camera = self.target_camera();
        let origin = point(
            bounds.origin.x + bounds.size.width / 2.0 + camera.pan.x,
            bounds.origin.y + bounds.size.height / 2.0 + camera.pan.y,
        );
        let screen = point(
            origin.x + px(centre.x * camera.zoom),
            origin.y + px(centre.y * camera.zoom),
        );
        let half_width = px(width * camera.zoom / 2.0);
        let half_height = px(height * camera.zoom / 2.0);
        let visible = screen.x - half_width >= bounds.left()
            && screen.x + half_width <= bounds.right()
            && screen.y - half_height >= bounds.top()
            && screen.y + half_height <= bounds.bottom();
        if visible {
            return;
        }
        let pan = point(px(-centre.x * camera.zoom), px(-centre.y * camera.zoom));
        self.animate_camera(Camera { pan, ..camera }, cx);
    }

    // -- Editing --------------------------------------------------------------

    fn delete_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        // The run writes each step's result back into the plan, so the plan's
        // shape stays fixed until it finishes, as Add Step and Tidy already do.
        if self.is_running(cx) {
            self.report(
                "Stop the run before deleting from the plan.".to_string(),
                cx,
            );
            return;
        }
        match selection {
            Selection::Node(id) => {
                let title = self
                    .graph(cx)
                    .and_then(|graph| graph.node(&id))
                    .map(|node| node.title.trim().to_string())
                    .unwrap_or_default();
                let path = self.focus.child(id);
                let activity_path = path.clone();
                if self.edit_checked(move |graph| graph.remove_node_at(&path), cx) {
                    self.record_activity(
                        Some(activity_path),
                        "Deleted this step from the plan",
                        cx,
                    );
                    self.set_selection(None, window, cx);
                    let deleted = if title.is_empty() {
                        "Deleted a step".to_string()
                    } else {
                        format!("Deleted \"{title}\"")
                    };
                    self.notice(format!("{deleted}. Press {UNDO_SHORTCUT} to undo."), cx);
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
                    self.notice(
                        format!("Deleted a connection. Press {UNDO_SHORTCUT} to undo."),
                        cx,
                    );
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
    /// It stays until dismissed, since it usually says why something did not
    /// happen.
    fn report(&self, message: String, cx: &mut Context<Self>) {
        log::warn!("Architect: {message}");
        self.show_toast(message, false, cx);
    }

    /// Confirms something that went as asked, without logging it as a problem.
    /// It hides itself: the canvas already shows the change.
    fn notice(&self, message: String, cx: &mut Context<Self>) {
        self.show_toast(message, true, cx);
    }

    fn show_toast(&self, message: String, autohide: bool, cx: &mut Context<Self>) {
        use workspace::notifications::NotificationId;

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if !autohide {
            let toast = workspace::Toast::new(NotificationId::unique::<ArchitectReport>(), message);
            workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx));
            return;
        }
        let number = self.notice_count.get();
        self.notice_count.set(number + 1);
        let previous = number
            .checked_sub(1)
            .map(NotificationId::composite::<ArchitectNotice>);
        let toast = workspace::Toast::new(
            NotificationId::composite::<ArchitectNotice>(number),
            message,
        )
        .autohide();
        workspace.update(cx, |workspace, cx| {
            if let Some(previous) = &previous {
                workspace.dismiss_toast(previous, cx);
            }
            workspace.show_toast(toast, cx);
        });
    }

    /// Makes sure the drawer is following the conversation the Agent panel
    /// is showing now. Called as the canvas renders, which is when it looks
    /// that conversation up.
    fn follow_plan_conversation(&mut self, cx: &mut Context<Self>) {
        let conversation = self.plan_conversation_view(cx);
        let followed = self
            .plan_conversation_subscription
            .as_ref()
            .map(|(id, _)| *id);
        if conversation.as_ref().map(|view| view.entity_id()) == followed {
            return;
        }
        self.plan_conversation_subscription = conversation.map(|conversation| {
            let subscription = cx.subscribe(
                &conversation,
                |this, conversation, event: &AcpServerViewEvent, cx| {
                    let AcpServerViewEvent::NavigatedToThread(session_id) = event else {
                        return;
                    };
                    if conversation.read(cx).root_session_id.as_ref() == Some(session_id) {
                        this.plan_drawer_shows_plan = true;
                        this.inspected_step_session = None;
                        cx.notify();
                    }
                },
            );
            (conversation.entity_id(), subscription)
        });
    }

    /// Opens the conversation drawer on the step being run, which is where it
    /// asks to be allowed to do things.
    fn watch_running_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The step's thread is normally loaded as it starts; this covers a
        // plan conversation that missed it.
        if let Some(session_id) = self.run_step_session(cx) {
            self.load_step_thread(session_id, window, cx);
        }
        self.remember_transient_focus(window, cx);
        self.plan_drawer_shows_plan = false;
        self.inspected_step_session = None;
        self.plan_conversation_open = true;
        self.inspector_drawer_open = true;
        cx.notify();
    }

    /// Loads a step's thread into the plan's conversation, if it is not there
    /// already, and redraws once it is so the drawer can show it.
    fn load_step_thread(
        &mut self,
        session_id: agent_client_protocol::schema::v1::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.plan_conversation_view(cx) else {
            return;
        };
        let load = view.update(cx, |view, cx| {
            view.ensure_subagent_thread_loaded(session_id, window, cx)
        });
        cx.spawn(async move |this, cx| {
            load.await?;
            this.update(cx, |_, cx| cx.notify())
        })
        .detach_and_log_err(cx);
    }

    /// Opens the conversation drawer on the thread a step ran in, so what it
    /// did can be read after the run.
    fn open_step_visit(
        &mut self,
        session_id: agent_client_protocol::schema::v1::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.load_step_thread(session_id.clone(), window, cx);
        self.remember_transient_focus(window, cx);
        self.plan_drawer_shows_plan = false;
        self.inspected_step_session = Some(session_id);
        self.plan_conversation_open = true;
        self.inspector_drawer_open = true;
        cx.notify();
    }

    fn open_plan_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_transient_focus(window, cx);
        self.plan_drawer_shows_plan = true;
        self.inspected_step_session = None;
        self.plan_conversation_open = true;
        self.inspector_drawer_open = true;
        self.record_activity(None, "Opened the overall plan conversation", cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn start_planning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread.read(cx).session_mode() != agent::SessionMode::Architect {
            self.thread.update(cx, |thread, cx| {
                thread.set_session_mode(agent::SessionMode::Architect, cx);
            });
        }
        self.open_plan_conversation(window, cx);
        let composer = self
            .plan_conversation_view(cx)
            .and_then(|conversation| conversation.read(cx).root_thread_view())
            .map(|thread_view| thread_view.read(cx).message_editor.clone());
        if let Some(composer) = composer {
            composer.focus_handle(cx).focus(window, cx);
        }
    }

    fn add_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // New steps land in the middle of the view rather than at the origin,
        // so one appears where the user is looking.
        let centre = self.to_canvas(self.origin());
        let id = NodeId(format!("step-{}", uuid::Uuid::new_v4().simple()));
        let mut node = ArchitectNode::new(id.clone(), "New step");
        node.position = Some(centre);

        let added = self.edit_graph(
            move |graph| {
                graph.add_node(node);
            },
            cx,
        );
        if !added {
            return;
        }
        let path = self.focus.child(id.clone());
        self.record_activity(Some(path), "Added this step to the plan", cx);
        self.set_selection(Some(Selection::Node(id)), window, cx);
    }

    /// Copies the selected step beside itself as a fresh draft, keeping its
    /// brief and any plan inside it but none of its connections.
    fn duplicate_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selection::Node(id)) = self.selection.clone() else {
            return;
        };
        if self.is_running(cx) {
            self.report("Stop the run before duplicating a step.".to_string(), cx);
            return;
        }
        let Some(mut copy) = self.graph(cx).and_then(|graph| graph.node(&id)).cloned() else {
            return;
        };
        reset_for_copy(&mut copy);
        copy.id = NodeId(format!("step-{}", uuid::Uuid::new_v4().simple()));
        copy.title = format!("{} (copy)", copy.title.trim());
        copy.position = copy.position.map(|position| Position {
            x: snap(position.x + DUPLICATE_OFFSET),
            y: snap(position.y + DUPLICATE_OFFSET),
        });
        let copy_id = copy.id.clone();
        let added = self.edit_graph(
            move |graph| {
                graph.add_node(copy);
            },
            cx,
        );
        if !added {
            return;
        }
        let path = self.focus.child(copy_id.clone());
        self.record_activity(Some(path), "Duplicated a step", cx);
        self.select_and_reveal(Selection::Node(copy_id), window, cx);
    }

    /// Selects the first or last step in the order the plan runs.
    fn select_end_step(&mut self, last: bool, window: &mut Window, cx: &mut Context<Self>) {
        let order = self
            .graph(cx)
            .map(ArchitectGraph::execution_order)
            .unwrap_or_default();
        let target = if last { order.last() } else { order.first() };
        if let Some(id) = target.cloned() {
            self.select_and_reveal(Selection::Node(id), window, cx);
        }
    }

    fn tidy_up(&mut self, cx: &mut Context<Self>) {
        if !self.edit_graph(|graph| graph.relayout(), cx) {
            return;
        }
        self.record_activity(
            Some(self.focus.clone()),
            "Arranged the steps automatically",
            cx,
        );
        self.zoom_to_fit(cx);
    }

    fn problem_selection(problem: &GraphProblem) -> Selection {
        match problem {
            GraphProblem::DuplicateNode(id)
            | GraphProblem::Unreachable(id)
            | GraphProblem::Unlocked(id)
            | GraphProblem::EndlessLoop(id)
            | GraphProblem::InSubplan { node: id, .. } => Selection::Node(id.clone()),
            GraphProblem::DanglingEdge { edge, .. } | GraphProblem::EmptyCondition(edge) => {
                Selection::Edge(edge.clone())
            }
        }
    }

    /// Selects the first thing on this level that blocks a run. Review Plan is
    /// only offered while there is one.
    fn review_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first_problem = self.graph(cx).and_then(|graph| {
            graph
                .blocking_problems()
                .first()
                .map(Self::problem_selection)
        });
        let Some(selection) = first_problem else {
            return;
        };
        self.record_activity(None, "Reviewed the plan: issues need attention", cx);
        self.select_and_reveal(selection, window, cx);
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
            self.select_and_reveal(Selection::Node(id), window, cx);
        }
    }

    // -- Input ----------------------------------------------------------------

    fn handle_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.modifiers.control || event.modifiers.platform {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => f32::from(pixels.y),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_HEIGHT,
            };
            self.zoom_by((delta * SCROLL_ZOOM_RATE).exp(), Some(event.position), cx);
        } else {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels,
                ScrollDelta::Lines(lines) => lines.map(|value| px(value * SCROLL_LINE_HEIGHT)),
            };
            self.pan_by(delta, cx);
        }
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Each step of a pinch arrives as the fraction to grow by.
        let factor = (1.0 + event.delta).max(0.1);
        self.zoom_by(factor, Some(event.position), cx);
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
        self.last_pressed_node = None;
        self.focus_handle.focus(window, cx);

        // Shift-dragging over empty canvas selects with a rectangle, adding to
        // what is already selected.
        if event.modifiers.shift {
            self.interaction = Interaction::Selecting {
                start: event.position,
                at: event.position,
            };
            cx.notify();
            return;
        }

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
                self.pan_by(delta, cx);
            }
            Interaction::DraggingNode { id, grab } => {
                let id = id.clone();
                let grab = *grab;
                let canvas = self.to_canvas(event.position);
                let position = Position {
                    x: snap(canvas.x - grab.x),
                    y: snap(canvas.y - grab.y),
                };
                self.next_undo_group = Some(UndoGroup::Move(id.clone()));
                let path = self.focus.child(id);
                self.edit_checked(move |graph| graph.move_node_at(&path, position), cx);
            }
            Interaction::Connecting { at, .. } | Interaction::Selecting { at, .. } => {
                *at = event.position;
                cx.notify();
            }
            Interaction::DraggingGroup { .. } => self.drag_group_to(event.position, cx),
        }
    }

    fn handle_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Interaction::Selecting { start, .. } = &self.interaction {
            let start = *start;
            self.interaction = Interaction::None;
            self.finish_marquee(start, event.position, window, cx);
        } else if let Interaction::DraggingGroup { anchor, moved, .. } = &self.interaction {
            let (anchor, moved) = (anchor.clone(), *moved);
            self.interaction = Interaction::None;
            if !moved {
                self.set_selection(Some(Selection::Node(anchor)), window, cx);
            }
        }
        if let Interaction::Connecting { from, .. } = &self.interaction {
            let from = from.clone();
            let target = self.node_at(event.position, cx).filter(|to| *to != from);
            if target.is_some() && self.is_running(cx) {
                self.report("Stop the run before connecting steps.".to_string(), cx);
            } else if let Some(to) = target {
                let (from_id, to_id) = (from.clone(), to.clone());
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
                    self.select_new_loop(&from_id, &to_id, window, cx);
                }
            }
        }
        self.interaction = Interaction::None;
        // The gesture is over; the next drag is a separate change.
        self.undo_group = None;
        cx.notify();
    }

    /// A connection drawn back round a loop is selected, so the inspector
    /// shows its Loop section, and the user is told when the plan could now
    /// never leave the loop.
    fn select_new_loop(
        &mut self,
        from: &NodeId,
        to: &NodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((edge, endless)) = self.graph(cx).and_then(|graph| {
            let edge = graph
                .edges
                .iter()
                .rev()
                .find(|edge| &edge.from == from && &edge.to == to)
                .filter(|edge| graph.is_loop_edge(edge))?;
            let endless = graph
                .problems()
                .iter()
                .any(|problem| matches!(problem, GraphProblem::EndlessLoop(_)));
            Some((edge.id.clone(), endless))
        }) else {
            return;
        };
        self.set_selection(Some(Selection::Edge(edge)), window, cx);
        if endless {
            self.show_toast(
                "That connection makes a loop the plan could never leave. Give it a condition \
                 or a repeat limit."
                    .to_string(),
                false,
                cx,
            );
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let canvas_focused = self.focus_handle.is_focused(window);
        let modifiers = event.keystroke.modifiers;
        // Zoom keys go without Ctrl or Cmd, which with these keys already
        // resize the fonts everywhere in the workspace.
        let zoom_key = canvas_focused && !modifiers.secondary() && !modifiers.alt;
        match event.keystroke.key.as_str() {
            "z" | "Z" if canvas_focused && modifiers.secondary() => {
                let direction = if modifiers.shift {
                    HistoryDirection::Redo
                } else {
                    HistoryDirection::Undo
                };
                self.step_history(direction, window, cx);
            }
            "y" if canvas_focused && modifiers.control && !cfg!(target_os = "macos") => {
                self.step_history(HistoryDirection::Redo, window, cx);
            }
            "d" if canvas_focused && modifiers.secondary() && !modifiers.shift => {
                self.duplicate_selection(window, cx);
            }
            "a" if canvas_focused && modifiers.secondary() && !modifiers.shift => {
                self.select_all_steps(window, cx);
            }
            "delete" | "backspace" if canvas_focused && self.has_bulk_selection() => {
                self.delete_bulk_selection(window, cx);
            }
            "home" if canvas_focused => self.select_end_step(false, window, cx),
            "end" if canvas_focused => self.select_end_step(true, window, cx),
            "delete" | "backspace" if canvas_focused => self.delete_selection(window, cx),
            "down" | "right" if canvas_focused => self.select_adjacent_step(true, window, cx),
            "up" | "left" if canvas_focused => self.select_adjacent_step(false, window, cx),
            "=" | "+" if zoom_key => self.zoom_by(ZOOM_STEP, None, cx),
            "-" if zoom_key => self.zoom_by(1.0 / ZOOM_STEP, None, cx),
            "0" if zoom_key => self.set_zoom(1.0, None, cx),
            "1" | "!" if zoom_key && modifiers.shift => self.zoom_to_fit(cx),
            "enter" if canvas_focused => {
                if self.selection.is_some() || self.has_bulk_selection() {
                    self.open_inspector_drawer(window, cx);
                }
            }
            // Escape closes the innermost transient surface before changing
            // navigation state, without ever leaving Architect mode.
            "escape" => {
                // A rectangle being dragged out is cancelled on its own, keeping
                // what was selected before it.
                let marquee = matches!(self.interaction, Interaction::Selecting { .. });
                self.interaction = Interaction::None;
                if marquee {
                    cx.notify();
                } else if self.plan_conversation_open {
                    self.close_plan_conversation(window, cx);
                } else if self.inspector_drawer_open {
                    self.close_inspector_drawer(window, cx);
                } else if self.outline_drawer_open {
                    self.close_outline_drawer(window, cx);
                } else if self.selection.is_some() || self.has_bulk_selection() {
                    self.set_selection(None, window, cx);
                } else {
                    self.drill_out(window, cx);
                }
            }
            _ => {}
        }
    }
}

/// A copy starts as a fresh draft: it has not been settled, argued out in a
/// conversation, or run, whatever the original has been through.
fn reset_for_copy(node: &mut ArchitectNode) {
    node.locked = false;
    node.chat = None;
    node.result = None;
    if let Some(subplan) = node.subplan.as_deref_mut() {
        for child in &mut subplan.nodes {
            reset_for_copy(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::Path, rc::Rc};

    use acp_thread::AgentConnection as _;
    use fs::Fs as _;
    use gpui::{Modifiers, MouseMoveEvent, Render, Task, TestAppContext, VisualTestContext, size};
    use project::{FakeFs, Project};
    use serde_json::json;
    use ui::prelude::*;
    use util::path_list::PathList;
    use workspace::{
        Workspace,
        dock::PanelButtons,
        item::{Item, test::TestItem},
    };

    use super::*;
    use crate::conversation_view::tests::init_test;

    struct ArchitectIntegrationRoot {
        workspace: Entity<Workspace>,
        architect: Rc<RefCell<Option<Entity<ArchitectPane>>>>,
    }

    impl Render for ArchitectIntegrationRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            if let Some(architect) = self.architect.borrow().clone() {
                div().size_full().child(architect).into_any_element()
            } else {
                div()
                    .size_full()
                    .child(self.workspace.clone())
                    .into_any_element()
            }
        }
    }

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

        let direct_architect = Rc::new(RefCell::new(None));
        let direct_architect_for_root = direct_architect.clone();
        let (test_root, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::test_new(project.clone(), window, cx));
            ArchitectIntegrationRoot {
                workspace,
                architect: direct_architect_for_root,
            }
        });
        let workspace = test_root.read_with(cx, |root, _cx| root.workspace.clone());
        let (weak_workspace, async_window_context) =
            cx.update(|window, cx| (workspace.downgrade(), window.to_async(cx)));
        let project_panel =
            ProjectPanel::load(weak_workspace.clone(), async_window_context.clone())
                .await
                .expect("the native Project panel should load");
        let git_panel = GitPanel::load(weak_workspace.clone(), async_window_context.clone())
            .await
            .expect("the native Git panel should load");
        let terminal_panel = TerminalPanel::load(weak_workspace, async_window_context)
            .await
            .expect("the native Terminal panel should load");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(project_panel, window, cx);
            workspace.add_panel(git_panel, window, cx);
            workspace.add_panel(terminal_panel, window, cx);
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

            pane.step_history(HistoryDirection::Undo, window, cx);
            let graph = pane.root_graph(cx).unwrap();
            assert!(
                graph.node(&target).is_some(),
                "undo restores a deleted step"
            );
            assert_eq!(graph.edges.len(), 1, "undo restores the step's connections");
            pane.step_history(HistoryDirection::Redo, window, cx);
            let graph = pane.root_graph(cx).unwrap();
            assert!(graph.node(&target).is_none(), "redo deletes the step again");
            assert!(graph.edges.is_empty());

            pane.interaction = Interaction::DraggingNode {
                id: parent.clone(),
                grab: point(0.0, 0.0),
            };
            for position in [point(px(160.0), px(40.0)), point(px(240.0), px(80.0))] {
                pane.handle_mouse_move(
                    &MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    },
                    window,
                    cx,
                );
            }
            pane.handle_mouse_up(
                &MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(240.0), px(80.0)),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                },
                window,
                cx,
            );
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert_eq!(
                pane.root_graph(cx)
                    .and_then(|graph| graph.node(&parent))
                    .and_then(|node| node.position),
                Some(Position { x: 80.0, y: 40.0 }),
                "one drag undoes as one change"
            );

            // The agent editing the plan makes the recorded snapshots stale.
            let unchanged = pane.root_graph(cx).cloned().unwrap();
            let changed_path = NodePath::root(parent.clone());
            pane.thread.update(cx, |thread, cx| {
                thread.update_architect_graph(
                    move |graph| {
                        if let Some(node) = graph.node_at_mut(&changed_path) {
                            node.intent = "Changed by the agent".to_string();
                        }
                    },
                    cx,
                )
            });
            pane.step_history(HistoryDirection::Redo, window, cx);
            assert_eq!(
                pane.root_graph(cx)
                    .and_then(|graph| graph.node(&parent))
                    .and_then(|node| node.position),
                Some(Position { x: 80.0, y: 40.0 }),
                "redo must not overwrite a plan changed elsewhere"
            );
            assert!(pane.undo_stack.is_empty() && pane.redo_stack.is_empty());
            pane.thread.update(cx, |thread, cx| {
                thread.set_architect_graph(Some(unchanged), cx)
            });

            pane.selection = Some(Selection::Node(parent.clone()));
            pane.duplicate_selection(window, cx);
            let copy = pane
                .root_graph(cx)
                .and_then(|graph| graph.nodes.last())
                .cloned()
                .expect("duplicating a step should add a copy of it");
            assert_eq!(copy.title, "Parent (copy)");
            assert!(!copy.locked, "a copy starts as a draft");
            assert!(copy.has_subplan(), "a copy keeps its nested plan");
            assert_eq!(pane.selection, Some(Selection::Node(copy.id.clone())));
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert!(
                pane.root_graph(cx).unwrap().node(&copy.id).is_none(),
                "undo removes the copy"
            );
            assert_eq!(
                pane.selection, None,
                "a selection that was undone away is dropped"
            );

            let child_locked = |pane: &ArchitectPane, cx: &Context<ArchitectPane>| {
                pane.root_graph(cx)
                    .and_then(|root| root.graph_at(&NodePath::root(parent.clone())))
                    .and_then(|nested| nested.node(&child))
                    .is_some_and(|node| node.locked)
            };
            pane.toggle_lock(parent.clone(), window, cx);
            assert!(
                pane.root_graph(cx).unwrap().node(&parent).unwrap().locked,
                "a step holding an open plan can be locked"
            );
            assert!(child_locked(pane, cx), "locking it locks the plan inside");
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert!(!pane.root_graph(cx).unwrap().node(&parent).unwrap().locked);
            assert!(!child_locked(pane, cx), "one undo reopens both");
            pane.drill_into(parent.clone(), window, cx);
            assert_eq!(pane.focus, NodePath::root(parent.clone()));
            pane.toggle_lock(child.clone(), window, cx);
            assert!(pane.graph(cx).unwrap().node(&child).unwrap().locked);
            assert!(pane.drill_out(window, cx));
            assert_eq!(pane.selection, Some(Selection::Node(parent.clone())));
            pane.toggle_lock(parent.clone(), window, cx);
            assert!(pane.root_graph(cx).unwrap().node(&parent).unwrap().locked);

            // Several steps at once: a rectangle gathers them, and locking,
            // moving and deleting act on all of them as one change each.
            let mut drafts = Vec::new();
            for _ in 0..2 {
                pane.add_step(window, cx);
                let Some(Selection::Node(draft)) = pane.selection.clone() else {
                    panic!("a new step should be selected");
                };
                drafts.push(draft);
            }
            pane.set_selection(None, window, cx);
            pane.interaction = Interaction::Selecting {
                start: point(px(-600.0), px(-600.0)),
                at: point(px(-600.0), px(-600.0)),
            };
            pane.handle_mouse_up(
                &MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(600.0), px(600.0)),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                },
                window,
                cx,
            );
            assert_eq!(
                pane.bulk.len(),
                3,
                "the rectangle selects every step it touches"
            );
            assert_eq!(pane.selection, None);

            pane.lock_bulk_selection(true, cx);
            assert!(
                drafts
                    .iter()
                    .all(|id| pane.root_graph(cx).unwrap().node(id).unwrap().locked)
            );
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert!(
                drafts
                    .iter()
                    .all(|id| !pane.root_graph(cx).unwrap().node(id).unwrap().locked),
                "locking several steps undoes as one change"
            );
            assert_eq!(pane.bulk.len(), 3, "undo keeps the steps selected");

            pane.lock_all_steps(window, cx);
            assert!(
                pane.root_graph(cx).unwrap().is_fully_locked_deeply(),
                "Lock All settles every step at every depth"
            );
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert!(
                drafts
                    .iter()
                    .all(|id| !pane.root_graph(cx).unwrap().node(id).unwrap().locked),
                "Lock All undoes as one change"
            );

            let positions = |pane: &ArchitectPane, cx: &Context<ArchitectPane>| {
                pane.bulk
                    .iter()
                    .map(|id| pane.root_graph(cx).unwrap().node(id).unwrap().position)
                    .collect::<Vec<_>>()
            };
            let before = positions(pane, cx);
            pane.start_group_drag(drafts[0].clone(), point(px(0.0), px(0.0)), cx);
            for position in [point(px(16.0), px(8.0)), point(px(40.0), px(16.0))] {
                pane.handle_mouse_move(
                    &MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    },
                    window,
                    cx,
                );
            }
            pane.handle_mouse_up(
                &MouseUpEvent {
                    button: MouseButton::Left,
                    position: point(px(40.0), px(16.0)),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                },
                window,
                cx,
            );
            let moved: Vec<_> = before
                .iter()
                .map(|position| {
                    position.map(|position| Position {
                        x: position.x + 40.0,
                        y: position.y + 16.0,
                    })
                })
                .collect();
            assert_eq!(
                positions(pane, cx),
                moved,
                "the steps move together, settled or not"
            );
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert_eq!(positions(pane, cx), before, "one drag undoes as one change");

            pane.delete_bulk_selection(window, cx);
            let graph = pane.root_graph(cx).unwrap();
            assert!(drafts.iter().all(|id| graph.node(id).is_none()));
            assert!(graph.node(&parent).is_some(), "a locked step is kept");
            assert_eq!(
                pane.selection,
                Some(Selection::Node(parent.clone())),
                "the step that was kept stays selected"
            );
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert!(
                drafts
                    .iter()
                    .all(|id| pane.root_graph(cx).unwrap().node(id).is_some()),
                "deleting several steps undoes as one change"
            );
            pane.set_bulk_selection(drafts.clone(), window, cx);
            pane.delete_bulk_selection(window, cx);
            assert!(pane.bulk.is_empty() && pane.selection.is_none());

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
            assert_eq!(pane.running_nodes(cx), vec![parent.clone()]);
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
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.activate_item(&architect, true, true, window, cx),
                "the retained Architect item should be activatable before drawing"
            );
        });
        architect.update_in(cx, |architect, window, cx| {
            architect.selection = None;
            architect.pan = point(px(0.0), px(0.0));
            architect.focus_handle.focus(window, cx);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        architect.read_with(cx, |architect, _cx| {
            assert_eq!(architect.mode(), ArchitectWorkspaceMode::Architect);
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_item_as::<ArchitectPane>(cx),
                Some(architect.clone()),
                "the Architect surface should own the active workspace item"
            );
            assert!(
                !architect.read(cx).show_in_tab_bar(cx),
                "Architect must never leak into the native Code tab strip"
            );
            assert!(
                !architect.read(cx).allows_workspace_docks(cx),
                "Architect must reject native dock actions before they mutate layout"
            );
            let status_bar = workspace.status_bar().read(cx);
            assert!(
                status_bar.shows_item::<rendering::ArchitectStatusItem>(cx),
                "Architect View should keep its own status item"
            );
            assert!(
                !status_bar.shows_item::<PanelButtons>(cx),
                "Architect View should hide the panel buttons it cannot use"
            );
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<ProjectPanel>(window, cx);
            assert!(
                workspace
                    .all_docks()
                    .into_iter()
                    .all(|dock| !dock.read(cx).is_open()),
                "Architect should reject direct panel opens without a deferred close"
            );
            assert!(
                !workspace.toggle_panel_focus::<ProjectPanel>(window, cx),
                "Architect should reject panel focus actions"
            );
            workspace.toggle_dock(DockPosition::Left, window, cx);
            workspace.reveal_panel::<ProjectPanel>(window, cx);
            workspace.left_dock().update(cx, |dock, cx| {
                dock.set_open(true, window, cx);
            });
            assert!(
                workspace
                    .all_docks()
                    .into_iter()
                    .all(|dock| !dock.read(cx).is_open()),
                "Architect should reject actions and direct dock-open attempts"
            );
        });

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
                workspace
                    .status_bar()
                    .read(cx)
                    .shows_item::<PanelButtons>(cx),
                "Editor View should show the panel buttons again"
            );
            assert!(
                code_focus.is_focused(window),
                "Code should restore the native item's focus"
            );
            assert!(
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .filter(|item| item.show_in_tab_bar(cx))
                    .all(|item| item.item_id() != architect.entity_id()),
                "the retained Architect entity must stay out of Code tabs"
            );
        });
        architect.read_with(cx, |architect, cx| {
            assert!(
                architect.allows_workspace_docks(cx),
                "Code should re-enable native dock actions"
            );
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

        // Opening a file while Architect View is showing switches to Editor
        // View with that file in front.
        let opened = workspace.update_in(cx, |workspace, window, cx| {
            let opened = cx.new(TestItem::new);
            workspace.add_item_to_active_pane(Box::new(opened.clone()), None, true, window, cx);
            opened
        });
        cx.run_until_parked();
        let opened_focus = opened.read_with(cx, |opened, cx| opened.focus_handle(cx));
        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(
                workspace.active_item_as::<TestItem>(cx),
                Some(opened.clone()),
                "the opened item should stay in front in Editor View"
            );
            assert!(
                opened_focus.is_focused(window),
                "the opened item should keep focus in Editor View"
            );
            assert!(
                !workspace.active_pane().read(cx).is_zoomed(),
                "Editor View should not leave the pane zoomed"
            );
            assert_eq!(
                workspace.item_of_type::<ArchitectPane>(cx),
                None,
                "Editor View should detach Architect from the pane"
            );
        });
        architect.read_with(cx, |architect, cx| {
            assert_eq!(architect.mode(), ArchitectWorkspaceMode::Code);
            assert!(
                architect.allows_workspace_docks(cx),
                "Editor View should re-enable the panels"
            );
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.remove_item(opened.entity_id(), false, false, window, cx);
            });
            ArchitectPane::open(thread.clone(), workspace, window, cx);
        });
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert_eq!(
                architect.mode(),
                ArchitectWorkspaceMode::Architect,
                "returning to Architect View must not bounce back to Editor View"
            );
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
        workspace.read_with(cx, |workspace, cx| {
            let left_dock = workspace.dock_at_position(DockPosition::Left).read(cx);
            assert!(left_dock.is_open(), "Code should open the native left dock");
            assert_eq!(
                left_dock.active_panel().map(|panel| panel.panel_id()),
                project_panel.as_ref().map(|panel| panel.entity_id()),
                "Project should be the active native left-dock panel"
            );
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let left_dock = workspace.dock_at_position(DockPosition::Left).read(cx);
            assert!(
                left_dock.is_open(),
                "Code should keep the native left dock open"
            );
            assert_eq!(
                left_dock.active_panel().map(|panel| panel.panel_id()),
                git_panel.as_ref().map(|panel| panel.entity_id()),
                "Git should replace Project as the active native left-dock tab"
            );
        });
        // Rendering the Agent panel in Code runs the restoration check while
        // the panel is being updated, so the check must not read the panel.
        agent_panel_entity.update_in(cx, |agent_panel, window, cx| {
            agent_panel.restore_architect_if_needed(thread.clone(), true, window, cx);
        });
        cx.run_until_parked();
        agent_panel_entity.update_in(cx, |agent_panel, window, cx| {
            agent_panel.defer_open_architect_workspace(window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_item_as::<ArchitectPane>(cx),
                Some(architect.clone()),
                "the Agent-panel button should defer Architect activation without re-entering the panel"
            );
            assert!(
                workspace
                    .all_docks()
                    .into_iter()
                    .all(|dock| !dock.read(cx).is_open()),
                "deferred Architect activation should hide every Code dock"
            );
        });
        workspace.update_in(cx, |workspace, window, cx| {
            ArchitectPane::activate_code(workspace, window, cx);
            let left_dock = workspace.dock_at_position(DockPosition::Left).read(cx);
            assert_eq!(
                left_dock.active_panel().map(|panel| panel.panel_id()),
                git_panel.as_ref().map(|panel| panel.entity_id()),
                "Code should restore Git as the active native left-dock tab"
            );
            ArchitectPane::open(thread.clone(), workspace, window, cx);

            workspace.active_pane().update(cx, |pane, cx| {
                pane.remove_item(code_item.entity_id(), false, false, window, cx);
            });
            ArchitectPane::activate_code(workspace, window, cx);
            assert_eq!(
                workspace.item_of_type::<ArchitectPane>(cx),
                None,
                "Code should detach Architect from the native pane"
            );
            assert!(
                workspace.active_item(cx).is_none(),
                "Code without editors should restore a genuinely empty pane"
            );
            assert_eq!(
                agent_panel_entity.read(cx).retained_architect_pane(),
                Some(architect.clone()),
                "the Agent panel should retain Architect state outside the Code pane"
            );
            assert!(
                workspace.active_pane().read(cx).items().next().is_none(),
                "an empty Code workspace must not contain an Architect tab"
            );
            workspace.add_item_to_active_pane(Box::new(code_item.clone()), None, true, window, cx);
            ArchitectPane::open(thread.clone(), workspace, window, cx);
        });

        *direct_architect.borrow_mut() = Some(architect.clone());
        test_root.update(cx, |_root, cx| cx.notify());
        cx.run_until_parked();

        let base_graph = thread
            .read_with(cx, |thread, _| thread.architect_graph().cloned())
            .expect("the nested test graph should remain available");
        cx.simulate_resize(size(px(1500.0), px(900.0)));
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(Some(Selection::Node(parent.clone())), window, cx);
        });
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert!(
                architect.inspector.is_some(),
                "selecting a step should create the contextual inspector"
            );
        });
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
        architect.read_with(cx, |architect, _| {
            assert_eq!(architect.inspector_tab, InspectorTab::Conversation);
        });

        architect.update_in(cx, |architect, window, cx| {
            architect.open_plan_conversation(window, cx);
        });
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert!(
                architect.plan_conversation_open,
                "the root plan conversation should have an explicit open state"
            );
        });
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
        architect.read_with(cx, |architect, _| {
            assert!(
                architect.edge_inspector.is_some(),
                "selecting a connection should create its inspector"
            );
        });
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
            let visit = thread.note_architect_run_position(
                NodePath::root(parent.clone()),
                "Parent".into(),
                1,
                1,
                cx,
            );
            thread.finish_architect_run_step(
                visit,
                Some("Validated the selected step output".into()),
                cx,
            );
        });
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            let run = thread
                .architect_run()
                .expect("the active Architect run should remain available");
            assert!(run.is_running());
            assert_eq!(run.current.as_ref(), Some(&NodePath::root(parent.clone())));
            assert_eq!(run.current_title.as_ref(), "Parent");
            assert_eq!(run.step_number, 1);
            assert_eq!(
                run.remote_workflow_url(),
                Some("https://example.com/workflows/architect-run")
            );
            let step = run
                .history()
                .first()
                .expect("the finished run step should remain in history");
            assert_eq!(step.path, NodePath::root(parent.clone()));
            assert_eq!(step.title.as_ref(), "Parent");
            assert_eq!(step.attempt, 1);
            assert!(!step.is_running());
            assert_eq!(
                step.summary.as_deref(),
                Some("Validated the selected step output")
            );
        });
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
            let run = thread
                .architect_run()
                .expect("the completed local run should remain available");
            assert_eq!(
                run.outcome.as_ref(),
                Some(&architect::RunOutcome::Completed)
            );
            assert_eq!(run.remote_workflow_url(), None);
        });
        cx.run_until_parked();

        cx.simulate_resize(size(px(1200.0), px(800.0)));
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert!(architect.inspector.is_some());
        });

        cx.simulate_resize(size(px(900.0), px(760.0)));
        architect.update_in(cx, |architect, window, cx| {
            architect.focus_handle.focus(window, cx);
            architect.open_inspector_drawer(window, cx);
        });
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert!(architect.inspector_drawer_open);
        });
        architect.update_in(cx, |architect, window, cx| {
            architect.close_inspector_drawer(window, cx);
            assert!(!architect.inspector_drawer_open);
            assert!(architect.focus_handle.is_focused(window));
        });

        cx.simulate_resize(size(px(680.0), px(700.0)));
        architect.update_in(cx, |architect, window, cx| {
            architect.focus_handle.focus(window, cx);
            architect.open_outline_drawer(true, window, cx);
            assert!(architect.search_editor.focus_handle(cx).is_focused(window));
        });
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert!(architect.outline_drawer_open);
        });
        architect.update_in(cx, |architect, window, cx| {
            architect.close_outline_drawer(window, cx);
            assert!(!architect.outline_drawer_open);
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
            let expected_node_count = graph.nodes.len();
            thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));
            architect.update_in(cx, |architect, window, cx| {
                architect.set_selection(None, window, cx);
                architect.focus = NodePath::default();
                architect.pan = point(px(0.0), px(0.0));
                architect.zoom = 1.0;
                cx.notify();
            });
            cx.run_until_parked();
            thread.read_with(cx, |thread, _| {
                assert_eq!(
                    thread.architect_graph().map(|graph| graph.nodes.len()),
                    Some(expected_node_count)
                );
            });
            architect.read_with(cx, |architect, _| {
                assert!(architect.selection.is_none());
                assert!(architect.focus.is_empty());
                assert_eq!(architect.pan, point(px(0.0), px(0.0)));
                assert_eq!(architect.zoom, 1.0);
            });
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
        thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.architect_graph().map(|graph| graph.nodes.len()),
                Some(200)
            );
        });
        architect.update(cx, |architect, cx| {
            architect.pan = point(px(-1000.0), px(0.0));
            cx.notify();
        });
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert_eq!(architect.pan, point(px(-1000.0), px(0.0)));
            assert_eq!(architect.zoom, 1.0);
        });
        architect.update(cx, |architect, cx| {
            architect.zoom = 0.5;
            architect.pan = point(px(-500.0), px(0.0));
            cx.notify();
        });
        cx.run_until_parked();
        architect.read_with(cx, |architect, _| {
            assert_eq!(architect.pan, point(px(-500.0), px(0.0)));
            assert_eq!(architect.zoom, 0.5);
        });

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
        thread.read_with(cx, |thread, _| {
            let run = thread
                .architect_run()
                .expect("the large-plan run should remain available");
            assert!(run.is_running());
            assert_eq!(
                run.current.as_ref(),
                Some(&NodePath::root(NodeId::from("large-1")))
            );
            assert_eq!(run.current_title.as_ref(), "Large plan step 1");
            assert_eq!(run.step_number, 1);
            let step = run
                .history()
                .first()
                .expect("the large-plan run should record its active step");
            assert!(step.is_running());
            assert_eq!(step.path, NodePath::root(NodeId::from("large-1")));
        });
        thread.update(cx, |thread, cx| thread.stop_architect_run(cx));
        thread.read_with(cx, |thread, _| {
            let run = thread
                .architect_run()
                .expect("the stopped run should retain its final state");
            assert_eq!(run.current, None);
            assert_eq!(
                run.outcome.as_ref(),
                Some(&architect::RunOutcome::Cancelled)
            );
        });

        thread.update(cx, |thread, cx| thread.set_architect_graph(None, cx));
        architect.update_in(cx, |architect, window, cx| {
            architect.set_selection(None, window, cx);
            architect.focus_handle.focus(window, cx);
        });
        cx.run_until_parked();
        thread.read_with(cx, |thread, _| {
            assert!(thread.architect_graph().is_none());
        });
        architect.read_with(cx, |architect, _| {
            assert!(architect.selection.is_none());
        });
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
        assert_eq!(
            reopened_architect.entity_id(),
            architect.entity_id(),
            "reopening Architect should reattach the retained workspace surface"
        );
    }

    /// A plan thread of its own, for tests that only need the canvas.
    struct TestPlan {
        project: Entity<Project>,
        thread: Entity<Thread>,
        _connection: Rc<agent::NativeAgentConnection>,
        _session: Entity<acp_thread::AcpThread>,
    }

    async fn test_plan(cx: &mut TestAppContext) -> TestPlan {
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
        let session = cx
            .update(|cx| {
                connection.clone().new_session(
                    project.clone(),
                    PathList::new(&[Path::new("/a")]),
                    cx,
                )
            })
            .await
            .expect("the Architect test session should open");
        let session_id = session.read_with(cx, |thread, _| thread.session_id().clone());
        let thread = cx
            .update(|cx| connection.thread(&session_id, cx))
            .expect("the native thread should exist");
        TestPlan {
            project,
            thread,
            _connection: connection,
            _session: session,
        }
    }

    #[gpui::test]
    async fn canvas_zoom_glides_and_outline_clicks_follow_list_conventions(
        cx: &mut TestAppContext,
    ) {
        fn near(a: Position, b: Position) -> bool {
            (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01
        }

        let plan = test_plan(cx).await;
        let project = plan.project.clone();
        let thread = plan.thread;
        let mut graph = ArchitectGraph::default();
        for (index, id) in ["a", "b", "c", "d"].into_iter().enumerate() {
            let mut node = ArchitectNode::new(id, format!("Step {index}"));
            node.position = Some(Position {
                x: index as f32 * 320.0,
                y: 0.0,
            });
            graph.add_node(node);
        }
        graph.connect("a", "b");
        graph.connect("b", "c");
        graph.connect("c", "d");
        thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));

        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
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
        // The canvas reports its bounds when it is painted. Setting them here
        // lets the view be driven without drawing it.
        let viewport = Bounds::new(point(px(100.0), px(50.0)), size(px(1000.0), px(800.0)));
        pane.update(cx, |pane, _| pane.viewport.set(Some(viewport)));

        // Zooming at the cursor glides there, keeping what is under the cursor
        // under it on every frame.
        let cursor = point(px(340.0), px(260.0));
        let under_cursor = pane.update(cx, |pane, cx| {
            let under_cursor = pane.to_canvas(cursor);
            pane.zoom_by(1.5, Some(cursor), cx);
            assert_eq!(pane.zoom, 1.0, "a zoom starts from what is on screen");
            under_cursor
        });
        cx.executor().advance_clock(CAMERA_ANIMATION / 2);
        cx.run_until_parked();
        let halfway = pane.read_with(cx, |pane, _| {
            assert!(
                pane.zoom > 1.0 && pane.zoom < 1.5,
                "halfway through, the zoom is between its ends, not {}",
                pane.zoom
            );
            assert!(
                near(pane.to_canvas(cursor), under_cursor),
                "the point under the cursor stays under it throughout"
            );
            pane.zoom
        });

        // Zooming again mid-glide carries on from what is on screen, towards a
        // target that builds on the first.
        pane.update(cx, |pane, cx| {
            pane.zoom_by(1.2, Some(cursor), cx);
            assert_eq!(
                pane.zoom, halfway,
                "a new zoom mid-glide carries on from what is on screen"
            );
        });
        cx.executor().advance_clock(CAMERA_ANIMATION + CAMERA_FRAME);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert!(
                (pane.zoom - 1.8).abs() < 1e-4,
                "the second zoom should build on where the first was heading, not {}",
                pane.zoom
            );
            assert!(near(pane.to_canvas(cursor), under_cursor));
            assert!(
                pane.camera_animation.is_none(),
                "the glide stops once it lands"
            );
        });

        // The buttons and keys zoom about the middle of the view, and no
        // further than the limit.
        let centre = viewport.center();
        let under_centre = pane.update(cx, |pane, cx| {
            let under_centre = pane.to_canvas(centre);
            pane.zoom_by(10.0, None, cx);
            under_centre
        });
        cx.executor().advance_clock(CAMERA_ANIMATION + CAMERA_FRAME);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.zoom, MAX_ZOOM);
            assert!(near(pane.to_canvas(centre), under_centre));
        });

        // Fit glides too, and lands framing the whole plan: four steps 320
        // apart and 280 wide, in a view 1000 wide less its margin.
        pane.update(cx, |pane, cx| {
            pane.zoom_to_fit(cx);
            assert_eq!(pane.zoom, MAX_ZOOM, "fitting glides rather than jumps");
        });
        cx.executor().advance_clock(CAMERA_ANIMATION + CAMERA_FRAME);
        cx.run_until_parked();
        let fitted = (1000.0 - 64.0) / 1240.0;
        pane.read_with(cx, |pane, _| {
            assert!((pane.zoom - fitted).abs() < 1e-4);
            assert!((f32::from(pane.pan.x) + 480.0 * fitted).abs() < 0.01);
            assert!(f32::from(pane.pan.y).abs() < 0.01);
        });

        // A drag mid-glide moves the view at once and carries the glide along.
        pane.update(cx, |pane, cx| {
            pane.set_zoom(1.0, None, cx);
            pane.pan_by(point(px(30.0), px(-10.0)), cx);
        });
        cx.executor().advance_clock(CAMERA_ANIMATION + CAMERA_FRAME);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.zoom, 1.0);
            assert!((f32::from(pane.pan.x) + 450.0).abs() < 0.01);
            assert!((f32::from(pane.pan.y) + 10.0).abs() < 0.01);
        });

        let listed = pane.update(cx, |pane, cx| {
            pane.graph(cx)
                .map(ArchitectGraph::execution_order)
                .unwrap_or_default()
        });
        assert_eq!(listed.len(), 4);
        let [a, b, c, d] = [0, 1, 2, 3].map(|index| listed[index].clone());
        let ctrl = Modifiers::secondary_key();
        let shift = Modifiers::shift();
        let ctrl_shift = Modifiers {
            shift: true,
            ..Modifiers::secondary_key()
        };
        pane.update_in(cx, |pane, window, cx| {
            pane.click_outline_step(b.clone(), &listed, Modifiers::none(), window, cx);
            assert_eq!(pane.selection, Some(Selection::Node(b.clone())));

            pane.click_outline_step(d.clone(), &listed, ctrl, window, cx);
            assert_eq!(
                pane.bulk,
                vec![b.clone(), d.clone()],
                "Ctrl-click adds a step to the selection"
            );
            assert_eq!(pane.selection, None);
            pane.click_outline_step(b.clone(), &listed, ctrl, window, cx);
            assert_eq!(
                pane.selection,
                Some(Selection::Node(d.clone())),
                "Ctrl-click takes a step back out, keeping the rest"
            );
            assert!(pane.bulk.is_empty());

            pane.click_outline_step(c.clone(), &listed, shift, window, cx);
            assert_eq!(
                pane.bulk,
                vec![b.clone(), c.clone()],
                "Shift-click reaches from the row last clicked without Shift"
            );

            pane.click_outline_step(a.clone(), &listed, Modifiers::none(), window, cx);
            pane.click_outline_step(c.clone(), &listed, shift, window, cx);
            assert_eq!(pane.bulk, vec![a.clone(), b.clone(), c.clone()]);
            assert_eq!(pane.selection, None);
            pane.click_outline_step(b.clone(), &listed, shift, window, cx);
            assert_eq!(
                pane.bulk,
                vec![a.clone(), b.clone()],
                "another Shift-click reaches from the same row"
            );
            pane.click_outline_step(d.clone(), &listed, ctrl_shift, window, cx);
            assert_eq!(
                pane.bulk, listed,
                "Ctrl and Shift together add the range to the selection"
            );

            pane.click_outline_step(d.clone(), &listed, Modifiers::none(), window, cx);
            pane.click_outline_step(b.clone(), &listed, shift, window, cx);
            assert_eq!(
                pane.bulk,
                vec![b.clone(), c.clone(), d.clone()],
                "a range upwards is still in outline order"
            );

            pane.set_selection(Some(Selection::Node(c.clone())), window, cx);
            pane.click_outline_step(a.clone(), &listed, shift, window, cx);
            assert_eq!(
                pane.bulk,
                vec![a.clone(), b.clone(), c.clone()],
                "a step selected on the canvas anchors the range"
            );

            pane.set_selection(None, window, cx);
            pane.click_outline_step(b.clone(), &listed, shift, window, cx);
            assert_eq!(
                pane.selection,
                Some(Selection::Node(b.clone())),
                "with nothing to reach from, Shift-click selects the step alone"
            );
        });
    }

    #[gpui::test]
    async fn step_model_inspector_edits_use_registry_ids_and_recheck_guards(
        cx: &mut TestAppContext,
    ) {
        fn step_model(
            pane: &ArchitectPane,
            path: &NodePath,
            cx: &Context<ArchitectPane>,
        ) -> Option<architect::StepModel> {
            pane.root_graph(cx)
                .and_then(|graph| graph.node_at(path))
                .and_then(|node| node.model.clone())
        }

        let plan = test_plan(cx).await;
        let project = plan.project.clone();
        let thread = plan.thread;
        let mut nested = ArchitectGraph::default();
        nested.add_node(ArchitectNode::new("step", "Nested step"));
        let mut parent = ArchitectNode::new("parent", "Parent");
        parent.subplan = Some(Box::new(nested));
        let mut graph = ArchitectGraph::default();
        graph.add_node(parent);
        graph.add_node(ArchitectNode::new("step", "Root step"));
        thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));
        let path = NodePath::from(vec!["parent".into(), "step".into()]);
        let model = cx.update(|cx| {
            inspector::available_step_models(cx)
                .into_iter()
                .next()
                .expect("the registry should offer the fake provider's model")
                .0
        });
        assert_eq!(model.provider, "fake");
        assert_eq!(model.model, "fake");

        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
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
            pane.drill_into("parent".into(), window, cx);
            pane.set_selection(Some(Selection::Node("step".into())), window, cx);
            assert!(pane.set_step_model(path.clone(), Some(model.clone()), cx));
            assert_eq!(step_model(pane, &path, cx), Some(model.clone()));
            assert!(step_model(pane, &NodePath::root("step".into()), cx).is_none());
            pane.step_history(HistoryDirection::Undo, window, cx);
            assert!(step_model(pane, &path, cx).is_none());
            pane.step_history(HistoryDirection::Redo, window, cx);
            assert_eq!(step_model(pane, &path, cx), Some(model.clone()));

            // A menu opened inside the parent must retain its full path even
            // if navigation changes before its deferred callback is delivered.
            pane.drill_out(window, cx);
            assert!(pane.set_step_model(path.clone(), None, cx));
            assert!(pane.set_step_model(path.clone(), Some(model.clone()), cx));
            pane.set_selection(Some(Selection::Node("parent".into())), window, cx);
            pane.duplicate_selection(window, cx);
            let copy = pane
                .root_graph(cx)
                .and_then(|graph| graph.nodes.last())
                .expect("a copy should be added");
            assert_eq!(
                copy.subplan()
                    .and_then(|graph| graph.node(&"step".into()))
                    .and_then(|node| node.model.as_ref()),
                Some(&model)
            );
        });
        cx.run_until_parked();

        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(
                |graph| {
                    graph
                        .lock_deeply_at(&NodePath::root("parent".into()))
                        .expect("the parent should lock");
                    graph.node_at_mut(&path).expect("child should exist").locked = false;
                },
                cx,
            );
        });
        pane.update_in(cx, |pane, _, cx| {
            assert!(!pane.set_step_model(path.clone(), None, cx));
        });
        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(
                |graph| {
                    graph.unlock_all();
                    graph
                        .set_locked_at(&path, true)
                        .expect("the step should lock");
                },
                cx,
            );
        });
        pane.update_in(cx, |pane, _, cx| {
            assert!(!pane.set_step_model(path.clone(), None, cx));
        });
        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(|graph| graph.unlock_all(), cx);
        });
        pane.update_in(cx, |pane, _, cx| {
            pane.run_starting.set(true);
            assert!(!pane.set_step_model(path.clone(), None, cx));
            pane.run_starting.set(false);
        });
        thread.update(cx, |thread, cx| {
            thread.start_architect_run(path.clone(), "Nested step".into(), Task::ready(()), cx);
        });
        pane.update_in(cx, |pane, _, cx| {
            assert!(!pane.set_step_model(path.clone(), None, cx));
            assert_eq!(step_model(pane, &path, cx), Some(model.clone()));
        });
        thread.update(cx, |thread, cx| {
            thread.finish_architect_run(architect::RunOutcome::Completed, cx);
        });
        cx.update(|_, cx| {
            language_model::LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.unregister_provider(
                    language_model::LanguageModelProviderId(model.provider.clone().into()),
                    cx,
                );
            });
        });
        cx.run_until_parked();
        pane.update_in(cx, |pane, _, cx| {
            assert!(inspector::available_step_models(cx).is_empty());
            assert!(!pane.set_step_model(path.clone(), Some(model.clone()), cx));
            assert!(pane.set_step_model(path.clone(), None, cx));
            assert!(step_model(pane, &path, cx).is_none());
        });
    }

    #[gpui::test]
    async fn inspector_tracks_revised_briefs_without_resetting_local_edits(cx: &mut TestAppContext) {
        let plan = test_plan(cx).await;
        let project = plan.project.clone();
        let thread = plan.thread;
        let path = NodePath::root("step".into());
        let mut node = ArchitectNode::new("step", "Original title");
        node.responsibility = "Original responsibility".into();
        node.intent = "Original goal".into();
        node.capture = "Original capture".into();
        node.rules = vec!["Original rule".into()];
        node.locked = true;
        let mut graph = ArchitectGraph::default();
        graph.add_node(node);
        thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));

        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
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
        let editors = pane.update_in(cx, |pane, window, cx| {
            pane.set_selection(Some(Selection::Node("step".into())), window, cx);
            pane.inspector
                .as_ref()
                .expect("step should be selected")
                .editors()
        });
        cx.run_until_parked();
        let [title, responsibility, goal, capture, new_rule] = editors.clone();

        cx.update(|_, cx| {
            agent::update_architect_step(
                &thread,
                &path,
                Some("Revised goal".into()),
                Some(vec!["Revised rule".into()]),
                Some("Revised capture".into()),
                cx,
            )
            .expect("the coordinator should be able to revise a locked pending step");
        });
        cx.run_until_parked();
        assert_eq!(goal.read_with(cx, |editor, cx| editor.text(cx)), "Revised goal");
        assert_eq!(
            capture.read_with(cx, |editor, cx| editor.text(cx)),
            "Revised capture"
        );
        pane.update_in(cx, |pane, _, cx| {
            let node = pane
                .root_graph(cx)
                .and_then(|graph| graph.node_at(&path))
                .expect("the revised step should exist");
            assert!(node.locked);
            assert_eq!(node.rules, vec!["Revised rule"]);
            assert!(
                pane.undo_stack.is_empty(),
                "synchronizing must not write back to the plan"
            );
            assert_eq!(
                pane.inspector
                    .as_ref()
                    .expect("inspector should remain open")
                    .editors(),
                editors
            );
        });
        for editor in &editors {
            assert!(editor.read_with(cx, |editor, cx| editor.read_only(cx)));
        }

        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(
                |graph| {
                    let node = graph.node_at_mut(&path).expect("step should exist");
                    node.title = "Revised title".into();
                    node.responsibility = "Revised responsibility".into();
                },
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            title.read_with(cx, |editor, cx| editor.text(cx)),
            "Revised title"
        );
        assert_eq!(
            responsibility.read_with(cx, |editor, cx| editor.text(cx)),
            "Revised responsibility"
        );

        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(|graph| graph.unlock_all(), cx);
        });
        cx.run_until_parked();
        pane.update_in(cx, |pane, window, cx| {
            new_rule.update(cx, |editor, cx| {
                editor.set_text("Unsubmitted rule", window, cx);
            });
            capture.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([text::Point::new(0, 2)..text::Point::new(0, 5)]);
                });
            });
            goal.update(cx, |editor, cx| editor.set_text("Local typing", window, cx));
            agent::update_architect_step(
                &thread,
                &path,
                Some("Competing external goal".into()),
                Some(vec!["Latest rule".into()]),
                None,
                cx,
            )
            .expect("an unlocked pending step should be revisable");
            // Flush the source while the local BufferEdited notification is
            // still queued, to exercise the in-flight typing guard directly.
            pane.refresh_inspector(window, cx);
            assert_eq!(goal.read(cx).text(cx), "Local typing");
            pane.set_selection(Some(Selection::Node("step".into())), window, cx);
            pane.set_selection(Some(Selection::Node("step".into())), window, cx);
        });
        cx.run_until_parked();
        thread.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        pane.update_in(cx, |pane, window, cx| {
            assert_eq!(
                pane.inspector
                    .as_ref()
                    .expect("inspector should remain open")
                    .editors(),
                editors
            );
            assert_eq!(goal.read(cx).text(cx), "Local typing");
            assert_eq!(new_rule.read(cx).text(cx), "Unsubmitted rule");
            assert!(capture.focus_handle(cx).is_focused(window));
            capture.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let selection = editor.selections.newest::<text::Point>(&snapshot);
                assert_eq!(selection.start, text::Point::new(0, 2));
                assert_eq!(selection.end, text::Point::new(0, 5));
            });
            let node = pane
                .root_graph(cx)
                .and_then(|graph| graph.node_at(&path))
                .expect("step should exist");
            assert_eq!(node.intent, "Local typing");
            assert_eq!(node.rules, vec!["Latest rule"]);
        });
    }

    /// The agent locks and unlocks steps from the chat, which changes the plan
    /// without going through the canvas. An inspector open on such a step
    /// used to keep its fields editable after the step was locked.
    #[gpui::test]
    async fn an_open_inspector_follows_locks_changed_from_the_chat(cx: &mut TestAppContext) {
        fn title_is_read_only(pane: &Entity<ArchitectPane>, cx: &mut VisualTestContext) -> bool {
            let title = pane.read_with(cx, |pane, _| {
                pane.inspector
                    .as_ref()
                    .expect("the selected step should have an inspector")
                    .title
                    .clone()
            });
            title.read_with(cx, |editor, cx| editor.read_only(cx))
        }

        let plan = test_plan(cx).await;
        let project = plan.project.clone();
        let thread = plan.thread;
        let mut graph = ArchitectGraph::default();
        graph.add_node(ArchitectNode::new("schema", "Define schema"));
        thread.update(cx, |thread, cx| thread.set_architect_graph(Some(graph), cx));

        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
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
            pane.set_selection(Some(Selection::Node(NodeId::from("schema"))), window, cx);
        });
        cx.run_until_parked();
        assert!(!title_is_read_only(&pane, cx));

        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(|graph| graph.lock_all(), cx);
        });
        cx.run_until_parked();
        assert!(
            title_is_read_only(&pane, cx),
            "a step locked from the chat should turn its inspector read-only"
        );

        thread.update(cx, |thread, cx| {
            thread.update_architect_graph(|graph| graph.unlock_all(), cx);
        });
        cx.run_until_parked();
        assert!(
            !title_is_read_only(&pane, cx),
            "a step unlocked from the chat should be editable again"
        );
    }
}
