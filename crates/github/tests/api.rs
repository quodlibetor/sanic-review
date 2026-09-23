//! The client against a mock GitHub, using hand-written fixtures in the shape
//! of real API responses.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use sanic_core::{
    pr::{CONVERSATION_THREAD, PrKey, ReviewState, TeamRef},
    repo::RepoName,
};
use sanic_github::{ApiError, Client, NotificationPoll, Token};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header, method, path, query_param},
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

#[tokio::test]
async fn pull_request_snapshot_includes_threads_reviews_and_files() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({
            "variables": { "owner": "org", "name": "repo", "number": 7 }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("pr.json")))
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

    let snap = client(&server)
        .pull_request(&key("org/repo", 7), "me", true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snap.author, "alice");
    assert_eq!(snap.body, "Retries flaky fetches.\n\nCloses #3.");
    assert_eq!(snap.head_sha, "aaa111");
    assert!(
        snap.review_requested,
        "direct request for `Me` matches `me`"
    );
    assert_eq!(
        snap.requested_teams,
        [TeamRef::new("lacework-dev", "storage-platform")]
    );
    assert_eq!(snap.reviews[0].state, ReviewState::ChangesRequested);
    assert_eq!(snap.reviews[1].author, "ghost");
    assert_eq!(snap.threads[0].id, CONVERSATION_THREAD);
    assert_eq!(snap.threads[1].path.as_deref(), Some("src/retry.rs"));
    assert_eq!(snap.threads[1].comments[1].author, "alice");
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
