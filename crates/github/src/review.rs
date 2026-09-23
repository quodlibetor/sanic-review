//! Posting a review: the one GitHub write path. Only `sanic-web` calls it,
//! from the handler behind the dashboard's Confirm button; nothing else may.

use std::time::Duration;

use color_eyre::eyre::WrapErr;
use sanic_core::{pr::PrKey, run::Side};
use serde::{Deserialize, Serialize};

use crate::{ApiError, Client};

/// How long a post may take before it's given up on, so a hung connection
/// doesn't hold the dashboard's submit lock for good.
const POST_TIMEOUT: Duration = Duration::from_mins(1);

/// A review to create, serialized as GitHub's create-review request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewReview {
    /// The revision the comments are anchored to.
    pub commit_id: String,
    pub body: String,
    pub event: ReviewEvent,
    pub comments: Vec<NewComment>,
}

/// The review's verdict. `Approve` is only ever the user's own pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewEvent {
    Comment,
    RequestChanges,
    Approve,
}

impl ReviewEvent {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::RequestChanges => "REQUEST_CHANGES",
            Self::Approve => "APPROVE",
        }
    }
}

/// An inline comment in a [`NewReview`]. A multi-line comment has a
/// `start_line` on the same side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewComment {
    pub path: String,
    pub body: String,
    pub line: u32,
    pub side: Side,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<Side>,
}

/// The review GitHub created.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PostedReview {
    pub id: u64,
    pub html_url: String,
}

impl Client {
    /// Creates and submits `review` on `key`, as the token's user. Sent
    /// once; failures are returned, never retried.
    pub async fn post_review(
        &self,
        key: &PrKey,
        review: &NewReview,
    ) -> Result<PostedReview, ApiError> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/reviews",
            key.repo.owner, key.repo.name, key.number
        ));
        let what = format!("posting a review on {}", key.url());
        let req = self.post(&url).json(review).timeout(POST_TIMEOUT);
        let resp = self.send(req, &what).await?;
        Ok(resp
            .json()
            .await
            .wrap_err_with(|| format!("decoding the reply to {what}"))?)
    }
}
