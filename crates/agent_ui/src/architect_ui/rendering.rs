use architect::{
    ArchitectGraph, ArchitectNode, EdgeCondition, GraphProblem, NodeId, NodePath, Position,
    RunOutcome,
};
use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    MouseButton, MouseDownEvent, PathBuilder, Pixels, Render, SharedString, Window, canvas, div,
    point, px,
};
use ui::{TintColor, Tooltip, prelude::*};
use workspace::{
    ShareProject,
    item::{Item, ItemEvent},
};

use super::geometry::{EdgeCurve, NODE_WIDTH, paint_curve};
use super::{
    ArchitectPane, DETAIL_ZOOM_THRESHOLD, EXPANDED_CHILD_LIMIT, Interaction, MAX_ZOOM, MIN_ZOOM,
    Selection,
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

impl ArchitectPane {
    // -- Rendering ------------------------------------------------------------

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
                Icon::new(IconName::GitBranch)
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
        let problems: Vec<GraphProblem> = root
            .map(ArchitectGraph::blocking_problems)
            .unwrap_or_default();
        let all_locked = step_count > 0 && locked_count == step_count;
        let ready_to_run = root.is_some_and(ArchitectGraph::is_fully_locked_deeply)
            && root.is_some_and(|root| root.blocking_problems().is_empty());
        let run_tooltip = if ready_to_run {
            "Run the plan, one step at a time"
        } else if step_count == 0 {
            "There is no plan yet"
        } else if !problems.is_empty() {
            "Fix the problems with the plan first"
        } else {
            "Lock every step first"
        };

        let running = self.is_running(cx);
        let run_status: Option<SharedString> =
            self.thread
                .read(cx)
                .architect_run()
                .and_then(|run| match &run.outcome {
                    Some(outcome) => root.map(|root| outcome.describe(root).into()),
                    None => Some(
                        format!(
                            "Step {} of {} · {}",
                            run.step_number, run_step_count, run.current_title
                        )
                        .into(),
                    ),
                });

        // One line, in the order the plan is read: where you are, what it is,
        // then what is being done to it.
        let nested_count = graph.map_or(0, |graph| {
            graph.nodes.iter().filter(|node| node.has_subplan()).count()
        });
        let mut summary = match step_count {
            0 => "no steps yet".to_string(),
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
        let readiness = if running {
            "Running"
        } else if ready_to_run {
            "Ready to run"
        } else if step_count == 0 {
            "Waiting for a plan"
        } else {
            "Needs review"
        };

        h_flex()
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
                                } else {
                                    IconName::Warning
                                }),
                                if running {
                                    Color::Info
                                } else if ready_to_run {
                                    Color::Success
                                } else {
                                    Color::Warning
                                },
                                if running {
                                    cx.theme().status().info_border
                                } else if ready_to_run {
                                    cx.theme().status().success_border
                                } else {
                                    cx.theme().status().warning_border
                                },
                                if running {
                                    cx.theme().status().info_background
                                } else if ready_to_run {
                                    cx.theme().status().success_background
                                } else {
                                    cx.theme().status().warning_background
                                },
                            )),
                    )
                    .when(!compact, |this| {
                        this.child(
                            Label::new(format!("{summary} · live workspace state"))
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
                    .when(!compact, |this| {
                        this.child(
                            Button::new("architect-share", "Share")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(
                                    Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall),
                                )
                                .tooltip(Tooltip::text("Share this project with collaborators"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ShareProject), cx);
                                }),
                        )
                    })
                    .when(!compact, |this| {
                        this.child(
                            Button::new("architect-review", "Review Plan")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::ListTodo).size(IconSize::XSmall))
                                .disabled(step_count == 0)
                                .tooltip(Tooltip::text("Focus the first step that needs attention"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.review_plan(window, cx)),
                                ),
                        )
                    })
                    .when(compact, |this| {
                        this.child(
                            IconButton::new("architect-share-compact", IconName::ArrowUpRight)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Share this project with collaborators"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ShareProject), cx);
                                }),
                        )
                        .child(
                            IconButton::new("architect-review-compact", IconName::ListTodo)
                                .icon_size(IconSize::Small)
                                .disabled(step_count == 0)
                                .tooltip(Tooltip::text("Focus the first step that needs attention"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.review_plan(window, cx)),
                                ),
                        )
                    })
                    .child(if running {
                        Button::new("architect-stop", "Stop")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(TintColor::Warning))
                            .start_icon(Icon::new(IconName::Stop).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Stop the run and the turn it is waiting on"))
                            .on_click(cx.listener(|this, _, _, cx| this.stop_run(cx)))
                    } else {
                        Button::new("architect-run", "Run")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::XSmall))
                            .disabled(!ready_to_run)
                            .tooltip(Tooltip::text(run_tooltip))
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx)))
                    })
                    .child(
                        Button::new("architect-open-code", "Code")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::Code).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Return to the Code workspace"))
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
        let breadcrumb = self.render_breadcrumb(cx);

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
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::ListTree).size(IconSize::XSmall))
                                .tooltip(Tooltip::text("Open plan navigation"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.outline_drawer_open = true;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(breadcrumb),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_none()
                    .when(!show_navigation, |this| {
                        this.child(
                            Button::new("architect-search", "Search")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(
                                    Icon::new(IconName::MagnifyingGlass).size(IconSize::XSmall),
                                )
                                .disabled(self.graph(cx).is_none_or(ArchitectGraph::is_empty))
                                .tooltip(Tooltip::text(
                                    "Open the ordered plan navigator to find a step",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.outline_drawer_open = true;
                                    this.search_editor.focus_handle(cx).focus(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("architect-tidy", "Tidy")
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .start_icon(Icon::new(IconName::RotateCw).size(IconSize::XSmall))
                                .disabled(running)
                                .tooltip(Tooltip::text("Tidy up the graph layout"))
                                .on_click(cx.listener(|this, _, _, cx| this.tidy_up(cx))),
                        )
                        .child(
                            Button::new("architect-add-step", "Add Step")
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
                    .when(show_navigation, |this| {
                        this.child(
                            IconButton::new("architect-search-compact", IconName::MagnifyingGlass)
                                .icon_size(IconSize::Small)
                                .disabled(self.graph(cx).is_none_or(ArchitectGraph::is_empty))
                                .tooltip(Tooltip::text("Search plan steps"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.outline_drawer_open = true;
                                    this.search_editor.focus_handle(cx).focus(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            IconButton::new("architect-tidy-compact", IconName::RotateCw)
                                .icon_size(IconSize::Small)
                                .disabled(running)
                                .tooltip(Tooltip::text("Tidy up the graph layout"))
                                .on_click(cx.listener(|this, _, _, cx| this.tidy_up(cx))),
                        )
                        .child(
                            IconButton::new("architect-add-step-compact", IconName::Plus)
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
        let blocking_problems = graph
            .map(ArchitectGraph::blocking_problems)
            .unwrap_or_default();
        let incomplete_handoffs = graph
            .map(ArchitectGraph::steps_without_capture)
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
        let ready = !all_ordered_nodes.is_empty() && blocking_problems.is_empty();

        v_flex()
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
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .child(chip(
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
                                    .child(
                                        IconButton::new(
                                            "architect-outline-narrower",
                                            IconName::Dash,
                                        )
                                        .icon_size(IconSize::XSmall)
                                        .tooltip(Tooltip::text("Make the plan outline narrower"))
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.resize_outline(-16.0, cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        IconButton::new("architect-outline-wider", IconName::Plus)
                                            .icon_size(IconSize::XSmall)
                                            .tooltip(Tooltip::text("Make the plan outline wider"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.resize_outline(16.0, cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(format!("{} settled", settled))
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
                    .child(
                        Button::new("architect-plan-conversation", "Plan Conversation")
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
                                        |(index, problem)| {
                                            let selection = Self::problem_selection(problem);
                                            Button::new(
                                                ("architect-readiness-problem", index),
                                                truncate(&problem.to_string(), 42),
                                            )
                                            .full_width()
                                            .label_size(LabelSize::XSmall)
                                            .style(ButtonStyle::Subtle)
                                            .start_icon(
                                                Icon::new(IconName::Warning)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Warning),
                                            )
                                            .tooltip(Tooltip::text(problem.to_string()))
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
                                        |(index, id)| {
                                            let id = id.clone();
                                            Button::new(
                                                ("architect-readiness-handoff", index),
                                                "Add a handoff summary",
                                            )
                                            .full_width()
                                            .label_size(LabelSize::XSmall)
                                            .style(ButtonStyle::Subtle)
                                            .start_icon(
                                                Icon::new(IconName::ArrowRight)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Warning),
                                            )
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.set_selection(
                                                        Some(Selection::Node(id.clone())),
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
                                    let id = node.id.clone();
                                    let is_selected = selected == Some(&node.id);
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
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.set_selection(
                                                Some(Selection::Node(id.clone())),
                                                window,
                                                cx,
                                            )
                                        }))
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .min_w_0()
                                                .child(
                                                    Label::new(format!("{:02}", index + 1))
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(node.title)
                                                        .size(LabelSize::Small)
                                                        .truncate(),
                                                ),
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
                .map(|graph| outcome.describe(graph).into())
                .unwrap_or_else(|| SharedString::from("Run finished")),
            None => format!("Running {}", run.current_title).into(),
        };
        let succeeded = run.outcome.as_ref().is_some_and(RunOutcome::is_success);
        let cancelled = matches!(run.outcome.as_ref(), Some(RunOutcome::Cancelled));
        let failed = run.outcome.is_some() && !succeeded && !cancelled;
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
                        .min_w_0()
                        .gap_0p5()
                        .child(Label::new(status).size(LabelSize::Small).truncate())
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
                .child(div().flex_1())
                .child(
                    Label::new(format!(
                        "{} of {} · {}s",
                        completed.min(total),
                        total,
                        elapsed.as_secs()
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
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
        let scale = ((WIDTH - PADDING * 2.0) / span_x).min((HEIGHT - PADDING * 2.0) / span_y);

        let running = self.running_node(cx).cloned();
        let selected = match &self.selection {
            Some(Selection::Node(id)) => Some(id.clone()),
            _ => None,
        };

        let blocks = positions.into_iter().map(|(id, position)| {
            let is_running = running.as_ref() == Some(&id);
            let is_selected = selected.as_ref() == Some(&id);
            div()
                .absolute()
                .left(px(PADDING + (position.x - min_x) * scale - 4.0))
                .top(px(PADDING + (position.y - min_y) * scale - 2.5))
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
                .absolute()
                .right(px(16.0))
                .bottom(px(16.0))
                .w(px(WIDTH))
                .h(px(HEIGHT))
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background.opacity(0.9))
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
                    .icon_size(IconSize::XSmall)
                    .disabled(zoom <= MIN_ZOOM + f32::EPSILON)
                    .tooltip(Tooltip::text("Zoom out"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_zoom(zoom / 1.2, None, cx)),
                    ),
            )
            .child(
                Button::new("architect-zoom-reset", format!("{:.0}%", zoom * 100.0))
                    .label_size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .style(ButtonStyle::Subtle)
                    .tooltip(Tooltip::text("Back to actual size"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_zoom(1.0, None, cx))),
            )
            .child(
                Button::new("architect-fit", "Fit")
                    .label_size(LabelSize::XSmall)
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Maximize).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Fit the entire plan in the graph workspace"))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_to_fit(cx))),
            )
            .child(
                IconButton::new("architect-zoom-in", IconName::Plus)
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
        let planning = self.thread.read(cx).session_mode() == agent::SessionMode::Plan;

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
                        "Describe what you want built in the chat. The steps will appear here as \
                         a flowchart you can rearrange, discuss one at a time, and lock when you \
                         are happy with them.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
            )
            // Plans are only drafted in Plan mode, and the mode pill beside the
            // composer is easy to miss. Offering the switch on the empty canvas
            // means the canvas explains what it needs to fill itself.
            .child(if planning {
                Label::new("Plan mode is on — the agent will draft before it builds.")
                    .size(LabelSize::Small)
                    .color(Color::Accent)
                    .into_any_element()
            } else {
                Button::new("architect-switch-to-plan", "Switch to Plan mode")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .start_icon(Icon::new(IconName::ListTodo).size(IconSize::Small))
                    .tooltip(Tooltip::text(
                        "Withholds the tools that change the project, so the agent plans \
                         instead of building.",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.thread.update(cx, |thread, cx| {
                            thread.set_session_mode(agent::SessionMode::Plan, cx)
                        });
                    }))
                    .into_any_element()
            })
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

        graph
            .edges
            .iter()
            .enumerate()
            .filter_map(|(ix, edge)| {
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

                let midpoint = curve.midpoint();
                let screen = self.to_screen(midpoint);
                // Absolute children are positioned within the container, so the
                // window origin has to come back out.
                let left = screen.x - bounds.origin.x;
                let top = screen.y - bounds.origin.y;

                let tooltip: SharedString = match edge.condition.label() {
                    Some(label) if matches!(edge.condition, EdgeCondition::LlmEvaluated { .. }) => {
                        format!("The model decides: {label}").into()
                    }
                    Some(label) => format!("Taken only if {label}").into(),
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
        let Some((nodes, invalid, execution_order)) = self.graph(cx).map(|graph| {
            let invalid: Vec<NodeId> = graph
                .problems()
                .iter()
                .filter_map(|problem| match problem {
                    GraphProblem::Unreachable(id) => Some(id.clone()),
                    _ => None,
                })
                .collect();
            (graph.nodes.clone(), invalid, graph.execution_order())
        }) else {
            return Vec::new();
        };

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
                let is_invalid = invalid.contains(&node.id);
                let step_number = execution_order
                    .iter()
                    .position(|id| id == &node.id)
                    .map(|index| index + 1)
                    .unwrap_or(ix + 1);

                Some(
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(width)
                        .h(height)
                        .child(self.render_node(ix, step_number, node, is_invalid, cx))
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
        invalid: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors().clone();
        let error_border = cx.theme().status().error_border;
        let selected = self.selection == Some(Selection::Node(node.id.clone()));
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
                    this.set_selection(Some(Selection::Node(id.clone())), window, cx);

                    if event.click_count >= 2 {
                        this.handle_node_click(id.clone(), event.click_count, window, cx);
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
                            .color(Color::Accent)
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
                                .when(invalid, |this| {
                                    this.child(chip(
                                        "unreachable",
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

        v_flex()
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

impl Render for ArchitectPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close plan navigation"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.outline_drawer_open = false;
                                this.focus_handle.focus(window, cx);
                                cx.notify();
                            })),
                    ),
                )
                .into_any()
        });
        let inspector_drawer = (!inline_inspector && self.inspector_drawer_open).then(|| {
            div()
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
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close inspector"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.inspector_drawer_open = false;
                                this.focus_handle.focus(window, cx);
                                cx.notify();
                            })),
                    ),
                )
                .into_any()
        });

        v_flex()
            .key_context("ArchitectPane")
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
                            .child(graph)
                            .children(inspector),
                    )
                    .children(outline_drawer)
                    .children(inspector_drawer),
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

    fn preserve_preview(&self, _cx: &App) -> bool {
        true
    }

    fn discarded(
        &self,
        _project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_after_discard(window, cx);
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
}
