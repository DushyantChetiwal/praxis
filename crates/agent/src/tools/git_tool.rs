use std::{fmt::Write as _, sync::Arc};

use crate::{AgentTool, ToolCallEventStream, ToolCapability, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use git::{
    repository::DiffType,
    status::{FileStatus, StatusCode},
};
use gpui::{App, Entity, SharedString, Task};
use project::{Project, git_store::Repository};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_DIFF_BYTES: usize = 80_000;
const MAX_DIFF_BYTES: usize = 200_000;

fn repositories(project: &Entity<Project>, cx: &App) -> Result<Vec<Entity<Repository>>, String> {
    let mut repositories = project
        .read(cx)
        .repositories(cx)
        .values()
        .cloned()
        .collect::<Vec<_>>();
    repositories.sort_by_key(|repository| {
        repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .display()
            .to_string()
    });
    if repositories.is_empty() {
        Err("The project does not contain a Git repository.".into())
    } else {
        Ok(repositories)
    }
}

fn repository_name(repository: &Entity<Repository>, cx: &App) -> String {
    repository
        .read(cx)
        .snapshot()
        .work_directory_abs_path
        .display()
        .to_string()
}

fn is_credential_query_key(key: &str) -> bool {
    let encoded = format!("{key}=");
    let decoded = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .map_or_else(|| key.to_string(), |(key, _)| key.into_owned());
    let normalized = decoded
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();

    normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("password")
        || normalized.contains("passwd")
        || normalized.contains("credential")
        || normalized.contains("authorization")
        || normalized.contains("signature")
        || normalized == "auth"
        || normalized == "sig"
        || normalized == "key"
        || normalized.ends_with("apikey")
}

fn redact_query_credentials(remote: &str) -> String {
    let Some(query_start) = remote.find('?') else {
        return remote.to_string();
    };
    let query_end = remote[query_start..]
        .find('#')
        .map_or(remote.len(), |offset| query_start + offset);
    let mut output = String::with_capacity(remote.len());
    output.push_str(&remote[..=query_start]);
    for (index, pair) in remote[query_start + 1..query_end].split('&').enumerate() {
        if index > 0 {
            output.push('&');
        }
        let key = pair.split_once('=').map_or(pair, |(key, _)| key);
        if is_credential_query_key(key) {
            write!(output, "{key}=[redacted]").unwrap();
        } else {
            output.push_str(pair);
        }
    }
    output.push_str(&remote[query_end..]);
    output
}

fn redact_unparsed_userinfo(remote: &str) -> String {
    let authority_start = remote.find("://").map_or(0, |index| index + 3);
    let authority_end = remote[authority_start..]
        .find(['/', '?', '#'])
        .map_or(remote.len(), |offset| authority_start + offset);
    let authority = &remote[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return remote.to_string();
    };

    let userinfo_end = authority_start + at + 1;
    let mut output = String::with_capacity(remote.len());
    if authority_start == 0 {
        output.push_str("[redacted]@");
    } else {
        output.push_str(&remote[..authority_start]);
    }
    output.push_str(&remote[userinfo_end..]);
    output
}

fn redact_remote_url(remote: &str) -> String {
    let without_userinfo = match url::Url::parse(remote) {
        Ok(mut url) if url.has_host() => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        _ => redact_unparsed_userinfo(remote),
    };
    redact_query_credentials(&without_userinfo)
}

fn status_char(status: StatusCode) -> char {
    match status {
        StatusCode::Modified => 'M',
        StatusCode::TypeChanged => 'T',
        StatusCode::Added => 'A',
        StatusCode::Deleted => 'D',
        StatusCode::Renamed => 'R',
        StatusCode::Copied => 'C',
        StatusCode::Unmodified => ' ',
    }
}

fn status_code(status: FileStatus) -> String {
    match status {
        FileStatus::Untracked => "??".into(),
        FileStatus::Ignored => "!!".into(),
        FileStatus::Unmerged(_) => "UU".into(),
        FileStatus::Tracked(status) => format!(
            "{}{}",
            status_char(status.index_status),
            status_char(status.worktree_status)
        ),
    }
}

fn truncate_utf8(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }

    let mut boundary = max_bytes;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    text.push_str(
        "\n\n[Diff truncated. Increase `max_bytes` to inspect more, up to 200000 bytes.]\n",
    );
    text
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitStatusToolInput {}

/// Inspect the branch and working-tree status of every Git repository in the project.
/// This uses Zed's Git backend directly and cannot modify repository state.
pub struct GitStatusTool {
    project: Entity<Project>,
}

impl GitStatusTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for GitStatusTool {
    type Input = GitStatusToolInput;
    type Output = String;

    const NAME: &'static str = "git_status";

    fn capability() -> ToolCapability {
        ToolCapability::ReadOnly
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Inspect Git status".into()
    }

    fn run(
        self: Arc<Self>,
        _input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let output = repositories(&self.project, cx).map(|repositories| {
            let mut output = String::new();
            for repository in repositories {
                let snapshot = repository.read(cx).snapshot();
                writeln!(output, "# {}", snapshot.work_directory_abs_path.display()).unwrap();
                if let Some(branch) = snapshot.branch.as_ref() {
                    write!(output, "Branch: {}", branch.name()).unwrap();
                    if let Some(upstream) = branch.upstream.as_ref() {
                        write!(output, " -> {}", upstream.ref_name).unwrap();
                        if let Some(tracking) = upstream.tracking.status() {
                            write!(
                                output,
                                " (ahead {}, behind {})",
                                tracking.ahead, tracking.behind
                            )
                            .unwrap();
                        } else if upstream.tracking.is_gone() {
                            output.push_str(" (upstream gone)");
                        }
                    }
                    output.push('\n');
                } else {
                    output.push_str("Branch: detached or unborn HEAD\n");
                }

                let statuses = snapshot.status().collect::<Vec<_>>();
                if statuses.is_empty() {
                    output.push_str("Working tree clean\n\n");
                } else {
                    for entry in statuses {
                        writeln!(
                            output,
                            "{} {}",
                            status_code(entry.status),
                            entry.repo_path.as_unix_str()
                        )
                        .unwrap();
                    }
                    output.push('\n');
                }
            }
            output
        });
        Task::ready(output)
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitBranchesToolInput {}

/// List local and remote branches for every Git repository in the project.
pub struct GitBranchesTool {
    project: Entity<Project>,
}

impl GitBranchesTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for GitBranchesTool {
    type Input = GitBranchesToolInput;
    type Output = String;

    const NAME: &'static str = "git_branches";

    fn capability() -> ToolCapability {
        ToolCapability::ReadOnly
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "List Git branches".into()
    }

    fn run(
        self: Arc<Self>,
        _input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let repositories = match repositories(&self.project, cx) {
            Ok(repositories) => repositories,
            Err(error) => return Task::ready(Err(error)),
        };
        let jobs = repositories
            .into_iter()
            .map(|repository| {
                let name = repository_name(&repository, cx);
                let receiver = repository.update(cx, |repository, _cx| repository.branches());
                (name, receiver)
            })
            .collect::<Vec<_>>();

        cx.spawn(async move |_cx| {
            let mut output = String::new();
            for (repository, receiver) in jobs {
                let result = receiver
                    .await
                    .map_err(|error| {
                        format!("Could not inspect branches for {repository}: {error}")
                    })?
                    .map_err(|error| {
                        format!("Could not inspect branches for {repository}: {error}")
                    })?;
                writeln!(output, "# {repository}").unwrap();
                if let Some(error) = result.error {
                    writeln!(output, "Warning: {error}").unwrap();
                }
                if result.branches.is_empty() {
                    output.push_str("No branches found\n\n");
                    continue;
                }
                for branch in result.branches {
                    let marker = if branch.is_head { '*' } else { ' ' };
                    write!(output, "{marker} {}", branch.name()).unwrap();
                    if let Some(upstream) = branch.upstream.as_ref() {
                        write!(output, " -> {}", upstream.ref_name).unwrap();
                        if let Some(tracking) = upstream.tracking.status() {
                            write!(
                                output,
                                " (ahead {}, behind {})",
                                tracking.ahead, tracking.behind
                            )
                            .unwrap();
                        }
                    }
                    if let Some(commit) = branch.most_recent_commit.as_ref() {
                        write!(output, " | {} {}", commit.sha, commit.subject).unwrap();
                    }
                    output.push('\n');
                }
                output.push('\n');
            }
            Ok(output)
        })
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitRemotesToolInput {}

/// List remote names and URLs for every Git repository in the project.
pub struct GitRemotesTool {
    project: Entity<Project>,
}

impl GitRemotesTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for GitRemotesTool {
    type Input = GitRemotesToolInput;
    type Output = String;

    const NAME: &'static str = "git_remotes";

    fn capability() -> ToolCapability {
        ToolCapability::ReadOnly
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "List Git remotes".into()
    }

    fn run(
        self: Arc<Self>,
        _input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let repositories = match repositories(&self.project, cx) {
            Ok(repositories) => repositories,
            Err(error) => return Task::ready(Err(error)),
        };
        let jobs = repositories
            .into_iter()
            .map(|repository| {
                let name = repository_name(&repository, cx);
                let receiver = repository.update(cx, |repository, _cx| repository.remote_urls());
                (name, receiver)
            })
            .collect::<Vec<_>>();

        cx.spawn(async move |_cx| {
            let mut output = String::new();
            for (repository, receiver) in jobs {
                let remotes = receiver
                    .await
                    .map_err(|error| {
                        format!("Could not inspect remotes for {repository}: {error}")
                    })?
                    .map_err(|error| {
                        format!("Could not inspect remotes for {repository}: {error}")
                    })?;
                writeln!(output, "# {repository}").unwrap();
                let mut remotes = remotes.into_iter().collect::<Vec<_>>();
                remotes.sort_by(|left, right| left.0.cmp(&right.0));
                if remotes.is_empty() {
                    output.push_str("No remotes configured\n\n");
                } else {
                    for (name, url) in remotes {
                        writeln!(output, "{name}: {}", redact_remote_url(&url)).unwrap();
                    }
                    output.push('\n');
                }
            }
            Ok(output)
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffMode {
    /// Changes between HEAD and the working tree, including staged changes.
    #[default]
    Worktree,
    /// Changes staged in the index relative to HEAD.
    Staged,
    /// Changes since the merge base with `base_ref`.
    MergeBase,
}

/// Read a Git diff without invoking a shell command.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitDiffToolInput {
    #[serde(default)]
    pub mode: GitDiffMode,
    /// Required when `mode` is `merge_base`, for example `origin/main`.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Maximum UTF-8 bytes returned per repository. Defaults to 80000 and is capped at 200000.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

pub struct GitDiffTool {
    project: Entity<Project>,
}

impl GitDiffTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for GitDiffTool {
    type Input = GitDiffToolInput;
    type Output = String;

    const NAME: &'static str = "git_diff";

    fn capability() -> ToolCapability {
        ToolCapability::ReadOnly
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input.map(|input| input.mode) {
            Ok(GitDiffMode::Staged) => "Inspect staged Git diff".into(),
            Ok(GitDiffMode::MergeBase) => "Inspect merge-base Git diff".into(),
            _ => "Inspect Git diff".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let mode = input.mode;
            let base_ref: Option<SharedString> = match mode {
                GitDiffMode::MergeBase => Some(
                    input
                        .base_ref
                        .filter(|base_ref| !base_ref.trim().is_empty())
                        .ok_or_else(|| "`base_ref` is required for a merge-base diff.".to_string())?
                        .into(),
                ),
                _ => None,
            };
            let max_bytes = input
                .max_bytes
                .unwrap_or(DEFAULT_DIFF_BYTES)
                .clamp(1, MAX_DIFF_BYTES);
            let jobs = cx.update(|cx| {
                repositories(&project, cx).map(|repositories| {
                    repositories
                        .into_iter()
                        .map(|repository| {
                            let name = repository_name(&repository, cx);
                            let diff_type = match mode {
                                GitDiffMode::Worktree => DiffType::HeadToWorktree,
                                GitDiffMode::Staged => DiffType::HeadToIndex,
                                GitDiffMode::MergeBase => DiffType::MergeBase {
                                    base_ref: base_ref.clone().unwrap(),
                                },
                            };
                            let receiver = repository
                                .update(cx, |repository, cx| repository.diff(diff_type, cx));
                            (name, receiver)
                        })
                        .collect::<Vec<_>>()
                })
            })?;

            let mut output = String::new();
            for (repository, receiver) in jobs {
                let diff = receiver
                    .await
                    .map_err(|error| format!("Could not read diff for {repository}: {error}"))?
                    .map_err(|error| format!("Could not read diff for {repository}: {error}"))?;
                writeln!(output, "# {repository}").unwrap();
                if diff.is_empty() {
                    output.push_str("No changes\n\n");
                } else {
                    output.push_str(&truncate_utf8(diff, max_bytes));
                    output.push_str("\n\n");
                }
            }
            Ok(output)
        })
    }
}

/// Inspect one commit's metadata and changed files in every Git repository.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitShowToolInput {
    /// A commit, tag, branch, or other revision accepted by Git, such as `HEAD`.
    pub revision: String,
}

pub struct GitShowTool {
    project: Entity<Project>,
}

impl GitShowTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for GitShowTool {
    type Input = GitShowToolInput;
    type Output = String;

    const NAME: &'static str = "git_show";

    fn capability() -> ToolCapability {
        ToolCapability::ReadOnly
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        input
            .map(|input| format!("Inspect Git revision `{}`", input.revision).into())
            .unwrap_or_else(|_| "Inspect Git revision".into())
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let revision = input.revision.trim().to_string();
            if revision.is_empty() {
                return Err("`revision` cannot be empty.".into());
            }
            let jobs = cx.update(|cx| {
                repositories(&project, cx).map(|repositories| {
                    repositories
                        .into_iter()
                        .map(|repository| {
                            let name = repository_name(&repository, cx);
                            let details = repository
                                .update(cx, |repository, _cx| repository.show(revision.clone()));
                            let diff = repository.update(cx, |repository, _cx| {
                                repository.load_commit_diff(revision.clone(), false)
                            });
                            (name, details, diff)
                        })
                        .collect::<Vec<_>>()
                })
            })?;

            let mut output = String::new();
            let multiple_repositories = jobs.len() > 1;
            for (repository, details, diff) in jobs {
                let details = match details.await {
                    Ok(Ok(details)) => details,
                    Ok(Err(error)) if multiple_repositories => {
                        writeln!(output, "# {repository}\nRevision not found: {error}\n").unwrap();
                        continue;
                    }
                    Ok(Err(error)) => return Err(format!("Could not inspect {revision}: {error}")),
                    Err(error) => return Err(format!("Could not inspect {revision}: {error}")),
                };
                let diff = diff
                    .await
                    .map_err(|error| {
                        format!("Could not read changed files for {repository}: {error}")
                    })?
                    .map_err(|error| {
                        format!("Could not read changed files for {repository}: {error}")
                    })?;

                writeln!(output, "# {repository}").unwrap();
                writeln!(output, "Commit: {}", details.sha).unwrap();
                writeln!(
                    output,
                    "Author: {} <{}>",
                    details.author_name, details.author_email
                )
                .unwrap();
                writeln!(output, "Timestamp: {}", details.commit_timestamp).unwrap();
                writeln!(output, "Message:\n{}", details.message).unwrap();
                if diff.is_shallow_boundary {
                    output.push_str("Warning: this commit is a shallow-history boundary.\n");
                }
                if diff.files.is_empty() {
                    output.push_str("Changed files: none\n\n");
                } else {
                    output.push_str("Changed files:\n");
                    for file in diff.files {
                        writeln!(output, "- {:?} {}", file.status(), file.path.as_unix_str())
                            .unwrap();
                    }
                    output.push('\n');
                }
            }
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_match_porcelain_layout() {
        assert_eq!(status_code(FileStatus::Untracked), "??");
        assert_eq!(
            status_code(FileStatus::Tracked(git::status::TrackedStatus {
                index_status: StatusCode::Added,
                worktree_status: StatusCode::Modified,
            })),
            "AM"
        );
    }

    #[test]
    fn diff_truncation_preserves_utf8_boundaries() {
        let output = truncate_utf8("aébc".into(), 2);
        assert!(output.starts_with('a'));
        assert!(!output.starts_with("aé"));
        assert!(output.contains("Diff truncated"));
    }

    #[test]
    fn remote_redaction_removes_url_userinfo() {
        assert_eq!(
            redact_remote_url("https://oauth2:top-secret@example.com/org/repo.git"),
            "https://example.com/org/repo.git"
        );
        assert_eq!(
            redact_remote_url("ssh://git:password@example.com/org/repo.git"),
            "ssh://example.com/org/repo.git"
        );
    }

    #[test]
    fn remote_redaction_removes_scp_style_userinfo() {
        assert_eq!(
            redact_remote_url("git@example.com:org/repo.git"),
            "[redacted]@example.com:org/repo.git"
        );
        assert_eq!(
            redact_remote_url("oauth2:top-secret@example.com:org/repo.git"),
            "[redacted]@example.com:org/repo.git"
        );
    }

    #[test]
    fn remote_redaction_hides_credential_query_parameters() {
        assert_eq!(
            redact_remote_url(
                "https://example.com/org/repo.git?ref=main&access_token=secret&X-Amz-Signature=signed#fragment"
            ),
            "https://example.com/org/repo.git?ref=main&access_token=[redacted]&X-Amz-Signature=[redacted]#fragment"
        );
        assert_eq!(
            redact_remote_url("https://example.com/repo?api%5Fkey=secret&monkey=visible"),
            "https://example.com/repo?api%5Fkey=[redacted]&monkey=visible"
        );
    }

    #[test]
    fn malformed_remote_redaction_fails_closed() {
        assert_eq!(
            redact_remote_url("https://user:secret@/repo?token=secret"),
            "https:///repo?token=[redacted]"
        );
        assert_eq!(
            redact_remote_url("not a url?credential=secret&branch=main"),
            "not a url?credential=[redacted]&branch=main"
        );
    }
}
