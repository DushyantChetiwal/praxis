use std::{path::PathBuf, sync::Arc};

use gpui::{ClickEvent, DismissEvent, EventEmitter, FocusHandle, Focusable, Render, WeakEntity};
use project::project_settings::ProjectSettings;
use remote::{ConnectionState, RemoteConnectionOptions};
use remote_connection::RemoteConnectionModal;
use settings::Settings;
use ui::{ElevationIndex, Modal, ModalFooter, ModalHeader, Section, prelude::*};
use workspace::{
    AppState, ModalView, MultiWorkspace, OpenOptions, Workspace, notifications::DetachAndPromptErr,
};

use crate::open_remote_project;

enum Host {
    CollabGuestProject,
    RemoteServerProject(RemoteConnectionOptions, bool),
}

pub struct DisconnectedOverlay {
    workspace: WeakEntity<Workspace>,
    host: Host,
    focus_handle: FocusHandle,
    finished: bool,
}

impl EventEmitter<DismissEvent> for DisconnectedOverlay {}
impl Focusable for DisconnectedOverlay {
    fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}
impl ModalView for DisconnectedOverlay {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        workspace::DismissDecision::Dismiss(self.finished)
    }
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl DisconnectedOverlay {
    pub fn register(
        workspace: &mut Workspace,
        window: Option<&mut Window>,
        cx: &mut Context<Workspace>,
    ) {
        workspace.register_action(Self::reconnect_workspace);
        let Some(window) = window else {
            return;
        };
        cx.subscribe_in(
            workspace.project(),
            window,
            |workspace, project, event, window, cx| {
                if !matches!(
                    event,
                    project::Event::DisconnectedFromHost
                        | project::Event::DisconnectedFromRemote { .. }
                ) {
                    return;
                }
                let handle = cx.entity().downgrade();

                let remote_connection_options = project.read(cx).remote_connection_options(cx);
                let host = if let Some(remote_connection_options) = remote_connection_options {
                    Host::RemoteServerProject(
                        remote_connection_options,
                        matches!(
                            event,
                            project::Event::DisconnectedFromRemote {
                                server_not_running: true
                            }
                        ),
                    )
                } else {
                    Host::CollabGuestProject
                };

                workspace.toggle_modal(window, cx, |_, cx| DisconnectedOverlay {
                    finished: false,
                    workspace: handle,
                    host,
                    focus_handle: cx.focus_handle(),
                });
            },
        )
        .detach();
    }

    /// Lets a reconnect requested away from the overlay, such as from Praxis
    /// Remote, reopen the project exactly like the overlay's button does.
    fn reconnect_workspace(
        workspace: &mut Workspace,
        _: &zed_actions::remote::Reconnect,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().read(cx);
        if project.remote_connection_state(cx) != Some(ConnectionState::Disconnected)
            || workspace.active_modal::<RemoteConnectionModal>(cx).is_some()
        {
            return;
        }
        let Some(connection_options) = project.remote_connection_options(cx) else {
            return;
        };
        if let Some(overlay) = workspace.active_modal::<Self>(cx) {
            overlay.update(cx, |overlay, cx| {
                overlay.finished = true;
                cx.emit(DismissEvent);
            });
        }
        let app_state = workspace.app_state().clone();
        let paths = workspace_paths(workspace, cx);
        reopen_remote_project(connection_options, paths, app_state, window, cx);
    }

    fn handle_reconnect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent);

        let Host::RemoteServerProject(connection_options, _) = &self.host else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let (app_state, paths) = {
            let workspace = workspace.read(cx);
            (workspace.app_state().clone(), workspace_paths(workspace, cx))
        };
        reopen_remote_project(connection_options.clone(), paths, app_state, window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent)
    }
}

fn workspace_paths(workspace: &Workspace, cx: &App) -> Vec<PathBuf> {
    workspace
        .root_paths(cx)
        .iter()
        .map(|path| path.to_path_buf())
        .collect()
}

fn reopen_remote_project(
    connection_options: RemoteConnectionOptions,
    paths: Vec<PathBuf>,
    app_state: Arc<AppState>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(window_handle) = window.window_handle().downcast::<MultiWorkspace>() else {
        return;
    };
    window
        .spawn(cx, async move |cx| {
            open_remote_project(
                connection_options,
                paths,
                app_state,
                OpenOptions {
                    requesting_window: Some(window_handle),
                    ..Default::default()
                },
                cx,
            )
            .await?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to reconnect", window, cx, |_, _, _| None);
}

impl Render for DisconnectedOverlay {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_reconnect = matches!(self.host, Host::RemoteServerProject(..));

        let message = match &self.host {
            Host::CollabGuestProject => {
                "Your connection to the remote project has been lost.".to_string()
            }
            Host::RemoteServerProject(options, server_not_running) => {
                let autosave = if ProjectSettings::get_global(cx)
                    .session
                    .restore_unsaved_buffers
                {
                    "\nUnsaved changes are stored locally."
                } else {
                    ""
                };
                let reason = if *server_not_running {
                    "process exiting unexpectedly"
                } else {
                    "not responding"
                };
                format!(
                    "Your connection to {} has been lost due to the server {reason}.{autosave}",
                    options.display_name(),
                )
            }
        };

        div()
            .track_focus(&self.focus_handle(cx))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .occlude()
            .w(rems(24.))
            .max_h(rems(40.))
            .child(
                Modal::new("disconnected", None)
                    .header(
                        ModalHeader::new()
                            .show_dismiss_button(true)
                            .child(Headline::new("Disconnected").size(HeadlineSize::Small)),
                    )
                    .section(Section::new().child(Label::new(message)))
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("close-window", "Close Window")
                                        .style(ButtonStyle::Filled)
                                        .layer(ElevationIndex::ModalSurface)
                                        .on_click(cx.listener(move |_, _, window, _| {
                                            window.remove_window();
                                        })),
                                )
                                .when(can_reconnect, |el| {
                                    el.child(
                                        Button::new("reconnect", "Reconnect")
                                            .style(ButtonStyle::Filled)
                                            .layer(ElevationIndex::ModalSurface)
                                            .start_icon(Icon::new(IconName::ArrowCircle))
                                            .on_click(cx.listener(Self::handle_reconnect)),
                                    )
                                }),
                        ),
                    ),
            )
    }
}
