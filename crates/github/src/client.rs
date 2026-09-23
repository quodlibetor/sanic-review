use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{self, WrapErr, eyre};
use reqwest::{
    RequestBuilder, Response, StatusCode,
    header::{self, HeaderMap},
};

use crate::token::Token;

const USER_AGENT: &str = concat!("sanic-review/", env!("CARGO_PKG_VERSION"));
/// Used when GitHub rate-limits without saying for how long.
const DEFAULT_RETRY: Duration = Duration::from_mins(1);

/// Failures the poller handles differently from "log and carry on".
#[derive(Debug)]
pub enum ApiError {
    /// Wait this long before calling GitHub again.
    RateLimited {
        retry_after: Duration,
    },
    /// The token was rejected; retrying won't help.
    Unauthorized,
    Other(eyre::Report),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimited { retry_after } => {
                write!(f, "rate limited by GitHub for {}s", retry_after.as_secs())
            }
            Self::Unauthorized => {
                f.write_str("GitHub rejected the token; run `gh auth login` or update GITHUB_TOKEN")
            }
            Self::Other(report) => write!(f, "{report:?}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<eyre::Report> for ApiError {
    fn from(report: eyre::Report) -> Self {
        Self::Other(report)
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(err: reqwest::Error) -> Self {
        Self::Other(err.into())
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api_url: String,
    token: Token,
}

impl Client {
    pub fn new(api_url: &str, token: Token) -> eyre::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .wrap_err("building HTTP client")?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api_url)
    }

    pub(crate) fn get(&self, url: &str) -> RequestBuilder {
        self.authed(self.http.get(url))
    }

    pub(crate) fn post(&self, url: &str) -> RequestBuilder {
        self.authed(self.http.post(url))
    }

    fn authed(&self, req: RequestBuilder) -> RequestBuilder {
        req.bearer_auth(self.token.expose())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// Sends `req`, turning rate limits and auth failures into [`ApiError`]
    /// variants. `304 Not Modified` is returned as a response, not an error.
    pub(crate) async fn send(&self, req: RequestBuilder, what: &str) -> Result<Response, ApiError> {
        let resp = req
            .send()
            .await
            .wrap_err_with(|| format!("requesting {what}"))?;
        let status = resp.status();
        if status.is_success() || status == StatusCode::NOT_MODIFIED {
            return Ok(resp);
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(ApiError::Unauthorized);
        }
        if let Some(retry_after) = rate_limit(status, resp.headers()) {
            return Err(ApiError::RateLimited { retry_after });
        }
        let body = resp.text().await.unwrap_or_default();
        // Secondary rate limits can arrive as a bare 403 whose only signal
        // is the message.
        if status == StatusCode::FORBIDDEN && body.to_ascii_lowercase().contains("rate limit") {
            return Err(ApiError::RateLimited {
                retry_after: DEFAULT_RETRY,
            });
        }
        Err(eyre!("GitHub returned {status} for {what}: {body}").into())
    }
}

/// Recognizes GitHub's primary (`x-ratelimit-remaining: 0`) and secondary
/// (`retry-after`) rate limits.
fn rate_limit(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let number = |name: &str| -> Option<u64> { headers.get(name)?.to_str().ok()?.parse().ok() };
    if let Some(secs) = number("retry-after") {
        return Some(Duration::from_secs(secs));
    }
    if number("x-ratelimit-remaining") == Some(0) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        let wait = number("x-ratelimit-reset").map_or(DEFAULT_RETRY, |reset| {
            Duration::from_secs(reset.saturating_sub(now) + 1)
        });
        return Some(wait);
    }
    (status == StatusCode::TOO_MANY_REQUESTS).then_some(DEFAULT_RETRY)
}

/// The `rel="next"` URL from a `Link` header.
pub(crate) fn next_link(headers: &HeaderMap) -> Option<String> {
    let link = headers.get(header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == r#"rel="next""#)
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    header::HeaderName::from_static(k),
                    HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn finds_next_link() {
        let h = headers(&[(
            "link",
            r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#,
        )]);
        assert_eq!(
            next_link(&h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(next_link(&headers(&[])), None);
    }

    #[test]
    fn recognizes_rate_limits() {
        assert_eq!(
            rate_limit(StatusCode::FORBIDDEN, &headers(&[("retry-after", "30")])),
            Some(Duration::from_secs(30))
        );
        assert!(
            rate_limit(
                StatusCode::FORBIDDEN,
                &headers(&[("x-ratelimit-remaining", "0")])
            )
            .is_some()
        );
        // A plain 403 is a permissions problem, not a rate limit.
        assert_eq!(rate_limit(StatusCode::FORBIDDEN, &headers(&[])), None);
    }
}
