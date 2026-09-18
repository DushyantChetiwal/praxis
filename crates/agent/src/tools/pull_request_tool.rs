use std::sync::Arc;

use crate::sandboxing::{NetworkRequest, SandboxRequest};
use crate::{AgentTool, ToolCallEventStream, ToolCapability, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, anyhow, bail};
use futures::{AsyncReadExt as _, FutureExt as _};
use gpui::{App, SharedString, Task};
use http_client::{AsyncBody, HttpClient as _, HttpClientWithUrl, HttpRequestExt as _, Request};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_RESPONSE_BYTES: usize = 2_000_000;
const DEFAULT_OUTPUT_BYTES: usize = 120_000;
const MAX_OUTPUT_BYTES: usize = 500_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestSection {
    Files,
    Conversation,
    Reviews,
    Checks,
}

/// Inspect a GitHub pull request or GitLab merge request using HTTPS GET requests only.
///
/// Pass the browser URL of the PR/MR. The summary is always returned. Use `sections`
/// to request changed files, discussion comments, reviews/approvals, or checks/pipelines.
/// Public repositories need no token. Private GitHub repositories require `GITHUB_TOKEN`;
/// private GitLab projects require `GITLAB_TOKEN` in Zed's environment.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PullRequestToolInput {
    /// A `https://github.com/<owner>/<repo>/pull/<number>` or GitLab
    /// `https://gitlab.com/<namespace>/<project>/-/merge_requests/<iid>` URL.
    pub url: String,
    /// Additional data to retrieve. The PR/MR summary is always included.
    #[serde(default)]
    pub sections: Vec<PullRequestSection>,
    /// Maximum UTF-8 bytes returned. Defaults to 120000 and is capped at 500000.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

pub struct PullRequestTool {
    http_client: Arc<HttpClientWithUrl>,
}

impl PullRequestTool {
    pub fn new(http_client: Arc<HttpClientWithUrl>) -> Self {
        Self { http_client }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PullRequestTarget {
    Github {
        owner: String,
        repo: String,
        number: u64,
    },
    Gitlab {
        project: String,
        number: u64,
    },
}

impl PullRequestTarget {
    fn api_host(&self) -> &'static str {
        match self {
            Self::Github { .. } => "api.github.com",
            Self::Gitlab { .. } => "gitlab.com",
        }
    }

    fn provider_name(&self) -> &'static str {
        match self {
            Self::Github { .. } => "github",
            Self::Gitlab { .. } => "gitlab",
        }
    }
}

fn parse_pull_request_url(input: &str) -> anyhow::Result<PullRequestTarget> {
    let url = url::Url::parse(input).context("invalid pull request URL")?;
    if url.scheme() != "https" {
        bail!("pull request URLs must use HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        bail!("pull request URLs cannot contain credentials or a custom port");
    }
    let host = url.host_str().context("pull request URL has no host")?;
    let segments = url
        .path_segments()
        .context("pull request URL has no path")?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    match host {
        "github.com" => {
            if segments.len() != 4 || segments[2] != "pull" {
                bail!("expected a GitHub URL like https://github.com/owner/repo/pull/123");
            }
            Ok(PullRequestTarget::Github {
                owner: segments[0].to_string(),
                repo: segments[1].to_string(),
                number: segments[3]
                    .parse()
                    .context("GitHub pull request number is invalid")?,
            })
        }
        "gitlab.com" => {
            let marker = segments
                .windows(2)
                .position(|pair| pair[0] == "-" && pair[1] == "merge_requests")
                .context(
                    "expected a GitLab URL like https://gitlab.com/group/project/-/merge_requests/123",
                )?;
            if marker < 2 || segments.len() != marker + 3 {
                bail!("expected a GitLab URL ending in /-/merge_requests/<iid>");
            }
            Ok(PullRequestTarget::Gitlab {
                project: segments[..marker].join("/"),
                number: segments[marker + 2]
                    .parse()
                    .context("GitLab merge request number is invalid")?,
            })
        }
        _ => bail!("only github.com and gitlab.com pull request URLs are supported"),
    }
}

fn github_api_url(owner: &str, repo: &str, suffix: &[&str]) -> anyhow::Result<url::Url> {
    let mut url = url::Url::parse("https://api.github.com")?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("GitHub API URL cannot be a base"))?
        .extend(["repos", owner, repo])
        .extend(suffix.iter().copied());
    Ok(url)
}

fn gitlab_api_url(project: &str, number: u64, suffix: &[&str]) -> anyhow::Result<url::Url> {
    let mut url = url::Url::parse("https://gitlab.com")?;
    let number = number.to_string();
    url.path_segments_mut()
        .map_err(|_| anyhow!("GitLab API URL cannot be a base"))?
        .extend(["api", "v4", "projects", project, "merge_requests", &number])
        .extend(suffix.iter().copied());
    Ok(url)
}

fn with_page_size(mut url: url::Url, page_size: u16) -> url::Url {
    url.query_pairs_mut()
        .append_pair("per_page", &page_size.to_string());
    url
}

async fn fetch_json(
    http_client: &Arc<HttpClientWithUrl>,
    target: &PullRequestTarget,
    url: url::Url,
) -> anyhow::Result<Value> {
    let mut request = Request::get(url.as_str())
        .header("Accept", "application/json")
        .header("User-Agent", "Zed-Agent")
        .follow_redirects(http_client::RedirectPolicy::NoFollow);
    match target {
        PullRequestTarget::Github { .. } => {
            request = request.header("X-GitHub-Api-Version", "2022-11-28");
            if let Ok(token) = std::env::var("GITHUB_TOKEN") {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
        }
        PullRequestTarget::Gitlab { .. } => {
            if let Ok(token) = std::env::var("GITLAB_TOKEN") {
                request = request.header("PRIVATE-TOKEN", token);
            }
        }
    }

    let mut response = http_client
        .send(request.body(AsyncBody::default())?)
        .await
        .with_context(|| format!("failed to fetch {url}"))?;
    let status = response.status();
    let mut body = Vec::new();
    response
        .body_mut()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .await?;
    if body.len() > MAX_RESPONSE_BYTES {
        bail!("response from {url} exceeded {MAX_RESPONSE_BYTES} bytes");
    }
    if !status.is_success() {
        let body = String::from_utf8_lossy(&body);
        bail!("{url} returned HTTP {}: {body}", status.as_u16());
    }
    serde_json::from_slice(&body).with_context(|| format!("{url} returned invalid JSON"))
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
    text.push_str("\n\n[Pull request output truncated. Increase `max_bytes` to inspect more.]\n");
    text
}

async fn inspect_github(
    client: &Arc<HttpClientWithUrl>,
    target: &PullRequestTarget,
    owner: &str,
    repo: &str,
    number: u64,
    sections: &[PullRequestSection],
) -> anyhow::Result<Value> {
    let number_string = number.to_string();
    let summary_url = github_api_url(owner, repo, &["pulls", &number_string])?;
    let summary = fetch_json(client, target, summary_url).await?;
    let mut output = serde_json::Map::new();
    output.insert("provider".into(), json!("github"));
    output.insert("summary".into(), summary.clone());

    for section in sections {
        match section {
            PullRequestSection::Files => {
                let url = with_page_size(
                    github_api_url(owner, repo, &["pulls", &number_string, "files"])?,
                    100,
                );
                output.insert("files".into(), fetch_json(client, target, url).await?);
            }
            PullRequestSection::Conversation => {
                let issue_comments = with_page_size(
                    github_api_url(owner, repo, &["issues", &number_string, "comments"])?,
                    100,
                );
                let review_comments = with_page_size(
                    github_api_url(owner, repo, &["pulls", &number_string, "comments"])?,
                    100,
                );
                output.insert(
                    "conversation".into(),
                    json!({
                        "issue_comments": fetch_json(client, target, issue_comments).await?,
                        "review_comments": fetch_json(client, target, review_comments).await?,
                    }),
                );
            }
            PullRequestSection::Reviews => {
                let url = with_page_size(
                    github_api_url(owner, repo, &["pulls", &number_string, "reviews"])?,
                    100,
                );
                output.insert("reviews".into(), fetch_json(client, target, url).await?);
            }
            PullRequestSection::Checks => {
                let head_sha = summary
                    .pointer("/head/sha")
                    .and_then(Value::as_str)
                    .context("GitHub response did not include the head commit SHA")?;
                let url = with_page_size(
                    github_api_url(owner, repo, &["commits", head_sha, "check-runs"])?,
                    100,
                );
                output.insert("checks".into(), fetch_json(client, target, url).await?);
            }
        }
    }
    Ok(Value::Object(output))
}

async fn inspect_gitlab(
    client: &Arc<HttpClientWithUrl>,
    target: &PullRequestTarget,
    project: &str,
    number: u64,
    sections: &[PullRequestSection],
) -> anyhow::Result<Value> {
    let summary = fetch_json(client, target, gitlab_api_url(project, number, &[])?).await?;
    let mut output = serde_json::Map::new();
    output.insert("provider".into(), json!("gitlab"));
    output.insert("summary".into(), summary);

    for section in sections {
        let (key, suffix, page_size) = match section {
            PullRequestSection::Files => ("files", "diffs", 100),
            PullRequestSection::Conversation => ("conversation", "notes", 100),
            PullRequestSection::Reviews => ("reviews", "approvals", 100),
            PullRequestSection::Checks => ("checks", "pipelines", 20),
        };
        let url = with_page_size(gitlab_api_url(project, number, &[suffix])?, page_size);
        output.insert(key.into(), fetch_json(client, target, url).await?);
    }
    Ok(Value::Object(output))
}

impl AgentTool for PullRequestTool {
    type Input = PullRequestToolInput;
    type Output = String;

    const NAME: &'static str = "pull_request";

    fn capability() -> ToolCapability {
        ToolCapability::ExternalRead
    }

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Fetch
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        input
            .map(|input| format!("Inspect pull request {}", input.url).into())
            .unwrap_or_else(|_| "Inspect pull request".into())
    }

    fn allow_in_restricted_mode() -> bool {
        false
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let http_client = self.http_client.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let target = parse_pull_request_url(&input.url).map_err(|error| error.to_string())?;

            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.url.clone()]);
                event_stream.authorize(
                    format!("Inspect {} pull request", target.provider_name()),
                    context,
                    cx,
                )
            });
            futures::select! {
                result = authorize.fuse() => result.map_err(|error| error.to_string())?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Pull request inspection cancelled by user".into());
                }
            };

            let needs_host_grant = cx.update(|cx| {
                crate::sandboxing::sandboxing_enabled(cx)
                    && !event_stream.unsandboxed_access_granted(cx)
            });
            if needs_host_grant {
                let host = http_proxy::HostPattern::parse(target.api_host())
                    .map_err(|error| error.to_string())?;
                let authorize_host = cx.update(|cx| {
                    event_stream.authorize_sandbox(
                        SandboxRequest {
                            network: NetworkRequest::Hosts(vec![host]),
                            ..Default::default()
                        },
                        String::new(),
                        cx,
                    )
                });
                futures::select! {
                    result = authorize_host.fuse() => result.map_err(|error| error.to_string())?,
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Pull request inspection cancelled by user".into());
                    }
                };
            }

            let result = match &target {
                PullRequestTarget::Github {
                    owner,
                    repo,
                    number,
                } => {
                    inspect_github(&http_client, &target, owner, repo, *number, &input.sections)
                        .await
                }
                PullRequestTarget::Gitlab { project, number } => {
                    inspect_gitlab(&http_client, &target, project, *number, &input.sections).await
                }
            }
            .map_err(|error| error.to_string())?;

            let max_bytes = input
                .max_bytes
                .unwrap_or(DEFAULT_OUTPUT_BYTES)
                .clamp(1, MAX_OUTPUT_BYTES);
            let output = serde_json::to_string_pretty(&result)
                .map_err(|error| format!("could not format pull request response: {error}"))?;
            Ok(truncate_utf8(output, max_bytes))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_pull_request_urls() {
        assert_eq!(
            parse_pull_request_url("https://github.com/zed-industries/zed/pull/123").unwrap(),
            PullRequestTarget::Github {
                owner: "zed-industries".into(),
                repo: "zed".into(),
                number: 123,
            }
        );
    }

    #[test]
    fn parses_nested_gitlab_merge_request_urls() {
        assert_eq!(
            parse_pull_request_url("https://gitlab.com/example/nested/project/-/merge_requests/42")
                .unwrap(),
            PullRequestTarget::Gitlab {
                project: "example/nested/project".into(),
                number: 42,
            }
        );
    }

    #[test]
    fn rejects_non_https_and_unrecognized_hosts() {
        assert!(parse_pull_request_url("http://github.com/o/r/pull/1").is_err());
        assert!(parse_pull_request_url("https://example.com/o/r/pull/1").is_err());
    }

    #[test]
    fn api_urls_encode_gitlab_project_paths() {
        let url = gitlab_api_url("group/nested/project", 12, &["diffs"]).unwrap();
        assert_eq!(
            url.as_str(),
            "https://gitlab.com/api/v4/projects/group%2Fnested%2Fproject/merge_requests/12/diffs"
        );
    }
}
