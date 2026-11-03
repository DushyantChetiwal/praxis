use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, Entity, Task};
use project::Project;
use serde_json::{Value, json};
use workspace::{AppState, OpenMode, OpenOptions, WorkspaceMatching};

fn host_path(path: &str, fallback: Option<&Path>) -> Result<PathBuf> {
    let path = match path {
        "" => fallback
            .map(Path::to_path_buf)
            .unwrap_or_else(|| paths::home_dir().to_path_buf()),
        path => PathBuf::from(path),
    };
    if !path.is_absolute() {
        bail!("Enter an absolute folder path on the connected computer");
    }
    Ok(path)
}

/// Lists folders through the project's filesystem, so a project on WSL or a
/// remote host browses that computer rather than this one.
pub(super) async fn list(
    fs: &dyn Fs,
    fallback: Option<PathBuf>,
    host: String,
    path: &str,
    offset: usize,
) -> Result<Value> {
    let canonical = fs
        .canonicalize(&host_path(path, fallback.as_deref())?)
        .await
        .context("Could not find that folder")?;
    let path = util::paths::SanitizedPath::new(&canonical).as_path();
    let mut folders = Vec::new();
    let mut entries = fs
        .read_dir(path)
        .await
        .context("Could not read that folder")?;
    while let Some(entry) = entries.next().await {
        let child = entry.context("Could not read a folder entry")?;
        if !fs.is_dir(&child).await {
            continue;
        }
        let Some(name) = child
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(child) = child.to_str() else {
            continue;
        };
        folders.push((name, child.to_string()));
    }
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
        "path": path.to_str().context("This folder path is not valid Unicode")?,
        "parent": path.parent().and_then(Path::to_str),
        "folders": entries,
        "next_offset": (next < total).then_some(next),
    }))
}

pub(super) fn open(
    path: &str,
    project: Entity<Project>,
    app_state: Arc<AppState>,
    cx: &mut App,
) -> Task<Result<Value>> {
    let path = match host_path(path, None) {
        Ok(path) => path,
        Err(error) => return Task::ready(Err(error)),
    };
    cx.spawn(async move |cx| {
        let fs = project.read_with(cx, |project, _| project.fs().clone());
        let metadata = fs
            .metadata(&path)
            .await?
            .context("That folder no longer exists")?;
        if !metadata.is_dir {
            bail!("Choose a folder, not a file");
        }
        // A folder on a connected computer opens in this window, so it stays on
        // that computer instead of opening a local path that does not exist.
        if project
            .read_with(cx, |project, cx| project.remote_connection_options(cx))
            .is_some()
        {
            project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(&path, true, cx)
                })
                .await?;
            let window =
                cx.update(|cx| cx.active_window().map(|window| window.window_id().as_u64()));
            return Ok(json!({ "opened": true, "window": window }));
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
    use gpui::AppContext as _;

    #[gpui::test]
    async fn folder_listing_pages_without_rewriting_host_paths(cx: &mut gpui::TestAppContext) {
        let directory = tempfile::tempdir().expect("folder");
        for index in 0..103 {
            std::fs::create_dir(directory.path().join(format!("folder {index:03}")))
                .expect("child");
        }
        let fs = fs::RealFs::new(None, cx.background_executor().clone());
        let path = directory.path().to_str().expect("path");
        let first = list(fs.as_ref(), None, "test".into(), path, 0)
            .await
            .expect("first page");
        let second = list(
            fs.as_ref(),
            None,
            "test".into(),
            path,
            first["next_offset"].as_u64().expect("cursor") as usize,
        )
        .await
        .expect("second page");
        assert_eq!(first["folders"].as_array().expect("folders").len(), 100);
        assert_eq!(second["folders"].as_array().expect("folders").len(), 3);
        assert_eq!(second["next_offset"], Value::Null);
        assert!(Path::new(first["folders"][0]["path"].as_str().expect("child path")).is_dir());
        assert!(host_path("relative/folder", None).is_err());
    }

    #[gpui::test]
    async fn folder_listing_reads_the_project_filesystem(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(AppState::test);
        let filesystem = app_state.fs.as_fake();
        let root = PathBuf::from(util::path!("/remote/project"));
        filesystem
            .insert_tree(&root, json!({ "a": {}, "b": {}, "file.txt": "x" }))
            .await;
        let fs: &dyn Fs = filesystem.as_ref();
        let listing = list(fs, None, "Debian".into(), root.to_str().expect("path"), 0)
            .await
            .expect("listing");
        assert_eq!(listing["host"], "Debian");
        let names: Vec<_> = listing["folders"]
            .as_array()
            .expect("folders")
            .iter()
            .map(|folder| folder["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["a", "b"]);
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
                    project,
                    app_state,
                    cx,
                )
            })
            .await
            .expect("second window");
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
