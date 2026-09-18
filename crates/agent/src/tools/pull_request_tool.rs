use std::{future::Future, sync::Arc};

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
const MAX_PAGES: usize = 20;
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

#[derive(Default)]
struct ResponseBudget {
    used: usize,
}

struct JsonPage {
    value: Value,
    next: Option<url::Url>,
}

fn json_request(
    target: &PullRequestTarget,
    url: &url::Url,
    github_token: Option<&str>,
    gitlab_token: Option<&str>,
) -> anyhow::Result<Request<AsyncBody>> {
    let mut request = Request::get(url.as_str())
        .header("Accept", "application/json")
        .header("User-Agent", "Zed-Agent")
        .follow_redirects(http_client::RedirectPolicy::NoFollow);
    match target {
        PullRequestTarget::Github { .. } => {
            request = request.header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(token) = github_token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
        }
        PullRequestTarget::Gitlab { .. } => {
            if let Some(token) = gitlab_token {
                request = request.header("PRIVATE-TOKEN", token);
            }
        }
    }
    Ok(request.body(AsyncBody::default())?)
}

fn validated_next_page(
    target: &PullRequestTarget,
    current: &url::Url,
    link: Option<&str>,
    gitlab_next_page: Option<&str>,
) -> anyhow::Result<Option<url::Url>> {
    let from_link = link.and_then(|header| {
        header.split(',').find_map(|entry| {
            let mut parts = entry.split(';');
            let target = parts.next()?.trim();
            let is_next = parts.any(|part| {
                let part = part.trim();
                part == "rel=\"next\"" || part == "rel=next"
            });
            is_next.then(|| target.trim_start_matches('<').trim_end_matches('>'))
        })
    });
    let mut next = if let Some(next) = from_link {
        Some(current.join(next).context("pagination link is invalid")?)
    } else if let Some(page) = gitlab_next_page.filter(|page| !page.is_empty()) {
        let mut next = current.clone();
        let pairs = next
            .query_pairs()
            .filter(|(key, _)| key != "page")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        next.set_query(None);
        next.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("page", page);
        Some(next)
    } else {
        None
    };

    if let Some(next) = next.as_ref()
        && (next.scheme() != "https"
            || next.host_str() != Some(target.api_host())
            || !next.username().is_empty()
            || next.password().is_some()
            || next.port().is_some())
    {
        bail!("pagination link left the authorized HTTPS API host");
    }
    Ok(next.take())
}

async fn fetch_json_page(
    http_client: &Arc<HttpClientWithUrl>,
    target: &PullRequestTarget,
    url: url::Url,
    budget: &mut ResponseBudget,
) -> anyhow::Result<JsonPage> {
    let github_token = std::env::var("GITHUB_TOKEN").ok();
    let gitlab_token = std::env::var("GITLAB_TOKEN").ok();
    let request = json_request(
        target,
        &url,
        github_token.as_deref(),
        gitlab_token.as_deref(),
    )?;
    let mut response = http_client
        .send(request)
        .await
        .with_context(|| format!("failed to fetch {url}"))?;
    let status = response.status();
    if status.is_redirection() {
        bail!("{url} returned a redirect; redirects are not allowed");
    }
    let link = response
        .headers()
        .get("link")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let gitlab_next_page = response
        .headers()
        .get("x-next-page")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let remaining = MAX_RESPONSE_BYTES.saturating_sub(budget.used);
    if remaining == 0 {
        bail!("combined pull request responses exceeded {MAX_RESPONSE_BYTES} bytes");
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .take((remaining + 1) as u64)
        .read_to_end(&mut body)
        .await?;
    if body.len() > remaining {
        bail!("combined pull request responses exceeded {MAX_RESPONSE_BYTES} bytes");
    }
    budget.used += body.len();
    if !status.is_success() {
        let body = String::from_utf8_lossy(&body);
        bail!("{url} returned HTTP {}: {body}", status.as_u16());
    }
    let value =
        serde_json::from_slice(&body).with_context(|| format!("{url} returned invalid JSON"))?;
    let next = validated_next_page(target, &url, link.as_deref(), gitlab_next_page.as_deref())?;
    Ok(JsonPage { value, next })
}

fn append_json_page(combined: &mut Value, page: Value) -> anyhow::Result<()> {
    match (combined, page) {
        (Value::Array(combined), Value::Array(mut page)) => combined.append(&mut page),
        (Value::Object(combined), Value::Object(page)) => {
            for (key, value) in page {
                match (combined.get_mut(&key), value) {
                    (Some(Value::Array(combined)), Value::Array(mut page)) => {
                        combined.append(&mut page)
                    }
                    (None, value) => {
                        combined.insert(key, value);
                    }
                    _ => {}
                }
            }
        }
        _ => bail!("paginated API responses changed JSON shape between pages"),
    }
    Ok(())
}

async fn fetch_paginated_json(
    http_client: &Arc<HttpClientWithUrl>,
    target: &PullRequestTarget,
    mut url: url::Url,
    budget: &mut ResponseBudget,
) -> anyhow::Result<Value> {
    let mut combined = None;
    for page_number in 1..=MAX_PAGES {
        let page = fetch_json_page(http_client, target, url, budget).await?;
        if let Some(combined) = combined.as_mut() {
            append_json_page(combined, page.value)?;
        } else {
            combined = Some(page.value);
        }
        let Some(next) = page.next else {
            return Ok(combined.unwrap_or(Value::Null));
        };
        if page_number == MAX_PAGES {
            bail!("pagination exceeded the limit of {MAX_PAGES} pages");
        }
        url = next;
    }
    unreachable!("the bounded pagination loop always returns")
}

fn section_value(result: anyhow::Result<Value>) -> Value {
    result.unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

async fn inspection_or_cancellation<Inspection, Cancellation>(
    inspection: Inspection,
    cancellation: Cancellation,
) -> Result<Value, String>
where
    Inspection: Future<Output = anyhow::Result<Value>>,
    Cancellation: Future<Output = ()>,
{
    let inspection = inspection.fuse();
    let cancellation = cancellation.fuse();
    futures::pin_mut!(inspection, cancellation);
    futures::select! {
        result = inspection => result.map_err(|error| error.to_string()),
        _ = cancellation => Err("Pull request inspection cancelled by user".into()),
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
    let mut budget = ResponseBudget::default();
    let summary_url = github_api_url(owner, repo, &["pulls", &number_string])?;
    let summary = fetch_json_page(client, target, summary_url, &mut budget)
        .await?
        .value;
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
                output.insert(
                    "files".into(),
                    section_value(fetch_paginated_json(client, target, url, &mut budget).await),
                );
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
                        "issue_comments": section_value(
                            fetch_paginated_json(client, target, issue_comments, &mut budget).await
                        ),
                        "review_comments": section_value(
                            fetch_paginated_json(client, target, review_comments, &mut budget).await
                        ),
                    }),
                );
            }
            PullRequestSection::Reviews => {
                let url = with_page_size(
                    github_api_url(owner, repo, &["pulls", &number_string, "reviews"])?,
                    100,
                );
                output.insert(
                    "reviews".into(),
                    section_value(fetch_paginated_json(client, target, url, &mut budget).await),
                );
            }
            PullRequestSection::Checks => {
                let checks = async {
                    let head_sha = summary
                        .pointer("/head/sha")
                        .and_then(Value::as_str)
                        .context("GitHub response did not include the head commit SHA")?;
                    let url = with_page_size(
                        github_api_url(owner, repo, &["commits", head_sha, "check-runs"])?,
                        100,
                    );
                    fetch_paginated_json(client, target, url, &mut budget).await
                }
                .await;
                output.insert("checks".into(), section_value(checks));
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
    let mut budget = ResponseBudget::default();
    let summary = fetch_json_page(
        client,
        target,
        gitlab_api_url(project, number, &[])?,
        &mut budget,
    )
    .await?
    .value;
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
        output.insert(
            key.into(),
            section_value(fetch_paginated_json(client, target, url, &mut budget).await),
        );
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

            let inspection = async {
                match &target {
                    PullRequestTarget::Github {
                        owner,
                        repo,
                        number,
                    } => {
                        inspect_github(&http_client, &target, owner, repo, *number, &input.sections)
                            .await
                    }
                    PullRequestTarget::Gitlab { project, number } => {
                        inspect_gitlab(&http_client, &target, project, *number, &input.sections)
                            .await
                    }
                }
            };
            let result =
                inspection_or_cancellation(inspection, event_stream.cancelled_by_user()).await?;

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

    fn github_target() -> PullRequestTarget {
        PullRequestTarget::Github {
            owner: "zed-industries".into(),
            repo: "zed".into(),
            number: 1,
        }
    }

    #[test]
    fn authentication_headers_are_provider_specific() {
        let github = json_request(
            &github_target(),
            &url::Url::parse("https://api.github.com/repos/o/r/pulls/1").unwrap(),
            Some("github-secret"),
            Some("unused"),
        )
        .unwrap();
        assert_eq!(github.headers()["authorization"], "Bearer github-secret");
        assert!(github.headers().get("private-token").is_none());

        let gitlab_target = PullRequestTarget::Gitlab {
            project: "group/project".into(),
            number: 1,
        };
        let gitlab = json_request(
            &gitlab_target,
            &url::Url::parse("https://gitlab.com/api/v4/projects/group%2Fproject").unwrap(),
            Some("unused"),
            Some("gitlab-secret"),
        )
        .unwrap();
        assert_eq!(gitlab.headers()["private-token"], "gitlab-secret");
        assert!(gitlab.headers().get("authorization").is_none());
    }

    #[test]
    fn redirects_are_rejected() {
        let client = http_client::FakeHttpClient::create(|_| async move {
            Ok(http_client::Response::builder()
                .status(302)
                .header("Location", "https://example.com/stolen")
                .body(AsyncBody::default())
                .unwrap())
        });
        let mut budget = ResponseBudget::default();
        let error = futures::executor::block_on(fetch_json_page(
            &client,
            &github_target(),
            url::Url::parse("https://api.github.com/repos/o/r/pulls/1").unwrap(),
            &mut budget,
        ))
        .err()
        .expect("redirects must fail");
        assert!(error.to_string().contains("redirects are not allowed"));
    }

    #[test]
    fn pagination_combines_pages_and_stays_on_the_api_host() {
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let client = http_client::FakeHttpClient::create(move |request| {
            let recorded = recorded.clone();
            async move {
                let uri = request.uri().to_string();
                recorded.lock().push(uri.clone());
                let mut response = http_client::Response::builder().status(200);
                let body = if uri.contains("page=2") {
                    r#"[{"id":2}]"#
                } else {
                    response = response.header(
                        "Link",
                        "<https://api.github.com/repos/o/r/pulls/1/files?per_page=100&page=2>; rel=\"next\"",
                    );
                    r#"[{"id":1}]"#
                };
                Ok(response.body(AsyncBody::from(body)).unwrap())
            }
        });
        let mut budget = ResponseBudget::default();
        let value = futures::executor::block_on(fetch_paginated_json(
            &client,
            &github_target(),
            url::Url::parse("https://api.github.com/repos/o/r/pulls/1/files?per_page=100").unwrap(),
            &mut budget,
        ))
        .unwrap();
        assert_eq!(value, json!([{ "id": 1 }, { "id": 2 }]));
        assert_eq!(requests.lock().len(), 2);

        let error = validated_next_page(
            &github_target(),
            &url::Url::parse("https://api.github.com/repos/o/r/pulls/1/files").unwrap(),
            Some("<https://example.com/collect>; rel=\"next\""),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("authorized HTTPS API host"));
    }

    #[test]
    fn cumulative_response_size_is_capped_across_pages() {
        let client = http_client::FakeHttpClient::create(|request| async move {
            let mut response = http_client::Response::builder().status(200);
            if !request.uri().to_string().contains("page=2") {
                response = response.header(
                    "Link",
                    "<https://api.github.com/repos/o/r/pulls/1/files?page=2>; rel=\"next\"",
                );
            }
            Ok(response.body(AsyncBody::from("[]")).unwrap())
        });
        let mut budget = ResponseBudget {
            used: MAX_RESPONSE_BYTES - 2,
        };
        let error = futures::executor::block_on(fetch_paginated_json(
            &client,
            &github_target(),
            url::Url::parse("https://api.github.com/repos/o/r/pulls/1/files").unwrap(),
            &mut budget,
        ))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("combined pull request responses")
        );
    }

    #[test]
    fn section_failures_preserve_the_summary() {
        let client = http_client::FakeHttpClient::create(|request| async move {
            if request.uri().path().ends_with("/files") {
                Ok(http_client::Response::builder()
                    .status(500)
                    .body(AsyncBody::from(r#"{"message":"failed"}"#))
                    .unwrap())
            } else {
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(AsyncBody::from(r#"{"head":{"sha":"abc"}}"#))
                    .unwrap())
            }
        });
        let result = futures::executor::block_on(inspect_github(
            &client,
            &github_target(),
            "zed-industries",
            "zed",
            1,
            &[PullRequestSection::Files],
        ))
        .expect("a section failure should not discard the summary");
        assert_eq!(result.pointer("/summary/head/sha"), Some(&json!("abc")));
        assert!(
            result
                .pointer("/files/error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("HTTP 500"))
        );
    }

    #[test]
    fn inspection_can_be_cancelled_while_http_is_pending() {
        let result = futures::executor::block_on(inspection_or_cancellation(
            futures::future::pending::<anyhow::Result<Value>>(),
            futures::future::ready(()),
        ));
        assert_eq!(
            result,
            Err("Pull request inspection cancelled by user".into())
        );
    }

    #[test]
    fn output_truncation_preserves_utf8_boundaries() {
        let output = truncate_utf8("aébc".into(), 2);
        assert!(output.starts_with('a'));
        assert!(!output.starts_with("aé"));
        assert!(output.contains("Pull request output truncated"));
    }
}
