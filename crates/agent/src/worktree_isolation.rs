use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use project::git_store::worktrees_directory_for_repo;
use util::command::new_command;
use util::paths::PathStyle;

const MAX_BRANCH_SLUG_LEN: usize = 48;
const MAX_BRANCH_SUFFIX_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedWorktreeSpec {
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    pub branch_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeDiffSummary {
    pub branch: String,
    pub changed_files: usize,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeMergeOutcome {
    pub merged: bool,
    pub branch: String,
    pub summary: String,
}

fn sanitize_ref_segment(input: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(input.len().min(max_len));
    let mut prev_dash = true;

    for ch in input.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '_' {
            prev_dash = false;
            ch.to_ascii_lowercase()
        } else if !prev_dash {
            prev_dash = true;
            '-'
        } else {
            continue;
        };

        if out.len() >= max_len {
            break;
        }
        out.push(mapped);
    }

    while out.ends_with('-') || out.ends_with('_') {
        out.pop();
    }
    while out.starts_with('-') || out.starts_with('_') {
        out.remove(0);
    }

    if out.ends_with("lock") && input.trim_end().ends_with(".lock") {
        let trimmed = out.trim_end_matches("lock").trim_end_matches('-');
        out = trimmed.to_string();
    }

    out
}

pub fn sanitize_worktree_branch(label: &str, suffix: &str) -> String {
    let clean_label = sanitize_ref_segment(label, MAX_BRANCH_SLUG_LEN);
    let clean_suffix = sanitize_ref_segment(suffix, MAX_BRANCH_SUFFIX_LEN);

    let slug = if clean_label.is_empty() {
        "agent".to_string()
    } else {
        clean_label
    };

    if clean_suffix.is_empty() {
        format!("praxis/{slug}")
    } else {
        format!("praxis/{slug}-{clean_suffix}")
    }
}

pub fn plan_isolated_worktree(
    main_repo_path: &Path,
    label: &str,
    suffix: &str,
    worktree_setting: &str,
    path_style: PathStyle,
) -> Result<IsolatedWorktreeSpec> {
    let base_dir = worktrees_directory_for_repo(
        main_repo_path,
        worktree_setting,
        path_style,
    )?;
    let branch_name = sanitize_worktree_branch(label, suffix);
    let dir_name = branch_name.replace('/', "-");
    let worktree_path = base_dir.join(dir_name);

    Ok(IsolatedWorktreeSpec {
        repo_root: main_repo_path.to_path_buf(),
        worktree_path,
        branch_name,
    })
}

pub async fn create_local_git_worktree(spec: &IsolatedWorktreeSpec) -> Result<()> {
    if let Some(parent) = spec.worktree_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "creating worktree parent directory {}",
                parent.display()
            )
        })?;
    }

    let output = new_command("git")
        .arg("-C")
        .arg(&spec.repo_root)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&spec.branch_name)
        .arg(&spec.worktree_path)
        .output()
        .await
        .context("running `git worktree add`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree add failed: {}", stderr.trim());
    }

    Ok(())
}

pub async fn summarize_local_worktree_diff(
    worktree_path: &Path,
    branch: &str,
) -> Result<WorktreeDiffSummary> {
    let output = new_command("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("--no-pager")
        .arg("diff")
        .arg("--stat")
        .arg("HEAD")
        .output()
        .await
        .context("running `git diff --stat HEAD`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff --stat HEAD failed: {}", stderr.trim());
    }

    let summary = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let changed_files = if summary.is_empty() {
        0
    } else {
        let lines: Vec<&str> = summary.lines().collect();
        if lines.len() >= 2
            && lines.last().is_some_and(|last| {
                last.contains("changed")
                    || last.contains("insertion")
                    || last.contains("deletion")
            })
        {
            lines.len().saturating_sub(1)
        } else {
            lines.len()
        }
    };

    Ok(WorktreeDiffSummary {
        branch: branch.to_string(),
        changed_files,
        summary,
    })
}

pub async fn merge_local_git_worktree(
    main_repo_path: &Path,
    branch: &str,
) -> Result<WorktreeMergeOutcome> {
    let status_output = new_command("git")
        .arg("-C")
        .arg(main_repo_path)
        .arg("status")
        .arg("--porcelain")
        .output()
        .await
        .context("checking clean working tree before merge")?;

    if !status_output.status.success() {
        let stderr = String::from_utf8_lossy(&status_output.stderr);
        bail!("git status failed: {}", stderr.trim());
    }

    if !String::from_utf8_lossy(&status_output.stdout)
        .trim()
        .is_empty()
    {
        bail!(
            "Main repository working tree has uncommitted changes; commit or stash them before merging `{branch}`."
        );
    }

    let merge_output = new_command("git")
        .arg("-C")
        .arg(main_repo_path)
        .arg("merge")
        .arg("--no-edit")
        .arg(branch)
        .output()
        .await
        .context("running `git merge --no-edit`")?;

    if !merge_output.status.success() {
        let stderr = String::from_utf8_lossy(&merge_output.stderr);
        bail!("git merge failed: {}", stderr.trim());
    }

    let summary = String::from_utf8_lossy(&merge_output.stdout)
        .trim()
        .to_string();

    Ok(WorktreeMergeOutcome {
        merged: true,
        branch: branch.to_string(),
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_worktree_branch_edge_cases() {
        assert_eq!(
            sanitize_worktree_branch("Fix Auth Bug", "a1b2"),
            "praxis/fix-auth-bug-a1b2"
        );
        assert_eq!(
            sanitize_worktree_branch("../../../etc/passwd", "1"),
            "praxis/etc-passwd-1"
        );
        assert_eq!(
            sanitize_worktree_branch("branch.lock", "99"),
            "praxis/branch-99"
        );
        assert_eq!(
            sanitize_worktree_branch("---...///", ""),
            "praxis/agent"
        );
        assert_eq!(
            sanitize_worktree_branch("🚀 Unicode Café", "s1"),
            "praxis/unicode-caf-s1"
        );

        let long_label = "a".repeat(120);
        let long_suffix = "b".repeat(40);
        let branch = sanitize_worktree_branch(&long_label, &long_suffix);
        assert!(branch.starts_with("praxis/"));
        assert!(branch.len() <= "praxis/".len() + MAX_BRANCH_SLUG_LEN + 1 + MAX_BRANCH_SUFFIX_LEN);
        assert!(!branch.contains(".."));
        assert!(!branch.ends_with(".lock"));
    }

    #[test]
    fn test_plan_isolated_worktree() {
        let repo = Path::new("/code/praxis");
        let spec = plan_isolated_worktree(
            repo,
            "Parallel Feature",
            "w1",
            "../worktrees",
            PathStyle::Unix,
        )
        .unwrap();

        assert_eq!(spec.repo_root, repo);
        assert_eq!(spec.branch_name, "praxis/parallel-feature-w1");
        assert_eq!(
            spec.worktree_path,
            PathBuf::from("/code/worktrees/praxis/praxis-parallel-feature-w1")
        );

        assert!(
            plan_isolated_worktree(repo, "Test", "1", "..", PathStyle::Unix).is_err()
        );
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git command should spawn");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_local_git_worktree_create_diff_and_merge() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let repo_path = temp.path().join("repo");
            std::fs::create_dir_all(&repo_path).unwrap();

            run_git(&repo_path, &["init"]);
            run_git(&repo_path, &["config", "user.email", "test@praxis.dev"]);
            run_git(&repo_path, &["config", "user.name", "Praxis Test"]);

            std::fs::write(repo_path.join("README.md"), "initial\n").unwrap();
            run_git(&repo_path, &["add", "README.md"]);
            run_git(&repo_path, &["commit", "-m", "Initial commit"]);

            let spec = plan_isolated_worktree(
                &repo_path,
                "Worker Task",
                "t1",
                "../worktrees",
                PathStyle::local(),
            )
            .unwrap();

            create_local_git_worktree(&spec).await.unwrap();
            assert!(spec.worktree_path.exists());

            std::fs::write(spec.worktree_path.join("README.md"), "updated in worktree\n")
                .unwrap();

            let diff = summarize_local_worktree_diff(&spec.worktree_path, &spec.branch_name)
                .await
                .unwrap();
            assert_eq!(diff.branch, spec.branch_name);
            assert_eq!(diff.changed_files, 1);
            assert!(diff.summary.contains("README.md"));

            run_git(&spec.worktree_path, &["add", "README.md"]);
            run_git(&spec.worktree_path, &["commit", "-m", "Worktree change"]);

            // Dirty main repo must refuse merge
            std::fs::write(repo_path.join("dirty.txt"), "uncommitted\n").unwrap();
            assert!(merge_local_git_worktree(&repo_path, &spec.branch_name)
                .await
                .is_err());
            std::fs::remove_file(repo_path.join("dirty.txt")).unwrap();

            let outcome = merge_local_git_worktree(&repo_path, &spec.branch_name)
                .await
                .unwrap();
            assert!(outcome.merged);
            assert_eq!(outcome.branch, spec.branch_name);
            let merged_contents =
                std::fs::read_to_string(repo_path.join("README.md")).unwrap();
            assert_eq!(merged_contents, "updated in worktree\n");
        });
    }
}
