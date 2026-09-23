use std::time::Duration;

use color_eyre::eyre::WrapErr;
use reqwest::{StatusCode, header};
use sanic_core::{pr::PrKey, repo::RepoName};
use serde::Deserialize;

use crate::client::{ApiError, Client, next_link};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: String,
    /// GitHub's reason, e.g. `review_requested`, `author`, `comment`.
    pub reason: String,
    pub updated_at: String,
    /// Set when the notification is about a pull request.
    pub pr: Option<PrKey>,
}

#[derive(Debug)]
pub enum NotificationPoll {
    NotModified {
        poll_interval: Option<Duration>,
    },
    Changed {
        notifications: Vec<Notification>,
        /// Pass back as `if_modified_since` on the next poll.
        last_modified: Option<String>,
        poll_interval: Option<Duration>,
    },
}

#[derive(Deserialize)]
struct RawNotification {
    id: String,
    reason: String,
    updated_at: String,
    subject: RawSubject,
}

#[derive(Deserialize)]
struct RawSubject {
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
}

impl Client {
    /// Unread notifications, all pages. Never marks anything read.
    pub async fn notifications(
        &self,
        if_modified_since: Option<&str>,
    ) -> Result<NotificationPoll, ApiError> {
        let mut req = self
            .get(&self.url("/notifications"))
            .query(&[("per_page", "50")]);
        if let Some(since) = if_modified_since {
            req = req.header(header::IF_MODIFIED_SINCE, since);
        }
        let resp = self.send(req, "notifications").await?;
        let poll_interval = resp
            .headers()
            .get("x-poll-interval")
            .and_then(|v| v.to_str().ok()?.parse().ok())
            .map(Duration::from_secs);
        if resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(NotificationPoll::NotModified { poll_interval });
        }
        let last_modified = resp
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let mut notifications = Vec::new();
        let mut resp = resp;
        loop {
            let next = next_link(resp.headers());
            let page: Vec<RawNotification> =
                resp.json().await.wrap_err("decoding notifications")?;
            notifications.extend(page.into_iter().map(Notification::from));
            let Some(next) = next else { break };
            resp = self.send(self.get(&next), "notifications").await?;
        }
        Ok(NotificationPoll::Changed {
            notifications,
            last_modified,
            poll_interval,
        })
    }
}

impl From<RawNotification> for Notification {
    fn from(raw: RawNotification) -> Self {
        let pr = (raw.subject.kind == "PullRequest")
            .then(|| raw.subject.url.as_deref().and_then(parse_pull_url))
            .flatten();
        Self {
            id: raw.id,
            reason: raw.reason,
            updated_at: raw.updated_at,
            pr,
        }
    }
}

/// Parses `.../repos/{owner}/{name}/pulls/{number}`.
fn parse_pull_url(url: &str) -> Option<PrKey> {
    let (_, rest) = url.split_once("/repos/")?;
    let mut parts = rest.split('/');
    let (owner, name, pulls, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (pulls == "pulls" && parts.next().is_none()).then_some(())?;
    Some(PrKey {
        repo: RepoName::new(owner, name),
        number: number.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pull_urls() {
        assert_eq!(
            parse_pull_url("https://api.github.com/repos/Org/Repo/pulls/12"),
            Some(PrKey {
                repo: RepoName::new("org", "repo"),
                number: 12
            })
        );
        assert_eq!(
            parse_pull_url("https://api.github.com/repos/o/r/issues/12"),
            None
        );
        assert_eq!(
            parse_pull_url("https://api.github.com/repos/o/r/pulls/12/files"),
            None
        );
    }
}
