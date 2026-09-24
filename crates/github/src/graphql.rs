//! GraphQL queries: the viewer, PR search, and full PR snapshots.

use color_eyre::eyre::{WrapErr, eyre};
use sanic_core::{
    pr::{
        CONVERSATION_THREAD, Comment, InProgressComment, InProgressReview, Placement, PrKey,
        PrSnapshot, Reaction, Review, ReviewState, TeamRef, Thread, is_login,
    },
    repo::RepoName,
    run::Side,
    state::reactions_wanted,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

use crate::{
    client::{ApiError, Client, next_link},
    review,
};

/// Connections fetch the newest items (`last:`), since new comments are what
/// triggers care about.
const PR_QUERY: &str = r"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number title body url isDraft state headRefOid baseRefOid updatedAt
      reviewDecision mergeStateStatus
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      author { login }
      reviewRequests(first: 100) {
        nodes {
          requestedReviewer {
            __typename
            ... on User { login }
            ... on Team { slug organization { login } }
          }
        }
      }
      reviews(last: 100) {
        pageInfo { hasPreviousPage }
        nodes { id author { __typename login } state body submittedAt commit { oid } }
      }
      # Only your own pending review is visible to you.
      pending: reviews(states: [PENDING], first: 1) {
        nodes {
          id author { login }
          comments(last: 100) {
            pageInfo { hasPreviousPage }
            nodes { id path line startLine originalLine originalStartLine outdated body }
          }
        }
      }
      comments(last: 100) {
        pageInfo { hasPreviousPage }
        nodes { ...comment reactions(last: 20) { nodes { createdAt user { login } } } }
      }
      reviewThreads(last: 100) {
        pageInfo { hasPreviousPage }
        nodes {
          id path line startLine diffSide isResolved isOutdated originalLine originalStartLine
          comments(last: 100) {
            pageInfo { hasPreviousPage }
            nodes { ...comment ... on PullRequestReviewComment { originalCommit { oid } } }
          }
        }
      }
    }
  }
}

# Your reaction answers a comment. `reactionGroups` says whether for free;
# the reactions themselves say when, but on every thread comment they'd
# double the query's rate-limit cost, so only the conversation gets them.
# The few thread comments whose reactions matter get theirs afterwards,
# with `REACTIONS_QUERY`.
# Neither kind of comment is `UniformResourceLocatable`, so each is asked
# for its `url` by name.
fragment comment on Comment {
  id author { __typename login } body createdAt
  ... on IssueComment { url }
  ... on PullRequestReviewComment { url }
  ... on Reactable { reactionGroups { viewerHasReacted } }
}";

const VIEWER_QUERY: &str = "query { viewer { login } }";

/// Every GraphQL document sent, for the schema check in the tests. A new
/// query goes here too.
#[cfg(test)]
pub(crate) const QUERIES: &[(&str, &str)] = &[
    ("COUNT_QUERY", COUNT_QUERY),
    ("PR_QUERY", PR_QUERY),
    ("REACTIONS_QUERY", REACTIONS_QUERY),
    ("SEARCH_QUERY", SEARCH_QUERY),
    ("VIEWER_QUERY", VIEWER_QUERY),
    ("REPLY_MUTATION", review::REPLY_MUTATION),
    ("SUBMIT_REVIEW_MUTATION", review::SUBMIT_REVIEW_MUTATION),
    ("DELETE_REVIEW_MUTATION", review::DELETE_REVIEW_MUTATION),
    ("REVIEW_STATE_QUERY", review::REVIEW_STATE_QUERY),
    ("MY_REVIEWS_QUERY", review::MY_REVIEWS_QUERY),
    ("REACTION_MUTATION", review::REACTION_MUTATION),
    ("THUMBS_UP_QUERY", review::THUMBS_UP_QUERY),
    ("REVIEW_COMMENTS_QUERY", review::REVIEW_COMMENTS_QUERY),
];

const SEARCH_QUERY: &str = r"
query($q: String!, $after: String) {
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest { number repository { nameWithOwner } } }
  }
}";

/// Comments' reactions, by node id: the PR author's reaction to your
/// comment answers it.
const REACTIONS_QUERY: &str = r"
query($ids: [ID!]!) {
  nodes(ids: $ids) {
    id
    ... on Reactable { reactions(last: 20) { nodes { createdAt user { login } } } }
  }
}";

/// The most ids `nodes(ids:)` takes at once.
const MAX_NODES: usize = 100;

/// Only how many match: the search's first page carries the count.
const COUNT_QUERY: &str = r"
query($q: String!) {
  search(query: $q, type: ISSUE, first: 1) { issueCount }
}";

#[derive(Serialize)]
struct Request<'a, V> {
    query: &'a str,
    variables: V,
}

#[derive(Deserialize)]
struct Response<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlError {
    message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

impl Client {
    async fn graphql_raw<V: Serialize, T: DeserializeOwned>(
        &self,
        query: &str,
        variables: V,
        what: &str,
    ) -> Result<Response<T>, ApiError> {
        let req = self
            .post(&self.url("/graphql"))
            .json(&Request { query, variables });
        self.graphql_sent(req, what).await
    }

    async fn graphql_sent<T: DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<Response<T>, ApiError> {
        let resp: Response<T> = self
            .send(req, what)
            .await?
            .json()
            .await
            .wrap_err_with(|| format!("decoding {what}"))?;
        if resp
            .errors
            .iter()
            .any(|e| e.kind.as_deref() == Some("RATE_LIMITED"))
        {
            return Err(ApiError::RateLimited {
                retry_after: std::time::Duration::from_mins(1),
            });
        }
        Ok(resp)
    }

    pub(crate) async fn graphql<V: Serialize, T: DeserializeOwned>(
        &self,
        query: &str,
        variables: V,
        what: &str,
    ) -> Result<T, ApiError> {
        let resp = self.graphql_raw(query, variables, what).await?;
        if !resp.errors.is_empty() {
            return Err(eyre!("GraphQL errors for {what}: {}", messages(&resp.errors)).into());
        }
        resp.data
            .ok_or_else(|| eyre!("GraphQL returned no data for {what}").into())
    }

    /// Sends `mutation`, a write, once, giving up after
    /// [`review::POST_TIMEOUT`]. Any GraphQL error is a failure.
    pub(crate) async fn mutate<V: Serialize, T: DeserializeOwned>(
        &self,
        mutation: &str,
        variables: V,
        what: &str,
    ) -> Result<T, ApiError> {
        self.graphql_within(mutation, variables, what, review::POST_TIMEOUT)
            .await
    }

    /// [`Client::graphql`], giving up after `timeout`.
    pub(crate) async fn graphql_within<V: Serialize, T: DeserializeOwned>(
        &self,
        query: &str,
        variables: V,
        what: &str,
        timeout: std::time::Duration,
    ) -> Result<T, ApiError> {
        let req = self
            .post(&self.url("/graphql"))
            .json(&Request { query, variables })
            .timeout(timeout);
        let resp: Response<T> = self.graphql_sent(req, what).await?;
        if !resp.errors.is_empty() {
            return Err(eyre!("GraphQL errors for {what}: {}", messages(&resp.errors)).into());
        }
        resp.data
            .ok_or_else(|| eyre!("GraphQL returned no data for {what}").into())
    }

    /// `query`'s data, or `None` when GitHub says what it names doesn't
    /// exist.
    pub(crate) async fn graphql_or_missing<V: Serialize, T: DeserializeOwned>(
        &self,
        query: &str,
        variables: V,
        what: &str,
    ) -> Result<Option<T>, ApiError> {
        let resp: Response<T> = self.graphql_raw(query, variables, what).await?;
        let missing = resp
            .errors
            .iter()
            .all(|e| e.kind.as_deref() == Some("NOT_FOUND"));
        if !resp.errors.is_empty() && !missing {
            return Err(eyre!("GraphQL errors for {what}: {}", messages(&resp.errors)).into());
        }
        Ok(if resp.errors.is_empty() {
            resp.data
        } else {
            None
        })
    }

    /// The authenticated user's login.
    pub async fn viewer_login(&self) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            viewer: Login,
        }
        let data: Data = self.graphql(VIEWER_QUERY, json!({}), "viewer").await?;
        Ok(data.viewer.login)
    }

    /// Open PRs matching a GitHub search, e.g. `review-requested:@me`. The
    /// `is:open is:pr` qualifiers are added here.
    pub async fn search_prs(&self, qualifiers: &str) -> Result<Vec<PrKey>, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            search: Search,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Search {
            page_info: PageInfo,
            nodes: Vec<SearchNode>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: bool,
            end_cursor: Option<String>,
        }
        // Non-PR results come back as empty objects.
        #[derive(Deserialize)]
        struct SearchNode {
            number: Option<u32>,
            repository: Option<SearchRepo>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SearchRepo {
            name_with_owner: String,
        }

        let q = format!("is:open is:pr {qualifiers}");
        let mut keys = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let data: Data = self
                .graphql(
                    SEARCH_QUERY,
                    json!({ "q": q, "after": after }),
                    &format!("search `{q}`"),
                )
                .await?;
            for node in data.search.nodes {
                if let (Some(number), Some(repo)) = (node.number, node.repository) {
                    keys.push(PrKey {
                        repo: RepoName::parse(&repo.name_with_owner)?,
                        number,
                    });
                }
            }
            match data.search.page_info {
                PageInfo {
                    has_next_page: true,
                    end_cursor: Some(cursor),
                } => after = Some(cursor),
                _ => break,
            }
        }
        Ok(keys)
    }

    /// How many open PRs match a GitHub search, fetching none of them. The
    /// `is:open is:pr` qualifiers are added here, as for
    /// [`Client::search_prs`].
    pub async fn count_prs(&self, qualifiers: &str) -> Result<u32, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            search: Count,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Count {
            issue_count: u32,
        }
        let q = format!("is:open is:pr {qualifiers}");
        let data: Data = self
            .graphql(COUNT_QUERY, json!({ "q": q }), &format!("count `{q}`"))
            .await?;
        Ok(data.search.issue_count)
    }

    /// A full snapshot of the PR from `me`'s point of view, or `None` if it
    /// doesn't exist, isn't visible or isn't open. Closed and merged PRs
    /// keep their pending review requests, and notifications about them stay
    /// unread, so they'd otherwise look like fresh requests. Changed files
    /// are fetched only when `with_files` is set.
    pub async fn pull_request(
        &self,
        key: &PrKey,
        me: &str,
        with_files: bool,
    ) -> Result<Option<PrSnapshot>, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            repository: Option<Repository>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Repository {
            pull_request: Option<RawPr>,
        }

        let what = format!("PR {}", key.url());
        let resp: Response<Data> = self
            .graphql_raw(
                PR_QUERY,
                json!({ "owner": key.repo.owner, "name": key.repo.name, "number": key.number }),
                &what,
            )
            .await?;
        let not_found = resp
            .errors
            .iter()
            .all(|e| e.kind.as_deref() == Some("NOT_FOUND"));
        if !resp.errors.is_empty() && !not_found {
            return Err(eyre!("GraphQL errors for {what}: {}", messages(&resp.errors)).into());
        }
        let Some(raw) = resp
            .data
            .and_then(|d| d.repository)
            .and_then(|r| r.pull_request)
        else {
            return Ok(None);
        };
        if raw.state.as_deref().is_some_and(|state| state != "OPEN") {
            return Ok(None);
        }
        let files = if with_files {
            Some(self.pull_files(key).await?)
        } else {
            None
        };
        let mut snapshot = raw.into_snapshot(key.clone(), me, files);
        self.add_reactions(&mut snapshot, me).await?;
        Ok(Some(snapshot))
    }

    /// Fetches the reactions to your review-thread comments that wait on
    /// an answer, which only a reaction could give: see
    /// [`reactions_wanted`]. The PR query leaves them out, since on every
    /// comment they'd double its cost; the conversation's come with it.
    async fn add_reactions(&self, snapshot: &mut PrSnapshot, me: &str) -> Result<(), ApiError> {
        #[derive(Deserialize)]
        struct Data {
            nodes: Vec<Option<Node>>,
        }
        #[derive(Deserialize)]
        struct Node {
            id: String,
            #[serde(default)]
            reactions: Option<Connection<RawReaction>>,
        }
        let author = (!snapshot.is_authored_by(me)).then(|| snapshot.author.clone());
        // The conversation's came with the PR.
        let conversation: Vec<&str> = snapshot
            .threads
            .iter()
            .filter(|t| t.id == CONVERSATION_THREAD)
            .flat_map(|t| &t.comments)
            .map(|c| c.id.as_str())
            .collect();
        let wanted: Vec<String> = reactions_wanted(&snapshot.threads, me, author.as_deref())
            .into_iter()
            .filter(|id| !conversation.contains(id))
            .map(str::to_owned)
            .collect();
        let what = format!("reactions on {}", snapshot.key.url());
        for ids in wanted.chunks(MAX_NODES) {
            let resp: Response<Data> = self
                .graphql_raw(REACTIONS_QUERY, json!({ "ids": ids }), &what)
                .await?;
            // A comment deleted since the PR query comes back `null`, with
            // a `NOT_FOUND` error; the rest still count.
            if resp
                .errors
                .iter()
                .any(|e| e.kind.as_deref() != Some("NOT_FOUND"))
            {
                return Err(eyre!("GraphQL errors for {what}: {}", messages(&resp.errors)).into());
            }
            let nodes = resp.data.map(|d| d.nodes).unwrap_or_default();
            for node in nodes.into_iter().flatten() {
                let Some(comment) = snapshot
                    .threads
                    .iter_mut()
                    .flat_map(|t| &mut t.comments)
                    .find(|c| c.id == node.id)
                else {
                    continue;
                };
                let raw = node.reactions.map(|r| r.nodes).unwrap_or_default();
                comment.reactions = latest_reactions(raw, &comment.created_at);
            }
        }
        Ok(())
    }

    /// Teams the authenticated user belongs to. Needs the `read:org` scope.
    pub async fn my_teams(&self) -> Result<Vec<TeamRef>, ApiError> {
        #[derive(Deserialize)]
        struct Team {
            slug: String,
            organization: Login,
        }
        let mut url = format!("{}?per_page=100", self.url("/user/teams"));
        let mut teams = Vec::new();
        loop {
            let resp = self.send(self.get(&url), "your teams").await?;
            let next = next_link(resp.headers());
            let page: Vec<Team> = resp.json().await.wrap_err("decoding your teams")?;
            teams.extend(
                page.iter()
                    .map(|t| TeamRef::new(&t.organization.login, &t.slug)),
            );
            match next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(teams)
    }

    /// Logins of the orgs the authenticated user belongs to. Needs
    /// `read:org`.
    pub async fn my_orgs(&self) -> Result<Vec<String>, ApiError> {
        let mut url = format!("{}?per_page=100", self.url("/user/orgs"));
        let mut orgs = Vec::new();
        loop {
            let resp = self.send(self.get(&url), "your orgs").await?;
            let next = next_link(resp.headers());
            let page: Vec<Login> = resp.json().await.wrap_err("decoding your orgs")?;
            orgs.extend(page.into_iter().map(|o| o.login.to_ascii_lowercase()));
            match next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(orgs)
    }

    /// Changed paths, including the old path of renamed files.
    async fn pull_files(&self, key: &PrKey) -> Result<Vec<String>, ApiError> {
        #[derive(Deserialize)]
        struct File {
            filename: String,
            previous_filename: Option<String>,
        }
        let what = format!("files of {}", key.url());
        let mut url = format!(
            "{}?per_page=100",
            self.url(&format!(
                "/repos/{}/{}/pulls/{}/files",
                key.repo.owner, key.repo.name, key.number
            ))
        );
        let mut paths = Vec::new();
        loop {
            let resp = self.send(self.get(&url), &what).await?;
            let next = next_link(resp.headers());
            let page: Vec<File> = resp
                .json()
                .await
                .wrap_err_with(|| format!("decoding {what}"))?;
            for file in page {
                paths.push(file.filename);
                paths.extend(file.previous_filename);
            }
            match next {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(paths)
    }
}

fn messages(errors: &[GqlError]) -> String {
    errors
        .iter()
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

/// Deleted accounts come back as a null author.
fn login(author: Option<Login>) -> String {
    author.map_or_else(|| "ghost".into(), |a| a.login)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    #[serde(default)]
    page_info: Option<BackPage>,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackPage {
    has_previous_page: bool,
}

impl<T> Connection<T> {
    /// The nodes, warning if older ones were cut off.
    fn into_nodes(self, what: &str, key: &PrKey) -> Vec<T> {
        if self.page_info.is_some_and(|p| p.has_previous_page) {
            tracing::warn!(
                url = %key.url(),
                "{what} truncated to the newest {}",
                self.nodes.len()
            );
        }
        self.nodes
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    title: String,
    // Always sent by GitHub; defaulted so hand-written mocks can omit it.
    #[serde(default)]
    body: String,
    url: String,
    is_draft: bool,
    /// `OPEN`, `CLOSED` or `MERGED`; hand-written mocks may omit it.
    #[serde(default)]
    state: Option<String>,
    head_ref_oid: String,
    base_ref_oid: String,
    // Always sent by GitHub; hand-written mocks may omit it.
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    merge_state_status: Option<String>,
    #[serde(default)]
    commits: Option<RawCommits>,
    author: Option<Login>,
    review_requests: Connection<RawReviewRequest>,
    reviews: Connection<RawReview>,
    // Always asked for; hand-written mocks may omit it.
    #[serde(default)]
    pending: Option<Connection<RawPending>>,
    comments: Connection<RawComment>,
    review_threads: Connection<RawThread>,
}

/// A review pending on GitHub, which is only ever your own.
#[derive(Deserialize)]
struct RawPending {
    id: String,
    author: Option<Login>,
    comments: Connection<RawPendingComment>,
}

impl RawPending {
    /// `me`'s, among `pending`.
    fn yours(pending: Option<Connection<Self>>, me: &str, key: &PrKey) -> Option<InProgressReview> {
        pending?
            .nodes
            .into_iter()
            .find(|r| r.author.as_ref().is_some_and(|a| is_login(&a.login, me)))
            .map(|r| r.into_review(key))
    }

    fn into_review(self, key: &PrKey) -> InProgressReview {
        let comments = self
            .comments
            .into_nodes("your pending review's comments", key)
            .into_iter()
            .map(|c| {
                // Off the head, it only has lines where it was left.
                let (line, start_line) = match c.line {
                    Some(line) => (Some(line), c.start_line),
                    None => (c.original_line, c.original_start_line),
                };
                InProgressComment {
                    id: c.id,
                    path: c.path,
                    line,
                    start_line,
                    outdated: c.outdated,
                    body: c.body,
                }
            })
            .collect();
        InProgressReview {
            id: self.id,
            comments,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPendingComment {
    id: String,
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    // Always sent by GitHub; hand-written mocks may omit it.
    #[serde(default)]
    outdated: bool,
    body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReviewRequest {
    requested_reviewer: Option<RawReviewer>,
}

/// A user (`login`) or a team (`slug` and `organization`).
#[derive(Deserialize)]
struct RawReviewer {
    login: Option<String>,
    slug: Option<String>,
    organization: Option<Login>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReview {
    id: String,
    author: Option<RawAuthor>,
    state: String,
    body: String,
    submitted_at: Option<String>,
    // Always asked for; hand-written mocks may omit it.
    #[serde(default)]
    commit: Option<RawCommit>,
}

#[derive(Deserialize)]
struct RawAuthor {
    #[serde(rename = "__typename", default)]
    kind: Option<String>,
    login: String,
}

#[derive(Deserialize)]
struct RawCommit {
    oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    id: String,
    author: Option<RawAuthor>,
    body: String,
    created_at: String,
    // Always asked for; hand-written mocks may omit it.
    #[serde(default)]
    url: Option<String>,
    /// Only asked for on thread comments.
    #[serde(default)]
    original_commit: Option<RawCommit>,
    // Always asked for; hand-written mocks may omit it.
    #[serde(default)]
    reaction_groups: Vec<RawReactionGroup>,
    /// Only asked for on conversation comments.
    #[serde(default)]
    reactions: Option<Connection<RawReaction>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReactionGroup {
    viewer_has_reacted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReaction {
    created_at: Option<String>,
    user: Option<Login>,
}

#[derive(Deserialize)]
struct RawCommits {
    nodes: Vec<RawCommitNode>,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawHeadCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHeadCommit {
    status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    id: String,
    path: Option<String>,
    line: Option<u32>,
    // Always asked for; hand-written mocks may omit them.
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    diff_side: Option<Side>,
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    #[serde(default)]
    original_line: Option<u32>,
    #[serde(default)]
    original_start_line: Option<u32>,
    comments: Connection<RawComment>,
}

/// GitHub Apps are `Bot`s; `[bot]` also catches accounts whose type isn't
/// sent, as in hand-written mocks.
fn is_bot(author: Option<&RawAuthor>) -> bool {
    author.is_some_and(|a| a.kind.as_deref() == Some("Bot") || a.login.ends_with("[bot]"))
}

impl RawComment {
    fn into_comment(self, me: &str) -> Comment {
        let by_bot = is_bot(self.author.as_ref());
        let created_at = self.created_at;
        let reactions = latest_reactions(
            self.reactions.map(|r| r.nodes).unwrap_or_default(),
            &created_at,
        );
        let reacted_at = reactions
            .iter()
            .find(|r| is_login(&r.login, me))
            .map(|r| r.at.clone())
            // The token is yours, so the viewer's reaction is yours; when
            // is unknown, so the comment's time stands in.
            .or_else(|| {
                self.reaction_groups
                    .iter()
                    .any(|g| g.viewer_has_reacted)
                    .then(|| created_at.clone())
            });
        Comment {
            id: self.id,
            author: self.author.map_or_else(|| "ghost".into(), |a| a.login),
            body: self.body,
            created_at,
            url: self.url,
            by_bot,
            reacted_at,
            reactions,
        }
    }
}

/// Each login's latest reaction, once; deleted accounts' are dropped. A
/// reaction without a time takes the comment's, `created_at`.
fn latest_reactions(raw: Vec<RawReaction>, created_at: &str) -> Vec<Reaction> {
    let mut reactions: Vec<Reaction> = Vec::new();
    for r in raw {
        let Some(user) = r.user else { continue };
        let at = r.created_at.unwrap_or_else(|| created_at.to_owned());
        match reactions
            .iter_mut()
            .find(|seen| is_login(&seen.login, &user.login))
        {
            Some(seen) if at > seen.at => seen.at = at,
            Some(_) => {}
            None => reactions.push(Reaction {
                login: user.login,
                at,
            }),
        }
    }
    reactions
}

fn review_state(raw: &str) -> ReviewState {
    match raw {
        "APPROVED" => ReviewState::Approved,
        "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
        "DISMISSED" => ReviewState::Dismissed,
        "PENDING" => ReviewState::Pending,
        _ => ReviewState::Commented,
    }
}

impl RawPr {
    fn into_snapshot(self, key: PrKey, me: &str, files: Option<Vec<String>>) -> PrSnapshot {
        let requested_teams = self
            .review_requests
            .nodes
            .iter()
            .filter_map(|r| {
                let r = r.requested_reviewer.as_ref()?;
                Some(TeamRef::new(
                    &r.organization.as_ref()?.login,
                    r.slug.as_deref()?,
                ))
            })
            .collect();
        let review_requested = self.review_requests.nodes.iter().any(|r| {
            r.requested_reviewer
                .as_ref()
                .and_then(|r| r.login.as_deref())
                .is_some_and(|l| is_login(l, me))
        });
        let reviews = self
            .reviews
            .into_nodes("reviews", &key)
            .into_iter()
            .map(|r| {
                let by_bot = is_bot(r.author.as_ref());
                Review {
                    state: review_state(&r.state),
                    id: r.id,
                    author: r.author.map_or_else(|| "ghost".into(), |a| a.login),
                    body: r.body,
                    submitted_at: r.submitted_at.unwrap_or_default(),
                    commit: r.commit.map(|c| c.oid),
                    by_bot,
                }
            })
            .collect();
        let mut threads: Vec<Thread> = vec![Thread {
            id: CONVERSATION_THREAD.into(),
            path: None,
            line: None,
            resolved: false,
            place: Placement::default(),
            comments: self
                .comments
                .into_nodes("conversation comments", &key)
                .into_iter()
                .map(|c| c.into_comment(me))
                .collect(),
        }];
        for t in self.review_threads.into_nodes("review threads", &key) {
            let comments = t.comments.into_nodes("thread comments", &key);
            // The thread's first comment is where it was left.
            let original_commit = comments
                .first()
                .and_then(|c| c.original_commit.as_ref())
                .map(|c| c.oid.clone());
            threads.push(Thread {
                comments: comments.into_iter().map(|c| c.into_comment(me)).collect(),
                id: t.id,
                path: t.path,
                line: t.line,
                resolved: t.is_resolved,
                place: Placement {
                    start_line: t.start_line,
                    side: t.diff_side,
                    head: Some(self.head_ref_oid.clone()),
                    outdated: t.is_outdated,
                    original_start_line: t.original_start_line,
                    original_line: t.original_line,
                    original_commit,
                },
            });
        }
        PrSnapshot {
            title: self.title,
            body: self.body,
            url: self.url,
            author: login(self.author),
            head_sha: self.head_ref_oid,
            base_sha: self.base_ref_oid,
            is_draft: self.is_draft,
            review_requested,
            requested_teams,
            reviews,
            threads,
            files,
            updated_at: self.updated_at,
            review_decision: self.review_decision,
            merge_state: self.merge_state_status,
            checks: self
                .commits
                .and_then(|c| c.nodes.into_iter().next())
                .and_then(|n| n.commit.status_check_rollup)
                .map(|r| r.state),
            in_progress: RawPending::yours(self.pending, me, &key),
            key,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn truncation_warning_names_the_pr_without_a_span() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let key = PrKey {
            repo: RepoName::parse("o/r").unwrap(),
            number: 7,
        };
        let connection = Connection {
            page_info: Some(BackPage {
                has_previous_page: true,
            }),
            nodes: vec![()],
        };

        tracing::subscriber::with_default(subscriber, || {
            // Disabled at this level, as under `RUST_LOG=warn`.
            let _span = tracing::info_span!("refresh", url = %key.url()).entered();
            connection.into_nodes("reviews", &key);
        });

        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("reviews truncated to the newest 1"), "{logs}");
        assert!(logs.contains("url=https://github.com/o/r/pull/7"), "{logs}");
    }
}
