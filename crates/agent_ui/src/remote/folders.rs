use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use gpui::{App, Task};
use serde_json::{Value, json};
use workspace::{AppState, OpenMode, OpenOptions, WorkspaceMatching};

fn host_path(path: &str) -> Result<PathBuf> {
    let path = if path.is_empty() {
        paths::home_dir().to_path_buf()
    } else {
        PathBuf::from(path)
    };
    if !path.is_absolute() {
        bail!("Enter an absolute folder path on the connected computer");
    }
    Ok(path)
}

pub(super) fn list(path: &str, offset: usize) -> Result<Value> {
    let canonical =
        std::fs::canonicalize(host_path(path)?).context("Could not find that folder")?;
    let path = util::paths::SanitizedPath::new(&canonical).as_path();
    let mut folders = Vec::new();
    for entry in std::fs::read_dir(path).context("Could not read that folder")? {
        let entry = entry.context("Could not read a folder entry")?;
        let child = entry.path();
        if child.is_dir() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(child) = child.to_str() else {
                continue;
            };
            folders.push((name, child.to_string()));
        }
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
        "host": std::env::consts::OS,
        "path": path.to_str().context("This folder path is not valid Unicode")?,
        "parent": path.parent().and_then(Path::to_str),
        "folders": entries,
        "next_offset": (next < total).then_some(next),
    }))
}

pub(super) fn open(path: &str, app_state: Arc<AppState>, cx: &mut App) -> Task<Result<Value>> {
    let path = match host_path(path) {
        Ok(path) => path,
        Err(error) => return Task::ready(Err(error)),
    };
    cx.spawn(async move |cx| {
        let metadata = app_state
            .fs
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

    #[test]
    fn folder_listing_pages_without_rewriting_host_paths() {
        let directory = tempfile::tempdir().expect("folder");
        for index in 0..103 {
            std::fs::create_dir(directory.path().join(format!("folder {index:03}")))
                .expect("child");
        }
        let path = directory.path().to_str().expect("path");
        let first = list(path, 0).expect("first page");
        let second = list(
            path,
            first["next_offset"].as_u64().expect("cursor") as usize,
        )
        .expect("second page");
        assert_eq!(first["folders"].as_array().expect("folders").len(), 100);
        assert_eq!(second["folders"].as_array().expect("folders").len(), 3);
        assert_eq!(second["next_offset"], Value::Null);
        assert!(Path::new(first["folders"][0]["path"].as_str().expect("child path")).is_dir());
        assert!(host_path("relative/folder").is_err());
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
        let first = cx
            .update(|cx| {
                open(
                    first_path.to_str().expect("first path"),
                    app_state.clone(),
                    cx,
                )
            })
            .await
            .expect("first window");
        let second = cx
            .update(|cx| open(second_path.to_str().expect("second path"), app_state, cx))
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
