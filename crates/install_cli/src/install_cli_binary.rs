use super::register_zed_scheme;
use anyhow::{Context as _, Result};
use gpui::{AppContext as _, AsyncApp, Context, PromptLevel, Window, actions};
use release_channel::ReleaseChannel;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use util::ResultExt;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::notifications::{DetachAndPromptErr, NotificationId};
use workspace::{Toast, Workspace};

actions!(
    cli,
    [
        /// Installs the application CLI tool to the system PATH.
        InstallCliBinary,
    ]
);

const CANT_INSTALL_DOCS_URL: &str = "https://zed.dev/docs/macos#cant-install-cli";

/// Attempts to install the CLI symlink. Returns the installed path on success,
/// or `None` if the user dismissed the macOS administrator authentication
/// prompt. Returns an error if the install could not be completed, most
/// commonly because the user is not an admin.
async fn install_script(cx: &AsyncApp) -> Result<Option<PathBuf>> {
    let (cli_path, cli_name) = cx.update(|cx| {
        let cli_name = match ReleaseChannel::global(cx) {
            ReleaseChannel::Dev => "praxis",
            _ => "zed",
        };
        (cx.path_for_auxiliary_executable("cli"), cli_name)
    });
    let cli_path = cli_path?;
    let link_path = Path::new("/usr/local/bin").join(cli_name);
    let bin_dir_path = link_path
        .parent()
        .context("CLI installation path has no parent directory")?;

    // Don't re-create symlink if it points to the same CLI binary.
    if smol::fs::read_link(&link_path).await.ok().as_ref() == Some(&cli_path) {
        return Ok(Some(link_path.into()));
    }

    // If the symlink is not there or is outdated, first try replacing it
    // without escalating.
    smol::fs::remove_file(&link_path).await.log_err();
    if smol::fs::unix::symlink(&cli_path, &link_path)
        .await
        .log_err()
        .is_some()
    {
        return Ok(Some(link_path.into()));
    }

    // The symlink could not be created without escalating, so use osascript
    // with admin privileges to create it.
    let output = smol::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(
                "do shell script \" \
                    mkdir -p \'{}\' && \
                    ln -sf \'{}\' \'{}\' \
                \" with administrator privileges",
                bin_dir_path.to_string_lossy(),
                cli_path.to_string_lossy(),
                link_path.to_string_lossy(),
            ),
        ])
        .output()
        .await?;

    if output.status.success() {
        return Ok(Some(link_path.into()));
    }

    // osascript reports "User canceled." (error -128) when the administrator
    // prompt is dismissed. Treat that as a cancellation rather than a failure
    // so we don't show an error the user already chose to avoid.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("User canceled") || stderr.contains("-128") {
        return Ok(None);
    }

    // The privileged write failed, most commonly because the user is not an
    // admin.
    anyhow::bail!("error running osascript: {}", stderr.trim());
}

pub fn install_cli_binary(window: &mut Window, cx: &mut Context<Workspace>) {
    let release_channel = ReleaseChannel::global(cx);
    let app_name = release_channel.display_name();
    let cli_name = match release_channel {
        ReleaseChannel::Dev => "praxis",
        _ => "zed",
    };
    let linux_prompt_detail = format!(
        "If you installed {app_name} from a release, add ~/.local/bin to your PATH.\n\nIf you installed it through a package manager, you may need to create an alias or symlink manually."
    );

    cx.spawn_in(window, async move |workspace, cx| {
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            let prompt = cx.prompt(
                PromptLevel::Warning,
                "CLI should already be installed",
                Some(&linux_prompt_detail),
                &["OK"],
            );
            cx.background_spawn(prompt).detach();
            return Ok(());
        }
        let path = match install_script(cx.deref()).await {
            Ok(Some(path)) => path,
            // The user dismissed the administrator prompt; nothing to do.
            Ok(None) => return Ok(()),
            Err(error) => {
                log::error!("failed to install {cli_name} CLI: {error:#}");
                workspace.update(cx, |workspace, cx| {
                    struct CliInstallFailed;

                    workspace.show_notification(
                        NotificationId::unique::<CliInstallFailed>(),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                MessageNotification::new(
                                    format!("You can add `{cli_name}` to your PATH manually."),
                                    cx,
                                )
                                .with_title(format!("Couldn't install the {app_name} CLI"))
                                .more_info_message("Show me how")
                                .more_info_url(CANT_INSTALL_DOCS_URL)
                            })
                        },
                    );
                })?;
                return Ok(());
            }
        };

        workspace.update_in(cx, |workspace, _, cx| {
            struct InstalledZedCli;

            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<InstalledZedCli>(),
                    format!(
                        "Installed `{cli_name}` to {}. You can launch {app_name} from your terminal.",
                        path.to_string_lossy()
                    ),
                ),
                cx,
            )
        })?;
        register_zed_scheme(cx).await.log_err();
        Ok(())
    })
    .detach_and_prompt_err("Cannot install the application CLI", window, cx, |_, _, _| None);
}
