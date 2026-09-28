//! GitHub as Praxis Remote uses it: signing in with the device flow of the
//! Praxis Remote GitHub App, keeping the token fresh, and the REST calls the
//! channel makes. The app asks only for the user-level "Gists" permission,
//! which works without the app being installed anywhere.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use chrono::Utc;
use futures::AsyncReadExt as _;
use gpui::BackgroundExecutor;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, Request, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Praxis Remote GitHub App. A client ID is public by design; a build can
/// use its own app by setting `PRAXIS_REMOTE_CLIENT_ID` when compiling.
pub(super) const CLIENT_ID: &str = match option_env!("PRAXIS_REMOTE_CLIENT_ID") {
    Some(client_id) => client_id,
    None => "Iv23li9FOJx34CAaRKpA",
};

const API: &str = "https://api.github.com";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const MAX_RESPONSE_BYTES: u64 = 8_000_000;
/// Tokens are refreshed this long before GitHub says they expire.
const REFRESH_MARGIN_SECONDS: i64 = 300;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// When the access token expires, in Unix seconds. GitHub Apps can turn
    /// expiry off, in which case there is neither this nor a refresh token.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl Tokens {
    fn from_grant(body: &Value) -> Result<Self> {
        let access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .context("GitHub did not send a token")?
            .to_string();
        let refresh_token = body
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        let expires_at = body
            .get("expires_in")
            .and_then(Value::as_i64)
            .map(|seconds| Utc::now().timestamp() + seconds);
        Ok(Self {
            access_token,
            refresh_token,
            expires_at,
        })
    }

    pub(super) fn needs_refresh(&self) -> bool {
        self.refresh_token.is_some()
            && self
                .expires_at
                .is_some_and(|at| at - Utc::now().timestamp() < REFRESH_MARGIN_SECONDS)
    }
}

/// What the user types at github.com to let this computer sign in.
#[derive(Clone, Debug)]
pub(super) struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// A sign-in GitHub refused for good, as opposed to one that could not reach
/// GitHub. The user has to sign in again.
#[derive(Debug)]
pub(super) struct SignedOut(pub String);

impl std::fmt::Display for SignedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SignedOut {}

pub(super) fn is_signed_out(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SignedOut>().is_some()
}

pub(super) async fn start_device_flow(http: &Arc<dyn HttpClient>) -> Result<DeviceCode> {
    let body = post_form(http, DEVICE_CODE_URL, &[("client_id", CLIENT_ID)]).await?;
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        bail!("GitHub would not start the sign-in: {}", describe(error, &body));
    }
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("GitHub's sign-in answer had no {key}"))
    };
    Ok(DeviceCode {
        device_code: text("device_code")?,
        user_code: text("user_code")?,
        verification_uri: text("verification_uri")
            .unwrap_or_else(|_| "https://github.com/login/device".to_string()),
        interval: body.get("interval").and_then(Value::as_u64).unwrap_or(5).max(1),
        expires_in: body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(900),
    })
}

/// Waits until the user enters the code at github.com, or refuses, or the
/// code expires.
pub(super) async fn await_device_token(
    http: &Arc<dyn HttpClient>,
    code: &DeviceCode,
    executor: &BackgroundExecutor,
) -> Result<Tokens> {
    let mut interval = code.interval;
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    loop {
        executor.timer(Duration::from_secs(interval)).await;
        if Instant::now() > deadline {
            bail!("the sign-in code expired before it was entered; start again");
        }
        let form = [
            ("client_id", CLIENT_ID),
            ("device_code", code.device_code.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];
        let body = match post_form(http, ACCESS_TOKEN_URL, &form).await {
            Ok(body) => body,
            Err(error) => {
                log::warn!("Praxis Remote could not reach GitHub while signing in: {error:#}");
                continue;
            }
        };
        if body.get("access_token").is_some() {
            return Tokens::from_grant(&body);
        }
        match body.get("error").and_then(Value::as_str).unwrap_or_default() {
            "authorization_pending" => {}
            "slow_down" => {
                interval = body
                    .get("interval")
                    .and_then(Value::as_u64)
                    .unwrap_or(interval + 5)
                    .max(interval + 1);
            }
            error => bail!("GitHub refused the sign-in: {}", describe(error, &body)),
        }
    }
}

/// Swaps a refresh token for a new pair. Tokens from the device flow need no
/// client secret for this.
pub(super) async fn refresh(http: &Arc<dyn HttpClient>, refresh_token: &str) -> Result<Tokens> {
    let form = [
        ("client_id", CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    let body = post_form(http, ACCESS_TOKEN_URL, &form).await?;
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        return Err(SignedOut(format!(
            "GitHub no longer accepts this computer's sign-in ({}); sign in again",
            describe(error, &body)
        ))
        .into());
    }
    Tokens::from_grant(&body)
}

fn describe(error: &str, body: &Value) -> String {
    match error {
        "expired_token" => "the code expired".to_string(),
        "access_denied" => "it was cancelled".to_string(),
        "device_flow_disabled" => "the Praxis Remote app does not allow signing in this way".into(),
        "incorrect_client_credentials" | "unauthorized_client" => {
            "the Praxis Remote app was not recognized".to_string()
        }
        _ => body
            .get("error_description")
            .and_then(Value::as_str)
            .filter(|description| !description.is_empty())
            .unwrap_or(error)
            .to_string(),
    }
}

async fn post_form(http: &Arc<dyn HttpClient>, url: &str, form: &[(&str, &str)]) -> Result<Value> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let request = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", "Praxis")
        .follow_redirects(http_client::RedirectPolicy::NoFollow)
        .body(AsyncBody::from(body.into_bytes()))?;
    let mut response = http.send(request).await?;
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut bytes)
        .await?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(body) => Ok(body),
        Err(_) if status == StatusCode::NOT_FOUND => {
            bail!("the Praxis Remote app was not recognized by GitHub")
        }
        Err(_) => bail!("GitHub answered {} to the sign-in", status.as_u16()),
    }
}

/// A call GitHub refused, keeping the status for the callers that care which.
#[derive(Debug)]
pub(super) struct GitHubError {
    pub status: StatusCode,
    message: String,
}

impl std::fmt::Display for GitHubError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GitHubError {}

pub(super) fn github_status(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .downcast_ref::<GitHubError>()
        .map(|error| error.status)
}

pub(super) fn is_gone(error: &anyhow::Error) -> bool {
    matches!(
        github_status(error),
        Some(StatusCode::NOT_FOUND | StatusCode::GONE)
    )
}

pub(super) struct Reply {
    pub status: StatusCode,
    pub etag: Option<String>,
    /// The `Link` header's last page, for lists longer than one page.
    pub last_page: Option<String>,
    pub body: Value,
}

/// The REST API, signed in as the user.
pub(super) struct Api {
    http: Arc<dyn HttpClient>,
    pub tokens: Tokens,
}

impl Api {
    pub(super) fn new(http: Arc<dyn HttpClient>, tokens: Tokens) -> Self {
        Self { http, tokens }
    }

    pub(super) fn http(&self) -> &Arc<dyn HttpClient> {
        &self.http
    }

    /// Calls `path`, relative to the API, or a full API URL such as a `Link`.
    pub(super) async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        etag: Option<&str>,
    ) -> Result<Reply> {
        let url = if path.starts_with(API) {
            path.to_string()
        } else if path.starts_with('/') {
            format!("{API}{path}")
        } else {
            bail!("refusing to call {path:?}, which is not part of GitHub's API");
        };
        let mut request = Request::builder()
            .method(method.clone())
            .uri(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "Praxis")
            .header("Authorization", format!("Bearer {}", self.tokens.access_token))
            .follow_redirects(http_client::RedirectPolicy::NoFollow);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        let body = match body {
            Some(body) => {
                request = request.header("Content-Type", "application/json");
                AsyncBody::from(serde_json::to_vec(&body)?)
            }
            None => AsyncBody::default(),
        };
        let mut response = self.http.send(request.body(body)?).await?;
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let etag = header("etag");
        let last_page = header("link").and_then(|link| last_page(&link));
        let mut bytes = Vec::new();
        response
            .body_mut()
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut bytes)
            .await?;

        if status == StatusCode::NOT_MODIFIED {
            return Ok(Reply {
                status,
                etag,
                last_page,
                body: Value::Null,
            });
        }
        if !status.is_success() {
            let message = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|body| {
                    body.get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            return Err(GitHubError {
                status,
                message: format!(
                    "GitHub answered {} to {method} {path}: {message}",
                    status.as_u16()
                ),
            }
            .into());
        }
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .with_context(|| format!("GitHub's answer to {method} {path} was not JSON"))?
        };
        Ok(Reply {
            status,
            etag,
            last_page,
            body,
        })
    }

    pub(super) async fn login(&self) -> Result<String> {
        let reply = self.call(Method::GET, "/user", None, None).await?;
        reply
            .body
            .get("login")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("GitHub did not say who is signed in"))
    }
}

/// The URL of the last page in a `Link` header, if it is one of GitHub's.
fn last_page(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        if !params.split(';').any(|param| param.trim() == "rel=\"last\"") {
            return None;
        }
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        url.starts_with(API).then(|| url.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_link_header_gives_the_last_page_only_if_it_is_githubs() {
        let link = "<https://api.github.com/gists/1/comments?page=2>; rel=\"next\", \
                    <https://api.github.com/gists/1/comments?page=3>; rel=\"last\"";
        assert_eq!(
            last_page(link).as_deref(),
            Some("https://api.github.com/gists/1/comments?page=3")
        );
        assert_eq!(last_page("<https://evil.example/x>; rel=\"last\""), None);
        assert_eq!(last_page("<https://api.github.com/x>; rel=\"next\""), None);
    }

    #[test]
    fn tokens_are_read_from_a_grant_and_refreshed_before_they_expire() {
        let tokens = Tokens::from_grant(&json!({
            "access_token": "ghu_a",
            "refresh_token": "ghr_b",
            "expires_in": 28800,
        }))
        .expect("a grant");
        assert_eq!(tokens.refresh_token.as_deref(), Some("ghr_b"));
        assert!(!tokens.needs_refresh());

        let expiring = Tokens {
            expires_at: Some(Utc::now().timestamp() + 60),
            ..tokens.clone()
        };
        assert!(expiring.needs_refresh());

        let never = Tokens::from_grant(&json!({ "access_token": "ghu_a" })).expect("a grant");
        assert!(!never.needs_refresh(), "tokens without expiry are kept");
        assert!(Tokens::from_grant(&json!({ "error": "bad" })).is_err());
    }
}
