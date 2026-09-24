//! The client against a mock GitHub, using hand-written fixtures in the shape
//! of real API responses.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use sanic_core::run::Side;
use sanic_core::{
    pr::{
        CONVERSATION_THREAD, InProgressComment, InProgressReview, Placement, PrKey, Reaction,
        ReviewState, TeamRef,
    },
    repo::RepoName,
};
use sanic_github::{
    ApiError, Client, NewComment, NewReaction, NewReply, NewReview, NotificationPoll,
    PostedComment, ReviewEvent, ReviewStatus, Token,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{
        body_json, body_partial_json, body_string_contains, header, method, path, query_param,
    },
};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn client(server: &MockServer) -> Client {
    Client::new(&server.uri(), Token::new("t0ken".into())).unwrap()
}

fn key(repo: &str, number: u32) -> PrKey {
    PrKey {
        repo: RepoName::parse(repo).unwrap(),
        number,
    }
}

#[tokio::test]
async fn notifications_follow_pages_and_keep_pr_subjects() {
    let server = MockServer::start().await;
    let page2 = format!("{}/notifications?page=2", server.uri());
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("notifications_page2.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .and(header("authorization", "Bearer t0ken"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fixture("notifications_page1.json"))
                .insert_header("link", format!(r#"<{page2}>; rel="next""#).as_str())
                .insert_header("last-modified", "Sun, 20 Sep 2026 10:00:00 GMT")
                .insert_header("x-poll-interval", "90"),
        )
        .mount(&server)
        .await;

    let NotificationPoll::Changed {
        notifications,
        last_modified,
        poll_interval,
    } = client(&server).notifications(None).await.unwrap()
    else {
        panic!("expected new notifications");
    };
    assert_eq!(
        last_modified.as_deref(),
        Some("Sun, 20 Sep 2026 10:00:00 GMT")
    );
    assert_eq!(poll_interval, Some(Duration::from_secs(90)));
    let prs: Vec<_> = notifications.iter().map(|n| n.pr.clone()).collect();
    assert_eq!(
        prs,
        [Some(key("org/repo", 7)), None, Some(key("org/other", 3))]
    );
}

#[tokio::test]
async fn unchanged_notifications_are_not_modified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .and(header("if-modified-since", "then"))
        .respond_with(ResponseTemplate::new(304).insert_header("x-poll-interval", "60"))
        .mount(&server)
        .await;
    let poll = client(&server).notifications(Some("then")).await.unwrap();
    assert!(matches!(
        poll,
        NotificationPoll::NotModified { poll_interval: Some(d) } if d == Duration::from_secs(60)
    ));
}

#[tokio::test]
async fn rate_limits_and_bad_tokens_are_distinguished() {
    let server = MockServer::start().await;
    Mock::given(path("/notifications"))
        .respond_with(ResponseTemplate::new(403).insert_header("retry-after", "12"))
        .mount(&server)
        .await;
    Mock::given(path("/graphql"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let c = client(&server);
    assert!(matches!(
        c.notifications(None).await,
        Err(ApiError::RateLimited { retry_after }) if retry_after == Duration::from_secs(12)
    ));
    assert!(matches!(
        c.viewer_login().await,
        Err(ApiError::Unauthorized)
    ));
}

#[tokio::test]
async fn graphql_rate_limit_error_is_recognized() {
    let server = MockServer::start().await;
    Mock::given(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }]
        })))
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server).viewer_login().await,
        Err(ApiError::RateLimited { .. })
    ));
}

#[tokio::test]
async fn count_reads_only_how_many_match() {
    let server = MockServer::start().await;
    Mock::given(path("/graphql"))
        .and(body_partial_json(json!({
            "variables": { "q": "is:open is:pr author:@me updated:<2026-09-09" }
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "data": { "search": { "issueCount": 9 } } })),
        )
        .mount(&server)
        .await;
    let count = client(&server)
        .count_prs("author:@me updated:<2026-09-09")
        .await
        .unwrap();
    assert_eq!(count, 9);
}

#[tokio::test]
async fn search_pages_through_results_and_skips_non_prs() {
    let server = MockServer::start().await;
    Mock::given(path("/graphql"))
        .and(body_partial_json(json!({ "variables": { "after": "c1" } })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": { "search": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [{ "number": 2, "repository": { "nameWithOwner": "org/b" } }]
            }}})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/graphql"))
        .and(body_partial_json(json!({
            "variables": { "q": "is:open is:pr review-requested:@me" }
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": { "search": {
                "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                "nodes": [{ "number": 1, "repository": { "nameWithOwner": "Org/A" } }, {}]
            }}})),
        )
        .mount(&server)
        .await;
    let keys = client(&server)
        .search_prs("review-requested:@me")
        .await
        .unwrap();
    assert_eq!(keys, [key("org/a", 1), key("org/b", 2)]);
}

/// A server answering for `pr.json`'s PR, its reactions follow-up and its
/// files.
async fn pr_server() -> MockServer {
    pr_server_answering(fixture("pr.json")).await
}

/// [`pr_server`], answering the PR query with `pr`.
async fn pr_server_answering(pr: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({
            "variables": { "owner": "org", "name": "repo", "number": 7 }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(&server)
        .await;
    // Only your comment still waiting on Alice wants its reactions: in the
    // other thread she replied, and the conversation's came with the PR. A
    // comment deleted since comes back `null`, and doesn't fail the rest.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            json!({ "variables": { "ids": ["RC_3"] } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "nodes": [{
                "id": "RC_3",
                "reactions": { "nodes": [
                    { "createdAt": "2026-09-20T10:05:00Z", "user": { "login": "alice" } },
                    { "createdAt": "2026-09-20T10:06:00Z", "user": { "login": "Alice" } },
                    { "createdAt": "2026-09-20T10:07:00Z", "user": null }
                ] }
            }, null] },
            "errors": [{
                "type": "NOT_FOUND",
                "message": "Could not resolve to a node with the global id of 'RC_gone'"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/org/repo/pulls/7/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "filename": "src/retry.rs" },
            { "filename": "src/new.rs", "previous_filename": "src/old.rs" }
        ])))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn pull_request_snapshot_says_who_reacted_and_when() {
    let server = pr_server().await;
    let snap = client(&server)
        .pull_request(&key("org/repo", 7), "me", true)
        .await
        .unwrap()
        .unwrap();
    let conversation = &snap.threads[0].comments;
    // Your reaction, not someone else's, by its own time.
    assert_eq!(
        conversation[0].reacted_at.as_deref(),
        Some("2026-09-20T07:10:00Z")
    );
    assert!(!conversation[0].by_bot);
    // Without the reaction's time, the comment's stands in.
    assert_eq!(
        conversation[1].reacted_at.as_deref(),
        Some("2026-09-20T07:30:00Z")
    );
    assert!(conversation[1].by_bot);
    // Others' reactions are kept, each login's latest once.
    assert_eq!(
        conversation[0].reactions,
        [
            Reaction {
                login: "dave".into(),
                at: "2026-09-20T07:05:00Z".into(),
            },
            Reaction {
                login: "ME".into(),
                at: "2026-09-20T07:10:00Z".into(),
            },
        ]
    );
    // A review-thread comment of yours awaiting the author gets them
    // afterwards, each login's latest once.
    assert_eq!(
        snap.threads[2].comments[0].reactions,
        [Reaction {
            login: "alice".into(),
            at: "2026-09-20T10:06:00Z".into(),
        }]
    );
    let thread = &snap.threads[1].comments;
    assert!(thread[0].reactions.is_empty());
    assert_eq!(thread[0].reacted_at, None);
    // Without the reactions themselves, whether you reacted still counts.
    assert_eq!(
        thread[1].reacted_at.as_deref(),
        Some("2026-09-20T09:30:00Z")
    );
}

#[tokio::test]
async fn pull_request_snapshot_includes_threads_reviews_and_files() {
    let server = pr_server().await;
    let snap = client(&server)
        .pull_request(&key("org/repo", 7), "me", true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snap.author, "alice");
    assert_eq!(snap.body, "Retries flaky fetches.\n\nCloses #3.");
    assert_eq!(snap.head_sha, "aaa111");
    assert_eq!(snap.updated_at.as_deref(), Some("2026-09-20T08:00:00Z"));
    assert_eq!(snap.review_decision.as_deref(), Some("CHANGES_REQUESTED"));
    assert_eq!(snap.merge_state.as_deref(), Some("BLOCKED"));
    assert_eq!(snap.checks.as_deref(), Some("PENDING"));
    assert_eq!(snap.in_progress, None);
    assert!(
        snap.review_requested,
        "direct request for `Me` matches `me`"
    );
    assert_eq!(
        snap.requested_teams,
        [TeamRef::new("lacework-dev", "storage-platform")]
    );
    assert_eq!(snap.reviews[0].state, ReviewState::ChangesRequested);
    assert_eq!(snap.reviews[0].commit.as_deref(), Some("aaa111"));
    assert!(!snap.reviews[0].by_bot);
    // A GitHub App's review, by author type.
    assert!(snap.reviews[1].by_bot);
    assert_eq!(snap.reviews[2].author, "ghost");
    assert_eq!(snap.reviews[2].commit, None);
    assert_eq!(snap.threads[0].id, CONVERSATION_THREAD);
    assert_eq!(snap.threads[1].path.as_deref(), Some("src/retry.rs"));
    assert_eq!(snap.threads[1].comments[1].author, "alice");
    // Where it sits: its range on the head as fetched, and where it was
    // left, from its first comment.
    assert_eq!(
        snap.threads[1].place,
        Placement {
            start_line: Some(40),
            side: Some(Side::Right),
            head: Some("aaa111".into()),
            outdated: false,
            original_start_line: Some(36),
            original_line: Some(38),
            original_commit: Some("fff000".into()),
        }
    );
    assert_eq!(
        snap.threads[1].comments[0].url.as_deref(),
        Some("https://github.com/org/repo/pull/7#discussion_r1")
    );
    assert_eq!(
        snap.files.as_deref(),
        Some(
            &[
                "src/retry.rs".into(),
                "src/new.rs".into(),
                "src/old.rs".into()
            ][..]
        )
    );
}

#[tokio::test]
async fn pull_request_snapshot_has_your_pending_review_with_its_comments() {
    let mut pr = fixture("pr.json");
    // Only your own is ever visible; `pr.json` has none.
    assert_eq!(pr["data"]["repository"]["pullRequest"].get("pending"), None);
    pr["data"]["repository"]["pullRequest"]["pending"] = json!({ "nodes": [{
        "id": "PRR_9",
        "author": { "login": "Me" },
        "comments": {
            "pageInfo": { "hasPreviousPage": false },
            "nodes": [
                { "id": "PRRC_1", "path": "src/retry.rs", "line": 42, "startLine": 40,
                  "originalLine": 38, "body": "Can this loop forever?" },
                { "id": "PRRC_2", "path": "src/old.rs", "line": null, "startLine": null,
                  "originalLine": 7, "originalStartLine": 5, "outdated": true,
                  "body": "Outdated now." }
            ]
        }
    }] });
    let server = pr_server_answering(pr).await;
    let snap = client(&server)
        .pull_request(&key("org/repo", 7), "me", false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snap.in_progress,
        Some(InProgressReview {
            id: "PRR_9".into(),
            comments: vec![
                InProgressComment {
                    id: "PRRC_1".into(),
                    path: "src/retry.rs".into(),
                    line: Some(42),
                    start_line: Some(40),
                    outdated: false,
                    body: "Can this loop forever?".into(),
                },
                // Off the head now: its line where it was left.
                InProgressComment {
                    id: "PRRC_2".into(),
                    path: "src/old.rs".into(),
                    line: Some(7),
                    start_line: Some(5),
                    outdated: true,
                    body: "Outdated now.".into(),
                },
            ],
        })
    );
}

#[tokio::test]
async fn my_teams_follow_pages() {
    let server = MockServer::start().await;
    let page2 = format!("{}/user/teams?page=2", server.uri());
    Mock::given(path("/user/teams"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "slug": "b", "organization": { "login": "Org" } }
        ])))
        .mount(&server)
        .await;
    Mock::given(path("/user/teams"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{ "slug": "a", "organization": { "login": "org" } }]))
                .insert_header("link", format!(r#"<{page2}>; rel="next""#).as_str()),
        )
        .mount(&server)
        .await;
    assert_eq!(
        client(&server).my_teams().await.unwrap(),
        [TeamRef::new("org", "a"), TeamRef::new("org", "b")]
    );
}

#[tokio::test]
async fn my_orgs_are_lowercased_logins() {
    let server = MockServer::start().await;
    Mock::given(path("/user/orgs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "login": "Lacework" }, { "login": "lacework-dev" }
        ])))
        .mount(&server)
        .await;
    assert_eq!(
        client(&server).my_orgs().await.unwrap(),
        ["lacework", "lacework-dev"]
    );
}

#[tokio::test]
async fn closed_pull_requests_are_none() {
    let server = MockServer::start().await;
    let mut pr = fixture("pr.json");
    pr["data"]["repository"]["pullRequest"]["state"] = json!("MERGED");
    Mock::given(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(&server)
        .await;
    let snap = client(&server)
        .pull_request(&key("org/repo", 7), "me", false)
        .await
        .unwrap();
    assert_eq!(snap, None);
}

#[tokio::test]
async fn missing_pull_request_is_none() {
    let server = MockServer::start().await;
    Mock::given(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "repository": null },
            "errors": [{ "type": "NOT_FOUND", "message": "Could not resolve to a Repository" }]
        })))
        .mount(&server)
        .await;
    let snap = client(&server)
        .pull_request(&key("org/gone", 1), "me", false)
        .await
        .unwrap();
    assert_eq!(snap, None);
}

fn new_review() -> NewReview {
    NewReview {
        commit_id: "h1".into(),
        body: "Looks fine.".into(),
        event: ReviewEvent::RequestChanges,
        comments: vec![
            NewComment {
                path: "src/lib.rs".into(),
                body: "off by one?".into(),
                line: 4,
                side: Side::Right,
                start_line: None,
                start_side: None,
            },
            NewComment {
                path: "src/old.rs".into(),
                body: "why remove this?".into(),
                line: 9,
                side: Side::Left,
                start_line: Some(7),
                start_side: Some(Side::Left),
            },
        ],
    }
}

fn replies() -> Vec<NewReply> {
    ["PRRT_1", "PRRT_2"]
        .into_iter()
        .map(|thread| NewReply {
            thread_id: thread.into(),
            body: format!("agreed, in {thread}"),
        })
        .collect()
}

/// GitHub creating `new_review()` pending, as review `PRR_80`.
async fn pending_review(server: &MockServer) {
    let mut body = serde_json::to_value(new_review()).unwrap();
    body.as_object_mut().unwrap().remove("event");
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .and(body_json(body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 80,
            "node_id": "PRR_80",
            "html_url": "https://github.com/org/repo/pull/7#pullrequestreview-80",
            "state": "PENDING",
        })))
        .expect(1)
        .mount(server)
        .await;
}

/// A GraphQL mutation named `name`, with `variables`, answered `data`.
fn mutation(name: &str, variables: &Value) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(name))
        .and(body_partial_json(json!({ "variables": variables })))
}

fn ok(data: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
}

#[tokio::test]
async fn a_review_is_created_pending_without_its_verdict() {
    let server = MockServer::start().await;
    pending_review(&server).await;
    let pending = client(&server)
        .create_pending_review(&key("org/repo", 7), &new_review())
        .await
        .unwrap();
    assert_eq!(pending.node_id, "PRR_80");
    assert_eq!(
        pending.html_url,
        "https://github.com/org/repo/pull/7#pullrequestreview-80"
    );
}

#[tokio::test]
async fn a_rejected_review_says_why_and_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .and(header("authorization", "Bearer t0ken"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Unprocessable Entity",
            "errors": ["Line could not be resolved"],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server)
        .create_pending_review(&key("org/repo", 7), &new_review())
        .await
        .unwrap_err();
    let ApiError::Other(report) = err else {
        panic!("expected a plain failure, got {err}");
    };
    let message = format!("{report:?}");
    assert!(message.contains("422"), "{message}");
    assert!(message.contains("Line could not be resolved"), "{message}");
}

#[tokio::test]
async fn replies_then_the_submit_go_to_the_pending_review() {
    let server = MockServer::start().await;
    for reply in replies() {
        mutation(
            "addPullRequestReviewThreadReply",
            &json!({ "review": "PRR_80", "thread": reply.thread_id, "body": reply.body }),
        )
        .respond_with(ok(
            &json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "C" } } }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    }
    mutation(
        "submitPullRequestReview",
        &json!({ "review": "PRR_80", "event": "REQUEST_CHANGES", "body": "Looks fine." }),
    )
    .respond_with(ok(
        &json!({ "submitPullRequestReview": { "pullRequestReview": { "id": "PRR_80" } } }),
    ))
    .expect(1)
    .mount(&server)
    .await;
    mutation("deletePullRequestReview", &json!({ "review": "PRR_80" }))
        .respond_with(ok(
            &json!({ "deletePullRequestReview": { "clientMutationId": null } }),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let github = client(&server);
    let pr = key("org/repo", 7);
    for reply in replies() {
        github.add_reply(&pr, "PRR_80", &reply).await.unwrap();
    }
    github
        .submit_review(&pr, "PRR_80", &new_review())
        .await
        .unwrap();
    github.delete_review(&pr, "PRR_80").await.unwrap();
    // In the order sent, as the preview shows them.
    let steps: Vec<String> = new_review()
        .steps(&pr, &replies())
        .into_iter()
        .map(|s| s.endpoint)
        .collect();
    assert_eq!(
        steps,
        [
            "POST /repos/org/repo/pulls/7/reviews",
            "GraphQL addPullRequestReviewThreadReply, with",
            "GraphQL addPullRequestReviewThreadReply, with",
            "GraphQL submitPullRequestReview, with",
        ]
    );
}

#[tokio::test]
async fn a_graphql_error_fails_the_write() {
    let server = MockServer::start().await;
    mutation("addPullRequestReviewThreadReply", &json!({}))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [{ "message": "Could not resolve to a node with the global id of 'PRRT_2'" }],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server)
        .add_reply(&key("org/repo", 7), "PRR_80", &replies()[1])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("PRRT_2"), "{err}");
}

#[tokio::test]
async fn a_review_is_found_pending_submitted_or_gone() {
    let server = MockServer::start().await;
    for (id, answer) in [
        (
            "PRR_1",
            json!({ "data": { "node": { "state": "PENDING" } } }),
        ),
        (
            "PRR_2",
            json!({ "data": { "node": { "state": "COMMENTED" } } }),
        ),
        (
            "PRR_3",
            json!({
                "data": { "node": null },
                "errors": [{ "type": "NOT_FOUND", "message": "Could not resolve to a node" }],
            }),
        ),
    ] {
        mutation("node(id: $review)", &json!({ "review": id }))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer))
            .mount(&server)
            .await;
    }
    let github = client(&server);
    let state = async |id| github.review_state(&key("org/repo", 7), id).await.unwrap();
    assert_eq!(state("PRR_1").await, ReviewStatus::Pending);
    assert_eq!(state("PRR_2").await, ReviewStatus::Submitted);
    assert_eq!(state("PRR_3").await, ReviewStatus::Gone);
}

#[tokio::test]
async fn a_reaction_is_a_thumbs_up_on_the_comment() {
    let server = MockServer::start().await;
    mutation("THUMBS_UP", &json!({ "subject": "PRRC_9" }))
        .and(body_string_contains("addReaction"))
        .respond_with(ok(
            &json!({ "addReaction": { "reaction": { "content": "THUMBS_UP" } } }),
        ))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .add_reaction(&NewReaction {
            comment_id: "PRRC_9".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn your_thumbs_up_is_found_among_the_comments_reactions() {
    let server = MockServer::start().await;
    for (id, groups) in [
        (
            "PRRC_1",
            json!([{ "content": "THUMBS_UP", "viewerHasReacted": true }]),
        ),
        (
            "PRRC_2",
            json!([
                { "content": "THUMBS_UP", "viewerHasReacted": false },
                { "content": "HEART", "viewerHasReacted": true },
            ]),
        ),
    ] {
        mutation("reactionGroups", &json!({ "subject": id }))
            .respond_with(ok(&json!({ "node": { "reactionGroups": groups } })))
            .mount(&server)
            .await;
    }
    let github = client(&server);
    let has = async |id: &str| {
        github
            .has_thumbs_up(&NewReaction {
                comment_id: id.into(),
            })
            .await
            .unwrap()
    };
    assert!(has("PRRC_1").await);
    assert!(!has("PRRC_2").await);
}

#[tokio::test]
async fn a_posted_reviews_comments_leave_out_its_replies() {
    let server = MockServer::start().await;
    let comment = |id: &str, body: &str, reply_to: Value| json!({ "id": id, "path": "src/lib.rs", "body": body, "replyTo": reply_to });
    mutation(
        "PullRequestReview { comments",
        &json!({ "review": "PRR_5" }),
    )
    .respond_with(ok(&json!({ "node": { "comments": { "nodes": [
            comment("PRRC_1", "Why `m`?", Value::Null),
            comment("PRRC_2", "Agreed.", json!({ "id": "PRRC_0" })),
        ] } } })))
    .expect(1)
    .mount(&server)
    .await;
    let comments = client(&server)
        .review_comments(&key("org/repo", 7), "PRR_5")
        .await
        .unwrap();
    assert_eq!(
        comments,
        [PostedComment {
            id: "PRRC_1".into(),
            path: "src/lib.rs".into(),
            body: "Why `m`?".into(),
        }]
    );
}
