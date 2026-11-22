use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Entity, Task};
use project::Project;
use serde_json::{Value, json};
use util::paths::PathStyle;
use workspace::{AppState, OpenMode, OpenOptions, WorkspaceMatching};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FolderTarget {
    Local,
    Remote,
}

impl FolderTarget {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            FolderTarget::Local => "local",
            FolderTarget::Remote => "remote",
        }
    }
}

pub(super) fn local_host_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else {
        "This PC"
    }
}

pub(super) fn targets_json(remote_host: Option<&str>) -> Value {
    let mut targets = vec![json!({
        "id": FolderTarget::Local.as_str(),
        "label": local_host_label(),
    })];
    if let Some(remote_host) = remote_host {
        targets.push(json!({
            "id": FolderTarget::Remote.as_str(),
            "label": remote_host,
        }));
    }
    Value::Array(targets)
}

/// Resolves whether a folder browse/open targets the local computer or the
/// selected window's remote host (WSL/SSH). Explicit `target` always wins;
/// when omitted on Windows, a Windows drive/UNC path like `C:\...` targets
/// the local PC even if the active window is a WSL project.
pub(super) fn resolve_target(
    requested: Option<&str>,
    path: &str,
    has_remote: bool,
    local_style: PathStyle,
) -> Result<FolderTarget> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some("local") => Ok(FolderTarget::Local),
        Some("remote") => {
            if has_remote {
                Ok(FolderTarget::Remote)
            } else {
                bail!("The selected window is not connected to a remote host")
            }
        }
        Some(other) => bail!("Unknown folder target {other:?}"),
        None => {
            let trimmed = path.trim();
            if has_remote
                && local_style == PathStyle::Windows
                && !trimmed.is_empty()
                && PathStyle::Windows.is_absolute(Path::new(trimmed))
                && !PathStyle::Unix.is_absolute(Path::new(trimmed))
            {
                return Ok(FolderTarget::Local);
            }
            if has_remote {
                Ok(FolderTarget::Remote)
            } else {
                Ok(FolderTarget::Local)
            }
        }
    }
}

/// Checks the path by the rules of the computer the project is on, since a
/// WSL path such as `/home/me` is not absolute by Windows' rules.
fn host_path(path: &str, fallback: Option<&Path>, path_style: PathStyle) -> Result<PathBuf> {
    let path = match path.trim() {
        "" => fallback
            .map(Path::to_path_buf)
            .unwrap_or_else(|| paths::home_dir().to_path_buf()),
        path => PathBuf::from(path),
    };
    if !path_style.is_absolute(&path) {
        bail!("Enter an absolute folder path on the connected computer");
    }
    Ok(path)
}

fn format_listing(
    host: String,
    target: FolderTarget,
    targets: Value,
    path: &Path,
    path_style: PathStyle,
    mut folders: Vec<(String, String)>,
    offset: usize,
) -> Result<Value> {
    folders.sort();
    let total = folders.len();
    let mut entries = Vec::new();
    let mut remaining = super::DIR_JSON_BUDGET;
    for (name, path) in folders.into_iter().skip(offset).take(100) {
        let entry = json!({ "name": name, "path": path });
        let cost = entry.to_string().len() + 1;
        if cost > remaining {
            if entries.is_empty() {
                bail!("That folder name is too large to display. Enter its path directly.");
            }
            break;
        }
        remaining -= cost;
        entries.push(entry);
    }
    let next = offset.saturating_add(entries.len());
    Ok(json!({
        "host": host,
        "target": target.as_str(),
        "targets": targets,
        "path": path.to_str().context("This folder path is not valid Unicode")?,
        "parent": path_style.parent(path).and_then(Path::to_str),
        "folders": entries,
        "next_offset": (next < total).then_some(next),
    }))
}

/// Lists folders directly on the local computer's filesystem, even when the
/// active window is connected to WSL or SSH.
pub(super) fn list_local(
    fs: Arc<dyn Fs>,
    remote_host: Option<String>,
    path: &str,
    offset: usize,
    cx: &mut App,
) -> Task<Result<Value>> {
    let path_style = PathStyle::local();
    let path = match host_path(path, None, path_style) {
        Ok(path) => path,
        Err(error) => return Task::ready(Err(error)),
    };
    let targets = targets_json(remote_host.as_deref());
    cx.background_spawn(async move {
        let mut stream = fs
            .read_dir(&path)
            .await
            .context("Could not read that folder")?;
        let mut folders = Vec::new();
        while let Some(entry) = stream.next().await {
            let Ok(entry_path) = entry else {
                continue;
            };
            if !fs.is_dir(&entry_path).await {
                continue;
            }
            let Some(name) = path_style
                .file_name(&entry_path)
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(full) = entry_path.to_str().map(str::to_owned) else {
                continue;
            };
            folders.push((name, full));
        }
        format_listing(
            std::env::consts::OS.to_string(),
            FolderTarget::Local,
            targets,
            &path,
            path_style,
            folders,
            offset,
        )
    })
}

/// Lists folders through the project, which asks the remote server when the
/// project lives on WSL or another computer.
pub(super) fn list(
    project: Entity<Project>,
    fallback: Option<PathBuf>,
    host: String,
    path: &str,
    offset: usize,
    cx: &mut App,
) -> Task<Result<Value>> {
    let path_style = project.read(cx).path_style(cx);
    let path = match host_path(path, fallback.as_deref(), path_style) {
        Ok(path) => path,
        Err(error) => return Task::ready(Err(error)),
    };
    let targets = targets_json(Some(&host));
    let query = path.to_string_lossy().into_owned();
    cx.spawn(async move |cx| {
        let entries = project
            .update(cx, |project, cx| project.list_directory(query, cx))
            .await
            .context("Could not read that folder")?;
        let folders: Vec<(String, String)> = entries
            .into_iter()
            .filter(|entry| entry.is_dir)
            .filter_map(|entry| {
                // Local listings return bare names; remote ones return full paths.
                let full = if path_style.is_absolute(&entry.path) {
                    entry.path
                } else {
                    path_style.join(&path, &entry.path)
                };
                let name = path_style.file_name(&full)?.to_str()?.to_owned();
                let full = full.to_str()?.to_owned();
                Some((name, full))
            })
            .collect();
        format_listing(
            host,
            FolderTarget::Remote,
            targets,
            &path,
            path_style,
            folders,
            offset,
        )
    })
}

pub(super) fn open(
    path: &str,
    target: FolderTarget,
    project: Entity<Project>,
    app_state: Arc<AppState>,
    cx: &mut App,
) -> Task<Result<Value>> {
    let path_style = match target {
        FolderTarget::Local => PathStyle::local(),
        FolderTarget::Remote => project.read(cx).path_style(cx),
    };
    let path = match host_path(path, None, path_style) {
        Ok(path) => path,
        Err(error) => return Task::ready(Err(error)),
    };
    cx.spawn(async move |cx| {
        if target == FolderTarget::Remote {
            let is_remote = project.read_with(cx, |project, cx| {
                project.remote_connection_options(cx).is_some()
            });
            if !is_remote {
                bail!("The selected window is not connected to a remote host");
            }
            project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(&path, true, cx)
                })
                .await?;
            let window =
                cx.update(|cx| cx.active_window().map(|window| window.window_id().as_u64()));
            return Ok(json!({ "opened": true, "window": window }));
        }
        let fs = app_state.fs.clone();
        let metadata = fs
            .metadata(&path)
            .await?
            .context("That folder no longer exists")?;
        if !metadata.is_dir {
            bail!("Choose a folder, not a file");
        }
        let opened = cx
            .update(|cx| {
                workspace::open_paths(
                    &[path],
                    app_state,
                    OpenOptions {
                        open_mode: OpenMode::NewWindow,
                        workspace_matching: WorkspaceMatching::None,
                        add_dirs_to_sidebar: false,
                        ..OpenOptions::default()
                    },
                    cx,
                )
            })
            .await?;
        Ok(json!({ "opened": true, "window": opened.window.window_id().as_u64() }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn folder_listing_pages_without_rewriting_host_paths(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(AppState::test);
        let filesystem = app_state.fs.as_fake();
        let root = PathBuf::from(util::path!("/host"));
        let mut tree = serde_json::Map::new();
        for index in 0..103 {
            tree.insert(format!("folder {index:03}"), json!({}));
        }
        filesystem.insert_tree(&root, Value::Object(tree)).await;
        let project = Project::test(app_state.fs.clone(), [], cx).await;

        let first = cx
            .update(|cx| {
                list(
                    project.clone(),
                    None,
                    "test".into(),
                    root.to_str().expect("path"),
                    0,
                    cx,
                )
            })
            .await
            .expect("first page");
        let second = cx
            .update(|cx| {
                list(
                    project,
                    None,
                    "test".into(),
                    root.to_str().expect("path"),
                    first["next_offset"].as_u64().expect("cursor") as usize,
                    cx,
                )
            })
            .await
            .expect("second page");
        assert_eq!(first["folders"].as_array().expect("folders").len(), 100);
        assert_eq!(second["folders"].as_array().expect("folders").len(), 3);
        assert_eq!(second["next_offset"], Value::Null);
        assert!(Path::new(first["folders"][0]["path"].as_str().expect("child path")).is_absolute());
        assert!(host_path("relative/folder", None, PathStyle::local()).is_err());
    }

    #[test]
    fn wsl_folders_are_absolute_on_any_client() {
        let home = Path::new("/home/me");
        assert_eq!(
            host_path("/home/me", None, PathStyle::Unix).expect("absolute"),
            home
        );
        assert_eq!(
            host_path("", Some(home), PathStyle::Unix).expect("fallback"),
            home
        );
        assert!(host_path("home/me", None, PathStyle::Unix).is_err());
        let child = PathStyle::Unix.join(home, "project");
        assert_eq!(child.to_str(), Some("/home/me/project"));
        assert_eq!(
            PathStyle::Unix.parent(&child).and_then(Path::to_str),
            Some("/home/me")
        );
    }

    #[gpui::test]
    async fn folder_listing_reads_the_project_filesystem(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(AppState::test);
        let filesystem = app_state.fs.as_fake();
        let root = PathBuf::from(util::path!("/remote/project"));
        filesystem
            .insert_tree(&root, json!({ "a": {}, "b": {}, "file.txt": "x" }))
            .await;
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let listing = cx
            .update(|cx| {
                list(
                    project,
                    None,
                    "Debian".into(),
                    root.to_str().expect("path"),
                    0,
                    cx,
                )
            })
            .await
            .expect("listing");
        assert_eq!(listing["host"], "Debian");
        assert_eq!(listing["target"], "remote");
        assert_eq!(listing["targets"].as_array().expect("targets").len(), 2);
        let names: Vec<_> = listing["folders"]
            .as_array()
            .expect("folders")
            .iter()
            .map(|folder| folder["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn resolve_target_supports_local_remote_and_windows_path_autodetect() {
        assert_eq!(
            resolve_target(Some("local"), "", true, PathStyle::Windows).expect("explicit local"),
            FolderTarget::Local
        );
        assert_eq!(
            resolve_target(Some("remote"), "", true, PathStyle::Windows).expect("explicit remote"),
            FolderTarget::Remote
        );
        assert!(resolve_target(Some("remote"), "", false, PathStyle::Windows).is_err());
        assert!(resolve_target(Some("other"), "", true, PathStyle::Windows).is_err());
        assert_eq!(
            resolve_target(None, r"C:\Users\me\Projects", true, PathStyle::Windows)
                .expect("windows drive path should auto-target local"),
            FolderTarget::Local
        );
        assert_eq!(
            resolve_target(None, "/home/me/project", true, PathStyle::Windows)
                .expect("unix path on remote window should target remote"),
            FolderTarget::Remote
        );
        assert_eq!(
            resolve_target(None, "", true, PathStyle::Windows)
                .expect("empty path on remote window defaults to remote"),
            FolderTarget::Remote
        );
        assert_eq!(
            resolve_target(None, "", false, PathStyle::Windows)
                .expect("empty path on local window defaults to local"),
            FolderTarget::Local
        );
    }

    #[gpui::test]
    async fn local_folder_listing_browses_local_fs_and_advertises_both_targets(
        cx: &mut gpui::TestAppContext,
    ) {
        let app_state = cx.update(AppState::test);
        let filesystem = app_state.fs.as_fake();
        let root = PathBuf::from(util::path!("/local/projects"));
        filesystem
            .insert_tree(&root, json!({ "alpha": {}, "beta": {}, "notes.txt": "hi" }))
            .await;

        let listing = cx
            .update(|cx| {
                list_local(
                    app_state.fs.clone(),
                    Some("Debian".into()),
                    root.to_str().expect("path"),
                    0,
                    cx,
                )
            })
            .await
            .expect("local listing");

        assert_eq!(listing["host"], std::env::consts::OS);
        assert_eq!(listing["target"], "local");
        let targets = listing["targets"].as_array().expect("targets");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0]["id"], "local");
        assert_eq!(targets[1]["id"], "remote");
        assert_eq!(targets[1]["label"], "Debian");
        let names: Vec<_> = listing["folders"]
            .as_array()
            .expect("folders")
            .iter()
            .map(|folder| folder["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[gpui::test]
    async fn opening_a_folder_preserves_windows_and_phone_identity(cx: &mut gpui::TestAppContext) {
        crate::conversation_view::tests::init_test(cx);
        let app_state = cx.update(AppState::test);
        let filesystem = app_state.fs.as_fake();
        let first_path = PathBuf::from(util::path!("/first"));
        let second_path = PathBuf::from(util::path!("/second"));
        filesystem.insert_tree(&first_path, json!({})).await;
        filesystem.insert_tree(&second_path, json!({})).await;
        let remote = cx.new(|_| super::super::PraxisRemote::new());
        remote.update(cx, |remote, _| {
            remote.phones.push(super::super::store::PhoneInfo {
                id: "paired-phone".into(),
                name: "Phone".into(),
                paired_at: chrono::Utc::now(),
            });
        });
        cx.update(|cx| cx.set_global(super::super::GlobalPraxisRemote(remote.clone())));
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let first = cx
            .update(|cx| {
                open(
                    first_path.to_str().expect("first path"),
                    FolderTarget::Local,
                    project.clone(),
                    app_state.clone(),
                    cx,
                )
            })
            .await
            .expect("first window");
        let second = cx
            .update(|cx| {
                open(
                    second_path.to_str().expect("second path"),
                    FolderTarget::Local,
                    project.clone(),
                    app_state.clone(),
                    cx,
                )
            })
            .await
            .expect("second window");
        assert!(
            cx.update(|cx| {
                open(
                    first_path.to_str().expect("first path"),
                    FolderTarget::Remote,
                    project,
                    app_state,
                    cx,
                )
            })
            .await
            .is_err()
        );
        assert_ne!(first["window"], second["window"]);
        cx.read(|cx| {
            let windows = super::super::workspace_windows(cx);
            for expected in [first["window"].as_u64(), second["window"].as_u64()] {
                assert!(
                    windows
                        .iter()
                        .any(|window| Some(window.window_id().as_u64()) == expected)
                );
            }
            let retained = super::super::PraxisRemote::global(cx).expect("same remote owner");
            assert_eq!(retained, remote);
            assert_eq!(retained.read(cx).phones[0].id, "paired-phone");
        });
    }
}
