//! Posting a review, its replies in existing threads, and reactions: the
//! GitHub write paths. Only `sanic-web` calls them, from the handler
//! behind the dashboard's Confirm button; nothing else may.

use std::time::Duration;

use color_eyre::eyre::WrapErr;
use sanic_core::{pr::PrKey, run::Side};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ApiError, Client, client::Failed};

/// How long a write, or a read a submit makes after one, may take before
/// it's given up on, so a hung connection doesn't hold the dashboard's
/// submit lock for good.
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

    /// The event [`ReviewEvent::as_str`] names.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Comment, Self::RequestChanges, Self::Approve]
            .into_iter()
            .find(|event| event.as_str() == name)
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

/// Your newest reviews of a PR, to learn whether one sent in a call whose
/// answer was lost was posted after all.
pub(crate) const MY_REVIEWS_QUERY: &str = r"
query($owner: String!, $name: String!, $number: Int!, $author: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviews(last: 20, author: $author) {
        nodes {
          url state body submittedAt commit { oid }
          comments(first: 100) { totalCount nodes { body } }
        }
      }
    }
  }
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

/// A posted review's inline comments, to link each to the draft it was
/// posted from. Replies in existing threads are in it too, with `replyTo`.
pub(crate) const REVIEW_COMMENTS_QUERY: &str = r"
query($review: ID!) {
  node(id: $review) {
    ... on PullRequestReview { comments(first: 100) { nodes { id path body replyTo { id } } } }
  }
}";

/// A [`NewReview`] without its verdict, which leaves it pending.
#[derive(Serialize)]
struct PendingBody<'a> {
    commit_id: &'a str,
    body: &'a str,
    comments: &'a [NewComment],
}

/// A review GitHub created: pending, which only you can see until it's
/// submitted, or submitted with its verdict.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedReview {
    pub id: u64,
    pub node_id: String,
    pub html_url: String,
}

/// Why [`Client::create_review`] failed: whether GitHub may have the
/// review anyway.
#[derive(Debug)]
pub enum PostError {
    /// GitHub answered with a client error, such as approving your own PR:
    /// it didn't post it.
    Refused(ApiError),
    /// No answer, a server error, or an answer that couldn't be read:
    /// GitHub may have posted it.
    Unknown(ApiError),
}

/// An inline comment a posted review started a thread with, as
/// [`Client::review_comments`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedComment {
    /// Its node id, which its thread's first comment has.
    pub id: String,
    pub path: String,
    pub body: String,
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

/// What a review sent in one call sent, to find it with
/// [`Client::find_review`].
#[derive(Debug, Clone, Copy)]
pub struct SentReview<'a> {
    pub commit_id: &'a str,
    pub event: ReviewEvent,
    pub body: &'a str,
    /// Its inline comments' bodies, in any order.
    pub comments: &'a [String],
    /// An RFC 3339 time before it was sent.
    pub after: &'a str,
}

impl NewReview {
    /// The requests posting this review with `replies` sends, in order.
    /// Without replies that's [`Client::create_review`] alone. With them,
    /// it's [`Client::create_pending_review`], [`Client::add_reply`] for
    /// each reply, then [`Client::submit_review`]; only the submit makes
    /// any of it visible to anyone else.
    #[must_use]
    pub fn steps(&self, key: &PrKey, replies: &[NewReply]) -> Vec<Step> {
        let endpoint = format!(
            "POST /repos/{}/{}/pulls/{}/reviews",
            key.repo.owner, key.repo.name, key.number
        );
        if replies.is_empty() {
            return vec![Step::new(endpoint, self)];
        }
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

/// The request [`Client::find_review`] sends for `me`'s reviews of `key`.
#[must_use]
pub fn find_review_step(key: &PrKey, me: &str) -> Step {
    Step::new(
        "GraphQL repository (your newest reviews of the PR), with".into(),
        &find_variables(key, me),
    )
}

fn find_variables(key: &PrKey, me: &str) -> Value {
    json!({
        "owner": key.repo.owner,
        "name": key.repo.name,
        "number": key.number,
        "author": me,
    })
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
    /// Creates and submits `review` on `key`, verdict and all, in one
    /// call, as the token's user. Sent once; a failure is returned, never
    /// retried.
    pub async fn create_review(
        &self,
        key: &PrKey,
        review: &NewReview,
    ) -> Result<CreatedReview, PostError> {
        let what = format!("posting the review on {}", key.url());
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/reviews",
            key.repo.owner, key.repo.name, key.number
        ));
        let req = self.post(&url).json(review).timeout(POST_TIMEOUT);
        let resp = self
            .send_answered(req, &what)
            .await
            .map_err(|failed| match failed {
                Failed { err, refused: true } => PostError::Refused(err),
                Failed { err, .. } => PostError::Unknown(err),
            })?;
        // GitHub took it, but without its page it's as good as unanswered.
        resp.json()
            .await
            .wrap_err_with(|| format!("decoding the reply to {what}"))
            .map_err(|err| PostError::Unknown(err.into()))
    }

    /// Creates `review` on `key` pending, without its verdict, as the
    /// token's user. Sent once; a failure is returned, never retried.
    pub async fn create_pending_review(
        &self,
        key: &PrKey,
        review: &NewReview,
    ) -> Result<CreatedReview, ApiError> {
        let what = format!("creating a pending review on {}", key.url());
        self.create(key, &review.pending_body(), &what).await
    }

    async fn create(
        &self,
        key: &PrKey,
        body: &impl Serialize,
        what: &str,
    ) -> Result<CreatedReview, ApiError> {
        let url = self.url(&format!(
            "/repos/{}/{}/pulls/{}/reviews",
            key.repo.owner, key.repo.name, key.number
        ));
        let req = self.post(&url).json(body).timeout(POST_TIMEOUT);
        let resp = self.send(req, what).await?;
        Ok(resp
            .json()
            .await
            .wrap_err_with(|| format!("decoding the reply to {what}"))?)
    }

    /// The page of `me`'s review of `key` that is `review`, submitted with
    /// its verdict, body and comments on its commit at `after` or later, if GitHub
    /// has one among `me`'s newest reviews of the PR.
    pub async fn find_review(
        &self,
        key: &PrKey,
        me: &str,
        review: &SentReview<'_>,
    ) -> Result<Option<String>, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            repository: Option<Repository>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Repository {
            pull_request: Option<PullRequest>,
        }
        #[derive(Deserialize)]
        struct PullRequest {
            reviews: Reviews,
        }
        #[derive(Deserialize)]
        struct Reviews {
            nodes: Vec<Option<Node>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            url: String,
            state: String,
            body: String,
            submitted_at: Option<String>,
            commit: Option<Commit>,
            comments: Comments,
        }
        #[derive(Deserialize)]
        struct Commit {
            oid: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Comments {
            total_count: usize,
            nodes: Vec<Option<CommentNode>>,
        }
        #[derive(Deserialize)]
        struct CommentNode {
            body: String,
        }
        let what = format!("looking for your review on {}", key.url());
        let data: Data = self
            .graphql(MY_REVIEWS_QUERY, find_variables(key, me), &what)
            .await?;
        let state = match review.event {
            ReviewEvent::Comment => "COMMENTED",
            ReviewEvent::RequestChanges => "CHANGES_REQUESTED",
            ReviewEvent::Approve => "APPROVED",
        };
        let nodes = data
            .repository
            .and_then(|r| r.pull_request)
            .map(|pr| pr.reviews.nodes)
            .unwrap_or_default();
        Ok(nodes.into_iter().flatten().find_map(|node| {
            let same = node.state == state
                && node.body == review.body
                && node.commit.is_some_and(|c| c.oid == review.commit_id)
                && node
                    .submitted_at
                    .is_some_and(|at| at.as_str() >= review.after)
                && node.comments.total_count == review.comments.len()
                && {
                    // Each body it has, taken once from what was sent: a
                    // like review with other comments isn't this one.
                    let mut unmatched: Vec<&str> =
                        review.comments.iter().map(String::as_str).collect();
                    node.comments.nodes.iter().flatten().all(|c| {
                        unmatched
                            .iter()
                            .position(|body| *body == c.body)
                            .map(|at| unmatched.swap_remove(at))
                            .is_some()
                    })
                };
            same.then_some(node.url)
        }))
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

    /// The comments posted review `node_id` started threads with: not its
    /// replies in existing threads. Given up on after [`POST_TIMEOUT`],
    /// since a submit waits on it.
    pub async fn review_comments(
        &self,
        key: &PrKey,
        node_id: &str,
    ) -> Result<Vec<PostedComment>, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            node: Option<Node>,
        }
        #[derive(Deserialize)]
        struct Node {
            comments: Option<Comments>,
        }
        #[derive(Deserialize)]
        struct Comments {
            nodes: Vec<Option<CommentNode>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CommentNode {
            id: String,
            path: String,
            body: String,
            reply_to: Option<Value>,
        }
        let what = format!("listing your review's comments on {}", key.url());
        let data: Data = self
            .graphql_within(
                REVIEW_COMMENTS_QUERY,
                json!({ "review": node_id }),
                &what,
                POST_TIMEOUT,
            )
            .await?;
        let nodes = data
            .node
            .and_then(|n| n.comments)
            .map(|c| c.nodes)
            .unwrap_or_default();
        Ok(nodes
            .into_iter()
            .flatten()
            .filter(|c| c.reply_to.is_none())
            .map(|c| PostedComment {
                id: c.id,
                path: c.path,
                body: c.body,
            })
            .collect())
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
