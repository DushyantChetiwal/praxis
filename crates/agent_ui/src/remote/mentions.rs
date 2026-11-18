use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use gpui::{App, Entity, Task};
use project::lsp_store::SymbolLocation;
use project::{Candidates, PathMatchCandidateSet};
use serde_json::{Value, json};
use workspace::Workspace;

use super::{FILE_JSON_BUDGET, is_private, root_thread_view, truncate};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 30;

pub(super) fn search(
    workspace: &Entity<Workspace>,
    args: &Value,
    cx: &mut App,
) -> Task<Result<Value>> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let kind = args
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("all")
        .to_string();
    let limit = parse_limit(args);

    let want_files = matches!(kind.as_str(), "all" | "file");
    let want_symbols = kind == "symbol" || (kind == "all" && !query.is_empty());

    let project = workspace.read(cx).project().clone();
    let workspace_handle = workspace.clone();

    let file_task = if want_files {
        let worktrees = workspace.read(cx).visible_worktrees(cx).collect::<Vec<_>>();
        let include_root_name = worktrees.len() > 1;
        let mut root_names = HashMap::with_capacity(worktrees.len());
        let candidate_sets = worktrees
            .into_iter()
            .map(|worktree| {
                let worktree = worktree.read(cx);
                root_names.insert(
                    worktree.id().to_usize(),
                    worktree.root_name_str().to_string(),
                );
                PathMatchCandidateSet {
                    snapshot: worktree.snapshot(),
                    include_ignored: worktree.root_entry().is_some_and(|entry| entry.is_ignored),
                    include_root_name,
                    candidates: Candidates::Entries,
                }
            })
            .collect::<Vec<_>>();

        let executor = cx.background_executor().clone();
        let query_clone = query.clone();
        let flag = Arc::new(AtomicBool::new(false));
        Some((
            root_names,
            include_root_name,
            cx.foreground_executor().spawn(async move {
                fuzzy::match_path_sets(
                    candidate_sets.as_slice(),
                    query_clone.as_str(),
                    &None,
                    false,
                    limit.saturating_mul(2),
                    &flag,
                    executor,
                )
                .await
            }),
        ))
    } else {
        None
    };

    let symbol_task = if want_symbols {
        let query_clone = query.clone();
        Some(project.update(cx, |project, cx| project.symbols(&query_clone, cx)))
    } else {
        None
    };

    cx.spawn(async move |cx| {
        let mut items = Vec::new();

        if let Some((root_names, include_root_name, task)) = file_task {
            let matches = task.await;
            cx.update(|cx| {
                for mat in matches {
                    if items.len() >= limit {
                        break;
                    }
                    if mat.is_dir {
                        continue;
                    }
                    let relative = mat.path.as_unix_str();
                    let Some(root_name) = root_names.get(&mat.worktree_id) else {
                        continue;
                    };
                    if is_private(&workspace_handle, root_name, relative, cx) {
                        continue;
                    }
                    let display_path = if include_root_name && !mat.path_prefix.is_empty() {
                        format!("{}/{}", mat.path_prefix.as_unix_str(), relative)
                    } else {
                        relative.to_string()
                    };
                    let label = mat.path.file_name().unwrap_or(relative);
                    items.push(json!({
                        "kind": "file",
                        "label": label,
                        "path": display_path,
                        "insert_text": format!("@{display_path} "),
                    }));
                }
            });
        }

        if items.len() < limit
            && let Some(task) = symbol_task
            && let Ok(symbols) = task.await
        {
            cx.update(|cx| {
                let project_read = project.read(cx);
                let include_root_name = workspace_handle.read(cx).visible_worktrees(cx).count() > 1;
                for symbol in symbols {
                    if items.len() >= limit {
                        break;
                    }
                    let SymbolLocation::InProject(project_path) = &symbol.path else {
                        continue;
                    };
                    let Some(worktree) = project_read.worktree_for_id(project_path.worktree_id, cx)
                    else {
                        continue;
                    };
                    let root_name = worktree.read(cx).root_name_str().to_string();
                    let relative = project_path.path.as_unix_str();
                    if is_private(&workspace_handle, &root_name, relative, cx) {
                        continue;
                    }
                    let rel_path = if include_root_name {
                        format!("{root_name}/{relative}")
                    } else {
                        relative.to_string()
                    };
                    let name = symbol.label.filter_text().to_string();
                    items.push(json!({
                        "kind": "symbol",
                        "label": name,
                        "path": rel_path,
                        "insert_text": format!("@{name} "),
                    }));
                }
            });
        }

        Ok(fit_items_payload(items, FILE_JSON_BUDGET))
    })
}

pub(super) fn skills(workspace: &Entity<Workspace>, args: &Value, cx: &App) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let limit = parse_limit(args);

    let view = root_thread_view(workspace, cx)?;
    let thread_view = view.read(cx);
    let is_native = thread_view.as_native_thread(cx).is_some();
    let capabilities = thread_view.session_capabilities.read();

    let raw_skills: Vec<(String, String)> = capabilities
        .completion_skills()
        .into_iter()
        .map(|skill| (skill.name.to_string(), skill.description.to_string()))
        .collect();

    let mut raw_commands: Vec<(String, String)> = capabilities
        .available_commands()
        .iter()
        .map(|command| {
            (
                command.name().to_string(),
                command.description().to_string(),
            )
        })
        .collect();

    if is_native && !raw_commands.iter().any(|(name, _)| name == "compact") {
        raw_commands.push((
            "compact".to_string(),
            "Compact the conversation history to free context window".to_string(),
        ));
    }

    Ok(filter_skills_and_commands(
        &raw_skills,
        &raw_commands,
        &query,
        limit,
        FILE_JSON_BUDGET,
    ))
}

fn parse_limit(args: &Value) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT)
}

fn matches_query(name: &str, description: &str, query_lower: &str) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    name.to_lowercase().contains(query_lower) || description.to_lowercase().contains(query_lower)
}

pub(super) fn filter_skills_and_commands(
    skills: &[(String, String)],
    commands: &[(String, String)],
    query: &str,
    limit: usize,
    budget: usize,
) -> Value {
    let query_lower = query.trim().to_lowercase();
    let filtered_skills: Vec<Value> = skills
        .iter()
        .filter(|(name, description)| matches_query(name, description, &query_lower))
        .take(limit)
        .map(|(name, description)| {
            json!({
                "name": name,
                "description": truncate(description, 240),
                "insert_text": format!("/{name} "),
            })
        })
        .collect();

    let filtered_commands: Vec<Value> = commands
        .iter()
        .filter(|(name, description)| matches_query(name, description, &query_lower))
        .take(limit)
        .map(|(name, description)| {
            json!({
                "name": name,
                "description": truncate(description, 240),
                "insert_text": format!("/{name} "),
            })
        })
        .collect();

    let mut payload = json!({
        "skills": filtered_skills,
        "commands": filtered_commands,
    });
    if payload.to_string().len() <= budget {
        return payload;
    }
    if let Some(skills_arr) = payload.get_mut("skills").and_then(Value::as_array_mut) {
        while payload.to_string().len() > budget && !skills_arr.is_empty() {
            skills_arr.pop();
        }
    }
    if let Some(commands_arr) = payload.get_mut("commands").and_then(Value::as_array_mut) {
        while payload.to_string().len() > budget && !commands_arr.is_empty() {
            commands_arr.pop();
        }
    }
    payload
}

pub(super) fn fit_items_payload(mut items: Vec<Value>, budget: usize) -> Value {
    let mut payload = json!({ "items": items });
    if payload.to_string().len() <= budget {
        return payload;
    }
    while payload.to_string().len() > budget && !items.is_empty() {
        items.pop();
        payload = json!({ "items": items });
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_skills_and_commands_formats_and_matches_case_insensitively() {
        let skills = vec![
            (
                "gpui-test".to_string(),
                "Write GPUI tests in Zed".to_string(),
            ),
            ("rust-fmt".to_string(), "Format Rust code".to_string()),
        ];
        let commands = vec![
            (
                "compact".to_string(),
                "Compact conversation history".to_string(),
            ),
            ("clear".to_string(), "Clear messages".to_string()),
        ];

        let result = filter_skills_and_commands(&skills, &commands, "COMP", 20, FILE_JSON_BUDGET);
        assert_eq!(result["skills"], json!([]));
        assert_eq!(result["commands"].as_array().expect("commands").len(), 1);
        assert_eq!(result["commands"][0]["name"], "compact");
        assert_eq!(result["commands"][0]["insert_text"], "/compact ");

        let all = filter_skills_and_commands(&skills, &commands, "", 20, FILE_JSON_BUDGET);
        assert_eq!(all["skills"].as_array().expect("skills").len(), 2);
        assert_eq!(all["skills"][0]["insert_text"], "/gpui-test ");
        assert_eq!(all["commands"].as_array().expect("commands").len(), 2);
    }

    #[test]
    fn fit_items_payload_respects_budget() {
        let items: Vec<Value> = (0..100)
            .map(|index| {
                json!({
                    "kind": "file",
                    "label": format!("file_{index}.rs"),
                    "path": format!("src/{}/file_{index}.rs", "a".repeat(50)),
                    "insert_text": format!("@src/file_{index}.rs "),
                })
            })
            .collect();
        let fitted = fit_items_payload(items, 1_000);
        assert!(fitted.to_string().len() <= 1_000);
    }
}
