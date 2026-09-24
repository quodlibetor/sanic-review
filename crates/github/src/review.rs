//! Posting a review, its replies in existing threads, and reactions: the
//! GitHub write paths. Only `sanic-web` calls them, from the handler
//! behind the dashboard's Confirm button; nothing else may.

use std::time::Duration;

use color_eyre::eyre::WrapErr;
use sanic_core::{pr::PrKey, run::Side};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ApiError, Client};

/// How long a write may take before it's given up on, so a hung connection
/// doesn't hold the dashboard's submit lock for good.
pub(crate) const POST_TIMEOUT: Duration = Duration::from_mins(1);

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

/// A reply in an existing review thread, posted as part of a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewReply {
    /// The thread's node id.
    pub thread_id: String,
    pub body: String,
}

/// A thumbs-up on an existing comment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct NewReaction {
    /// The comment's node id.
    pub comment_id: String,
}

/// One request a post sends, as the preview shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What it's sent to: a REST endpoint, or the GraphQL mutation.
    pub endpoint: String,
    /// The REST request's body, or the mutation's variables, as JSON
    /// that's sent field for field in this order.
    pub body: String,
}

impl Step {
    fn new(endpoint: String, body: &impl Serialize) -> Self {
        Self {
            endpoint,
            // Plain data, which always serializes.
            body: serde_json::to_string_pretty(body).unwrap_or_default(),
        }
    }
}

/// Stands in for the pending review's id in the preview: GitHub assigns
/// it when it creates the review.
pub const PENDING_REVIEW: &str = "<the id of the review created above>";

/// Adds a reply to a thread, in a pending review.
pub(crate) const REPLY_MUTATION: &str = r"
mutation($review: ID!, $thread: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {
    pullRequestReviewId: $review, pullRequestReviewThreadId: $thread, body: $body
  }) { comment { id } }
}";

/// Submits a pending review, making it and its replies visible.
pub(crate) const SUBMIT_REVIEW_MUTATION: &str = r"
mutation($review: ID!, $event: PullRequestReviewEvent!, $body: String) {
  submitPullRequestReview(input: {
    pullRequestReviewId: $review, event: $event, body: $body
  }) { pullRequestReview { id } }
}";

/// A review's state, to learn whether one left pending by an earlier
/// attempt was submitted after all.
pub(crate) const REVIEW_STATE_QUERY: &str = r"
query($review: ID!) {
  node(id: $review) { ... on PullRequestReview { state } }
}";

/// Deletes a pending review: one whose reply failed, or one an earlier
/// attempt left.
pub(crate) const DELETE_REVIEW_MUTATION: &str = r"
mutation($review: ID!) {
  deletePullRequestReview(input: { pullRequestReviewId: $review }) { clientMutationId }
}";

/// Whether you've already put a thumbs-up on a comment, so one whose
/// answer was lost isn't sent again.
pub(crate) const THUMBS_UP_QUERY: &str = r"
query($subject: ID!) {
  node(id: $subject) { ... on Reactable { reactionGroups { content viewerHasReacted } } }
}";

/// A thumbs-up.
pub(crate) const REACTION_MUTATION: &str = r"
mutation($subject: ID!) {
  addReaction(input: { subjectId: $subject, content: THUMBS_UP }) { reaction { content } }
}";

/// A [`NewReview`] without its verdict, which leaves it pending.
#[derive(Serialize)]
struct PendingBody<'a> {
    commit_id: &'a str,
    body: &'a str,
    comments: &'a [NewComment],
}

/// A review GitHub created pending, which only you can see until it's
/// submitted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PendingReview {
    pub id: u64,
    pub node_id: String,
    pub html_url: String,
}

/// Where a review is, as [`Client::review_state`] finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatus {
    /// Still pending: nothing in it is visible to anyone else.
    Pending,
    /// Submitted, so everyone can see it.
    Submitted,
    /// Deleted, or never there.
    Gone,
}

impl NewReview {
    /// The requests posting this review with `replies` sends, in order:
    /// [`Client::create_pending_review`], [`Client::add_reply`] for each
    /// reply, then [`Client::submit_review`]. Only the submit makes any of
    /// it visible to anyone else.
    #[must_use]
    pub fn steps(&self, key: &PrKey, replies: &[NewReply]) -> Vec<Step> {
        let endpoint = format!(
            "POST /repos/{}/{}/pulls/{}/reviews",
            key.repo.owner, key.repo.name, key.number
        );
        std::iter::once(Step::new(endpoint, &self.pending_body()))
            .chain(replies.iter().map(|reply| {
                Step::new(
                    "GraphQL addPullRequestReviewThreadReply, with".into(),
                    &reply_variables(PENDING_REVIEW, reply),
                )
            }))
            .chain(std::iter::once(Step::new(
                "GraphQL submitPullRequestReview, with".into(),
                &self.submit_variables(PENDING_REVIEW),
            )))
            .collect()
    }

    fn pending_body(&self) -> PendingBody<'_> {
        PendingBody {
            commit_id: &self.commit_id,
            body: &self.body,
            comments: &self.comments,
        }
    }

    fn submit_variables(&self, review: &str) -> Value {
        json!({ "review": review, "event": self.event.as_str(), "body": self.body })
    }
}

fn reply_variables(review: &str, reply: &NewReply) -> Value {
    json!({ "review": review, "thread": reply.thread_id, "body": reply.body })
}

/// The request [`Client::review_state`] sends for review `node_id`.
#[must_use]
pub fn review_state_step(node_id: &str) -> Step {
    Step::new(
        "GraphQL node (the review's state), with".into(),
        &json!({ "review": node_id }),
    )
}

impl NewReaction {
    /// The requests sending it takes: [`Client::has_thumbs_up`], then,
    /// unless you've reacted already, [`Client::add_reaction`].
    #[must_use]
    pub fn steps(&self) -> [Step; 2] {
        [
            Step::new(
                "GraphQL node (whether you've given it a 👍), with".into(),
                &self.variables(),
            ),
            Step::new(
                "GraphQL addReaction (THUMBS_UP), unless you have, with".into(),
                &self.variables(),
            ),
        ]
    }

    fn variables(&self) -> Value {
        json!({ "subject": self.comment_id })
    }
}

impl Client {
    /// Creates `review` on `key` pending, without its verdict, as the
    /// token's user. Sent once; a failure is returned, never retried.
    pub async fn create_pending_review(
        &self,
        key: &PrKey,
        review: &NewReview,
    ) -> Result<PendingReview, ApiError> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/reviews",
            key.repo.owner, key.repo.name, key.number
        ));
        let what = format!("creating a pending review on {}", key.url());
        let req = self
            .post(&url)
            .json(&review.pending_body())
            .timeout(POST_TIMEOUT);
        let resp = self.send(req, &what).await?;
        Ok(resp
            .json()
            .await
            .wrap_err_with(|| format!("decoding the reply to {what}"))?)
    }

    /// Adds `reply` to its thread in pending review `review`. Sent once.
    pub async fn add_reply(
        &self,
        key: &PrKey,
        review: &str,
        reply: &NewReply,
    ) -> Result<(), ApiError> {
        let what = format!("replying in a thread of {}", key.url());
        self.mutate::<_, Value>(REPLY_MUTATION, reply_variables(review, reply), &what)
            .await
            .map(|_| ())
    }

    /// Submits pending review `node_id` with `review`'s verdict and body,
    /// making it and its replies visible. Sent once.
    pub async fn submit_review(
        &self,
        key: &PrKey,
        node_id: &str,
        review: &NewReview,
    ) -> Result<(), ApiError> {
        let what = format!("submitting the review on {}", key.url());
        self.mutate::<_, Value>(
            SUBMIT_REVIEW_MUTATION,
            review.submit_variables(node_id),
            &what,
        )
        .await
        .map(|_| ())
    }

    /// Deletes pending review `node_id`. Sent once.
    pub async fn delete_review(&self, key: &PrKey, node_id: &str) -> Result<(), ApiError> {
        let what = format!("deleting the pending review on {}", key.url());
        self.mutate::<_, Value>(DELETE_REVIEW_MUTATION, json!({ "review": node_id }), &what)
            .await
            .map(|_| ())
    }

    /// Whether review `node_id` is pending, submitted or gone.
    pub async fn review_state(&self, key: &PrKey, node_id: &str) -> Result<ReviewStatus, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            node: Option<Node>,
        }
        #[derive(Deserialize)]
        struct Node {
            state: Option<String>,
        }
        let what = format!("checking a review on {}", key.url());
        let data: Option<Data> = self
            .graphql_or_missing(REVIEW_STATE_QUERY, json!({ "review": node_id }), &what)
            .await?;
        Ok(
            match data.and_then(|d| d.node).and_then(|n| n.state).as_deref() {
                None => ReviewStatus::Gone,
                Some("PENDING") => ReviewStatus::Pending,
                Some(_) => ReviewStatus::Submitted,
            },
        )
    }

    /// Whether `reaction`'s comment already has your thumbs-up.
    pub async fn has_thumbs_up(&self, reaction: &NewReaction) -> Result<bool, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            node: Option<Node>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            #[serde(default)]
            reaction_groups: Vec<Group>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Group {
            content: String,
            viewer_has_reacted: bool,
        }
        let data: Data = self
            .graphql(
                THUMBS_UP_QUERY,
                reaction.variables(),
                "checking your reactions",
            )
            .await?;
        Ok(data.node.is_some_and(|n| {
            n.reaction_groups
                .iter()
                .any(|g| g.content == "THUMBS_UP" && g.viewer_has_reacted)
        }))
    }

    /// Puts a thumbs-up on `reaction`'s comment, as the token's user.
    /// Sent once; a failure is returned, never retried.
    pub async fn add_reaction(&self, reaction: &NewReaction) -> Result<(), ApiError> {
        self.mutate::<_, Value>(
            REACTION_MUTATION,
            reaction.variables(),
            "reacting to a comment",
        )
        .await
        .map(|_| ())
    }
}
