use std::collections::{HashMap, HashSet};

use architect::{
    ArchitectGraph, ArchitectNode, EdgeCondition, GraphProblem, NodeId, NodePath, Position,
    RunOutcome,
};
use gpui::{
    App, Bounds, ClickEvent, Context, CursorStyle, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, MouseButton, MouseDownEvent, MouseUpEvent, PathBuilder, Pixels,
    Render, SharedString, Subscription, WeakEntity, Window, canvas, deferred, div, point, px,
};
use ui::{ContextMenu, Divider, TintColor, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, StatusItemView,
    item::{Item, ItemEvent, ItemHandle},
};

use super::bulk::{self, BulkCounts};
use super::geometry::{EdgeCurve, NODE_WIDTH, paint_curve};
use super::{
    ArchitectPane, ArchitectWorkspaceMode, DETAIL_ZOOM_THRESHOLD, DUPLICATE_SHORTCUT,
    EXPANDED_CHILD_LIMIT, HistoryDirection, Interaction, MAX_ZOOM, MIN_ZOOM, REDO_SHORTCUT,
    Selection, UNDO_SHORTCUT,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchitectLayout {
    Wide,
    Medium,
    Narrow,
    Compact,
}

impl ArchitectLayout {
    fn for_width(width: Pixels) -> Self {
        let width = f32::from(width);
        if width >= 1320.0 {
            Self::Wide
        } else if width >= 1060.0 {
            Self::Medium
        } else if width >= 760.0 {
            Self::Narrow
        } else {
            Self::Compact
        }
    }
}

fn node_intersects_viewport(
    left: Pixels,
    top: Pixels,
    width: Pixels,
    height: Pixels,
    viewport_width: Pixels,
    viewport_height: Pixels,
) -> bool {
    const VIEWPORT_OVERSCAN: f32 = 96.0;
    let overscan = px(VIEWPORT_OVERSCAN);
    left + width >= -overscan
        && top + height >= -overscan
        && left <= viewport_width + overscan
        && top <= viewport_height + overscan
}

/// The draggable boundaries between the Architect regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ArchitectDivider {
    Outline,
    Inspector,
}

struct DraggedArchitectDivider(ArchitectDivider);

impl Render for DraggedArchitectDivider {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

const DIVIDER_HIT_WIDTH: f32 = 6.0;

#[derive(Default)]
pub(super) struct ArchitectStatusItem {
    active: Option<WeakEntity<ArchitectPane>>,
    _subscription: Option<Subscription>,
}

impl Render for ArchitectStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pane) = self.active.as_ref().and_then(|pane| pane.upgrade()) else {
            return div().into_any();
        };
        let pane = pane.read(cx);
        if pane.mode != ArchitectWorkspaceMode::Architect {
            return div().into_any();
        }
        let thread = pane.thread.read(cx);
        let title = thread
            .title()
            .unwrap_or_else(|| SharedString::from("Untitled plan"));
        // The same words as the plan header, so the two never disagree.
        let graph = thread.architect_graph();
        let status = if pane.is_running(cx) {
            "running"
        } else if graph.is_some_and(|graph| {
            graph.is_fully_locked_deeply() && graph.blocking_problems().is_empty()
        }) {
            "ready to run"
        } else if graph.is_none_or(ArchitectGraph::is_empty) {
            "not started"
        } else {
            "needs review"
        };

        h_flex()
            .id("architect-status-context")
            .gap_1()
            .child(
                Icon::new(IconName::ListTree)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!("Architect · {} · {status}", truncate(&title, 36)))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .into_any()
    }
}

impl StatusItemView for ArchitectStatusItem {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = active_pane_item.and_then(|item| item.downcast::<ArchitectPane>());
        self.active = pane.as_ref().map(Entity::downgrade);
        self._subscription = pane.map(|pane| cx.observe(&pane, |_, _, cx| cx.notify()));
        cx.notify();
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

impl ArchitectPane {
    // -- Rendering ------------------------------------------------------------

    /// A zero-width boundary whose hit area straddles the seam, so dragging it
    /// resizes the region without shifting the layout by the handle's width.
    fn render_divider(&self, divider: ArchitectDivider, cx: &mut Context<Self>) -> AnyElement {
        let (id, tooltip) = match divider {
            ArchitectDivider::Outline => (
                "architect-outline-resize",
                "Drag to resize the plan outline",
            ),
            ArchitectDivider::Inspector => {
                ("architect-inspector-resize", "Drag to resize the inspector")
            }
        };
        let hover = cx.theme().colors().border_focused;
        div()
            .relative()
            .flex_none()
            .w(px(0.0))
            .h_full()
            .child(deferred(
                div()
                    .id(id)
                    .absolute()
                    .top(px(0.0))
                    .left(px(-DIVIDER_HIT_WIDTH / 2.0))
                    .w(px(DIVIDER_HIT_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .hover(move |style| style.bg(hover))
                    .tooltip(Tooltip::text(tooltip))
                    .on_drag(DraggedArchitectDivider(divider), |dragged, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| DraggedArchitectDivider(dragged.0))
                    })
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            if event.click_count == 2 {
                                this.reset_divider(divider, cx);
                                cx.stop_propagation();
                            }
                        }),
                    )
                    .occlude(),
            ))
            .into_any_element()
    }

    fn handle_divider_drag(
        &mut self,
        event: &DragMoveEvent<DraggedArchitectDivider>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.drag(cx).0 {
            ArchitectDivider::Outline => {
                self.set_outline_width(event.event.position.x - event.bounds.left(), cx);
            }
            ArchitectDivider::Inspector => {
                self.set_inspector_width(event.bounds.right() - event.event.position.x, cx);
            }
        }
    }

    /// The trail of steps opened to reach the plan on screen. It is the only
    /// way out of a nested plan that does not depend on remembering how you got
    /// in, so it is shown even at the top level, where it names the plan itself.
    /// The trail of steps opened to reach the plan on screen, for the toolbar.
    ///
    /// Shown even at the top level, where it names the plan itself: the trail is
    /// the only thing that says which of several nested plans you are looking
    /// at, and having it appear and disappear moves everything beside it.
    fn render_breadcrumb(&self, cx: &mut Context<Self>) -> AnyElement {
        let root = self.root_graph(cx);

        let mut crumbs: Vec<(usize, SharedString)> = vec![(0, "Plan".into())];
        for (depth, id) in self.focus.0.iter().enumerate() {
            let title = root
                .and_then(|root| root.graph_at(&NodePath(self.focus.0[..depth].to_vec())))
                .and_then(|graph| graph.node(id))
                .map(|node| SharedString::from(truncate(&node.title, 28)))
                .unwrap_or_else(|| SharedString::from(id.0.clone()));
            crumbs.push((depth + 1, title));
        }
        let last = crumbs.len().saturating_sub(1);
        let nested = !self.focus.is_empty();

        h_flex()
            .gap_0p5()
            .min_w_0()
            .child(
                Icon::new(IconName::ListTree)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .children(
                crumbs
                    .into_iter()
                    .enumerate()
                    .flat_map(|(ix, (depth, title))| {
                        let is_last = ix == last;
                        let separator = (ix > 0).then(|| {
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .into_any_element()
                        });
                        let crumb = if is_last {
                            Label::new(title)
                                .size(LabelSize::Small)
                                .truncate()
                                .into_any_element()
                        } else {
                            Button::new(("architect-crumb", ix), title)
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .color(Color::Muted)
                                .style(ButtonStyle::Subtle)
                                .tooltip(Tooltip::text("Go back up to this plan"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.focus_depth(depth, window, cx)
                                }))
                                .into_any_element()
                        };
                        separator.into_iter().chain(std::iter::once(crumb))
                    }),
            )
            .when(nested, |this| {
                this.child(
                    chip(
                        format!("level {}", self.focus.depth() + 1),
                        None,
                        Color::Muted,
                        cx.theme().colors().border,
                        cx.theme().colors().element_background,
                    )
                    .into_any_element(),
                )
            })
            .into_any()
    }

    fn render_plan_header(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let graph = self.graph(cx);
        let step_count = graph.map_or(0, |graph| graph.nodes.len());
        let locked_count = graph.map_or(0, |graph| {
            graph.nodes.iter().filter(|node| node.locked).count()
        });
        // Readiness is a property of the whole plan, not of the level on
        // screen: a run started from inside a sub-plan still runs everything.
        let root = self.root_graph(cx);
        let run_step_count = root.map_or(0, ArchitectGraph::step_count_deeply);
        let run_locked_count = root.map_or(0, ArchitectGraph::locked_step_count_deeply);
        let problems: Vec<GraphProblem> = root
            .map(ArchitectGraph::blocking_problems)
            .unwrap_or_default();
        let all_locked = step_count > 0 && locked_count == step_count;
        let ready_to_run = root.is_some_and(ArchitectGraph::is_fully_locked_deeply)
            && root.is_some_and(|root| root.blocking_problems().is_empty());
        let run_tooltip: SharedString = if ready_to_run {
            "Run the plan, one step at a time".into()
        } else if run_step_count == 0 {
            "Nothing to run yet. Start planning to draft steps.".into()
        } else if !problems.is_empty() {
            match problems.len() {
                1 => "Fix 1 problem first. Review Plan shows it.".into(),
                count => format!("Fix {count} problems first. Review Plan shows them.").into(),
            }
        } else {
            match run_step_count.saturating_sub(run_locked_count) {
                1 => "Lock the last open step to run the plan.".into(),
                open => format!("Lock the {open} open steps to run the plan.").into(),
            }
        };

        let running = self.is_running(cx);
        // A step running in a thread of its own can only be followed, or its
        // requests allowed, from that thread's conversation.
        let watch_step = running && self.run_step_session(cx).is_some();
        let step_pending = if watch_step {
            self.run_step_pending_permissions(cx)
        } else {
            0
        };
        let watch_label = if step_pending > 0 {
            "Approve Step"
        } else {
            "Watch Step"
        };
        let watch_tooltip: SharedString = match step_pending {
            0 => "Show the conversation of the step being run".into(),
            1 => "The step being run is waiting for you to allow something".into(),
            count => {
                format!("The step being run is waiting for you to allow {count} things").into()
            }
        };
        let watch_style = if step_pending > 0 {
            ButtonStyle::Tinted(TintColor::Warning)
        } else {
            ButtonStyle::Subtle
        };
        // Review jumps to the first thing on this level that blocks a run, so
        // with nothing blocking there is nothing for it to do.
        let review_count = if running {
            0
        } else {
            graph.map_or(0, |graph| graph.blocking_problems().len())
        };
        let review_tooltip: SharedString = match review_count {
            1 => "Select the one thing that needs attention".into(),
            count => format!("Select the first of {count} things that need attention").into(),
        };
        let run_status: Option<SharedString> =
            self.thread
                .read(cx)
                .architect_run()
                .and_then(|run| match &run.outcome {
                    Some(outcome) => root.map(|root| outcome.summary(root).into()),
                    // Loops can take a run past the number of steps, so no
                    // total is shown for the step number to exceed.
                    None => {
                        Some(format!("Step {} · {}", run.step_number, run.current_title).into())
                    }
                });

        // One line, in the order the plan is read: where you are, what it is,
        // then what is being done to it.
        let nested_count = graph.map_or(0, |graph| {
            graph.nodes.iter().filter(|node| node.has_subplan()).count()
        });
        let mut summary = match step_count {
            0 => "No steps yet".to_string(),
            1 => "1 step".to_string(),
            count => format!("{count} steps"),
        };
        if nested_count > 0 {
            summary.push_str(&format!(" · {nested_count} nested"));
        }
        if step_count > 0 {
            if all_locked {
                summary.push_str(" · all locked");
            } else {
                let unlocked = step_count - locked_count;
                summary.push_str(&format!(" · {unlocked} still open"));
            }
        }

        let plan_title = self
            .thread
            .read(cx)
            .title()
            .unwrap_or_else(|| SharedString::from("Untitled plan"));
        // Whether the plan has started is, like readiness, about the whole
        // plan: an empty sub-plan inside a drafted plan is not "Not started".
        let empty = run_step_count == 0 && !running;
        let readiness = if running {
            "Running"
        } else if ready_to_run {
            "Ready to run"
        } else if empty {
            "Not started"
        } else {
            "Needs review"
        };

        h_flex()
            .id("architect-plan-header")
            .w_full()
            .flex_none()
            .px_3()
            .py_2()
            .gap_3()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().toolbar_background)
            .child(
                v_flex()
                    .gap_0p5()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(Label::new(plan_title).size(LabelSize::Default).truncate())
                            .child(chip(
                                readiness,
                                Some(if running {
                                    IconName::PlayFilled
                                } else if ready_to_run {
                                    IconName::Check
                                } else if empty {
                                    IconName::Circle
                                } else {
                                    IconName::Warning
                                }),
                                if running {
                                    Color::Info
                                } else if ready_to_run {
                                    Color::Success
                                } else if empty {
                                    Color::Muted
                                } else {
                                    Color::Warning
                                },
                                if running {
                                    cx.theme().status().info_border
                                } else if ready_to_run {
                                    cx.theme().status().success_border
                                } else if empty {
                                    cx.theme().colors().border
                                } else {
                                    cx.theme().status().warning_border
                                },
                                if running {
                                    cx.theme().status().info_background
                                } else if ready_to_run {
                                    cx.theme().status().success_background
                                } else if empty {
                                    cx.theme().colors().element_background
                                } else {
                                    cx.theme().status().warning_background
                                },
                            )),
                    )
                    .when(!compact && !empty, |this| {
                        this.child(
                            Label::new(summary)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_none()
                    // The state of the run sits next to the control for it, so
                    // reading what is happening and stopping it are one glance
                    // and one reach apart.
                    .when_some(
                        (!compact).then_some(run_status).flatten(),
                        |this, status| {
                            this.child(
                                h_flex()
                                    .id("architect-run-status")
                                    .gap_1()
                                    .px_2()
                                    .py_0p5()
                                    .mr_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if running {
                                        cx.theme().status().info_border
                                    } else {
                                        cx.theme().colors().border
                                    })
                                    .bg(if running {
                                        cx.theme().status().info_background
                                    } else {
                                        cx.theme().colors().element_background
                                    })
                                    .child(
                                        Icon::new(if running {
                                            IconName::PlayFilled
                                        } else {
                                            IconName::Check
                                        })
                                        .size(IconSize::XSmall)
                                        .color(if running { Color::Info } else { Color::Muted }),
                                    )
                                    .child(
                                        Label::new(truncate(&status, 34))
                                            .size(LabelSize::Small)
                                            .color(if running {
                                                Color::Info
                                            } else {
                                                Color::Muted
                                            }),
                                    )
                                    .tooltip(Tooltip::text(status)),
                            )
                        },
                    )
                    .when(!compact && review_count > 0, |this| {
                        this.child(
                            Button::new("architect-review", "Review Plan")
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::ListTodo).size(IconSize::XSmall))
                                .tooltip(Tooltip::text(review_tooltip.clone()))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.review_plan(window, cx)),
                                ),
                        )
                    })
                    .when(compact && review_count > 0, |this| {
                        this.child(
                            IconButton::new("architect-review-compact", IconName::ListTodo)
                                .tab_index(0isize)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(review_tooltip))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.review_plan(window, cx)),
                                ),
                        )
                    })
                    .when(watch_step && !compact, |this| {
                        this.child(
                            Button::new("architect-watch-step", watch_label)
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(watch_style)
                                .start_icon(Icon::new(IconName::Eye).size(IconSize::XSmall))
                                .tooltip(Tooltip::text(watch_tooltip.clone()))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.watch_running_step(window, cx);
                                })),
                        )
                    })
                    .when(watch_step && compact, |this| {
                        this.child(
                            IconButton::new("architect-watch-step-compact", IconName::Eye)
                                .tab_index(0isize)
                                .icon_size(IconSize::Small)
                                .when(step_pending > 0, |this| this.icon_color(Color::Warning))
                                .tooltip(Tooltip::text(watch_tooltip))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.watch_running_step(window, cx);
                                })),
                        )
                    })
                    // An empty plan has nothing to run; the empty state offers
                    // Start planning instead of a Run that can only refuse.
                    .when(running || run_step_count > 0, |this| {
                        this.child(if running {
                            Button::new("architect-stop", "Stop")
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Tinted(TintColor::Warning))
                                .start_icon(Icon::new(IconName::Stop).size(IconSize::XSmall))
                                .tooltip(Tooltip::text(
                                    "Stop the run and the turn it is waiting on",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| this.stop_run(cx)))
                        } else {
                            Button::new("architect-run", "Run")
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::XSmall))
                                .disabled(!ready_to_run)
                                .tooltip(Tooltip::text(run_tooltip))
                                .on_click(cx.listener(|this, _, _, cx| this.run(cx)))
                        })
                    })
                    .child(
                        Button::new("architect-open-code", "Code")
                            .tab_index(0isize)
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::Code).size(IconSize::XSmall))
                            .tooltip(|_window, cx| {
                                Tooltip::for_action(
                                    "Return to the Code workspace",
                                    &crate::ToggleArchitectWorkspace,
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.request_code_mode(window, cx)
                            })),
                    ),
            )
            .into_any()
    }

    fn render_canvas_command_bar(
        &self,
        show_navigation: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running = self.is_running(cx);
        let empty_plan = self.graph(cx).is_none_or(ArchitectGraph::is_empty);
        let breadcrumb = self.render_breadcrumb(cx);
        let can_undo = !running && !self.undo_stack.is_empty();
        let can_redo = !running && !self.redo_stack.is_empty();
        let history_buttons = [
            IconButton::new("architect-undo", IconName::Undo)
                .tab_index(0isize)
                .icon_size(IconSize::Small)
                .disabled(!can_undo)
                .tooltip(Tooltip::text(format!("Undo ({UNDO_SHORTCUT})")))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.step_history(HistoryDirection::Undo, window, cx);
                })),
            IconButton::new("architect-redo", IconName::Redo)
                .tab_index(0isize)
                .icon_size(IconSize::Small)
                .disabled(!can_redo)
                .tooltip(Tooltip::text(format!("Redo ({REDO_SHORTCUT})")))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.step_history(HistoryDirection::Redo, window, cx);
                })),
        ];

        h_flex()
            .w_full()
            .flex_none()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex()
                    .gap_1()
                    .min_w_0()
                    .when(show_navigation, |this| {
                        this.child(
                            Button::new("architect-open-outline", "Plan")
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::ListTree).size(IconSize::XSmall))
                                .tooltip(Tooltip::text("Open plan navigation"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_outline_drawer(false, window, cx);
                                })),
                        )
                    })
                    .child(breadcrumb),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_none()
                    .children(history_buttons)
                    .child(Divider::vertical())
                    .when(!show_navigation && !empty_plan, |this| {
                        this.child(
                            Button::new("architect-search", "Search")
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(
                                    Icon::new(IconName::MagnifyingGlass).size(IconSize::XSmall),
                                )
                                .tooltip(Tooltip::text(
                                    "Open the ordered plan navigator to find a step",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_outline_drawer(true, window, cx);
                                })),
                        )
                        .child(
                            Button::new("architect-tidy", "Tidy")
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::RotateCw).size(IconSize::XSmall))
                                .disabled(running)
                                .tooltip(Tooltip::text("Arrange the steps automatically"))
                                .on_click(cx.listener(|this, _, _, cx| this.tidy_up(cx))),
                        )
                    })
                    .when(!show_navigation, |this| {
                        this.child(
                            Button::new("architect-add-step", "Add Step")
                                .tab_index(0isize)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                                .disabled(running)
                                .tooltip(Tooltip::text("Add a step to this plan"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.add_step(window, cx)),
                                ),
                        )
                    })
                    .when(show_navigation && !empty_plan, |this| {
                        this.child(
                            IconButton::new("architect-search-compact", IconName::MagnifyingGlass)
                                .tab_index(0isize)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Search plan steps"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_outline_drawer(true, window, cx);
                                })),
                        )
                        .child(
                            IconButton::new("architect-tidy-compact", IconName::RotateCw)
                                .tab_index(0isize)
                                .icon_size(IconSize::Small)
                                .disabled(running)
                                .tooltip(Tooltip::text("Arrange the steps automatically"))
                                .on_click(cx.listener(|this, _, _, cx| this.tidy_up(cx))),
                        )
                    })
                    .when(show_navigation, |this| {
                        this.child(
                            IconButton::new("architect-add-step-compact", IconName::Plus)
                                .tab_index(0isize)
                                .icon_size(IconSize::Small)
                                .disabled(running)
                                .tooltip(Tooltip::text("Add a step to this plan"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.add_step(window, cx)),
                                ),
                        )
                    }),
            )
            .into_any()
    }

    fn render_outline(&self, width: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let graph = self.graph(cx);
        let query = self.search_editor.read(cx).text(cx).trim().to_lowercase();
        let all_ordered_nodes: Vec<ArchitectNode> = graph
            .map(|graph| {
                graph
                    .execution_order()
                    .into_iter()
                    .filter_map(|id| graph.node(&id).cloned())
                    .collect()
            })
            .unwrap_or_default();
        let ordered_nodes: Vec<ArchitectNode> = all_ordered_nodes
            .iter()
            .filter(|node| {
                query.is_empty()
                    || node.title.to_lowercase().contains(&query)
                    || node.responsibility.to_lowercase().contains(&query)
                    || node.intent.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        let no_search_results = !query.is_empty() && ordered_nodes.is_empty();
        // Numbered by place in the whole plan, so a search narrows the list
        // without renumbering the steps it keeps.
        let step_numbers: HashMap<NodeId, usize> = all_ordered_nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), index + 1))
            .collect();
        // Steps a loop can bring the plan back to: the target of a connection
        // that points backwards and can lead round to where it left.
        let loop_targets: HashSet<NodeId> = graph
            .map(|graph| {
                graph
                    .edges
                    .iter()
                    .filter(|edge| {
                        let from = step_numbers.get(&edge.from);
                        let to = step_numbers.get(&edge.to);
                        matches!((from, to), (Some(from), Some(to)) if to <= from)
                            && graph.is_loop_edge(edge)
                    })
                    .map(|edge| edge.to.clone())
                    .collect()
            })
            .unwrap_or_default();
        // Described by step title: the ids in a problem's `Display` form are for
        // the model, not for the person reading the outline.
        let blocking_problems: Vec<(Selection, String)> = graph
            .map(|graph| {
                graph
                    .blocking_problems()
                    .iter()
                    .map(|problem| (Self::problem_selection(problem), problem.describe(graph)))
                    .collect()
            })
            .unwrap_or_default();
        // Named, because a list of identical "add a handoff" rows does not say
        // which step each one is for.
        let incomplete_handoffs: Vec<(NodeId, String)> = graph
            .map(|graph| {
                graph
                    .steps_without_capture()
                    .into_iter()
                    .map(|id| {
                        let title = graph
                            .node(&id)
                            .map(|node| node.title.trim().to_string())
                            .filter(|title| !title.is_empty())
                            .unwrap_or_else(|| "untitled step".to_string());
                        (id, title)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let selected = match &self.selection {
            Some(Selection::Node(id)) => Some(id),
            _ => None,
        };
        let running = self.running_node(cx);
        let run = self.thread.read(cx).architect_run();
        let run_active = self.is_running(cx);
        let failed_node = run.and_then(|run| match run.outcome.as_ref() {
            Some(RunOutcome::NodeLimit { node, .. } | RunOutcome::DepthLimit { node }) => {
                Some(node)
            }
            _ => None,
        });
        let settled = all_ordered_nodes.iter().filter(|node| node.locked).count();
        let completed = all_ordered_nodes
            .iter()
            .filter(|node| {
                node.result
                    .as_ref()
                    .is_some_and(|result| !result.summary.trim().is_empty())
            })
            .count();
        let draft = all_ordered_nodes.len().saturating_sub(settled);
        let has_steps = !all_ordered_nodes.is_empty();
        let ready = has_steps && blocking_problems.is_empty();

        v_flex()
            .id("architect-outline")
            .w(width)
            .h_full()
            .flex_none()
            .overflow_hidden()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .child(
                                Label::new("PLAN OUTLINE")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .when(has_steps, |this| {
                                this.child(chip(
                                    if ready { "Ready" } else { "Review" },
                                    Some(if ready {
                                        IconName::Check
                                    } else {
                                        IconName::Warning
                                    }),
                                    if ready {
                                        Color::Success
                                    } else {
                                        Color::Warning
                                    },
                                    if ready {
                                        cx.theme().status().success_border
                                    } else {
                                        cx.theme().status().warning_border
                                    },
                                    if ready {
                                        cx.theme().status().success_background
                                    } else {
                                        cx.theme().status().warning_background
                                    },
                                ))
                            }),
                    )
                    .when(has_steps, |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(format!("{settled} settled"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(format!("{draft} draft"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(format!("{completed} complete"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .bg(cx.theme().colors().editor_background)
                                .child(
                                    Icon::new(IconName::MagnifyingGlass)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(self.search_editor.clone()),
                        )
                    })
                    .child(
                        Button::new("architect-plan-conversation", "Plan Conversation")
                            .tab_index(0isize)
                            .full_width()
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .start_icon(Icon::new(IconName::Sparkle).size(IconSize::XSmall))
                            .tooltip(Tooltip::text(
                                "Open the root conversation that owns this plan",
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_plan_conversation(window, cx)
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("architect-outline-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_3()
                    .when(
                        !blocking_problems.is_empty() || !incomplete_handoffs.is_empty(),
                        |this| {
                            this.child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("READINESS")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .children(blocking_problems.iter().enumerate().map(
                                        |(index, (selection, description))| {
                                            let selection = selection.clone();
                                            Button::new(
                                                ("architect-readiness-problem", index),
                                                truncate(description, 42),
                                            )
                                            .tab_index(0isize)
                                            .full_width()
                                            .label_size(LabelSize::XSmall)
                                            .style(ButtonStyle::Subtle)
                                            .start_icon(
                                                Icon::new(IconName::Warning)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Warning),
                                            )
                                            .tooltip(Tooltip::text(description.clone()))
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.set_selection(
                                                        Some(selection.clone()),
                                                        window,
                                                        cx,
                                                    )
                                                }),
                                            )
                                        },
                                    ))
                                    .children(incomplete_handoffs.iter().enumerate().map(
                                        |(index, (id, title))| {
                                            let id = id.clone();
                                            Button::new(
                                                ("architect-readiness-handoff", index),
                                                truncate(&format!("Add a handoff for {title}"), 42),
                                            )
                                            .tab_index(0isize)
                                            .full_width()
                                            .label_size(LabelSize::XSmall)
                                            .style(ButtonStyle::Subtle)
                                            .start_icon(
                                                Icon::new(IconName::ArrowRight)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Warning),
                                            )
                                            .tooltip(Tooltip::text(format!(
                                                "Say what \"{title}\" must pass on to the steps \
                                                 after it"
                                            )))
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.select_and_reveal(
                                                        Selection::Node(id.clone()),
                                                        window,
                                                        cx,
                                                    )
                                                }),
                                            )
                                        },
                                    )),
                            )
                        },
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("EXECUTION ORDER")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .when(no_search_results, |this| {
                                this.child(
                                    Label::new("No steps match this search.")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .children(ordered_nodes.into_iter().enumerate().map(
                                |(index, node)| {
                                    let step_number =
                                        step_numbers.get(&node.id).copied().unwrap_or(index + 1);
                                    let has_nested = node.has_subplan();
                                    let loop_target = loop_targets.contains(&node.id);
                                    let click_id = node.id.clone();
                                    let keyboard_id = node.id.clone();
                                    let is_selected = selected == Some(&node.id)
                                        || self.in_bulk_selection(&node.id);
                                    let is_running = running == Some(&node.id);
                                    let is_failed = failed_node == Some(&node.id);
                                    let is_complete = node
                                        .result
                                        .as_ref()
                                        .is_some_and(|result| !result.summary.trim().is_empty());
                                    let (state, state_color) = if is_running {
                                        ("running", Color::Info)
                                    } else if is_failed {
                                        ("failed", Color::Error)
                                    } else if is_complete {
                                        ("completed", Color::Success)
                                    } else if node.locked {
                                        ("settled", Color::Success)
                                    } else if run_active {
                                        ("queued", Color::Muted)
                                    } else {
                                        ("draft", Color::Muted)
                                    };
                                    let responsibility = if node.responsibility.trim().is_empty() {
                                        "Responsibility not set".to_string()
                                    } else {
                                        node.responsibility.clone()
                                    };

                                    v_flex()
                                        .id(("architect-outline-step", index))
                                        .tab_index(0isize)
                                        .role(gpui::Role::Button)
                                        .aria_label(format!(
                                            "Step {step_number}: {} ({state})",
                                            node.title
                                        ))
                                        .w_full()
                                        .gap_0p5()
                                        .px_2()
                                        .py_1p5()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(if is_selected {
                                            cx.theme().colors().border_focused
                                        } else {
                                            cx.theme().colors().border_variant
                                        })
                                        .bg(if is_selected {
                                            cx.theme().colors().element_selected
                                        } else {
                                            cx.theme().colors().element_background
                                        })
                                        .cursor(CursorStyle::PointingHand)
                                        .focus_visible(|style| {
                                            style.border_color(cx.theme().colors().border_focused)
                                        })
                                        .on_key_down(cx.listener(
                                            move |this, event: &gpui::KeyDownEvent, window, cx| {
                                                match event.keystroke.key.as_str() {
                                                    "down" | "right" => {
                                                        this.select_adjacent_step(true, window, cx);
                                                        cx.stop_propagation();
                                                    }
                                                    "up" | "left" => {
                                                        this.select_adjacent_step(
                                                            false, window, cx,
                                                        );
                                                        cx.stop_propagation();
                                                    }
                                                    "enter" | "space" => {
                                                        this.select_and_reveal(
                                                            Selection::Node(keyboard_id.clone()),
                                                            window,
                                                            cx,
                                                        );
                                                        this.open_inspector_drawer(window, cx);
                                                        cx.stop_propagation();
                                                    }
                                                    _ => {}
                                                }
                                            },
                                        ))
                                        .on_click(cx.listener(
                                            move |this, event: &ClickEvent, window, cx| {
                                                if event.modifiers().shift {
                                                    this.toggle_in_bulk_selection(
                                                        click_id.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                } else {
                                                    this.select_and_reveal(
                                                        Selection::Node(click_id.clone()),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            },
                                        ))
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .min_w_0()
                                                .child(
                                                    Label::new(format!("{step_number:02}"))
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(node.title)
                                                        .size(LabelSize::Small)
                                                        .truncate(),
                                                )
                                                .when(loop_target, |this| {
                                                    this.child(
                                                        Icon::new(IconName::RotateCcw)
                                                            .size(IconSize::XSmall)
                                                            .color(Color::Warning),
                                                    )
                                                })
                                                .when(has_nested, |this| {
                                                    this.child(
                                                        Icon::new(IconName::ListTree)
                                                            .size(IconSize::XSmall)
                                                            .color(Color::Accent),
                                                    )
                                                }),
                                        )
                                        .child(
                                            h_flex()
                                                .w_full()
                                                .justify_between()
                                                .gap_1()
                                                .child(
                                                    Label::new(responsibility)
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted)
                                                        .truncate(),
                                                )
                                                .child(
                                                    Label::new(state)
                                                        .size(LabelSize::XSmall)
                                                        .color(state_color),
                                                ),
                                        )
                                },
                            )),
                    ),
            )
            .into_any()
    }

    fn render_run_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let thread = self.thread.read(cx);
        let run = thread.architect_run()?;
        let total = self
            .root_graph(cx)
            .map(ArchitectGraph::step_count_deeply)
            .unwrap_or_default()
            .max(1);
        let completed = run
            .history()
            .iter()
            .filter(|step| !step.is_running())
            .count();
        let progress = (completed as f32 / total as f32).clamp(0.0, 1.0);
        let elapsed = run
            .history()
            .iter()
            .map(|step| step.elapsed())
            .sum::<std::time::Duration>();
        let status: SharedString = match &run.outcome {
            Some(outcome) => self
                .root_graph(cx)
                .map(|graph| outcome.summary(graph).into())
                .unwrap_or_else(|| SharedString::from("Run finished")),
            None => format!("Running {}", run.current_title).into(),
        };
        let succeeded = run.outcome.as_ref().is_some_and(RunOutcome::is_success);
        let cancelled = matches!(run.outcome.as_ref(), Some(RunOutcome::Cancelled));
        let failed = run.outcome.is_some() && !succeeded && !cancelled;
        let latest_output: Option<SharedString> = run.history().iter().rev().find_map(|step| {
            step.summary
                .as_ref()
                .map(|summary| format!("{}: {}", step.title, truncate(summary, 96)).into())
        });
        let finished = !run.is_running();
        let remote_workflow_url = run.remote_workflow_url().map(str::to_owned);
        let (border, background, color) = if failed {
            (
                cx.theme().status().error_border,
                cx.theme().status().error_background,
                Color::Error,
            )
        } else if cancelled {
            (
                cx.theme().status().warning_border,
                cx.theme().status().warning_background,
                Color::Warning,
            )
        } else if succeeded {
            (
                cx.theme().status().success_border,
                cx.theme().status().success_background,
                Color::Success,
            )
        } else {
            (
                cx.theme().status().info_border,
                cx.theme().status().info_background,
                Color::Info,
            )
        };

        Some(
            h_flex()
                .id("architect-run-bar")
                .w_full()
                .flex_none()
                .px_3()
                .py_1p5()
                .gap_3()
                .border_b_1()
                .border_color(border)
                .bg(background)
                .child(
                    Icon::new(if run.is_running() {
                        IconName::PlayFilled
                    } else if succeeded {
                        IconName::Check
                    } else {
                        IconName::Warning
                    })
                    .size(IconSize::Small)
                    .color(color),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .id("architect-run-status")
                                .tooltip(Tooltip::text(
                                    "Each step's full output is in the Plan Conversation",
                                ))
                                .child(Label::new(status).size(LabelSize::Small).truncate()),
                        )
                        .when_some(latest_output, |this, output| {
                            this.child(
                                div().id("architect-run-output").child(
                                    Label::new(output)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                            )
                        })
                        .child(
                            div()
                                .w(px(220.0))
                                .h(px(4.0))
                                .rounded_full()
                                .bg(cx.theme().colors().border_variant)
                                .child(div().w(px(220.0 * progress)).h_full().rounded_full().bg(
                                    if succeeded {
                                        cx.theme().status().success
                                    } else if failed {
                                        cx.theme().status().error
                                    } else if cancelled {
                                        cx.theme().status().warning
                                    } else {
                                        cx.theme().status().info
                                    },
                                )),
                        ),
                )
                .child(
                    Label::new(format!(
                        "{} of {} · {}",
                        completed.min(total),
                        total,
                        format_elapsed(elapsed)
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .when_some(remote_workflow_url, |this, url| {
                    this.child(
                        Button::new("architect-open-workflow", "Open Workflow")
                            .tab_index(0isize)
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Open this run's remote workflow"))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                })
                .when(finished, |this| {
                    this.child(
                        IconButton::new("architect-dismiss-run", IconName::Close)
                            .tab_index(0isize)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Dismiss this run's result"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.thread.update(cx, |thread, cx| {
                                    thread.dismiss_architect_run(cx);
                                });
                            })),
                    )
                })
                .into_any(),
        )
    }

    /// A scaled-down plan of the level on screen, so a graph too big to fit is
    /// still navigable. Clicking jumps the view.
    ///
    /// Nodes are drawn as bare blocks: at this size their titles would be
    /// unreadable, and the shape of the graph is what the minimap is for.
    fn render_minimap(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        const WIDTH: f32 = 168.0;
        const HEIGHT: f32 = 104.0;
        const PADDING: f32 = 8.0;
        // Room for the label, so the steps are never drawn over it.
        const TOP: f32 = 20.0;
        const INSET: f32 = 16.0;

        let graph = self.graph(cx)?;
        if graph.nodes.len() < 4 {
            return None;
        }

        let positions: Vec<(NodeId, Position)> = graph
            .nodes
            .iter()
            .filter_map(|node| node.position.map(|position| (node.id.clone(), position)))
            .collect();
        if positions.is_empty() {
            return None;
        }

        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (_, position) in &positions {
            min_x = min_x.min(position.x);
            min_y = min_y.min(position.y);
            max_x = max_x.max(position.x);
            max_y = max_y.max(position.y);
        }
        let span_x = (max_x - min_x).max(1.0);
        let span_y = (max_y - min_y).max(1.0);
        let scale = ((WIDTH - PADDING * 2.0) / span_x).min((HEIGHT - TOP - PADDING) / span_y);

        // The part of the plan on screen, so the minimap says where you are.
        let visible = self.viewport.get().map(|bounds| {
            let top_left = self.to_canvas(bounds.origin);
            let bottom_right = self.to_canvas(bounds.bottom_right());
            let left = (PADDING + (top_left.x - min_x) * scale).clamp(0.0, WIDTH);
            let top = (TOP + (top_left.y - min_y) * scale).clamp(TOP, HEIGHT);
            let right = (PADDING + (bottom_right.x - min_x) * scale).clamp(0.0, WIDTH);
            let bottom = (TOP + (bottom_right.y - min_y) * scale).clamp(TOP, HEIGHT);
            (left, top, (right - left).max(0.0), (bottom - top).max(0.0))
        });

        let running = self.running_node(cx).cloned();
        let selected = match &self.selection {
            Some(Selection::Node(id)) => Some(id.clone()),
            _ => None,
        };

        let blocks = positions.into_iter().map(|(id, position)| {
            let is_running = running.as_ref() == Some(&id);
            let is_selected = selected.as_ref() == Some(&id) || self.in_bulk_selection(&id);
            div()
                .absolute()
                .left(px(PADDING + (position.x - min_x) * scale - 4.0))
                .top(px(TOP + (position.y - min_y) * scale - 2.5))
                .w(px(9.0))
                .h(px(5.0))
                .rounded_sm()
                .bg(if is_running {
                    cx.theme().status().info
                } else if is_selected {
                    cx.theme().colors().text_accent
                } else {
                    cx.theme().colors().text_muted
                })
        });

        Some(
            div()
                .id("architect-minimap")
                .absolute()
                .right(px(INSET))
                .bottom(px(INSET))
                .w(px(WIDTH))
                .h(px(HEIGHT))
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background.opacity(0.9))
                .cursor_pointer()
                .tooltip(Tooltip::text("Click to show that part of the plan"))
                // Moves the view to the point clicked. The minimap sits a fixed
                // inset from the canvas corner, which is where it is measured from.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let Some(bounds) = this.viewport.get() else {
                            return;
                        };
                        let left = f32::from(bounds.right()) - INSET - WIDTH;
                        let top = f32::from(bounds.bottom()) - INSET - HEIGHT;
                        let x = min_x + (f32::from(event.position.x) - left - PADDING) / scale;
                        let y = min_y + (f32::from(event.position.y) - top - TOP) / scale;
                        this.pan = point(px(-x * this.zoom), px(-y * this.zoom));
                        cx.notify();
                    }),
                )
                .when_some(visible, |this, (left, top, width, height)| {
                    this.child(
                        div()
                            .absolute()
                            .left(px(left))
                            .top(px(top))
                            .w(px(width))
                            .h(px(height))
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().colors().border_focused)
                            .bg(cx.theme().colors().border_focused.opacity(0.08)),
                    )
                })
                .child(
                    div().absolute().left(px(8.0)).top(px(4.0)).child(
                        Label::new(match self.focus.0.last() {
                            None => "plan".to_string(),
                            Some(_) => format!(
                                "level {} · {}",
                                self.focus.depth() + 1,
                                truncate(&self.step_title(&self.focus, cx), 16)
                            ),
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
                )
                .children(blocks)
                .into_any(),
        )
    }

    /// Zoom, bottom-left, out of the way of the minimap.
    ///
    /// Scrolling with a modifier already zooms, but a plan that has scrolled off
    /// the edge is exactly when a user does not know which way to scroll, so the
    /// percentage doubles as a button back to a known state.
    fn render_zoom_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let zoom = self.zoom;

        h_flex()
            .absolute()
            .left(px(16.0))
            .bottom(px(16.0))
            .gap_0p5()
            .p_0p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx
                .theme()
                .colors()
                .elevated_surface_background
                .opacity(0.94))
            .child(
                IconButton::new("architect-zoom-out", IconName::Dash)
                    .tab_index(0isize)
                    .icon_size(IconSize::XSmall)
                    .disabled(zoom <= MIN_ZOOM + f32::EPSILON)
                    .tooltip(Tooltip::text("Zoom out"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_zoom(zoom / 1.2, None, cx)),
                    ),
            )
            .child(
                Button::new("architect-zoom-reset", format!("{:.0}%", zoom * 100.0))
                    .tab_index(0isize)
                    .label_size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .style(ButtonStyle::Subtle)
                    .tooltip(Tooltip::text("Back to actual size"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_zoom(1.0, None, cx))),
            )
            .child(
                Button::new("architect-fit", "Fit")
                    .tab_index(0isize)
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Maximize).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Fit the whole plan in view"))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_to_fit(cx))),
            )
            .child(
                IconButton::new("architect-zoom-in", IconName::Plus)
                    .tab_index(0isize)
                    .icon_size(IconSize::XSmall)
                    .disabled(zoom >= MAX_ZOOM - f32::EPSILON)
                    .tooltip(Tooltip::text("Zoom in"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_zoom(zoom * 1.2, None, cx)),
                    ),
            )
            .into_any()
    }

    fn render_empty_state(&self, cx: &Context<Self>) -> AnyElement {
        // Inside a step, the whole-plan invitation to start planning is the
        // wrong advice: the step exists, and what it lacks is steps of its own.
        if !self.focus.is_empty() {
            return self.render_empty_nested_plan(cx);
        }
        let planning = self.thread.read(cx).session_mode() == agent::SessionMode::Architect;

        v_flex()
            .id("architect-empty-state")
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::ListTree)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(Label::new("No plan yet").color(Color::Muted))
            .child(
                div().max_w(px(420.0)).child(
                    Label::new(
                        "Describe what you want built in the chat. The steps will appear here as \
                         a flowchart you can rearrange, discuss one at a time, and lock when you \
                         are happy with them.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
            )
            // Plans are only drafted in Architect mode, and the mode pill beside the
            // composer is easy to miss. One action switches the mode if needed
            // and puts the cursor where the goal is typed.
            .child(
                Button::new("architect-start-planning", "Start planning")
                    .tab_index(0isize)
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
                    .tooltip(Tooltip::text(if planning {
                        "Open the plan conversation and describe the goal"
                    } else {
                        "Switch to Architect mode and describe the goal"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| this.start_planning(window, cx))),
            )
            .when(planning, |this| {
                this.child(
                    Label::new("Architect mode is on — the agent will draft before it builds.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .bg(cx.theme().colors().editor_background)
            .into_any()
    }

    fn render_empty_nested_plan(&self, cx: &Context<Self>) -> AnyElement {
        let running = self.is_running(cx);

        v_flex()
            .id("architect-empty-nested-plan")
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::ListTree)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(Label::new("Nothing inside this step yet").color(Color::Muted))
            .child(
                div().max_w(px(420.0)).child(
                    Label::new(
                        "Break this step into smaller steps that run in its place. Press Escape \
                         to go back to the plan that contains it.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
            )
            .child(
                Button::new("architect-add-nested-step", "Add Step")
                    .tab_index(0isize)
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .disabled(running)
                    .tooltip(Tooltip::text("Add the first step inside this one"))
                    .on_click(cx.listener(|this, _, window, cx| this.add_step(window, cx))),
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
        let active_color = cx.theme().status().info;
        let completed_color = cx.theme().status().success.opacity(0.7);
        let pending_color = theme.text_accent.opacity(0.7);
        // A loop is the one part of a plan that is not obvious from the shape
        // alone, so it is the one part given a colour of its own.
        let loop_color = cx.theme().status().warning.opacity(0.85);

        let running = self.running_node(cx);
        let mut curves = Vec::new();
        for edge in &graph.edges {
            let (Some(from), Some(to)) = (
                graph.node(&edge.from).and_then(|node| node.position),
                graph.node(&edge.to).and_then(|node| node.position),
            ) else {
                continue;
            };
            let selected = self.selection == Some(Selection::Edge(edge.id.clone()));
            let active = running == Some(&edge.to);
            let completed = graph
                .node(&edge.from)
                .and_then(|node| node.result.as_ref())
                .is_some_and(|result| !result.summary.trim().is_empty())
                && graph
                    .node(&edge.to)
                    .and_then(|node| node.result.as_ref())
                    .is_some_and(|result| !result.summary.trim().is_empty());
            let curve = EdgeCurve::between(from, to);
            let color = if selected {
                selected_color
            } else if active {
                active_color
            } else if completed {
                completed_color
            } else if curve.backwards {
                loop_color
            } else {
                edge_color
            };
            curves.push((curve, color, selected || active));
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
        let uses = self.connection_uses(cx);

        graph
            .edges
            .iter()
            .enumerate()
            .filter_map(|(ix, edge)| {
                // Only a loop can be taken more than once in a pass, and only
                // there is the count news.
                let taken = uses
                    .get(&(edge.from.clone(), edge.to.clone()))
                    .copied()
                    .filter(|_| edge.max_repeats.is_some() || graph.is_loop_edge(edge))
                    .unwrap_or(0);
                let from = graph.node(&edge.from).and_then(|node| node.position)?;
                let to = graph.node(&edge.to).and_then(|node| node.position)?;
                let curve = EdgeCurve::between(from, to);

                // Two things can be worth saying about a connection, and never
                // both: what has to be true for it to be taken, or, once it has
                // been taken, that the earlier step's summary went along it.
                let (icon, text, color, border, background) = match edge.condition.label() {
                    Some(label) => {
                        let judged = matches!(edge.condition, EdgeCondition::LlmEvaluated { .. });
                        let icon = judged.then_some(IconName::Sparkle);
                        if curve.backwards {
                            (
                                icon,
                                label.to_string(),
                                Color::Warning,
                                cx.theme().status().warning_border,
                                cx.theme().status().warning_background,
                            )
                        } else {
                            (
                                icon,
                                label.to_string(),
                                Color::Default,
                                cx.theme().colors().border,
                                cx.theme().colors().elevated_surface_background,
                            )
                        }
                    }
                    None if edge.max_repeats.is_some() => (
                        Some(IconName::RotateCcw),
                        String::new(),
                        Color::Warning,
                        cx.theme().status().warning_border,
                        cx.theme().status().warning_background,
                    ),
                    None => {
                        // Nothing to say about an unconditional connection until
                        // the step behind it has actually produced something.
                        graph.node(&edge.from).and_then(|node| {
                            node.result
                                .as_ref()
                                .filter(|result| !result.summary.trim().is_empty())
                        })?;
                        (
                            Some(IconName::Check),
                            "summary passed".to_string(),
                            Color::Success,
                            cx.theme().status().success_border,
                            cx.theme().status().success_background,
                        )
                    }
                };
                // A repeat limit is the one fact about a loop worth reading at a
                // glance, so it leads the label rather than being truncated off.
                // Once a run has gone round, how much of the limit it used
                // replaces the limit itself.
                let text = match (edge.max_repeats, taken) {
                    (Some(limit), 0) if text.is_empty() => format!("repeats up to {limit}×"),
                    (Some(limit), 0) => format!("up to {limit}× · {text}"),
                    (Some(limit), taken) if text.is_empty() => {
                        format!("repeated {taken} of {limit}")
                    }
                    (Some(limit), taken) => format!("{taken} of {limit} · {text}"),
                    (None, 0) => text,
                    (None, taken) => format!("{taken}× · {text}"),
                };

                let midpoint = curve.midpoint();
                let screen = self.to_screen(midpoint);
                // Absolute children are positioned within the container, so the
                // window origin has to come back out.
                let left = screen.x - bounds.origin.x;
                let top = screen.y - bounds.origin.y;

                let tooltip: SharedString = match edge.condition.label() {
                    Some(label) if matches!(edge.condition, EdgeCondition::LlmEvaluated { .. }) => {
                        format!("The agent decides: {label}").into()
                    }
                    Some(label) => format!("Taken only if {label}").into(),
                    None if edge.max_repeats.is_some() => {
                        "Always taken, until its repeat limit is spent".into()
                    }
                    None => graph
                        .node(&edge.from)
                        .and_then(|node| node.result.as_ref())
                        .map(|result| {
                            SharedString::from(format!(
                                "What the next step was told:\n{}",
                                result.summary
                            ))
                        })
                        .unwrap_or_else(|| {
                            "The summary of the earlier step went along here".into()
                        }),
                };
                let tooltip = match edge.max_repeats {
                    Some(limit) => SharedString::from(format!(
                        "{tooltip}\nTaken at most {limit} times per run."
                    )),
                    None => tooltip,
                };
                let tooltip = match (edge.max_repeats, taken) {
                    (_, 0) => tooltip,
                    (Some(limit), taken) if taken >= limit as usize => SharedString::from(format!(
                        "{tooltip}\nThe latest run used all {limit} repeats."
                    )),
                    (_, 1) => {
                        SharedString::from(format!("{tooltip}\nThe latest run took it once."))
                    }
                    (_, taken) => SharedString::from(format!(
                        "{tooltip}\nThe latest run took it {taken} times."
                    )),
                };

                Some(
                    h_flex()
                        .absolute()
                        .left(left - px(90.0))
                        .top(top - px(11.0))
                        .w(px(180.0))
                        .justify_center()
                        .child(
                            chip(truncate(&text, 32), icon, color, border, background)
                                .id(("architect-edge-label", ix))
                                .tooltip(Tooltip::text(tooltip)),
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
        let Some((nodes, invalid, step_numbers)) = self.graph(cx).map(|graph| {
            let invalid: HashMap<NodeId, &'static str> = graph
                .problems()
                .iter()
                .filter_map(|problem| match problem {
                    GraphProblem::Unreachable(id) => Some((id.clone(), "unreachable")),
                    GraphProblem::EndlessLoop(id) => Some((id.clone(), "endless loop")),
                    _ => None,
                })
                .collect();
            let step_numbers: HashMap<NodeId, usize> = graph
                .execution_order()
                .into_iter()
                .enumerate()
                .map(|(index, id)| (id, index + 1))
                .collect();
            (graph.nodes.clone(), invalid, step_numbers)
        }) else {
            return Vec::new();
        };
        let running = self.is_running(cx);
        let pane = cx.weak_entity();
        let bulk_menu = self.has_bulk_selection().then(|| self.bulk_counts(cx));

        nodes
            .into_iter()
            .enumerate()
            .filter_map(|(ix, node)| {
                let position = node.position?;
                let screen = self.to_screen(position);
                let (node_width, node_height) = self.node_size(&node);
                let width = px(node_width * self.zoom);
                let height = px(node_height * self.zoom);
                let left = screen.x - bounds.origin.x - width / 2.0;
                let top = screen.y - bounds.origin.y - height / 2.0;
                if !node_intersects_viewport(
                    left,
                    top,
                    width,
                    height,
                    bounds.size.width,
                    bounds.size.height,
                ) {
                    return None;
                }
                let problem = invalid.get(&node.id).copied();
                let step_number = step_numbers.get(&node.id).copied().unwrap_or(ix + 1);
                let menu = NodeMenu {
                    id: node.id.clone(),
                    locked: node.locked,
                    has_subplan: node.has_subplan(),
                    running,
                    bulk: bulk_menu.filter(|_| self.in_bulk_selection(&node.id)),
                };
                let card = self.render_node(ix, step_number, node, problem, cx);
                let pane = pane.clone();

                // The menu sizes its hit area from its child, and an absolutely
                // positioned child has no size to give it, so the menu sits
                // inside the positioned box around a card of definite size.
                Some(
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(width)
                        .h(height)
                        .child(
                            right_click_menu(("architect-node-menu", ix))
                                .trigger(move |_, _, _| div().w(width).h(height).child(card))
                                .menu(move |window, cx| menu.build(pane.clone(), window, cx)),
                        )
                        .into_any(),
                )
            })
            .collect()
    }

    /// The steps of a sub-plan, drawn inside the step that holds them.
    ///
    /// They are cards rather than a list because the point of expanding in place
    /// is to see the shape of the work without leaving the plan around it, and a
    /// list would read as an attribute of the parent instead of as steps.
    fn render_expanded_children(
        &self,
        ix: usize,
        node: &ArchitectNode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(subplan) = node.subplan() else {
            return div().into_any();
        };
        let running_here = self.thread.read(cx).architect_running_step().cloned();
        let shown = subplan.nodes.len().min(EXPANDED_CHILD_LIMIT);
        let hidden = subplan.nodes.len() - shown;
        let drill_id = node.id.clone();

        v_flex()
            .id(("architect-node-children", ix))
            .w_full()
            .flex_none()
            .gap_1()
            .p_1p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().editor_background.opacity(0.6))
            .cursor(CursorStyle::PointingHand)
            .tooltip(Tooltip::text(
                "The plan inside this step. Click to open it on its own.",
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.drill_into(drill_id.clone(), window, cx);
            }))
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .children(subplan.nodes.iter().take(shown).enumerate().map(
                        |(child_ix, child)| {
                            let child_running = running_here.as_ref().is_some_and(|path| {
                                path.0.last() == Some(&child.id)
                                    && path.0.len() > self.focus.0.len()
                            });
                            let child_done = child
                                .result
                                .as_ref()
                                .is_some_and(|result| !result.summary.trim().is_empty());

                            v_flex()
                                .id(("architect-child", ix * 100 + child_ix))
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .px_1p5()
                                .py_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(if child_running {
                                    cx.theme().status().info_border
                                } else if child.locked {
                                    cx.theme().colors().border
                                } else {
                                    cx.theme().colors().border_variant
                                })
                                .bg(cx.theme().colors().surface_background)
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_0p5()
                                        .min_w_0()
                                        .when(child_running, |this| {
                                            this.child(
                                                Icon::new(IconName::PlayFilled)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Info),
                                            )
                                        })
                                        .when(child_done && !child_running, |this| {
                                            this.child(
                                                Icon::new(IconName::Check)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Success),
                                            )
                                        })
                                        .child(
                                            Label::new(child.title.clone())
                                                .size(LabelSize::XSmall)
                                                .truncate(),
                                        ),
                                )
                                .child(
                                    Label::new(if child.intent.trim().is_empty() {
                                        "no goal yet".to_string()
                                    } else {
                                        truncate(&child.intent, 22)
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                                )
                        },
                    ))
                    .when(hidden > 0, |this| {
                        this.child(
                            div().flex_none().child(
                                Label::new(format!("+{hidden}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        )
                    }),
            )
            .child(
                Label::new("the plan inside — click to open it")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any()
    }

    fn render_node(
        &self,
        ix: usize,
        step_number: usize,
        node: ArchitectNode,
        problem: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let invalid = problem.is_some();
        let theme = cx.theme().colors().clone();
        let error_border = cx.theme().status().error_border;
        let selected = self.selection == Some(Selection::Node(node.id.clone()))
            || self.in_bulk_selection(&node.id);
        let hovered = self.hovered_node.as_ref() == Some(&node.id);
        let detailed = self.zoom >= DETAIL_ZOOM_THRESHOLD;
        let running = self.running_node(cx) == Some(&node.id);
        let run = self.thread.read(cx).architect_run();
        let run_active = run.is_some_and(agent::ArchitectRun::is_running);
        let failed =
            run.and_then(|run| run.outcome.as_ref())
                .is_some_and(|outcome| match outcome {
                    RunOutcome::NodeLimit { node: failed, .. }
                    | RunOutcome::DepthLimit { node: failed } => failed == &node.id,
                    _ => false,
                });
        let subplan_steps = node.subplan().map_or(0, |subplan| subplan.nodes.len());
        // Naming the steps inside without opening it: enough to tell two
        // sub-plans apart at a glance, without the canvas drawing a graph
        // inside a graph.
        let subplan_preview: SharedString = node
            .subplan()
            .map(|subplan| {
                let titles: Vec<&str> = subplan
                    .nodes
                    .iter()
                    .take(6)
                    .map(|node| node.title.as_str())
                    .collect();
                let more = subplan.nodes.len().saturating_sub(titles.len());
                let mut preview = titles.join("  →  ");
                if more > 0 {
                    preview.push_str(&format!("  →  … and {more} more"));
                }
                format!("Opens the plan inside this step:\n{preview}").into()
            })
            .unwrap_or_else(|| SharedString::from("This step contains a plan. Open it."));
        let drill_id = node.id.clone();
        let attempt = node.result.as_ref().map_or(1, |result| result.attempt);
        let has_summary = node
            .result
            .as_ref()
            .is_some_and(|result| !result.summary.trim().is_empty());
        let summary_preview: SharedString = node
            .result
            .as_ref()
            .map(|result| SharedString::from(result.summary.clone()))
            .unwrap_or_default();

        // The step being carried out outranks selection, because during a run
        // where the agent is now is the thing worth being able to find.
        let border_color = if invalid || failed {
            error_border
        } else if running {
            cx.theme().status().info_border
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
        let connect_id = node.id.clone();
        let expand_id = node.id.clone();
        let expanded = self.expanded.contains(&node.id);
        // A step that has finished is worth reading as finished before anything
        // else about it, so it is marked in the title rather than in the chips.
        let done = has_summary && !running;
        let queued = run_active && !running && !done && !failed;

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
            // A ring outside the card, so the step being carried out is findable
            // in a plan too big to take in at once. Drawn as a sibling rather
            // than a thicker border, which would move the contents.
            .when(running, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset(px(-5.0))
                        .rounded_lg()
                        .border_1()
                        .border_color(cx.theme().status().info_border.opacity(0.55)),
                )
            })
            .text_size(px((13.0 * self.zoom).clamp(9.0, 15.0)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    if event.modifiers.shift {
                        this.last_pressed_node = None;
                        this.toggle_in_bulk_selection(id.clone(), window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    // Pressing one of several selected steps drags them all.
                    if this.in_bulk_selection(&id) {
                        this.last_pressed_node = Some(id.clone());
                        this.start_group_drag(id.clone(), event.position, cx);
                        cx.stop_propagation();
                        return;
                    }

                    let repeated =
                        event.click_count >= 2 && this.last_pressed_node.as_ref() == Some(&id);
                    this.last_pressed_node = Some(id.clone());
                    this.set_selection(Some(Selection::Node(id.clone())), window, cx);

                    // Double-click opens the step's plan: the gesture people
                    // already try on a box that looks like it contains something.
                    // A settled step with no plan has none to open, and one
                    // cannot be started there, so it is only selected.
                    let settled_leaf = this
                        .graph(cx)
                        .and_then(|graph| graph.node(&id))
                        .is_some_and(|node| node.locked && !node.has_subplan());
                    if repeated && !settled_leaf {
                        this.drill_into(id.clone(), window, cx);
                        cx.stop_propagation();
                        return;
                    }

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
                                h_flex()
                                    .gap_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(
                                        Label::new(format!("{step_number:02}"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .when(running, |this| {
                                        this.child(
                                            Icon::new(IconName::PlayFilled)
                                                .size(IconSize::XSmall)
                                                .color(Color::Info),
                                        )
                                    })
                                    .when(done, |this| {
                                        this.child(
                                            Icon::new(IconName::Check)
                                                .size(IconSize::XSmall)
                                                .color(Color::Success),
                                        )
                                    })
                                    .child(
                                        Label::new(node.title.clone())
                                            .size(if detailed {
                                                LabelSize::Default
                                            } else {
                                                LabelSize::Small
                                            })
                                            .truncate(),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap_0p5()
                                    // Only a step with something inside can be
                                    // opened up, and only in detail: the cards
                                    // are unreadable at a distance anyway.
                                    .when(subplan_steps > 0 && detailed, |this| {
                                        this.child(
                                            IconButton::new(
                                                ("architect-node-expand", ix),
                                                if expanded {
                                                    IconName::ChevronUp
                                                } else {
                                                    IconName::ChevronDown
                                                },
                                            )
                                            .tab_index(0isize)
                                            .icon_size(IconSize::XSmall)
                                            .icon_color(Color::Muted)
                                            .tooltip(Tooltip::text(if expanded {
                                                "Fold the steps inside back up"
                                            } else {
                                                "Show the steps inside, here"
                                            }))
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    if !this.expanded.remove(&expand_id) {
                                                        this.expanded.insert(expand_id.clone());
                                                    }
                                                    cx.notify();
                                                }),
                                            ),
                                        )
                                    })
                                    .child(
                                        Label::new(if running {
                                            "running"
                                        } else if failed {
                                            "failed"
                                        } else if done {
                                            "completed"
                                        } else if queued {
                                            "queued"
                                        } else if node.locked {
                                            "settled"
                                        } else {
                                            "draft"
                                        })
                                        .size(LabelSize::XSmall)
                                        .color(
                                            if running {
                                                Color::Info
                                            } else if failed {
                                                Color::Error
                                            } else if done || node.locked {
                                                Color::Success
                                            } else {
                                                Color::Muted
                                            },
                                        ),
                                    ),
                            ),
                    )
                    .when(detailed, |this| {
                        this.child(
                            Label::new(if node.responsibility.trim().is_empty() {
                                "Responsibility not set".to_string()
                            } else {
                                node.responsibility.clone()
                            })
                            .size(LabelSize::XSmall)
                            .color(if node.responsibility.trim().is_empty() {
                                Color::Placeholder
                            } else {
                                Color::Accent
                            })
                            .truncate(),
                        )
                        .child(
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
                        // The steps inside, drawn inside. One level only: a
                        // graph nested in a graph is unreadable, and drilling in
                        // is what goes deeper.
                        .when(expanded, |this| {
                            this.child(self.render_expanded_children(ix, &node, cx))
                        })
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_1p5()
                                .when_some(problem, |this, problem| {
                                    this.child(chip(
                                        problem,
                                        Some(IconName::Warning),
                                        Color::Error,
                                        cx.theme().status().error_border,
                                        cx.theme().status().error_background,
                                    ))
                                })
                                .when(subplan_steps > 0 && !expanded, |this| {
                                    this.child(
                                        div()
                                            .id(("architect-node-open-subplan", ix))
                                            .cursor(CursorStyle::PointingHand)
                                            .tooltip(Tooltip::text(subplan_preview.clone()))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.drill_into(drill_id.clone(), window, cx);
                                            }))
                                            .child(
                                                chip(
                                                    match subplan_steps {
                                                        1 => "1 step".to_string(),
                                                        count => format!("{count} steps"),
                                                    },
                                                    Some(IconName::ListTree),
                                                    Color::Accent,
                                                    cx.theme().status().info_border,
                                                    cx.theme().status().info_background,
                                                )
                                                // Says the chip goes somewhere,
                                                // which a count alone does not.
                                                .child(
                                                    Icon::new(IconName::ChevronRight)
                                                        .size(IconSize::XSmall)
                                                        .color(Color::Accent),
                                                ),
                                            ),
                                    )
                                })
                                .when(has_summary, |this| {
                                    this.child(
                                        div()
                                            .id(("architect-node-summary", ix))
                                            .tooltip(Tooltip::text(summary_preview.clone()))
                                            .child(chip(
                                                "summary",
                                                Some(IconName::Check),
                                                Color::Success,
                                                cx.theme().status().success_border,
                                                cx.theme().status().success_background,
                                            )),
                                    )
                                }),
                        )
                    }),
            )
            // A repeated step is the plan not going to plan, so the count rides
            // the corner of the card rather than queueing with the chips inside
            // it, where it would read as one more detail.
            .when(attempt > 1 && detailed, |this| {
                this.child(
                    div()
                        .id(("architect-node-attempt", ix))
                        .absolute()
                        .top(px(-10.0))
                        .right(px(10.0))
                        .tooltip(Tooltip::text(format!(
                            "A loop has brought this step round {attempt} times"
                        )))
                        .child(chip(
                            format!("attempt {attempt}"),
                            None,
                            Color::Warning,
                            cx.theme().status().warning_border,
                            cx.theme().status().warning_background,
                        )),
                )
            })
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

    fn render_graph_workspace(
        &self,
        has_plan: bool,
        show_navigation: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let command_bar = self.render_canvas_command_bar(show_navigation, cx);
        let edges = self.render_edges(cx);
        let edge_labels = self.render_edge_labels(cx);
        let nodes = self.render_nodes(cx);
        let minimap = self.render_minimap(cx);
        let zoom_control = self.render_zoom_control(cx);
        let marquee = self.render_marquee(cx);

        v_flex()
            .id("architect-graph-workspace")
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_hidden()
            .child(command_bar)
            .child(if has_plan {
                div()
                    .id("architect-canvas")
                    .relative()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_hidden()
                    .cursor(match self.interaction {
                        Interaction::None => CursorStyle::Arrow,
                        Interaction::Selecting { .. } => CursorStyle::Crosshair,
                        _ => CursorStyle::ClosedHand,
                    })
                    .on_scroll_wheel(cx.listener(Self::handle_scroll))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
                    .on_mouse_move(cx.listener(Self::handle_mouse_move))
                    .child(edges)
                    .children(edge_labels)
                    .children(nodes)
                    .children(marquee)
                    .children(minimap)
                    .child(zoom_control)
                    .into_any()
            } else {
                div()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .child(self.render_empty_state(cx))
                    .into_any()
            })
            .into_any()
    }
}

type NodeAction = fn(&mut ArchitectPane, NodeId, &mut Window, &mut Context<ArchitectPane>);

/// What a step's right-click menu offers, decided when the canvas renders so
/// the menu never lists an action the step cannot take.
#[derive(Clone)]
struct NodeMenu {
    id: NodeId,
    locked: bool,
    has_subplan: bool,
    running: bool,
    /// Set when the step is one of several selected, whose menu acts on all
    /// of them.
    bulk: Option<BulkCounts>,
}

type PaneAction = fn(&mut ArchitectPane, &mut Window, &mut Context<ArchitectPane>);

impl NodeMenu {
    fn build(
        &self,
        pane: WeakEntity<ArchitectPane>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        let menu = self.clone();
        if let Some(bulk) = menu.bulk {
            return Self::build_bulk(bulk, menu.running, pane, window, cx);
        }
        ContextMenu::build(window, cx, move |context_menu, _, _| {
            let action = |run: NodeAction| {
                let pane = pane.clone();
                let id = menu.id.clone();
                move |window: &mut Window, cx: &mut App| {
                    pane.update(cx, |pane, cx| {
                        pane.last_pressed_node = None;
                        pane.set_selection(Some(Selection::Node(id.clone())), window, cx);
                        run(pane, id.clone(), window, cx);
                    })
                    .log_err();
                }
            };
            let can_edit = !menu.running;
            let can_break_down = can_edit && !menu.locked;
            context_menu
                .when(menu.has_subplan || can_break_down, |this| {
                    this.entry(
                        if menu.has_subplan {
                            "Open Nested Plan"
                        } else {
                            "Break Into Steps"
                        },
                        None,
                        action(|pane, id, window, cx| pane.drill_into(id, window, cx)),
                    )
                })
                .entry(
                    "Discuss This Step",
                    None,
                    action(|pane, id, window, cx| pane.discuss_node(id, window, cx)),
                )
                .when(can_edit, |this| {
                    this.entry(
                        format!("Duplicate ({DUPLICATE_SHORTCUT})"),
                        None,
                        action(|pane, _, window, cx| pane.duplicate_selection(window, cx)),
                    )
                    .entry(
                        if menu.locked { "Unlock" } else { "Lock" },
                        None,
                        action(|pane, id, window, cx| pane.toggle_lock(id, window, cx)),
                    )
                })
                .when(can_break_down, |this| {
                    this.separator().entry(
                        "Delete",
                        None,
                        action(|pane, _, window, cx| pane.delete_selection(window, cx)),
                    )
                })
        })
    }

    /// The menu for a step that is one of several selected. Its actions apply
    /// to every selected step, and leave the selection as it is.
    fn build_bulk(
        bulk: BulkCounts,
        running: bool,
        pane: WeakEntity<ArchitectPane>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, move |context_menu, _, _| {
            let action = |run: PaneAction| {
                let pane = pane.clone();
                move |window: &mut Window, cx: &mut App| {
                    pane.update(cx, |pane, cx| run(pane, window, cx)).log_err();
                }
            };
            let can_change = !running && bulk.editable;
            context_menu
                .when(can_change && bulk.lockable > 0, |this| {
                    this.entry(
                        format!("Lock {}", bulk::count_label(bulk.lockable)),
                        None,
                        action(|pane, _, cx| pane.lock_bulk_selection(true, cx)),
                    )
                })
                .when(can_change && bulk.locked > 0, |this| {
                    this.entry(
                        format!("Unlock {}", bulk::count_label(bulk.locked)),
                        None,
                        action(|pane, _, cx| pane.lock_bulk_selection(false, cx)),
                    )
                })
                .entry(
                    "Clear Selection",
                    None,
                    action(|pane, window, cx| pane.set_selection(None, window, cx)),
                )
                .when(can_change && bulk.drafts > 0, |this| {
                    this.separator().entry(
                        format!("Delete {}", bulk::count_label(bulk.drafts)),
                        None,
                        action(|pane, window, cx| pane.delete_bulk_selection(window, cx)),
                    )
                })
        })
    }
}

/// A small tinted label. Nodes have room for about three words, so the canvas
/// says things with these rather than with sentences.
/// A small tinted badge. Returns a `Div` rather than an opaque element so a
/// caller can append its own trailing content, such as the chevron that marks a
/// chip as something to click.
pub(super) fn chip(
    label: impl Into<SharedString>,
    icon: Option<IconName>,
    color: Color,
    border: Hsla,
    background: Hsla,
) -> Div {
    h_flex()
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(background)
        .children(icon.map(|icon| Icon::new(icon).size(IconSize::XSmall).color(color)))
        .child(Label::new(label).size(LabelSize::XSmall).color(color))
}

pub(super) fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// Short enough for the run bar, precise enough to tell a slow run from a
/// stuck one.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

impl Render for ArchitectPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.mode != ArchitectWorkspaceMode::Architect {
            return div().size_full().into_any_element();
        }

        let has_plan = self.graph(cx).is_some_and(|graph| !graph.is_empty());
        let viewport_width = window.viewport_size().width;
        let layout = ArchitectLayout::for_width(viewport_width);
        let inline_outline = !matches!(layout, ArchitectLayout::Compact);
        let inline_inspector = matches!(layout, ArchitectLayout::Wide | ArchitectLayout::Medium);
        let outline_width = match layout {
            ArchitectLayout::Wide => self.outline_width,
            ArchitectLayout::Medium | ArchitectLayout::Narrow => px(204.0),
            ArchitectLayout::Compact => px((f32::from(viewport_width) - 24.0).clamp(240.0, 320.0)),
        };
        let inspector_width = match layout {
            ArchitectLayout::Wide => self.inspector_width,
            ArchitectLayout::Medium => px(312.0),
            ArchitectLayout::Narrow => px(348.0),
            ArchitectLayout::Compact => px((f32::from(viewport_width) - 24.0).clamp(280.0, 348.0)),
        };
        let header = self.render_plan_header(matches!(layout, ArchitectLayout::Compact), cx);
        let run_bar = self.render_run_bar(cx);
        let graph =
            self.render_graph_workspace(has_plan, matches!(layout, ArchitectLayout::Compact), cx);
        let outline = inline_outline.then(|| self.render_outline(outline_width, cx));
        let inspector = inline_inspector
            .then(|| self.render_inspector(inspector_width, cx))
            .flatten();
        let outline_drawer = (!inline_outline && self.outline_drawer_open).then(|| {
            div()
                .id("architect-outline-drawer")
                .absolute()
                .left(px(0.0))
                .top(px(0.0))
                .bottom(px(0.0))
                .w(outline_width)
                .shadow_lg()
                .child(self.render_outline(outline_width, cx))
                .child(
                    div().absolute().right(px(8.0)).top(px(8.0)).child(
                        IconButton::new("architect-close-outline", IconName::Close)
                            .tab_index(0isize)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close plan navigation"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_outline_drawer(window, cx);
                            })),
                    ),
                )
                .into_any()
        });
        let inspector_drawer = (!inline_inspector && self.inspector_drawer_open).then(|| {
            div()
                .id("architect-inspector-drawer")
                .absolute()
                .right(px(0.0))
                .top(px(0.0))
                .bottom(px(0.0))
                .w(inspector_width + px(8.0))
                .shadow_lg()
                .children(self.render_inspector(inspector_width, cx))
                .child(
                    div().absolute().right(px(14.0)).top(px(14.0)).child(
                        IconButton::new("architect-close-inspector", IconName::Close)
                            .tab_index(0isize)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close inspector"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_inspector_drawer(window, cx);
                            })),
                    ),
                )
                .into_any()
        });

        let resizable = matches!(layout, ArchitectLayout::Wide);
        let outline_divider = (resizable && outline.is_some())
            .then(|| self.render_divider(ArchitectDivider::Outline, cx));
        let inspector_divider = (resizable && inspector.is_some())
            .then(|| self.render_divider(ArchitectDivider::Inspector, cx));

        v_flex()
            .id("architect-pane")
            .key_context("ArchitectPane")
            .on_drag_move(cx.listener(Self::handle_divider_drag))
            .on_drop(cx.listener(|this, _: &DraggedArchitectDivider, _, cx| {
                this.persist_layout(cx);
            }))
            .tab_group()
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(header)
            .children(run_bar)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .size_full()
                            .overflow_hidden()
                            .children(outline)
                            .children(outline_divider)
                            .child(graph)
                            .children(inspector_divider)
                            .children(inspector),
                    )
                    .children(outline_drawer)
                    .children(inspector_drawer),
            )
            .into_any_element()
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

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.thread
            .read(cx)
            .title()
            .map(|title| format!("Architect · {}", truncate(&title, 48)).into())
            .unwrap_or_else(|| "Architect".into())
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        self.thread
            .read(cx)
            .title()
            .map(|title| format!("Architect plan: {title}").into())
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree))
    }

    fn show_in_tab_bar(&self, _cx: &App) -> bool {
        false
    }

    fn allows_workspace_docks(&self, _cx: &App) -> bool {
        self.mode == ArchitectWorkspaceMode::Code
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Architect Canvas Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn preserve_preview(&self, _cx: &App) -> bool {
        true
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.contains_focused(window, cx) {
            self.last_architect_focus = window.focused(cx);
        }
    }

    fn workspace_deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus_handle.contains_focused(window, cx) {
            self.last_architect_focus = window.focused(cx);
        }
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        self.restore_after_removal(cx);
    }

    fn include_in_nav_history() -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_actions_have_one_rendering_owner() {
        let source = include_str!("rendering.rs");
        for suffix in ["fit", "run", "stop", "plan-conversation"] {
            let id = format!("\"architect-{suffix}\"");
            assert_eq!(
                source.matches(&id).count(),
                1,
                "{id} must have exactly one rendering owner"
            );
        }
    }

    #[test]
    fn responsive_layout_uses_all_four_shell_states() {
        assert_eq!(
            ArchitectLayout::for_width(px(1500.0)),
            ArchitectLayout::Wide
        );
        assert_eq!(
            ArchitectLayout::for_width(px(1200.0)),
            ArchitectLayout::Medium
        );
        assert_eq!(
            ArchitectLayout::for_width(px(900.0)),
            ArchitectLayout::Narrow
        );
        assert_eq!(
            ArchitectLayout::for_width(px(600.0)),
            ArchitectLayout::Compact
        );
    }

    #[test]
    fn node_culling_keeps_overscan_and_rejects_far_off_nodes() {
        let viewport_width = px(1500.0);
        let viewport_height = px(900.0);
        let node_width = px(240.0);
        let node_height = px(160.0);

        assert!(node_intersects_viewport(
            px(0.0),
            px(0.0),
            node_width,
            node_height,
            viewport_width,
            viewport_height,
        ));
        assert!(node_intersects_viewport(
            px(-300.0),
            px(0.0),
            node_width,
            node_height,
            viewport_width,
            viewport_height,
        ));
        assert!(!node_intersects_viewport(
            px(-400.0),
            px(0.0),
            node_width,
            node_height,
            viewport_width,
            viewport_height,
        ));
        assert!(!node_intersects_viewport(
            px(1600.0),
            px(0.0),
            node_width,
            node_height,
            viewport_width,
            viewport_height,
        ));
        assert!(!node_intersects_viewport(
            px(0.0),
            px(1000.0),
            node_width,
            node_height,
            viewport_width,
            viewport_height,
        ));
    }

    #[test]
    fn truncation_preserves_unicode_boundaries_and_marks_overflow() {
        assert_eq!(truncate("Short plan", 20), "Short plan");
        assert_eq!(truncate("架構設計與驗證流程", 6), "架構設計與…");
        assert_eq!(truncate("🙂🙂🙂🙂", 3), "🙂🙂…");
    }

    #[test]
    fn elapsed_time_reads_naturally_at_every_scale() {
        use std::time::Duration;
        assert_eq!(format_elapsed(Duration::from_millis(900)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(754)), "12m 34s");
        assert_eq!(format_elapsed(Duration::from_secs(3_725)), "1h 02m");
    }
}
