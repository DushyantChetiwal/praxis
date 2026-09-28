//! Working on several steps at once. Shift-click a step, or shift-drag a
//! rectangle over empty canvas, to gather steps; then lock, unlock, delete, or
//! drag them together. Each of those is one change to undo.

use std::collections::HashSet;

use architect::{ArchitectGraph, GraphMutationError, NodeId, NodePath, Position};
use gpui::{AnyElement, Context, Pixels, Point, SharedString, Window, px};
use ui::{Tooltip, prelude::*};

use super::geometry::snap;
use super::{ArchitectPane, Interaction, Selection, UNDO_SHORTCUT, UndoGroup};

/// Below this, a shift-press on empty canvas was a click rather than a
/// rectangle, and selects nothing.
const MARQUEE_THRESHOLD: f32 = 4.0;

/// How many selected steps the inspector names before summarizing the rest.
const LISTED_STEPS: usize = 8;

pub(super) fn count_label(count: usize) -> String {
    if count == 1 {
        "1 step".to_string()
    } else {
        format!("{count} steps")
    }
}

/// Applies every edit or none of them. The plan is edited in place, so a
/// refusal partway through would otherwise leave half a change behind with no
/// undo entry for it.
fn all_or_nothing(
    graph: &mut ArchitectGraph,
    edit: impl FnOnce(&mut ArchitectGraph) -> Result<(), GraphMutationError>,
) -> Result<(), GraphMutationError> {
    let mut next = graph.clone();
    edit(&mut next)?;
    *graph = next;
    Ok(())
}

/// What the selected steps allow, so every surface offers the same actions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BulkCounts {
    pub locked: usize,
    pub drafts: usize,
    /// False inside a locked step, whose plan cannot be changed from here.
    pub editable: bool,
}

impl ArchitectPane {
    /// Whether the plan on screen can be changed, which it cannot while any
    /// step containing it is locked.
    pub(super) fn focus_is_editable(&self, cx: &Context<Self>) -> bool {
        let Some(mut graph) = self.root_graph(cx) else {
            return false;
        };
        for id in self.focus.iter() {
            let Some(node) = graph.node(id) else {
                return false;
            };
            if node.locked {
                return false;
            }
            let Some(subplan) = node.subplan.as_deref() else {
                return false;
            };
            graph = subplan;
        }
        true
    }

    pub(super) fn bulk_counts(&self, cx: &Context<Self>) -> BulkCounts {
        let Some(graph) = self.graph(cx) else {
            return BulkCounts::default();
        };
        let mut counts = BulkCounts {
            editable: self.focus_is_editable(cx),
            ..BulkCounts::default()
        };
        for node in self.bulk.iter().filter_map(|id| graph.node(id)) {
            if node.locked {
                counts.locked += 1;
            } else {
                counts.drafts += 1;
            }
        }
        counts
    }

    /// Drops steps that are no longer in the plan on screen, as after the agent
    /// or Tidy changed it. Fewer than two left is no longer a bulk selection.
    pub(super) fn prune_bulk_selection(&mut self, cx: &Context<Self>) {
        if self.bulk.is_empty() {
            return;
        }
        let remaining: Vec<NodeId> = self
            .graph(cx)
            .map(|graph| {
                self.bulk
                    .iter()
                    .filter(|id| graph.node(id).is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.bulk = if remaining.len() > 1 {
            remaining
        } else {
            Vec::new()
        };
    }

    pub(super) fn has_bulk_selection(&self) -> bool {
        self.bulk.len() > 1
    }

    pub(super) fn in_bulk_selection(&self, id: &NodeId) -> bool {
        self.has_bulk_selection() && self.bulk.contains(id)
    }

    /// The steps selected right now, whether one or several.
    fn selected_steps(&self) -> Vec<NodeId> {
        if self.has_bulk_selection() {
            self.bulk.clone()
        } else {
            match &self.selection {
                Some(Selection::Node(id)) => vec![id.clone()],
                _ => Vec::new(),
            }
        }
    }

    /// Selects these steps. One step is an ordinary selection, with its own
    /// inspector.
    pub(super) fn set_bulk_selection(
        &mut self,
        mut ids: Vec<NodeId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only steps drawn on this level can be selected together.
        let drawn: HashSet<NodeId> = self
            .graph(cx)
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter(|node| node.position.is_some())
                    .map(|node| node.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut seen = HashSet::new();
        ids.retain(|id| drawn.contains(id) && seen.insert(id.clone()));
        if ids.len() < 2 {
            let selection = ids.pop().map(Selection::Node);
            self.set_selection(selection, window, cx);
            return;
        }
        self.set_selection(None, window, cx);
        self.bulk = ids;
        if f32::from(window.viewport_size().width) < 1060.0 {
            self.open_inspector_drawer(window, cx);
        }
        cx.notify();
    }

    /// Shift-click: adds a step to the selection, or takes it back out.
    pub(super) fn toggle_in_bulk_selection(
        &mut self,
        id: NodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut ids = self.selected_steps();
        if let Some(index) = ids.iter().position(|selected| selected == &id) {
            ids.remove(index);
        } else {
            ids.push(id);
        }
        self.set_bulk_selection(ids, window, cx);
    }

    pub(super) fn select_all_steps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self
            .graph(cx)
            .map(|graph| graph.nodes.iter().map(|node| node.id.clone()).collect())
            .unwrap_or_default();
        self.set_bulk_selection(ids, window, cx);
    }

    /// Adds every step the rectangle touches to what is already selected.
    pub(super) fn finish_marquee(
        &mut self,
        start: Point<Pixels>,
        end: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dragged_x = (f32::from(end.x) - f32::from(start.x)).abs();
        let dragged_y = (f32::from(end.y) - f32::from(start.y)).abs();
        if dragged_x < MARQUEE_THRESHOLD && dragged_y < MARQUEE_THRESHOLD {
            cx.notify();
            return;
        }
        let (a, b) = (self.to_canvas(start), self.to_canvas(end));
        let (left, right) = (a.x.min(b.x), a.x.max(b.x));
        let (top, bottom) = (a.y.min(b.y), a.y.max(b.y));
        let inside: Vec<NodeId> = self
            .graph(cx)
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter(|node| {
                        let Some(position) = node.position else {
                            return false;
                        };
                        let (width, height) = self.node_size(node);
                        position.x + width / 2.0 >= left
                            && position.x - width / 2.0 <= right
                            && position.y + height / 2.0 >= top
                            && position.y - height / 2.0 <= bottom
                    })
                    .map(|node| node.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut ids = self.selected_steps();
        ids.extend(inside);
        self.set_bulk_selection(ids, window, cx);
    }

    /// The rectangle being dragged out, in the canvas's own coordinates.
    pub(super) fn render_marquee(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let Interaction::Selecting { start, at } = &self.interaction else {
            return None;
        };
        let bounds = self.viewport.get()?;
        let (start_x, at_x) = (f32::from(start.x), f32::from(at.x));
        let (start_y, at_y) = (f32::from(start.y), f32::from(at.y));
        let accent = cx.theme().colors().border_focused;
        Some(
            div()
                .absolute()
                .left(px(start_x.min(at_x)) - bounds.origin.x)
                .top(px(start_y.min(at_y)) - bounds.origin.y)
                .w(px((start_x - at_x).abs()))
                .h(px((start_y - at_y).abs()))
                .rounded_sm()
                .border_1()
                .border_color(accent)
                .bg(accent.opacity(0.08))
                .into_any(),
        )
    }

    /// Begins dragging every selected step along with the one pressed.
    pub(super) fn start_group_drag(
        &mut self,
        anchor: NodeId,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let starts: Vec<(NodeId, Position)> = self
            .graph(cx)
            .map(|graph| {
                self.bulk
                    .iter()
                    .filter_map(|id| Some((id.clone(), graph.node(id)?.position?)))
                    .collect()
            })
            .unwrap_or_default();
        self.interaction = Interaction::DraggingGroup {
            anchor,
            origin: self.to_canvas(at),
            starts,
            moved: false,
        };
        cx.notify();
    }

    /// Moves the dragged steps by the same snapped offset, so they keep their
    /// arrangement. Where a step sits is layout, so settled steps move too.
    pub(super) fn drag_group_to(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let canvas = self.to_canvas(at);
        let Interaction::DraggingGroup {
            origin,
            starts,
            moved,
            ..
        } = &mut self.interaction
        else {
            return;
        };
        let offset = Position {
            x: snap(canvas.x - origin.x),
            y: snap(canvas.y - origin.y),
        };
        if !*moved && offset == Position::ZERO {
            return;
        }
        *moved = true;
        let moves: Vec<(NodePath, Position)> = starts
            .iter()
            .map(|(id, start)| {
                let position = Position {
                    x: start.x + offset.x,
                    y: start.y + offset.y,
                };
                (self.focus.child(id.clone()), position)
            })
            .collect();
        self.next_undo_group = Some(UndoGroup::MoveSelection);
        self.edit_checked(
            move |graph| {
                all_or_nothing(graph, |graph| {
                    moves
                        .iter()
                        .try_for_each(|(path, position)| graph.move_node_at(path, *position))
                })
            },
            cx,
        );
    }

    /// Locks every step in the whole plan, at every depth, as one change to
    /// undo. Whatever level is on screen, it is the plan that runs.
    pub(super) fn lock_all_steps(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.interaction = Interaction::None;
        if self.is_running(cx) {
            self.report("Stop the run before locking steps.".to_string(), cx);
            return;
        }
        let open = self.root_graph(cx).map_or(0, |root| {
            root.step_count_deeply().saturating_sub(root.locked_step_count_deeply())
        });
        if open == 0 {
            return;
        }
        if self.edit_checked(
            |root| {
                root.lock_all();
                Ok(())
            },
            cx,
        ) {
            self.record_activity(None, format!("Locked the {} still open", count_label(open)), cx);
        }
        // An open inspector goes read-only with its step.
        self.refresh_inspector(window, cx);
    }

    /// Locks or unlocks every selected step. As with one step, locking a step
    /// that holds a plan locks that plan too, and unlocking reopens only the
    /// selected steps themselves.
    pub(super) fn lock_bulk_selection(&mut self, lock: bool, cx: &mut Context<Self>) {
        self.interaction = Interaction::None;
        if self.is_running(cx) {
            self.report(
                "Stop the run before locking or unlocking steps.".to_string(),
                cx,
            );
            return;
        }
        if !self.focus_is_editable(cx) {
            self.report(
                "The step containing this plan is locked. Unlock it to change these steps."
                    .to_string(),
                cx,
            );
            return;
        }
        let Some(graph) = self.graph(cx) else {
            return;
        };
        let changing: Vec<NodePath> = self
            .bulk
            .iter()
            .filter(|id| graph.node(id).is_some_and(|node| node.locked != lock))
            .map(|id| self.focus.child(id.clone()))
            .collect();

        let count = changing.len();
        if count > 0
            && self.edit_checked(
                move |graph| {
                    all_or_nothing(graph, |graph| {
                        changing.iter().try_for_each(|path| {
                            if lock {
                                graph.lock_deeply_at(path).map(|_| ())
                            } else {
                                graph.set_locked_at(path, false)
                            }
                        })
                    })
                },
                cx,
            )
        {
            let verb = if lock { "Locked" } else { "Unlocked" };
            self.record_activity(
                Some(self.focus.clone()),
                format!("{verb} {} at once", count_label(count)),
                cx,
            );
        }
        cx.notify();
    }

    /// Deletes every selected draft step. Locked steps are settled, so they are
    /// kept, and stay selected.
    pub(super) fn delete_bulk_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.interaction = Interaction::None;
        if self.is_running(cx) {
            self.report(
                "Stop the run before deleting from the plan.".to_string(),
                cx,
            );
            return;
        }
        if !self.focus_is_editable(cx) {
            self.report(
                "The step containing this plan is locked. Unlock it to delete these steps."
                    .to_string(),
                cx,
            );
            return;
        }
        let Some(graph) = self.graph(cx) else {
            return;
        };
        let (kept, deleting): (Vec<NodeId>, Vec<NodeId>) = self
            .bulk
            .iter()
            .filter(|id| graph.node(id).is_some())
            .cloned()
            .partition(|id| graph.node(id).is_some_and(|node| node.locked));
        if deleting.is_empty() {
            self.report(
                "Every selected step is locked. Unlock steps to delete them.".to_string(),
                cx,
            );
            return;
        }

        let count = deleting.len();
        let paths: Vec<NodePath> = deleting
            .into_iter()
            .map(|id| self.focus.child(id))
            .collect();
        let deleted = self.edit_checked(
            move |graph| {
                all_or_nothing(graph, |graph| {
                    paths.iter().try_for_each(|path| graph.remove_node_at(path))
                })
            },
            cx,
        );
        if !deleted {
            return;
        }
        self.record_activity(
            Some(self.focus.clone()),
            format!("Deleted {} from the plan", count_label(count)),
            cx,
        );
        let message = if kept.is_empty() {
            format!(
                "Deleted {}. Press {UNDO_SHORTCUT} to undo.",
                count_label(count)
            )
        } else {
            format!(
                "Deleted {} and kept {} that {} locked. Press {UNDO_SHORTCUT} to undo.",
                count_label(count),
                count_label(kept.len()),
                if kept.len() == 1 { "is" } else { "are" },
            )
        };
        self.set_bulk_selection(kept, window, cx);
        self.notice(message, cx);
    }

    /// Stands in for the step inspector while several steps are selected.
    pub(super) fn render_bulk_inspector(
        &self,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running = self.is_running(cx);
        let steps: Vec<(SharedString, bool)> = self
            .graph(cx)
            .map(|graph| {
                self.bulk
                    .iter()
                    .filter_map(|id| graph.node(id))
                    .map(|node| {
                        let title = node.title.trim();
                        let title = if title.is_empty() {
                            "untitled step"
                        } else {
                            title
                        };
                        (SharedString::from(title.to_string()), node.locked)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let BulkCounts {
            locked,
            drafts,
            editable,
        } = self.bulk_counts(cx);
        let can_change = !running && editable;
        let hidden = steps.len().saturating_sub(LISTED_STEPS);

        let actions = h_flex()
            .flex_wrap()
            .gap_1()
            .when(drafts > 0, |this| {
                this.child(
                    Button::new("architect-bulk-lock", "Lock")
                        .tab_index(0isize)
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::Lock).size(IconSize::XSmall))
                        .tooltip(Tooltip::text(format!(
                            "Settle the {} still in draft, with any plans inside them",
                            count_label(drafts)
                        )))
                        .on_click(cx.listener(|this, _, _, cx| this.lock_bulk_selection(true, cx))),
                )
            })
            .when(locked > 0, |this| {
                this.child(
                    Button::new("architect-bulk-unlock", "Unlock")
                        .tab_index(0isize)
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .start_icon(Icon::new(IconName::LockOff).size(IconSize::XSmall))
                        .tooltip(Tooltip::text(format!(
                            "Reopen the {} that {} settled",
                            count_label(locked),
                            if locked == 1 { "is" } else { "are" }
                        )))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.lock_bulk_selection(false, cx)),
                        ),
                )
            })
            .when(drafts > 0, |this| {
                this.child(
                    Button::new(
                        "architect-bulk-delete",
                        format!("Delete {}", count_label(drafts)),
                    )
                    .tab_index(0isize)
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .start_icon(Icon::new(IconName::Trash).size(IconSize::XSmall))
                    .tooltip(Tooltip::text(if locked > 0 {
                        format!(
                            "Remove the draft steps; locked ones are kept. {UNDO_SHORTCUT} \
                             undoes it."
                        )
                    } else {
                        format!("Remove these steps. {UNDO_SHORTCUT} undoes it.")
                    }))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.delete_bulk_selection(window, cx)),
                    ),
                )
            });

        div()
            .id("architect-bulk-inspector")
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
                                    .child(
                                        Label::new(format!(
                                            "{} selected",
                                            count_label(steps.len())
                                        ))
                                        .size(LabelSize::Default),
                                    )
                                    .child(
                                        Button::new("architect-bulk-clear", "Clear")
                                            .tab_index(0isize)
                                            .label_size(LabelSize::Small)
                                            .style(ButtonStyle::Subtle)
                                            .tooltip(Tooltip::text("Deselect every step (Escape)"))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.set_selection(None, window, cx);
                                            })),
                                    ),
                            )
                            .child(
                                Label::new(format!("{locked} settled · {drafts} draft"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        v_flex()
                            .id("architect-bulk-inspector-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_3()
                            .gap_3()
                            .when(can_change, |this| this.child(actions))
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("STEPS")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .children(steps.iter().take(LISTED_STEPS).map(
                                        |(title, locked)| {
                                            h_flex()
                                                .gap_1()
                                                .min_w_0()
                                                .child(
                                                    Icon::new(if *locked {
                                                        IconName::Lock
                                                    } else {
                                                        IconName::Circle
                                                    })
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(title.clone())
                                                        .size(LabelSize::Small)
                                                        .truncate(),
                                                )
                                        },
                                    ))
                                    .when(hidden > 0, |this| {
                                        this.child(
                                            Label::new(format!("and {hidden} more"))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    }),
                            )
                            .child(
                                Label::new(if running {
                                    "The plan is running, so these steps can be moved but not \
                                     changed."
                                } else if !editable {
                                    "The step containing this plan is locked, so these steps can \
                                     be moved but not changed."
                                } else {
                                    "Drag any selected step to move them together. Shift-click \
                                     a step to add or remove it."
                                })
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            ),
                    ),
            )
            .into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use architect::ArchitectNode;

    #[test]
    fn all_or_nothing_leaves_the_plan_untouched_on_a_refusal() {
        let mut graph = ArchitectGraph::default();
        let mut draft = ArchitectNode::new(NodeId::from("draft"), "Draft");
        draft.position = Some(Position::ZERO);
        graph.add_node(draft);
        let mut settled = ArchitectNode::new(NodeId::from("settled"), "Settled");
        settled.locked = true;
        graph.add_node(settled);
        let before = graph.clone();

        let result = all_or_nothing(&mut graph, |graph| {
            graph.remove_node_at(&NodePath::root(NodeId::from("draft")))?;
            graph.remove_node_at(&NodePath::root(NodeId::from("settled")))
        });

        assert!(result.is_err());
        assert_eq!(graph, before, "the first removal must be undone too");
    }
}
