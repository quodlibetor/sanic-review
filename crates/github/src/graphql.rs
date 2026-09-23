//! GraphQL queries: the viewer, PR search, and full PR snapshots.

use color_eyre::eyre::{WrapErr, eyre};
use sanic_core::{
    pr::{CONVERSATION_THREAD, Comment, PrKey, PrSnapshot, Review, ReviewState, TeamRef, Thread},
    repo::RepoName,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

use crate::client::{ApiError, Client, next_link};

/// Connections fetch the newest items (`last:`), since new comments are what
/// triggers care about.
const PR_QUERY: &str = r"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number title url isDraft headRefOid baseRefOid
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
        nodes { id author { login } state body submittedAt }
      }
      comments(last: 100) {
        pageInfo { hasPreviousPage }
        nodes { id author { login } body createdAt }
      }
      reviewThreads(last: 100) {
        pageInfo { hasPreviousPage }
        nodes {
          id path line isResolved
          comments(last: 100) {
            pageInfo { hasPreviousPage }
            nodes { id author { login } body createdAt }
          }
        }
      }
    }
  }
}";

const SEARCH_QUERY: &str = r"
query($q: String!, $after: String) {
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    pageInfo { hasNextPage endCursor }
    nodes { ... on PullRequest { number repository { nameWithOwner } } }
  }
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

    async fn graphql<V: Serialize, T: DeserializeOwned>(
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

    /// The authenticated user's login.
    pub async fn viewer_login(&self) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct Data {
            viewer: Login,
        }
        let data: Data = self
            .graphql("query { viewer { login } }", json!({}), "viewer")
            .await?;
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

    /// A full snapshot of the PR from `me`'s point of view, or `None` if it
    /// doesn't exist or isn't visible. Changed files are fetched only when
    /// `with_files` is set.
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

        let what = format!("PR {key}");
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
        let files = if with_files {
            Some(self.pull_files(key).await?)
        } else {
            None
        };
        Ok(Some(raw.into_snapshot(key.clone(), me, files)))
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

    /// Changed paths, including the old path of renamed files.
    async fn pull_files(&self, key: &PrKey) -> Result<Vec<String>, ApiError> {
        #[derive(Deserialize)]
        struct File {
            filename: String,
            previous_filename: Option<String>,
        }
        let what = format!("files of {key}");
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
            tracing::warn!(pr = %key, "{what} truncated to the newest {}", self.nodes.len());
        }
        self.nodes
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    title: String,
    url: String,
    is_draft: bool,
    head_ref_oid: String,
    base_ref_oid: String,
    author: Option<Login>,
    review_requests: Connection<RawReviewRequest>,
    reviews: Connection<RawReview>,
    comments: Connection<RawComment>,
    review_threads: Connection<RawThread>,
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
    author: Option<Login>,
    state: String,
    body: String,
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComment {
    id: String,
    author: Option<Login>,
    body: String,
    created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    id: String,
    path: Option<String>,
    line: Option<u32>,
    is_resolved: bool,
    comments: Connection<RawComment>,
}

impl From<RawComment> for Comment {
    fn from(raw: RawComment) -> Self {
        Self {
            id: raw.id,
            author: login(raw.author),
            body: raw.body,
            created_at: raw.created_at,
        }
    }
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
                .is_some_and(|l| l.eq_ignore_ascii_case(me))
        });
        let reviews = self
            .reviews
            .into_nodes("reviews", &key)
            .into_iter()
            .map(|r| Review {
                state: review_state(&r.state),
                id: r.id,
                author: login(r.author),
                body: r.body,
                submitted_at: r.submitted_at.unwrap_or_default(),
            })
            .collect();
        let mut threads: Vec<Thread> = vec![Thread {
            id: CONVERSATION_THREAD.into(),
            path: None,
            line: None,
            resolved: false,
            comments: self
                .comments
                .into_nodes("conversation comments", &key)
                .into_iter()
                .map(Comment::from)
                .collect(),
        }];
        for t in self.review_threads.into_nodes("review threads", &key) {
            threads.push(Thread {
                comments: t
                    .comments
                    .into_nodes("thread comments", &key)
                    .into_iter()
                    .map(Comment::from)
                    .collect(),
                id: t.id,
                path: t.path,
                line: t.line,
                resolved: t.is_resolved,
            });
        }
        PrSnapshot {
            title: self.title,
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
            key,
        }
    }
}
