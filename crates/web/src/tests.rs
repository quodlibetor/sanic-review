//! Handler tests: an in-memory store, a fake `serve`, and wiremock in place
//! of GitHub, so nothing leaves the machine.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use sanic_core::{
    clock::Clock,
    config::{CheckoutResolver, Config, Vcs},
    pr::{PrKey, PrSnapshot},
    repo::RepoName,
    run::{
        Confidence, DraftComment, InlineComment, ReviewRequest, ReviewResult, ReviewTrigger,
        Severity, Side, Verdict,
    },
    skip::SkipRules,
};
use sanic_github::{Client, Token};
use sanic_runner::review::{AgentProfile, RunSettings};
use sanic_store::Store;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::watch;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{any, body_json, method, path},
};

use crate::{Context, Control, Dashboard};

const HOST: &str = "127.0.0.1:7117";

#[derive(Default)]
struct FakeServe {
    started: Mutex<Vec<PrKey>>,
    /// `skip_titles` patterns added, with the profile each went to.
    skipped: Mutex<Vec<(String, Option<String>)>>,
}

impl Control for FakeServe {
    fn review_now(&self, key: PrKey) {
        self.started.lock().unwrap().push(key);
    }

    fn add_skip_title(&self, pattern: &str, profile: Option<&str>) -> color_eyre::Result<bool> {
        let entry = (pattern.to_owned(), profile.map(Into::into));
        let mut skipped = self.skipped.lock().unwrap();
        if skipped.contains(&entry) {
            return Ok(false);
        }
        skipped.push(entry);
        Ok(true)
    }

    fn run_settings(&self, profile: &str) -> color_eyre::Result<RunSettings> {
        let config = config();
        let profile = config
            .profiles
            .iter()
            .find(|p| p.name == profile)
            .ok_or_else(|| color_eyre::eyre::eyre!("no profile `{profile}`"))?;
        Ok(RunSettings::new(
            AgentProfile::from(profile),
            &config.runner,
            &config.github.git_url,
            vec![],
        ))
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_181_231)
    }
}

fn key(number: u32) -> PrKey {
    PrKey {
        repo: RepoName::new("org", "repo"),
        number,
    }
}

fn snapshot(number: u32, author: &str, title: &str) -> PrSnapshot {
    PrSnapshot {
        key: key(number),
        title: title.into(),
        body: format!("{title}, in detail."),
        url: key(number).url(),
        author: author.into(),
        head_sha: format!("head{number}"),
        base_sha: "base".into(),
        is_draft: false,
        review_requested: author != "me",
        requested_teams: vec![],
        reviews: vec![],
        threads: vec![],
        files: None,
        updated_at: None,
        review_decision: None,
        merge_state: None,
        checks: None,
    }
}

fn comment(path: &str, line: u32, side: Side, body: &str, unanchored: bool) -> DraftComment {
    DraftComment {
        comment: InlineComment {
            path: path.into(),
            line,
            start_line: None,
            side,
            body: body.into(),
            severity: Severity::Major,
            confidence: Confidence::High,
        },
        unanchored,
    }
}

const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@
 fn main() {
-    let n = 1;
+    let n = 2;
+    let m = n + 1;
     println!(\"{n}\");
 }
";

struct Fixture {
    dashboard: Dashboard,
    serve: Arc<FakeServe>,
    github: MockServer,
    /// When the scheduler would queue each debounced review.
    due: watch::Sender<HashMap<PrKey, Instant>>,
    /// The run of PR 7 with drafts: summary, an inline comment and an
    /// unanchored one, in that order.
    run: i64,
    drafts: [i64; 3],
    data: TempDir,
}

/// A store with: PR 7 you owe, reviewed; PR 8 you owe, whose review failed;
/// PR 10 you owe, a draft; and PR 9, yours.
async fn fixture(manual_reviews: bool) -> Fixture {
    let mut store = Store::open_in_memory().unwrap();
    for snap in [
        snapshot(7, "alice", "Add the thing"),
        snapshot(8, "bob", "Fix <script> escaping"),
        PrSnapshot {
            is_draft: true,
            ..snapshot(10, "carol", "WIP: try things")
        },
        snapshot(9, "me", "My change"),
    ] {
        store.record(&snap, "default", &[]).unwrap();
    }
    let request = |number| ReviewRequest {
        key: key(number),
        profile: "default".into(),
        head_sha: format!("head{number}"),
        base_sha: "base".into(),
        trigger: ReviewTrigger::Requested,
    };
    let run = store.queue_review(&request(7)).unwrap().unwrap().id;
    store.claim_run(run).unwrap();
    store
        .finish_review(
            run,
            &ReviewResult {
                summary: "Mostly fine.".into(),
                verdict: Verdict::RequestChanges,
                comments: vec![
                    comment("src/lib.rs", 3, Side::Right, "Why `m`?", false),
                    comment(
                        "src/gone.rs",
                        40,
                        Side::Right,
                        "This file isn't in the diff.",
                        true,
                    ),
                ],
                session_id: Some("sess-7".into()),
                transcript_path: "t".into(),
            },
        )
        .unwrap();
    let failed = store.queue_review(&request(8)).unwrap().unwrap().id;
    store.claim_run(failed).unwrap();
    store
        .fail_run(failed, "claude exited with 1\nand more detail")
        .unwrap();
    let ids: Vec<i64> = store
        .draft_rows(run)
        .unwrap()
        .iter()
        .map(|d| d.id)
        .collect();

    let data = TempDir::new().unwrap();
    let run_dir = data.path().join("runs").join(run.to_string());
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("pr.diff"), DIFF).unwrap();

    let github = MockServer::start().await;
    let serve = Arc::new(FakeServe::default());
    let (due, due_rx) = watch::channel(HashMap::new());
    let dashboard = Dashboard::new(Context {
        me: "me".into(),
        manual_reviews,
        data_dir: data.path().to_owned(),
        config_path: "/config/sanic review.toml".into(),
        store,
        github: Client::new(&github.uri(), Token::new("t0ken".into())).unwrap(),
        control: Arc::clone(&serve) as Arc<dyn Control>,
        due: due_rx,
        skips: watch::channel(skip_rules()).1,
        window: watch::channel(None).1,
        clock: Arc::new(FixedClock),
    })
    .unwrap();
    Fixture {
        dashboard,
        serve,
        github,
        due,
        run,
        drafts: [ids[0], ids[1], ids[2]],
        data,
    }
}

/// Unused: the test config has only `github =` entries.
struct NoCheckouts;

impl CheckoutResolver for NoCheckouts {
    fn resolve(
        &self,
        path: &std::path::Path,
        _: Option<&str>,
    ) -> color_eyre::Result<(Vcs, RepoName)> {
        Err(color_eyre::eyre::eyre!("no checkout at {}", path.display()))
    }
}

/// Skip rules from a config with two profiles and the default draft skip.
fn config() -> Config {
    let text = r#"
[runner]
claude = "/opt/my claude"

[profile.default]
repos = [{ github = "org" }]
skills = ["/skills/review"]

[profile.vuln]
repos = [{ github = "sec" }]
"#;
    Config::parse(text, std::path::Path::new("/"), &NoCheckouts).unwrap()
}

fn skip_rules() -> SkipRules {
    config().skip_rules()
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Fixture {
    fn router(&self) -> Router {
        self.dashboard.router()
    }

    fn token(&self) -> String {
        self.dashboard.app.csrf.token().to_owned()
    }

    async fn send(&self, req: Request<Body>) -> Reply {
        let resp = self.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(header::HOST, HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// A same-origin form post, with the CSRF token as a form field.
    async fn post(&self, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let token = self.token();
        let mut fields = fields.to_vec();
        fields.push(("csrf", &token));
        self.send(form(uri).body(encode(&fields)).unwrap()).await
    }

    /// Draft `i`'s current status, from the store.
    fn status(&self, i: usize) -> String {
        let store = self.dashboard.app.store();
        store.draft_row(self.drafts[i]).unwrap().unwrap().status
    }

    fn preview_uri(&self, event: &str) -> String {
        format!("/pr/org/repo/7/runs/{}/preview?event={event}", self.run)
    }

    fn submit_uri(&self) -> String {
        format!("/pr/org/repo/7/runs/{}/submit", self.run)
    }

    /// The payload the preview's confirm form would send back.
    async fn previewed_payload(&self, event: &str) -> String {
        let page = self.get(&self.preview_uri(event)).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        hidden_value(&page.body, "payload")
    }
}

/// A same-origin form post, without a token.
fn form(uri: &str) -> axum::http::request::Builder {
    Request::post(uri)
        .header(header::HOST, HOST)
        .header(header::ORIGIN, format!("http://{HOST}"))
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
}

fn encode(fields: &[(&str, &str)]) -> Body {
    Body::from(serde_urlencoded::to_string(fields).unwrap())
}

/// The value of the hidden input named `name`, unescaped as a browser
/// would.
fn hidden_value(html: &str, name: &str) -> String {
    let marker = format!(r#"name="{name}" value=""#);
    let start = html.find(&marker).unwrap() + marker.len();
    let end = start + html[start..].find('"').unwrap();
    html[start..end]
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// `html` with a line per tag and the random token and data dir masked, for
/// snapshots.
fn readable(fixture: &Fixture, html: &str) -> String {
    html.replace(&fixture.token(), "<token>")
        // Not `<data>`: the tag split below would put a newline in the
        // command it ends.
        .replace(&fixture.data.path().display().to_string(), "$DATA")
        // The chat commands run the binary serve is: here, the test's.
        .replace(
            &sanic_runner::chat::quote(&std::env::current_exe().unwrap().display().to_string()),
            "$EXE",
        )
        .replace("><", ">\n<")
}

#[tokio::test]
async fn index_lists_owed_reviews_and_your_prs_like_the_tui() {
    let f = fixture(false).await;
    let page = f.get("/").await;
    assert_eq!(page.status, StatusCode::OK);
    insta::assert_snapshot!(readable(&f, &page.body));
}

#[tokio::test]
async fn index_counts_down_waiting_reviews_and_marks_held_ones() {
    let f = fixture(true).await;
    // PR 7's latest run goes back to queued, which --manual-reviews holds.
    {
        let mut store = f.dashboard.app.store();
        let run = store.queue_review(&ReviewRequest {
            key: key(7),
            profile: "default".into(),
            head_sha: "newer".into(),
            base_sha: "base".into(),
            trigger: ReviewTrigger::Requested,
        });
        assert!(run.unwrap().is_some());
    }
    f.due.send_replace(HashMap::from([(
        key(8),
        Instant::now() + Duration::from_millis(3_600_500),
    )]));
    let page = f.get("/").await;
    assert!(page.body.contains("manual reviews"), "{}", page.body);
    assert!(
        page.body
            .contains(r#"<span class="status held">held</span>"#)
    );
    assert!(page.body.contains("waiting 1:00:00"), "{}", page.body);
}

#[tokio::test]
async fn a_pr_page_shows_drafts_in_their_diff_and_marks_the_pr_seen() {
    let f = fixture(false).await;
    assert!(f.get("/").await.body.contains(r#"class="unseen""#));
    let page = f.get("/pr/org/repo/7").await;
    assert_eq!(page.status, StatusCode::OK);
    insta::assert_snapshot!(readable(&f, &page.body));
    assert!(!f.get("/").await.body.contains(r#"class="unseen""#));

    assert_eq!(f.get("/pr/org/repo/99").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn drafts_are_edited_and_decided_with_htmx_or_plain_forms() {
    let f = fixture(false).await;
    let token = f.token();
    let edit = format!("/drafts/{}/edit", f.drafts[1]);

    // htmx: the header carries the token, and the card comes back.
    let fields = [("body", "Why `m`?\r\nIt's unused.")];
    let reply = f
        .send(
            form(&edit)
                .header("hx-request", "true")
                .header("x-csrf-token", &token)
                .body(encode(&fields))
                .unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.starts_with("<article"), "{}", reply.body);
    assert!(
        reply.body.contains("Why `m`?\nIt&#39;s unused.")
            || reply.body.contains("Why `m`?\nIt's unused.")
    );
    assert!(reply.body.contains("edited"));

    // A plain form: a redirect back to the draft.
    let status = format!("/drafts/{}/status", f.drafts[1]);
    let reply = f.post(&status, &[("status", "accepted")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.headers[header::LOCATION],
        format!("/pr/org/repo/7?run={}#draft-{}", f.run, f.drafts[1]).as_str()
    );
    assert_eq!(f.status(1), "accepted");
    f.post(&status, &[("status", "rejected")]).await;
    assert_eq!(f.status(1), "rejected");
    assert_eq!(
        f.post(&status, &[("status", "posted")]).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.post("/drafts/999/status", &[("status", "accepted")])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn state_changes_need_the_token_and_the_dashboards_own_origin() {
    let f = fixture(false).await;
    let status = format!("/drafts/{}/status", f.drafts[1]);
    let token = f.token();
    let refused = |reply: Reply| {
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    };

    // No token, a wrong one, and a right one sent from elsewhere.
    let fields = [("status", "accepted")];
    refused(f.send(form(&status).body(encode(&fields)).unwrap()).await);
    let wrong = [("status", "accepted"), ("csrf", "0000")];
    refused(f.send(form(&status).body(encode(&wrong)).unwrap()).await);
    let good = [("status", "accepted"), ("csrf", token.as_str())];
    let cross_origin = Request::post(&status)
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "https://evil.example")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    refused(f.send(cross_origin.body(encode(&good)).unwrap()).await);
    let cross_site = Request::post(&status)
        .header(header::HOST, HOST)
        .header("sec-fetch-site", "cross-site")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    refused(f.send(cross_site.body(encode(&good)).unwrap()).await);
    // A same-site page on another localhost port is still another origin.
    let other_port = Request::post(&status)
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "http://127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    refused(f.send(other_port.body(encode(&good)).unwrap()).await);
    // DNS rebinding: the page reads the dashboard under another name.
    let rebound = Request::get("/")
        .header(header::HOST, "evil.example:7117")
        .body(Body::empty())
        .unwrap();
    refused(f.send(rebound).await);
    for uri in [
        f.submit_uri(),
        "/pr/org/repo/8/review-now".into(),
        "/pr/org/repo/7/archive".into(),
    ] {
        refused(f.send(form(&uri).body(Body::empty()).unwrap()).await);
    }
    assert_eq!(f.status(1), "pending");
    assert!(f.serve.started.lock().unwrap().is_empty());

    // Without Origin or Sec-Fetch-Site, the token alone is enough.
    let bare = Request::post(&status)
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let reply = f.send(bare.body(encode(&good)).unwrap()).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(f.status(1), "accepted");
}

#[tokio::test]
async fn every_page_forbids_framing() {
    let f = fixture(false).await;
    for uri in ["/", "/pr/org/repo/7", &f.preview_uri("COMMENT")] {
        let reply = f.get(uri).await;
        assert_eq!(reply.headers[header::X_FRAME_OPTIONS], "DENY");
        let csp = reply.headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    }
}

/// Accepts the summary and both comments, as you would before submitting.
async fn accept_all(f: &Fixture) {
    for id in f.drafts {
        let reply = f
            .post(&format!("/drafts/{id}/status"), &[("status", "accepted")])
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
    }
}

#[tokio::test]
async fn nothing_is_posted_until_you_confirm_the_previewed_payload() {
    let f = fixture(false).await;
    // Any request to GitHub before the confirm fails the test.
    let guard = Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .named("GitHub before confirming")
        .mount_as_scoped(&f.github)
        .await;
    accept_all(&f).await;
    f.post(
        &format!("/drafts/{}/edit", f.drafts[0]),
        &[("body", "Mostly fine; see inline.")],
    )
    .await;
    f.get("/pr/org/repo/7").await;
    let preview = f.get(&f.preview_uri("REQUEST_CHANGES")).await;
    assert_eq!(preview.status, StatusCode::OK);
    insta::assert_snapshot!(readable(&f, &preview.body));
    drop(guard);

    let expected = json!({
        "commit_id": "head7",
        "body": "Mostly fine; see inline.\n\n\
            **[src/gone.rs:40](https://github.com/org/repo/blob/head7/src/gone.rs?plain=1#L40)**\n\n\
            This file isn't in the diff.",
        "event": "REQUEST_CHANGES",
        "comments": [
            { "path": "src/lib.rs", "body": "Why `m`?", "line": 3, "side": "RIGHT" },
        ],
    });
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .and(body_json(&expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 5,
            "html_url": "https://github.com/org/repo/pull/7#pullrequestreview-5",
        })))
        .expect(1)
        .mount(&f.github)
        .await;
    let payload = hidden_value(&preview.body, "payload");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
        expected
    );
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "REQUEST_CHANGES"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert!(posted.body.contains("pullrequestreview-5"));
    for i in 0..3 {
        assert_eq!(f.status(i), "posted");
    }

    // Confirming again finds nothing left to post, and sends nothing.
    let again = f
        .post(
            &f.submit_uri(),
            &[("event", "REQUEST_CHANGES"), ("payload", &payload)],
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_payload_that_changed_since_the_preview_is_not_posted() {
    let f = fixture(false).await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.github)
        .await;
    accept_all(&f).await;
    let payload = f.previewed_payload("COMMENT").await;
    f.post(
        &format!("/drafts/{}/edit", f.drafts[1]),
        &[("body", "Changed after the preview.")],
    )
    .await;
    let reply = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.body.contains("nothing was sent"));
    // A different verdict is a different payload too.
    let payload = f.previewed_payload("COMMENT").await;
    let reply = f
        .post(
            &f.submit_uri(),
            &[
                ("event", "APPROVE"),
                ("payload", &payload),
                ("approve", "yes"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(f.status(1), "accepted");
}

#[tokio::test]
async fn githubs_refusal_is_shown_and_not_retried() {
    let f = fixture(false).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Unprocessable Entity",
            "errors": ["Line could not be resolved"],
        })))
        .expect(1)
        .mount(&f.github)
        .await;
    accept_all(&f).await;
    let payload = f.previewed_payload("COMMENT").await;
    let reply = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert!(
        reply.body.contains("Line could not be resolved"),
        "{}",
        reply.body
    );
    assert!(reply.body.contains("won't be retried"), "{}", reply.body);
    for i in 0..3 {
        assert_eq!(f.status(i), "accepted");
    }
}

#[tokio::test]
async fn nothing_accepted_means_nothing_to_submit_except_an_approval() {
    let f = fixture(false).await;
    let reply = f.get(&f.preview_uri("COMMENT")).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.body.contains("nothing is accepted"));

    // You may approve with no body at all; the preview still asks first,
    // and confirming twice approves once.
    let payload = f.previewed_payload("APPROVE").await;
    let expected = json!({ "commit_id": "head7", "body": "", "event": "APPROVE", "comments": [] });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
        expected
    );
    Mock::given(method("POST"))
        .and(body_json(&expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 6,
            "html_url": "https://github.com/org/repo/pull/7#pullrequestreview-6",
        })))
        .expect(1)
        .mount(&f.github)
        .await;
    // The verdict alone doesn't approve: the page's own box must be ticked.
    let preview = f.get(&f.preview_uri("APPROVE")).await.body;
    assert!(
        preview.contains(
            r#"<input type="checkbox" name="approve" value="yes" required autocomplete="off">"#
        ),
        "{preview}"
    );
    let unticked = [("event", "APPROVE"), ("payload", payload.as_str())];
    assert_eq!(
        f.post(&f.submit_uri(), &unticked).await.status,
        StatusCode::CONFLICT
    );
    let confirm = [
        ("event", "APPROVE"),
        ("payload", payload.as_str()),
        ("approve", "yes"),
    ];
    assert_eq!(
        f.post(&f.submit_uri(), &confirm).await.status,
        StatusCode::OK
    );
    assert_eq!(
        f.post(&f.submit_uri(), &confirm).await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn review_now_asks_first_and_then_asks_serve() {
    let f = fixture(false).await;
    // PR 8's review failed, PR 10 is a skipped draft; PR 7 is drafted.
    let ask = f.get("/pr/org/repo/8/review-now").await;
    assert!(ask.body.contains("Rerun the review of"), "{}", ask.body);
    assert!(
        f.get("/pr/org/repo/10/review-now")
            .await
            .body
            .contains("draft-skipped")
    );
    assert!(
        f.get("/pr/org/repo/7/review-now")
            .await
            .body
            .contains("nothing to start")
    );
    assert!(f.serve.started.lock().unwrap().is_empty());

    // Posting to it anyway, as a stale confirm tab would, starts nothing.
    let reply = f.post("/pr/org/repo/7/review-now", &[]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(f.serve.started.lock().unwrap().is_empty());

    let reply = f.post("/pr/org/repo/8/review-now", &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers[header::LOCATION], "/pr/org/repo/8");
    assert_eq!(*f.serve.started.lock().unwrap(), [key(8)]);
}

#[tokio::test]
async fn archiving_writes_the_store_and_the_index_hides_archived_prs() {
    let f = fixture(false).await;
    let reply = f
        .post(
            "/pr/org/repo/8/archive",
            &[("archived", "true"), ("next", "index")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers[header::LOCATION], "/");
    let index = f.get("/").await.body;
    assert!(!index.contains("Fix &lt;script&gt; escaping"));
    assert!(
        index.contains("Reviews you owe (2 · 1 archived)"),
        "{index}"
    );
    let all = f.get("/?archived=true").await.body;
    assert!(all.contains("Fix &lt;script&gt; escaping"));
    assert!(all.contains(r#"<span class="status dim">archived</span>"#));

    let reply = f
        .post(
            "/pr/org/repo/8/archive",
            &[("archived", "false"), ("next", "pr")],
        )
        .await;
    assert_eq!(reply.headers[header::LOCATION], "/pr/org/repo/8");
    assert!(
        f.get("/")
            .await
            .body
            .contains("Fix &lt;script&gt; escaping")
    );
}

#[tokio::test]
async fn assets_are_embedded() {
    let f = fixture(false).await;
    for (file, kind) in [
        ("htmx.min.js", "text/javascript"),
        ("app.js", "text/javascript"),
        ("style.css", "text/css"),
    ] {
        let reply = f.get(&format!("/assets/{file}")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers[header::CONTENT_TYPE], kind);
        assert!(!reply.body.is_empty());
    }
    assert_eq!(f.get("/assets/nope.js").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pages_another_site_opens_are_only_a_link_to_themselves() {
    let f = fixture(false).await;
    accept_all(&f).await;
    let opened = |uri: &str, site: &'static str| {
        Request::get(uri)
            .header(header::HOST, HOST)
            .header("sec-fetch-site", site)
            .body(Body::empty())
            .unwrap()
    };
    // A page on another site opens the confirm page, hoping you're typing
    // a `y`.
    let preview = f.preview_uri("APPROVE");
    for site in ["cross-site", "same-site"] {
        let reply = f.send(opened(&preview, site)).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.body.contains(r#"id="confirm""#), "{}", reply.body);
        assert!(!reply.body.contains("app.js"));
        assert!(
            reply
                .body
                .contains(&format!(r#"href="{}""#, preview.replace('&', "&amp;")))
        );
    }
    // Nor does it mark a PR seen.
    f.send(opened("/pr/org/repo/7", "cross-site")).await;
    assert!(f.get("/").await.body.contains(r#"class="unseen""#));
    // Typed or bookmarked, or followed from the dashboard, it's the page.
    for site in ["none", "same-origin"] {
        let reply = f.send(opened(&preview, site)).await;
        assert!(reply.body.contains(r#"id="confirm""#), "{}", reply.body);
    }
    let asset = f.send(opened("/assets/app.js", "cross-site")).await;
    assert!(asset.body.contains("keydown"));
}

#[tokio::test]
async fn review_now_refuses_prs_that_arent_tracked() {
    let f = fixture(false).await;
    let reply = f.post("/pr/org/repo/99/review-now", &[]).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(f.serve.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_preview_points_out_what_markdown_hides() {
    let f = fixture(false).await;
    accept_all(&f).await;
    f.post(
        &format!("/drafts/{}/edit", f.drafts[1]),
        &[(
            "body",
            "cc @org/security <b>now</b> <!-- ignore --> ![x](https://e.example/t.png) \u{202e}",
        )],
    )
    .await;
    let page = f.get(&f.preview_uri("COMMENT")).await.body;
    for what in [
        "@-mentions",
        "HTML comments",
        "images",
        "HTML tags",
        "invisible or text-direction characters",
    ] {
        assert!(page.contains(what), "{what}: {page}");
    }
    // An email address isn't a mention, and a comparison isn't a tag.
    f.post(
        &format!("/drafts/{}/edit", f.drafts[1]),
        &[("body", "mail a@b.example if n < 3")],
    )
    .await;
    let page = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(!page.contains("easy to miss"), "{page}");
}

#[tokio::test]
async fn without_sec_fetch_site_a_foreign_referer_counts_as_elsewhere() {
    let f = fixture(false).await;
    accept_all(&f).await;
    let preview = f.preview_uri("COMMENT");
    let referred = |referer: &'static str| {
        Request::get(&preview)
            .header(header::HOST, HOST)
            .header(header::REFERER, referer)
            .body(Body::empty())
            .unwrap()
    };
    let reply = f.send(referred("https://evil.example/")).await;
    assert!(!reply.body.contains(r#"id="confirm""#), "{}", reply.body);
    // The dashboard's own pages send their own address.
    let reply = f
        .send(referred("http://127.0.0.1:7117/pr/org/repo/7"))
        .await;
    assert!(reply.body.contains(r#"id="confirm""#), "{}", reply.body);
    // Whatever case `Host` came in.
    let upper = Request::get(&preview)
        .header(header::HOST, "LOCALHOST:7117")
        .header(header::REFERER, "http://localhost:7117/pr/org/repo/7")
        .body(Body::empty())
        .unwrap();
    assert!(f.send(upper).await.body.contains(r#"id="confirm""#));
    // A lookalike host isn't this one, nor is another port.
    for referer in [
        "http://127.0.0.1:7117.evil.example/",
        "http://127.0.0.1:8080/",
    ] {
        let reply = f.send(referred(referer)).await;
        assert!(!reply.body.contains(r#"id="confirm""#), "{referer}");
    }
}

#[tokio::test]
async fn paths_that_cant_name_a_repo_are_not_found() {
    let f = fixture(false).await;
    for uri in [
        "/pr/org/re%0d%0apo/7",
        "/pr/o%20rg/repo/7",
        "/pr/%2e%2e/repo/7",
    ] {
        assert_eq!(f.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
        let reply = f.post(&format!("{uri}/review-now"), &[]).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(f.serve.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn i_edits_a_title_glob_with_a_live_preview() {
    let f = fixture(false).await;
    let page = f.get("/pr/org/repo/8/ignore").await;
    assert_eq!(page.status, StatusCode::OK);
    insta::assert_snapshot!(readable(&f, &page.body));
    // Only reviews you owe are skipped by title.
    assert_eq!(
        f.get("/pr/org/repo/9/ignore").await.status,
        StatusCode::NOT_FOUND
    );

    let hits = ignore_preview(&f, "*i*", "").await;
    assert!(hits.contains("Would skip (3)"), "{hits}");
    // In one profile, only its reviews: every one here is `default`'s.
    let hits = ignore_preview(&f, "*i*", "default").await;
    assert!(hits.contains("Would skip (3)"), "{hits}");
    let hits = ignore_preview(&f, "*i*", "vuln").await;
    assert!(hits.contains("Would skip (0)"), "{hits}");
    let hits = ignore_preview(&f, "fix <script>*", "").await;
    assert!(hits.contains("Would skip (1)"), "{hits}");
    assert!(
        hits.contains("https://github.com/org/repo/pull/8"),
        "{hits}"
    );
    assert!(
        ignore_preview(&f, "[", "")
            .await
            .contains("Not a valid pattern")
    );
    assert!(
        ignore_preview(&f, "", "")
            .await
            .contains("Not a valid pattern: empty")
    );
}

#[tokio::test]
async fn saving_a_glob_asks_serve_to_add_it_once() {
    let f = fixture(false).await;
    let save = "/pr/org/repo/8/ignore";
    let reply = f.post(save, &[("pattern", "Fix*"), ("profile", "")]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains("is now in"));
    let again = f.post(save, &[("pattern", "Fix*"), ("profile", "")]).await;
    assert!(again.body.contains("was already in"), "{}", again.body);
    let reply = f
        .post(save, &[("pattern", "Fix*"), ("profile", "vuln")])
        .await;
    assert!(reply.body.contains("[profile.vuln]"), "{}", reply.body);
    assert_eq!(
        *f.serve.skipped.lock().unwrap(),
        [
            ("Fix*".to_owned(), None),
            ("Fix*".to_owned(), Some("vuln".into()))
        ]
    );

    // Bad globs and profiles the config doesn't have are refused.
    for fields in [
        [("pattern", "["), ("profile", "")],
        [("pattern", ""), ("profile", "")],
        [("pattern", "Fix*"), ("profile", "nope")],
    ] {
        assert_eq!(f.post(save, &fields).await.status, StatusCode::CONFLICT);
    }
    // And a save without the token never reaches serve.
    let fields = [("pattern", "other*"), ("profile", "")];
    let reply = f.send(form(save).body(encode(&fields)).unwrap()).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(f.serve.skipped.lock().unwrap().len(), 2);
}

/// What PR 8's ignore editor previews for `pattern` going to `profile`.
async fn ignore_preview(f: &Fixture, pattern: &str, profile: &str) -> String {
    let query = serde_urlencoded::to_string([("pattern", pattern), ("profile", profile)]).unwrap();
    f.get(&format!("/pr/org/repo/8/ignore/preview?{query}"))
        .await
        .body
}

/// Exactly what Firefox sends for the dashboard's own "Review now" form
/// under `Referrer-Policy: no-referrer`.
#[tokio::test]
async fn firefoxs_same_origin_form_post_with_a_null_origin_is_accepted() {
    let f = fixture(false).await;
    let token = f.token();
    let body = || encode(&[("csrf", token.as_str())]);
    let firefox = Request::post("/pr/org/repo/8/review-now")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "null")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-dest", "document")
        .header("sec-fetch-user", "?1")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let reply = f.send(firefox.body(body()).unwrap()).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(*f.serve.started.lock().unwrap(), [key(8)]);

    // `null` alone isn't vouched for, nor alongside another site.
    let bare_null = Request::post("/pr/org/repo/8/review-now")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "null")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let reply = f.send(bare_null.body(body()).unwrap()).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    for site in ["cross-site", "same-site", "none"] {
        let req = Request::post("/pr/org/repo/8/review-now")
            .header(header::HOST, HOST)
            .header(header::ORIGIN, "null")
            .header("sec-fetch-site", site)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        let reply = f.send(req.body(body()).unwrap()).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{site}");
    }
    // Vouched for, it still needs the token, and only `null` is excused.
    let tokenless = Request::post("/pr/org/repo/8/review-now")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "null")
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let reply = f.send(tokenless.body(Body::empty()).unwrap()).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let foreign = Request::post("/pr/org/repo/8/review-now")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "https://evil.example")
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let reply = f.send(foreign.body(body()).unwrap()).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(f.serve.started.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pages_set_a_same_origin_referrer_policy() {
    let f = fixture(false).await;
    let reply = f.get("/").await;
    assert_eq!(reply.headers[header::REFERRER_POLICY], "same-origin");
}

#[tokio::test]
async fn the_pr_page_shows_how_to_chat_with_the_reviewer() {
    let f = fixture(false).await;
    let page = f.get("/pr/org/repo/7").await.body;
    let chat = &page[page.find(r#"<section id="chat">"#).unwrap()..];
    let chat = &chat[..chat.find("</section>").unwrap() + "</section>".len()];
    insta::assert_snapshot!(readable(&f, chat));
    // Nothing ran: the worktree is only named, never made.
    assert!(!f.data.path().join("worktrees").exists());
    // PR 8's review failed before a session, so there's nothing to chat with.
    assert!(!f.get("/pr/org/repo/8").await.body.contains(r#"id="chat""#));
}

#[tokio::test]
async fn a_chat_under_a_profile_since_removed_says_why_instead_of_a_command() {
    let f = fixture(false).await;
    let run = {
        let mut store = f.dashboard.app.store();
        let run = store
            .queue_review(&ReviewRequest {
                key: key(9),
                profile: "gone".into(),
                head_sha: "head9".into(),
                base_sha: "base".into(),
                trigger: ReviewTrigger::Requested,
            })
            .unwrap()
            .unwrap()
            .id;
        store.claim_run(run).unwrap();
        store
            .finish_review(
                run,
                &ReviewResult {
                    summary: "Fine.".into(),
                    verdict: Verdict::Comment,
                    comments: vec![],
                    session_id: Some("sess-9".into()),
                    transcript_path: "t".into(),
                },
            )
            .unwrap();
        run
    };
    let page = f.get("/pr/org/repo/9").await.body;
    assert!(
        page.contains(&format!("Can't chat with run {run}: no profile `gone`")),
        "{page}"
    );
    // `sanic-review chat` would refuse too, so there's nothing to copy.
    assert!(!page.contains("data-copy"), "{page}");
}
