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
    clock::{Clock, RecencyWindow, WindowChoice},
    config::{CheckoutResolver, Config, Vcs},
    pr::{Comment, Placement, PrKey, PrSnapshot, Reaction, Review, ReviewState, Thread},
    repo::RepoName,
    run::{
        Basis, Confidence, DraftComment, InlineComment, ReviewRequest, ReviewResult, ReviewTrigger,
        Severity, Side, Verdict,
    },
    skip::SkipRules,
};
use sanic_github::{Client, Token};
use sanic_runner::review::{AgentProfile, RunSettings};
use sanic_store::{DraftStatus, Store};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::watch;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{any, body_json, body_partial_json, body_string_contains, method, path},
};

use crate::{Context, Control, Dashboard, Sources};

const HOST: &str = "127.0.0.1:7117";

#[derive(Default)]
struct FakeServe {
    started: Mutex<Vec<PrKey>>,
    /// The PRs asked to be refreshed from GitHub, in order.
    refreshed: Mutex<Vec<PrKey>>,
    /// `skip_titles` patterns added, with the profile each went to.
    skipped: Mutex<Vec<(String, Option<String>)>>,
    /// The runs asked to be revised, with the draft if it's only that
    /// one, and their instructions.
    revised: Mutex<Vec<(i64, Option<i64>, String)>>,
    /// The run a revision starts; without one, it's refused.
    revision: Mutex<Option<i64>>,
    /// The recency window, as `serve` publishes it.
    window: Mutex<Option<watch::Sender<RecencyWindow>>>,
}

impl Control for FakeServe {
    fn review_now(&self, key: PrKey) {
        self.started.lock().unwrap().push(key);
    }

    fn refresh(&self, key: PrKey) {
        self.refreshed.lock().unwrap().push(key);
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

    fn regenerate(
        &self,
        run_id: i64,
        draft: Option<i64>,
        instruction: &str,
    ) -> color_eyre::Result<Result<i64, sanic_store::Refusal>> {
        self.revised
            .lock()
            .unwrap()
            .push((run_id, draft, instruction.to_owned()));
        Ok(self
            .revision
            .lock()
            .unwrap()
            .ok_or(sanic_store::Refusal::NoSession))
    }

    fn set_window(&self, choice: Option<WindowChoice>) -> color_eyre::Result<()> {
        if let Some(window) = &*self.window.lock().unwrap() {
            window.send_modify(|window| window.choice = choice);
        }
        Ok(())
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

/// The runner's mirrors, as far as the files view reads them: file
/// contents by commit and path.
#[derive(Default)]
struct FakeMirror(Mutex<HashMap<(String, String), String>>);

impl FakeMirror {
    fn add(&self, commit: &str, path: &str, text: &str) {
        self.0
            .lock()
            .unwrap()
            .insert((commit.into(), path.into()), text.into());
    }
}

impl Sources for FakeMirror {
    fn has_commit(&self, _: &RepoName, commit: &str) -> bool {
        self.0.lock().unwrap().keys().any(|(c, _)| c == commit)
    }

    fn file_at(
        &self,
        _: &RepoName,
        commit: &str,
        path: &str,
    ) -> color_eyre::Result<Option<Vec<u8>>> {
        let files = self.0.lock().unwrap();
        Ok(files
            .get(&(commit.into(), path.into()))
            .map(|text| text.clone().into_bytes()))
    }
}

/// A clock that moves only when a test moves it.
struct FixedClock(Mutex<SystemTime>);

impl Default for FixedClock {
    fn default() -> Self {
        Self(Mutex::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_181_231),
        ))
    }
}

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
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
        in_progress: None,
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
            note: None,
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
    /// Empty until a test adds files.
    mirror: Arc<FakeMirror>,
    github: MockServer,
    /// When the scheduler would queue each debounced review.
    due: watch::Sender<HashMap<PrKey, Instant>>,
    /// The run of PR 7 with drafts: summary, an inline comment and an
    /// unanchored one, in that order.
    run: i64,
    drafts: [i64; 3],
    data: TempDir,
    clock: Arc<FixedClock>,
}

/// The fixture's PRs: 7 you owe, reviewed, where alice answered you; 8 you
/// owe, with changes requested and a failed review; 10 you owe, a draft;
/// and 9, yours, approved and mergeable.
fn fixture_prs() -> Vec<PrSnapshot> {
    let said = |id: &str, author: &str, at: &str| Comment {
        id: id.into(),
        author: author.into(),
        body: "hm".into(),
        created_at: at.into(),
        url: None,
        by_bot: false,
        reacted_at: None,
        reactions: vec![],
    };
    vec![
        // alice answered you in a thread, and you haven't replied.
        PrSnapshot {
            threads: vec![Thread {
                id: "t1".into(),
                path: Some("src/lib.rs".into()),
                line: Some(2),
                resolved: false,
                place: Placement::default(),
                comments: vec![
                    said("c1", "me", "2026-09-20T00:00:00Z"),
                    said("c2", "alice", "2026-09-21T00:00:00Z"),
                ],
            }],
            ..snapshot(7, "alice", "Add the thing")
        },
        PrSnapshot {
            review_decision: Some("CHANGES_REQUESTED".into()),
            ..snapshot(8, "bob", "Fix <script> escaping")
        },
        PrSnapshot {
            is_draft: true,
            ..snapshot(10, "carol", "WIP: try things")
        },
        PrSnapshot {
            review_decision: Some("APPROVED".into()),
            merge_state: Some("CLEAN".into()),
            ..snapshot(9, "me", "My change")
        },
    ]
}

/// A store with [`fixture_prs`], a finished review of 7 and a failed one
/// of 8.
async fn fixture(manual_reviews: bool) -> Fixture {
    let mut store = Store::open_in_memory().unwrap();
    for snap in fixture_prs() {
        store.record(&snap, "me", "default", &[]).unwrap();
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
                summary_note: None,
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
    let (window, window_rx) = watch::channel(RecencyWindow {
        configured: Some(14),
        choice: None,
    });
    *serve.window.lock().unwrap() = Some(window);
    let clock = Arc::new(FixedClock::default());
    let mirror = Arc::new(FakeMirror::default());
    let dashboard = Dashboard::new(Context {
        me: "me".into(),
        manual_reviews,
        data_dir: data.path().to_owned(),
        config_path: "/config/sanic review.toml".into(),
        store,
        github: Client::new(&github.uri(), Token::new("t0ken".into())).unwrap(),
        control: Arc::clone(&serve) as Arc<dyn Control>,
        sources: Arc::clone(&mirror) as Arc<dyn Sources>,
        due: due_rx,
        skips: watch::channel(skip_rules()).1,
        window: window_rx,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    })
    .unwrap();
    Fixture {
        dashboard,
        serve,
        mirror,
        github,
        due,
        run,
        drafts: [ids[0], ids[1], ids[2]],
        data,
        clock,
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

    /// The preview with the verdict in the URL alone, as a link has it:
    /// an approval's isn't picked.
    fn preview_uri(&self, event: &str) -> String {
        format!("/pr/org/repo/7/runs/{}/preview?event={event}", self.run)
    }

    /// Where the PR page's verdict form, sent with `event`, goes: for
    /// Approve, with a new pick.
    async fn picked_preview_uri(&self, event: &str) -> String {
        let form = format!("/pr/org/repo/7/runs/{}/preview", self.run);
        let reply = self.post(&form, &[("event", event)]).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        reply.headers[header::LOCATION].to_str().unwrap().to_owned()
    }

    fn advance(&self, by: Duration) {
        *self.clock.0.lock().unwrap() += by;
    }

    fn submit_uri(&self) -> String {
        format!("/pr/org/repo/7/runs/{}/submit", self.run)
    }

    /// The payload the preview's confirm form would send back.
    async fn previewed_payload(&self, event: &str) -> String {
        let page = self.get(&self.picked_preview_uri(event).await).await;
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

/// `html` with a line per tag and the random token, data dir, times and
/// extra unset token variables masked, for snapshots.
fn readable(fixture: &Fixture, html: &str) -> String {
    // The chat commands also unset any other token variable the test's
    // environment has, such as CI's.
    let html = sanic_runner::chat::unset_vars()
        .iter()
        .skip(sanic_runner::claude::TOKEN_VARS.len())
        .fold(without_times(html), |html, var| {
            // Trailing space, so `X` doesn't eat the head of `X_FILE`.
            html.replace(&format!(" -u {} ", sanic_runner::chat::quote(var)), " ")
        });
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
async fn an_index_row_opens_the_page_its_title_links_to() {
    let f = fixture(false).await;
    let page = f.get("/").await.body;
    let rows: Vec<&str> = page.split(" data-row ").skip(1).collect();
    assert!(!rows.is_empty(), "{page}");
    // The script opens a row's `data-href` on a click outside its own
    // targets, so it's the title's link, which stays a real link.
    for row in rows {
        let (_, rest) = row
            .split_once(r#"data-href=""#)
            .unwrap_or_else(|| panic!("a row without data-href: {row}"));
        let href = &rest[..rest.find('"').unwrap()];
        assert!(
            row.contains(&format!(r#"<span class="t"><a href="{href}""#)),
            "{row}"
        );
    }
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
    assert!(page.body.contains(r#"<span class="chip held">held</span>"#));
    assert!(page.body.contains("waiting 1:00:00"), "{}", page.body);
}

#[tokio::test]
async fn a_pr_page_shows_drafts_in_their_diff_and_marks_the_pr_seen() {
    let f = fixture(false).await;
    assert!(f.get("/").await.body.contains(r#"class="newdot""#));
    let page = f.get("/pr/org/repo/7").await;
    assert_eq!(page.status, StatusCode::OK);
    insta::assert_snapshot!(readable(&f, &page.body));
    assert!(!f.get("/").await.body.contains(r#"class="newdot""#));

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
        format!("/drafts/{}/thread", f.drafts[1]),
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
    assert!(f.serve.refreshed.lock().unwrap().is_empty());

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
    takes_review(&f, &expected).await;
    let payload = hidden_value(&preview.body, "payload");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
        json!({ "left": null, "review": expected, "replies": [], "reactions": [] })
    );
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "REQUEST_CHANGES"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert!(posted.body.contains("pullrequestreview-5"));
    insta::assert_snapshot!("posted_card", readable(&f, card_of(&posted.body)));
    for i in 0..3 {
        assert_eq!(f.status(i), "posted");
    }
    // `serve` fetches the PR again now, so the lists show the review.
    assert_eq!(*f.serve.refreshed.lock().unwrap(), [key(7)]);

    // Confirming again finds nothing left to post, and sends nothing, so
    // there's nothing new to fetch.
    let again = f
        .post(
            &f.submit_uri(),
            &[("event", "REQUEST_CHANGES"), ("payload", &payload)],
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(f.serve.refreshed.lock().unwrap().len(), 1);
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
    let approval = f.get(&f.picked_preview_uri("APPROVE").await).await.body;
    let reply = f
        .post(
            &f.submit_uri(),
            &[
                ("event", "APPROVE"),
                ("payload", &payload),
                ("pick", &hidden_value(&approval, "pick")),
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
    assert!(reply.body.contains("nothing is retried"), "{}", reply.body);
    // A refusal is known not to be posted: it says so, not that GitHub
    // may have it, and the next submit has nothing to look for.
    assert!(
        reply
            .body
            .contains("GitHub refused it, so it wasn't posted."),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("may have taken it"), "{}", reply.body);
    assert_eq!(
        f.dashboard.app.store().pending_review(&key(7)).unwrap(),
        None
    );
    let again = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(!again.contains("never answered"), "{again}");
    assert!(!again.contains("your newest reviews"), "{again}");
    for i in 0..3 {
        assert_eq!(f.status(i), "accepted");
    }
    // GitHub took nothing, so there's nothing new to fetch.
    assert!(f.serve.refreshed.lock().unwrap().is_empty());
}

/// Records the fixture's PR 7 as polled with your own review pending on
/// GitHub, review `id`, with two comments.
fn with_your_pending_review(f: &Fixture, id: &str) {
    use sanic_core::pr::{InProgressComment, InProgressReview};
    let comment = |n: u32| InProgressComment {
        id: format!("PRRC_{n}"),
        path: "src/lib.rs".into(),
        line: Some(n),
        start_line: None,
        outdated: false,
        body: format!("pending {n}"),
    };
    let snap = PrSnapshot {
        in_progress: Some(InProgressReview {
            id: id.into(),
            comments: vec![comment(1), comment(2)],
        }),
        ..fixture_prs().swap_remove(0)
    };
    f.dashboard
        .app
        .store()
        .record(&snap, "me", "default", &[])
        .unwrap();
}

const YOURS_PENDING: &str =
    "You have a pending review on GitHub; submit or discard it there first.";

#[tokio::test]
async fn the_preview_says_your_pending_review_on_github_must_go_first() {
    let f = fixture(false).await;
    accept_all(&f).await;
    assert!(
        !f.get(&f.preview_uri("COMMENT"))
            .await
            .body
            .contains(YOURS_PENDING)
    );
    with_your_pending_review(&f, "PRR_yours");
    // From the store, as last polled: the preview asks GitHub nothing.
    let guard = Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .named("GitHub before confirming")
        .mount_as_scoped(&f.github)
        .await;
    let preview = f.get(&f.preview_uri("COMMENT")).await.body;
    drop(guard);
    assert!(preview.contains(YOURS_PENDING), "{preview}");
    assert!(preview.contains("2 comments when last polled"), "{preview}");
    assert!(
        preview.contains("https://github.com/org/repo/pull/7/files"),
        "{preview}"
    );

    // GitHub refuses it: the failure says why it most likely did.
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Unprocessable Entity",
            "errors": ["User can only have one pending review per pull request"],
        })))
        .expect(1)
        .mount(&f.github)
        .await;
    let payload = hidden_value(&preview, "payload");
    let reply = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert!(
        reply
            .body
            .contains("You have a pending review on GitHub, which is most likely why"),
        "{}",
        reply.body
    );
    for i in 0..3 {
        assert_eq!(f.status(i), "accepted");
    }
}

#[tokio::test]
async fn a_review_an_earlier_submit_left_pending_isnt_yours_to_submit_first() {
    let f = fixture(false).await;
    accept_all(&f).await;
    // The poll saw the pending review a submit here made and recorded.
    f.dashboard
        .app
        .store()
        .record_pending_review(
            &key(7),
            &sanic_store::PendingReview {
                run: f.run,
                drafts: vec![],
                on_github: sanic_store::OnGithub::Pending {
                    node_id: "PRR_ours".into(),
                    html_url: "https://github.com/org/repo/pull/7#pullrequestreview-1".into(),
                },
            },
        )
        .unwrap();
    with_your_pending_review(&f, "PRR_ours");
    let preview = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(!preview.contains(YOURS_PENDING), "{preview}");
    assert!(preview.contains("An earlier submit on this PR left a review pending."));
}

#[tokio::test]
async fn the_pr_page_says_how_many_of_your_pending_comments_the_agent_saw() {
    let f = fixture(false).await;
    assert!(
        !f.get("/pr/org/repo/7")
            .await
            .body
            .contains("pending review comment")
    );
    f.dashboard
        .app
        .store()
        .record_in_progress_shown(f.run, 2)
        .unwrap();
    let page = f.get("/pr/org/repo/7").await.body;
    assert!(
        page.contains("The agent saw 2 of your pending review comments on GitHub"),
        "{page}"
    );
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
        json!({ "left": null, "review": expected, "replies": [], "reactions": [] })
    );
    takes_review(&f, &expected).await;
    // Approve in a URL alone isn't picked: only the PR page's verdict
    // form, a post, hands out a pick.
    let unpicked = f.get(&f.preview_uri("APPROVE")).await;
    assert_eq!(unpicked.status, StatusCode::CONFLICT);
    assert!(unpicked.body.contains("pick Approve again on the PR page"));
    let pr_page = f.get("/pr/org/repo/7").await.body;
    assert!(pr_page.contains(r#"method="post""#), "{pr_page}");
    assert!(pr_page.contains(r#"value="APPROVE""#), "{pr_page}");
    // Another verdict's form goes to its preview with no pick.
    assert_eq!(
        f.picked_preview_uri("COMMENT").await,
        f.preview_uri("COMMENT")
    );
    let uri = f.picked_preview_uri("APPROVE").await;
    let preview = f.get(&uri).await.body;
    assert!(preview.contains(">Approve this PR<"), "{preview}");
    // Its button says it approves; nothing in the checklist repeats it.
    assert!(!preview.contains("You picked Approve"), "{preview}");
    assert!(!preview.contains(r#"type="checkbox""#));
    let pick = hidden_value(&preview, "pick");
    assert!(uri.ends_with(&format!("&pick={pick}")), "{uri}");
    let without_pick = [("event", "APPROVE"), ("payload", payload.as_str())];
    assert_eq!(
        f.post(&f.submit_uri(), &without_pick).await.status,
        StatusCode::CONFLICT
    );
    let confirm = [
        ("event", "APPROVE"),
        ("payload", payload.as_str()),
        ("pick", pick.as_str()),
    ];
    // Its preview shows the one request it sends, and it sends only that.
    assert_eq!(
        preview.matches("Exactly what's sent").count(),
        1,
        "{preview}"
    );
    assert!(
        preview.contains("&quot;event&quot;: &quot;APPROVE&quot;"),
        "{preview}"
    );
    assert_eq!(
        f.post(&f.submit_uri(), &confirm).await.status,
        StatusCode::OK
    );
    assert_eq!(f.github.received_requests().await.unwrap().len(), 1);
    // Posting used the pick up: neither the confirm nor its preview, from
    // history, comes back.
    let again = f.post(&f.submit_uri(), &confirm).await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert!(again.body.contains("pick Approve again on the PR page"));
    let replayed = f.get(&uri).await;
    assert_eq!(replayed.status, StatusCode::CONFLICT);
    assert!(replayed.body.contains("pick Approve again on the PR page"));
}

#[tokio::test]
async fn an_approval_pick_expires_and_is_for_its_run() {
    let f = fixture(false).await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.github)
        .await;
    let uri = f.picked_preview_uri("APPROVE").await;
    let preview = f.get(&uri).await.body;
    let (payload, pick) = (
        hidden_value(&preview, "payload"),
        hidden_value(&preview, "pick"),
    );
    let confirm = [("event", "APPROVE"), ("payload", &payload), ("pick", &pick)];
    // Another run's confirm can't use it.
    let other = format!("/pr/org/repo/7/runs/{}/submit", f.run + 1);
    assert_eq!(f.post(&other, &confirm).await.status, StatusCode::CONFLICT);
    let other = uri.replace(
        &format!("/runs/{}/", f.run),
        &format!("/runs/{}/", f.run + 1),
    );
    assert_eq!(f.get(&other).await.status, StatusCode::CONFLICT);
    // Nor, once it's expired, can its own.
    let uri = f.picked_preview_uri("APPROVE").await;
    let preview = f.get(&uri).await.body;
    f.advance(crate::submit::PICK_TTL);
    assert_eq!(f.get(&uri).await.status, StatusCode::CONFLICT);
    let expired = f
        .post(
            &f.submit_uri(),
            &[
                ("event", "APPROVE"),
                ("payload", &hidden_value(&preview, "payload")),
                ("pick", &hidden_value(&preview, "pick")),
            ],
        )
        .await;
    assert_eq!(expired.status, StatusCode::CONFLICT);
    assert!(
        expired.body.contains("Nothing was sent"),
        "{}",
        expired.body
    );
}

#[tokio::test]
async fn review_now_asks_first_and_then_asks_serve() {
    let f = fixture(false).await;
    // PR 8's review failed, PR 10 is a skipped draft; PR 7 is drafted.
    let ask = f.get("/pr/org/repo/8/review-now").await;
    assert!(
        ask.body.contains("<h1>Rerun this review?</h1>"),
        "{}",
        ask.body
    );
    insta::assert_snapshot!("review_now_card", readable(&f, card_of(&ask.body)));
    // It says why it failed, and what it costs.
    assert!(
        ask.body
            .contains("failed: claude exited with 1\nand more detail")
    );
    assert!(ask.body.contains("Spends tokens"));
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
            .contains("<h1>Nothing to start</h1>")
    );
    assert!(f.serve.started.lock().unwrap().is_empty());

    // Posting to it anyway, as a stale confirm tab would, starts nothing.
    let reply = f.post("/pr/org/repo/7/review-now", &[]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(f.serve.started.lock().unwrap().is_empty());

    let reply = f.post("/pr/org/repo/8/review-now", &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers[header::LOCATION], "/pr/org/repo/8");
    // Confirmed in a dialog over the index, it goes back there.
    let reply = f
        .post("/pr/org/repo/10/review-now", &[("next", "index")])
        .await;
    assert_eq!(reply.headers[header::LOCATION], "/");
    assert_eq!(*f.serve.started.lock().unwrap(), [key(8), key(10)]);
}

/// The top bar's pending draft count on the page at `uri`.
async fn pending_in_top_bar(f: &Fixture, uri: &str) -> String {
    let page = f.get(uri).await.body;
    let at = page.find(r#"<b class="cnt">"#).expect(&page) + r#"<b class="cnt">"#.len();
    page[at..at + page[at..].find('<').unwrap()].to_owned()
}

#[tokio::test]
async fn the_top_bar_counts_the_pending_drafts_the_lists_show() {
    let f = fixture(false).await;
    // PR 7's review: its summary and both comments.
    assert_eq!(pending_in_top_bar(&f, "/").await, "3");
    assert_eq!(pending_in_top_bar(&f, "/pr/org/repo/8").await, "3");
    // Older than the recency window, it's off the lists, and out of the
    // count until a window wide enough is picked.
    let old = PrSnapshot {
        updated_at: Some("2026-08-01T00:00:00Z".into()),
        ..fixture_prs().remove(0)
    };
    f.dashboard
        .app
        .store()
        .record(&old, "me", "default", &[])
        .unwrap();
    assert_eq!(pending_in_top_bar(&f, "/").await, "0");
    f.post("/window", &[("window", "all")]).await;
    assert_eq!(pending_in_top_bar(&f, "/").await, "3");
    // Archived, it's counted only where archived PRs are shown.
    f.dashboard.app.store().set_archived(&key(7), true).unwrap();
    assert_eq!(pending_in_top_bar(&f, "/").await, "0");
    assert_eq!(pending_in_top_bar(&f, "/?archived=true").await, "3");
    assert_eq!(pending_in_top_bar(&f, "/pr/org/repo/8").await, "0");
    f.dashboard
        .app
        .store()
        .set_archived(&key(7), false)
        .unwrap();
    // Closed, it's off the lists, and out of the count.
    f.dashboard.app.store().mark_closed(&key(7)).unwrap();
    assert_eq!(pending_in_top_bar(&f, "/?archived=true").await, "0");
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
        index.contains(r#"Reviews you owe <span class="dim">2</span>"#)
            && index.contains("show 1 archived"),
        "{index}"
    );
    let all = f.get("/?archived=true").await.body;
    assert!(all.contains("Fix &lt;script&gt; escaping"));
    assert!(all.contains(r#"<span class="chip dim">archived</span>"#));

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
        ("favicon.svg", "image/svg+xml"),
    ] {
        let reply = f.get(&format!("/assets/{file}")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers[header::CONTENT_TYPE], kind);
        assert!(!reply.body.is_empty());
    }
    assert_eq!(f.get("/assets/nope.js").await.status, StatusCode::NOT_FOUND);
    // Browsers that probe for the old icon path get the SVG.
    let ico = f.get("/favicon.ico").await;
    assert_eq!(ico.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(ico.headers[header::LOCATION], "/assets/favicon.svg");
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
    let preview = f.picked_preview_uri("APPROVE").await;
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
    assert!(f.get("/").await.body.contains(r#"class="newdot""#));
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
    let chat = &page[page.find(r#"<section class="chat" id="chat">"#).unwrap()..];
    let chat = &chat[..chat.find("</section>").unwrap() + "</section>".len()];
    insta::assert_snapshot!(readable(&f, chat));
    // Nothing ran: the worktree is only named, never made.
    assert!(!f.data.path().join("worktrees").exists());
    // PR 8's review failed before a session, so there's nothing to chat with.
    assert!(!f.get("/pr/org/repo/8").await.body.contains(r#"id="chat""#));
}

#[tokio::test]
async fn reviewing_an_already_reviewed_head_again_says_by_whom() {
    let f = fixture(false).await;
    let review = |id: &str, author: &str| Review {
        id: id.into(),
        author: author.into(),
        state: ReviewState::Commented,
        body: String::new(),
        submitted_at: format!("2026-09-2{}T00:00:00Z", id.len()),
        commit: Some("head11".into()),
        by_bot: false,
    };
    let snap = PrSnapshot {
        reviews: vec![review("r", "me"), review("r2", "alice")],
        ..snapshot(11, "dave", "Tidy things")
    };
    f.dashboard
        .app
        .store()
        .record(&snap, "me", "default", &[])
        .unwrap();

    let index = f.get("/").await.body;
    assert!(
        index.contains(r#"data-review-now="/pr/org/repo/11/review-now""#),
        "{index}"
    );
    let ask = f.get("/pr/org/repo/11/review-now").await.body;
    assert!(
        ask.contains("<h1>Already reviewed by you, alice. Review anyway?</h1>"),
        "{ask}"
    );
    assert!(ask.contains("It stays skipped for automatic reviews."));
    // The skipped-for-a-reason wording stays for the rest.
    let draft = f.get("/pr/org/repo/10/review-now").await.body;
    assert!(
        draft.contains("<h1>Review this draft-skipped PR anyway?</h1>"),
        "{draft}"
    );
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
                    summary_note: None,
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

#[tokio::test]
async fn prs_show_where_they_stand_as_in_the_tui() {
    let f = fixture(false).await;
    let index = f.get("/").await.body;
    for cell in [
        // PR 7 leads with its drafts, and says the rest after.
        ">1 to answer</span>",
        r#"<span class="u-quiet">changes requested</span>"#,
        // Yours leads with it.
        r#"<span class="chip u-good">mergeable</span><span class="pop""#,
    ] {
        assert!(index.contains(cell), "{cell}: {index}");
    }
    // Your PRs show their state instead of a review status.
    let mine = &index[index.find(r#"id="mine""#).unwrap()..];
    assert!(!mine.contains(r#"class="status"#), "{mine}");
    // And the PR page says it too.
    let page = f.get("/pr/org/repo/9").await.body;
    assert!(
        page.contains(r#"<span class="state good">mergeable</span>"#),
        "{page}"
    );
    // Not in either list any more, a PR's page still says where it stands.
    let unrequested = PrSnapshot {
        review_requested: false,
        ..fixture_prs().swap_remove(1)
    };
    f.dashboard
        .app
        .store()
        .record(&unrequested, "me", "default", &[])
        .unwrap();
    let page = f.get("/pr/org/repo/8").await.body;
    assert!(
        page.contains(r#"<span class="state quiet">changes requested</span>"#),
        "{page}"
    );
    // With nothing to say, the header says nothing.
    let page = f.get("/pr/org/repo/10").await.body;
    assert!(!page.contains(r#"class="state"#), "{page}");
    // Your own PRs too.
    f.dashboard
        .app
        .store()
        .record(&snapshot(11, "me", "Quiet change"), "me", "default", &[])
        .unwrap();
    let page = f.get("/pr/org/repo/11").await.body;
    assert!(!page.contains(r#"class="state"#), "{page}");
    // Archived, a PR of yours says so instead.
    f.post(
        "/pr/org/repo/9/archive",
        &[("archived", "true"), ("next", "index")],
    )
    .await;
    let all = f.get("/?archived=true").await.body;
    assert!(
        all.contains(r#"<span class="chip dim">archived</span><span class="pop""#),
        "{all}"
    );
}

#[tokio::test]
async fn the_index_groups_rows_by_what_they_ask_of_you() {
    let f = fixture(true).await;
    {
        let mut store = f.dashboard.app.store();
        // PR 12 waits out the quiet period; PR 13's review is held.
        for (number, title) in [(12, "Waiting one"), (13, "Held one")] {
            store
                .record(&snapshot(number, "dave", title), "me", "default", &[])
                .unwrap();
        }
        store
            .queue_review(&ReviewRequest {
                key: key(13),
                profile: "default".into(),
                head_sha: "head13".into(),
                base_sha: "base".into(),
                trigger: ReviewTrigger::Requested,
            })
            .unwrap();
    }
    let index = f.get("/").await.body;
    let group = |title: &str| group_of(&index, title);
    assert_eq!(group("Add the thing"), "NEEDS YOU", "{index}");
    assert_eq!(group("Fix &lt;script&gt; escaping"), "NEEDS YOU");
    assert_eq!(group("Held one"), "NEEDS YOU");
    assert_eq!(group("Waiting one"), "IN FLIGHT");
    assert_eq!(group("WIP: try things"), "NOTHING TO DO NOW");
    // The held one leads with it; the waiting one with its wait.
    assert!(index.contains(r#"<span class="chip held">held</span>"#));
    assert!(index.contains(r#"<span class="chip dim">waiting</span>"#));
    // Rows carry the PR they're about, for selection to follow.
    assert!(index.contains(r#"data-key="org/repo#13""#));

    // A PR whose failed run a new push replaces waits, as its status says.
    f.due.send_replace(HashMap::from([(
        key(8),
        Instant::now() + Duration::from_secs(60),
    )]));
    let index = f.get("/").await.body;
    assert_eq!(
        group_of(&index, "Fix &lt;script&gt; escaping"),
        "IN FLIGHT",
        "{index}"
    );
}

/// The heading of the group the row titled `title` is under.
fn group_of(index: &str, title: &str) -> String {
    let at = index
        .find(&format!(r#"title="{title}""#))
        .unwrap_or_else(|| panic!("no {title}: {index}"));
    let heading = &index[index[..at].rfind(r#"class="group-h"#).unwrap()..at];
    let text = &heading[heading.find('>').unwrap() + 1..];
    text.split(" ·").next().unwrap().to_owned()
}

/// Records PR `number` by `author` and a finished review of it whose
/// drafts, summary first, are then set to `statuses`, `posted` included.
fn reviewed_pr(f: &Fixture, snap: &PrSnapshot, statuses: &[&str]) {
    let mut store = f.dashboard.app.store();
    store.record(snap, "me", "default", &[]).unwrap();
    let run = store
        .queue_review(&ReviewRequest {
            key: snap.key.clone(),
            profile: "default".into(),
            head_sha: snap.head_sha.clone(),
            base_sha: "base".into(),
            trigger: ReviewTrigger::Requested,
        })
        .unwrap()
        .unwrap()
        .id;
    store.claim_run(run).unwrap();
    let comments = vec![comment("src/lib.rs", 3, Side::Right, "Hm.", false); statuses.len() - 1];
    store
        .finish_review(
            run,
            &ReviewResult {
                summary: "Fine.".into(),
                summary_note: None,
                verdict: Verdict::Comment,
                comments,
                session_id: None,
                transcript_path: "t".into(),
            },
        )
        .unwrap();
    let drafts = store.draft_rows(run).unwrap();
    let mut posted = Vec::new();
    for (draft, status) in drafts.iter().zip(statuses) {
        let status = match *status {
            "posted" => {
                posted.push(draft.id);
                DraftStatus::Accepted
            }
            "accepted" => DraftStatus::Accepted,
            "rejected" => DraftStatus::Rejected,
            _ => DraftStatus::Pending,
        };
        store.set_draft_status(draft.id, status).unwrap();
    }
    store.mark_posted(&posted).unwrap();
    store.record_view(&snap.key).unwrap();
}

#[tokio::test]
async fn decided_drafts_and_blocked_approvals_are_grouped_by_whose_move_it_is() {
    let f = fixture(false).await;
    let approved = |number, merge: &str, checks: &str, title| PrSnapshot {
        review_decision: Some("APPROVED".into()),
        merge_state: Some(merge.into()),
        checks: Some(checks.into()),
        in_progress: None,
        ..snapshot(number, "me", title)
    };
    {
        let mut store = f.dashboard.app.store();
        for snap in [
            approved(21, "UNSTABLE", "FAILURE", "Mine, ci failing"),
            approved(22, "DIRTY", "SUCCESS", "Mine, conflicts"),
            approved(23, "BLOCKED", "SUCCESS", "Mine, blocked"),
            approved(24, "BEHIND", "SUCCESS", "Mine, behind"),
            approved(25, "BLOCKED", "PENDING", "Mine, ci pending"),
        ] {
            store.record(&snap, "me", "default", &[]).unwrap();
        }
    }
    reviewed_pr(
        &f,
        &snapshot(31, "dave", "Two to post"),
        &["accepted", "rejected", "accepted"],
    );
    reviewed_pr(
        &f,
        &snapshot(32, "dave", "All rejected"),
        &["rejected", "rejected"],
    );
    let mut yours = Review {
        id: "r33".into(),
        author: "me".into(),
        state: ReviewState::Commented,
        body: String::new(),
        submitted_at: "2026-09-22T00:00:00Z".into(),
        commit: Some("head33".into()),
        by_bot: false,
    };
    let posted = PrSnapshot {
        reviews: vec![yours.clone()],
        ..snapshot(33, "dave", "Posted")
    };
    reviewed_pr(&f, &posted, &["posted", "rejected"]);
    // Rejected everything, but you've reviewed it yourself.
    yours.id = "r34".into();
    yours.commit = Some("head34".into());
    let reviewed = PrSnapshot {
        reviews: vec![yours],
        ..snapshot(34, "dave", "Rejected and reviewed")
    };
    reviewed_pr(&f, &reviewed, &["rejected"]);

    let index = f.get("/").await.body;
    for (title, group) in [
        ("Mine, ci failing", "NEEDS YOU"),
        ("Mine, conflicts", "NEEDS YOU"),
        ("Mine, blocked", "WAITING ON REVIEWERS"),
        ("Mine, behind", "READY"),
        ("Mine, ci pending", "READY"),
        ("Two to post", "NEEDS YOU"),
        ("All rejected", "NEEDS YOU"),
        ("Posted", "NOTHING TO DO NOW"),
        ("Rejected and reviewed", "NOTHING TO DO NOW"),
    ] {
        assert_eq!(group_of(&index, title), group, "{title}");
    }
    for lead in [
        r#"<span class="chip bad">ci failing</span>"#,
        r#"<span class="chip bad">conflicts</span>"#,
        r#"<span class="sig post">2 to post</span>"#,
        r#"<span class="sig post">submit review</span>"#,
        r#"<span class="chip ok">posted</span>"#,
    ] {
        assert!(index.contains(lead), "{lead}: {index}");
    }

    // A newer review on its way will replace the rejected drafts.
    f.due.send_replace(HashMap::from([(
        key(32),
        Instant::now() + Duration::from_secs(60),
    )]));
    let index = f.get("/").await.body;
    assert_eq!(group_of(&index, "All rejected"), "IN FLIGHT");
    assert!(!index.contains("submit review"), "{index}");
}

/// The text of the reviewed-by element of the row titled `title`, tags
/// and its popover left out.
fn reviewed_by(index: &str, title: &str) -> String {
    let at = index.find(&format!(r#"title="{title}""#)).unwrap();
    let start = index[..at].rfind(r#"<span class="rb""#).unwrap();
    let end = start + index[start..].find(r#"<span class="pop""#).unwrap();
    let mut text = String::new();
    let mut tag = false;
    for c in index[start..end].chars() {
        match c {
            '<' => tag = true,
            '>' => tag = false,
            c if !tag => text.push(c),
            _ => {}
        }
    }
    text
}

#[tokio::test]
async fn rows_say_who_reviewed_with_their_verdicts() {
    let f = fixture(false).await;
    let review = |author: &str, state, commit: &str| Review {
        id: format!("{author}-{commit}"),
        author: author.into(),
        state,
        body: String::new(),
        submitted_at: "2026-09-22T00:00:00Z".into(),
        commit: Some(commit.into()),
        by_bot: false,
    };
    let (ap, rc, cm) = (
        ReviewState::Approved,
        ReviewState::ChangesRequested,
        ReviewState::Commented,
    );
    let cases: Vec<(u32, Vec<Review>, &str)> = vec![
        (41, vec![review("me", cm, "head41")], "reviewed by you○"),
        (
            42,
            vec![review("alice", ap, "head42")],
            "reviewed by @alice✓",
        ),
        (
            43,
            vec![review("me", rc, "head43"), review("alice", ap, "head43")],
            "reviewed by you✗ and @alice✓",
        ),
        (
            44,
            vec![review("alice", ap, "head44"), review("bob", cm, "head44")],
            "reviewed by @alice✓ and @bob○",
        ),
        (
            45,
            vec![
                review("alice", ap, "head45"),
                review("bob", cm, "head45"),
                review("carol", ap, "head45"),
            ],
            "reviewed by @alice✓ + 2 others (✓○)",
        ),
        (
            46,
            vec![
                review("me", cm, "old46"),
                review("alice", ap, "head46"),
                review("bob", rc, "head46"),
            ],
            "reviewed by you○ and 2 others (✓✗) · 1 push since your review",
        ),
    ];
    {
        let mut store = f.dashboard.app.store();
        for (number, reviews, _) in &cases {
            let snap = PrSnapshot {
                reviews: reviews.clone(),
                ..snapshot(*number, "dave", &format!("PR {number}"))
            };
            store.record(&snap, "me", "default", &[]).unwrap();
        }
    }
    let index = f.get("/").await.body;
    assert_eq!(reviewed_by(&index, "Add the thing"), "no reviews");
    for (number, _, words) in &cases {
        assert_eq!(reviewed_by(&index, &format!("PR {number}")), *words);
    }
    // A review before the latest push is dimmed, and its popover says so.
    assert!(index.contains(r#"<span class="who stale" title="before the latest push">you"#));
    assert!(index.contains("before the latest push · 1 push since"));
    // Skipped because someone reviewed the head, the lead says only that.
    let at = index.find(r#"title="PR 42""#).unwrap();
    let row = &index[index[..at].rfind(r#"<div class="ib"#).unwrap()..at];
    assert!(
        row.contains(r#"<span class="chip dim">skipped</span>"#),
        "{row}"
    );
    assert!(row.contains("someone reviewed the current head"), "{row}");
}

#[tokio::test]
async fn rows_say_what_waits_on_the_author_or_the_reviewer() {
    let f = fixture(false).await;
    let said = |id: &str, author: &str, at: &str| Comment {
        id: id.into(),
        author: author.into(),
        body: "?".into(),
        created_at: at.into(),
        url: None,
        by_bot: false,
        reacted_at: None,
        reactions: vec![],
    };
    let thread = |id: &str, comments| Thread {
        id: id.into(),
        path: None,
        line: None,
        resolved: false,
        place: Placement::default(),
        comments,
    };
    let mut reacted = said("wc3", "me", "2026-09-20T00:00:00Z");
    reacted.reactions = vec![Reaction {
        login: "dave".into(),
        at: "2026-09-21T00:00:00Z".into(),
    }];
    let owed = PrSnapshot {
        threads: vec![
            thread("t1", vec![said("wc1", "me", "2026-09-20T00:00:00Z")]),
            thread("t2", vec![said("wc2", "me", "2026-09-20T00:00:00Z")]),
            // Dave's reaction answers you.
            thread("t3", vec![reacted]),
        ],
        ..snapshot(51, "dave", "Asked Dave")
    };
    let mine = PrSnapshot {
        threads: vec![thread(
            "t4",
            vec![
                said("wc4", "erin", "2026-09-19T00:00:00Z"),
                said("wc5", "me", "2026-09-20T00:00:00Z"),
            ],
        )],
        ..snapshot(52, "me", "Answered Erin")
    };
    {
        let mut store = f.dashboard.app.store();
        store.record(&owed, "me", "default", &[]).unwrap();
        store.record(&mine, "me", "default", &[]).unwrap();
    }
    let index = f.get("/").await.body;
    assert!(
        index.contains(
            r#"<span class="sig them" title="your comments the author hasn't answered">2 awaiting <b>@dave</b></span>"#
        ),
        "{index}"
    );
    assert!(index.contains(
        r#"<span class="sig them" title="your replies the reviewer hasn't answered">1 awaiting <b>@erin</b></span>"#
    ));
}

#[tokio::test]
async fn the_hidden_count_offers_wider_windows_until_reset() {
    let f = fixture(false).await;
    // Nothing counted yet: nothing to say.
    assert!(!f.get("/").await.body.contains("hidden-foot"));
    f.dashboard
        .app
        .store()
        .set_hidden(sanic_store::List::Owed, 14, 9)
        .unwrap();
    let index = f.get("/").await.body;
    let foot = &index[index.find(r#"<div class="hidden-foot">"#).unwrap()..];
    let foot = &foot[..foot.find("</div>").unwrap()];
    assert!(
        foot.starts_with(r#"<div class="hidden-foot">9 older PRs hidden · show last "#),
        "{foot}"
    );
    for (value, words) in [("30", "1 month"), ("180", "6 months"), ("all", "all")] {
        assert!(
            foot.contains(&format!(
                r#"<input type="hidden" name="window" value="{value}"><button class="linkbtn" type="submit">{words}</button>"#
            )),
            "{foot}"
        );
    }
    assert!(!foot.contains("back to default"));
    // Your PRs had none counted.
    assert_eq!(index.matches(r#"class="hidden-foot""#).count(), 1);

    // Picking one needs the token, like every write.
    let refused = f
        .send(form("/window").body(encode(&[("window", "180")])).unwrap())
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let picked = f.post("/window", &[("window", "180")]).await;
    assert_eq!(picked.status, StatusCode::SEE_OTHER);
    assert_eq!(picked.headers[header::LOCATION], "/");
    let index = f.get("/").await.body;
    assert!(index.contains("showing the last 180 days"), "{index}");
    assert!(index.contains(">all</button>"));
    assert!(!index.contains(">1 month</button>"));
    assert!(index.contains(">back to default</button></form> (14 days)"));
    // The window is shared: both lists offer the way back.
    assert_eq!(index.matches(r#"class="hidden-foot""#).count(), 2);

    assert_eq!(
        f.post("/window", &[("window", "soon")]).await.status,
        StatusCode::CONFLICT
    );
    // Picked with archived PRs shown, the index still shows them.
    let archived = f.get("/?archived=true").await.body;
    assert!(archived.contains(r#"<input type="hidden" name="archived" value="true">"#));
    let back = f
        .post("/window", &[("window", "default"), ("archived", "true")])
        .await;
    assert_eq!(back.headers[header::LOCATION], "/?archived=true");
    let index = f.get("/").await.body;
    assert!(index.contains("9 older PRs hidden"));
    assert!(!index.contains("back to default"));
}

/// A confirm or result page's card, as the dashboard's dialog lifts it.
fn card_of(html: &str) -> &str {
    let start = html.find(r#"<div class="cf"#).unwrap();
    let end = html[start..].find("</main>").unwrap();
    &html[start..start + end]
}

#[tokio::test]
async fn the_preview_says_what_the_review_leaves_out_and_where_it_lands() {
    let f = fixture(false).await;
    // Only the summary is accepted; the two comments stay pending.
    f.post(
        &format!("/drafts/{}/status", f.drafts[0]),
        &[("status", "accepted")],
    )
    .await;
    // The PR has moved on since the review.
    let moved = PrSnapshot {
        head_sha: "newer".into(),
        ..fixture_prs().swap_remove(0)
    };
    f.dashboard
        .app
        .store()
        .record(&moved, "me", "default", &[])
        .unwrap();
    let page = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(
        page.contains("the PR has moved on to <code>newer</code>"),
        "{page}"
    );
    assert!(page.contains("<b>2</b> draft(s) still pending aren't included"));
    assert!(page.contains(&format!(
        r#"href="/pr/org/repo/7?run={}#draft-{}""#,
        f.run, f.drafts[1]
    )));
}

#[tokio::test]
async fn the_chat_card_is_for_the_run_whose_drafts_the_page_shows() {
    let f = fixture(false).await;
    let newer = {
        let mut store = f.dashboard.app.store();
        let run = store
            .queue_review(&ReviewRequest {
                key: key(7),
                profile: "default".into(),
                head_sha: "newer".into(),
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
                    summary: "Fine now.".into(),
                    summary_note: None,
                    verdict: Verdict::Comment,
                    comments: vec![],
                    session_id: Some("sess-newer".into()),
                    transcript_path: "t".into(),
                },
            )
            .unwrap();
        run
    };
    let latest = f.get("/pr/org/repo/7").await.body;
    assert!(latest.contains(&format!("chat {newer} ")), "{latest}");
    let older = f.get(&format!("/pr/org/repo/7?run={}", f.run)).await.body;
    assert!(older.contains(&format!("chat {} ", f.run)), "{older}");
}

#[tokio::test]
async fn agent_asks_for_an_instruction_and_then_asks_serve_to_revise() {
    let f = fixture(false).await;
    let uri = format!("/pr/org/repo/7/runs/{}/regenerate", f.run);
    // The PR page offers it for a finished run with a session only.
    assert!(f.get("/pr/org/repo/7").await.body.contains(&uri));
    assert!(!f.get("/pr/org/repo/8").await.body.contains("/regenerate"));

    let ask = f.get(&uri).await;
    assert_eq!(ask.status, StatusCode::OK);
    insta::assert_snapshot!("agent_card", readable(&f, card_of(&ask.body)));

    // A post without the token starts nothing.
    let reply = f
        .send(
            form(&uri)
                .body(encode(&[("instruction", "be terser")]))
                .unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    // Nor does an empty instruction.
    let reply = f.post(&uri, &[("instruction", " \r\n")]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(f.serve.revised.lock().unwrap().is_empty());

    // serve's refusal is shown.
    let reply = f.post(&uri, &[("instruction", "be\r\nterser")]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(
        reply
            .body
            .contains("Not started: the run has no agent session to resume."),
        "{}",
        reply.body
    );
    *f.serve.revision.lock().unwrap() = Some(42);
    let reply = f.post(&uri, &[("instruction", "be terser")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers[header::LOCATION], "/pr/org/repo/7?run=42");
    assert_eq!(
        *f.serve.revised.lock().unwrap(),
        [
            (f.run, None, "be\nterser".to_owned()),
            (f.run, None, "be terser".to_owned())
        ]
    );
    // Only runs of that PR.
    let other = format!("/pr/org/repo/8/runs/{}/regenerate", f.run);
    assert_eq!(f.get(&other).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn revise_asks_serve_to_regenerate_just_that_draft() {
    let f = fixture(false).await;
    let draft = f.drafts[1];
    let uri = format!("/drafts/{draft}/revise");
    // Each draft of a run with a session offers it.
    let page = f.get("/pr/org/repo/7").await.body;
    for id in f.drafts {
        assert!(
            card_of_draft(&page, id).contains(&format!("/drafts/{id}/revise")),
            "{id}"
        );
    }
    // Not without the token, nor with an empty note.
    let reply = f
        .send(
            form(&uri)
                .body(encode(&[("instruction", "reword")]))
                .unwrap(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = f.post(&uri, &[("instruction", " ")]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(f.serve.revised.lock().unwrap().is_empty());
    // serve's refusal is shown.
    let reply = f.post(&uri, &[("instruction", "reword")]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(
        reply
            .body
            .contains("wasn&#39;t revised: the run has no agent session to resume.")
            || reply
                .body
                .contains("wasn't revised: the run has no agent session to resume."),
        "{}",
        reply.body
    );
    // Started, it's back to the draft, on the run it's of.
    *f.serve.revision.lock().unwrap() = Some(42);
    let reply = f
        .post(&uri, &[("instruction", "not true because\r\nit's checked")])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.headers[header::LOCATION],
        format!("/pr/org/repo/7?run={}#draft-{draft}", f.run).as_str()
    );
    assert_eq!(
        f.serve.revised.lock().unwrap()[1],
        (
            f.run,
            Some(draft),
            "not true because\nit's checked".to_owned()
        )
    );
    assert_eq!(
        f.post("/drafts/999/revise", &[("instruction", "x")])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_draft_being_revised_says_so_and_a_dropped_one_can_be_restored() {
    let f = fixture(false).await;
    let draft = f.drafts[1];
    let run = {
        let mut store = f.dashboard.app.store();
        let sanic_store::Regeneration::Queued(run) = store
            .queue_draft_regeneration(f.run, draft, "not true", |_| false)
            .unwrap()
        else {
            panic!("refused");
        };
        run
    };
    // While it's queued, its card says so and offers no second revision.
    let page = f.get("/pr/org/repo/7").await.body;
    let card = card_of_draft(&page, draft);
    assert!(card.contains("The agent is revising this draft"), "{card}");
    assert!(card.contains(&format!("?run={}", run.id)), "{card}");
    assert!(!card.contains("/revise"), "{card}");
    assert!(card_of_draft(&page, f.drafts[0]).contains("/revise"));

    // It drops it: the new run has it rejected, with why.
    {
        let mut store = f.dashboard.app.store();
        store.claim_run(run.id).unwrap();
        store
            .finish_draft_revision(
                run.id,
                run.revision.as_ref().unwrap(),
                &sanic_core::run::DraftRevision::Dropped {
                    reason: "DROPPED: `caller` checks it. <img src=x onerror=alert(1)> \
                             [x](javascript:alert(1)) <script>alert(1)</script>"
                        .into(),
                },
                Some("sess-8"),
                "t",
            )
            .unwrap();
    }
    let page = f.get("/pr/org/repo/7").await.body;
    assert!(page.contains("revises <a"), "{page}");
    let dropped = f.dashboard.app.store().draft_rows(run.id).unwrap()[1].id;
    let card = card_of_draft(&page, dropped);
    assert!(card.contains("dropped by the agent"), "{card}");
    // Why, rendered as Markdown and sanitised.
    let why = &card[card.find(r#"<div class="why">"#).unwrap()..];
    let why = &why[..why.find("</div>").unwrap()];
    assert!(
        why.contains("DROPPED: <code>caller</code> checks it."),
        "{why}"
    );
    for bad in ["<img", "onerror", "javascript:", "<script"] {
        assert!(!why.contains(bad), "{bad}: {why}");
    }
    assert!(why.contains("&lt;script&gt;"), "{why}");
    assert!(card.contains("Restore"), "{card}");
    // The run it was revised from links the result.
    let before = f.get(&format!("/pr/org/repo/7?run={}", f.run)).await.body;
    assert!(
        card_of_draft(&before, draft).contains("Revised with the agent in"),
        "{before}"
    );

    let reply = f
        .post(
            &format!("/drafts/{dropped}/status"),
            &[("status", "pending")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let page = f.get("/pr/org/repo/7").await.body;
    let card = card_of_draft(&page, dropped);
    assert!(!card.contains("DROPPED"), "{card}");
    assert!(card.contains("Why `m`?"), "{card}");
}

#[tokio::test]
async fn the_run_list_says_what_each_revision_revises_and_how() {
    let f = fixture(false).await;
    let regeneration = {
        let mut store = f.dashboard.app.store();
        match store
            .queue_regeneration(f.run, "Drop the nits.\nAnd more.", |_| false)
            .unwrap()
        {
            sanic_store::Regeneration::Queued(run) => run,
            sanic_store::Regeneration::Refused(why) => panic!("{why}"),
        }
    };
    let revision = regeneration.id;
    let page = f.get(&format!("/pr/org/repo/7?run={}", f.run)).await.body;
    // Until it finishes, the page shows the run it revises.
    assert!(page.contains("Run 1 of 2"), "{page}");
    let runs = &page[page.find("<ol").unwrap()..page.find("</ol>").unwrap()];
    // Numbered as the summary numbers them, linking by id.
    assert!(runs.contains(&format!(
        r#"<a href="/pr/org/repo/7?run={revision}">run 2</a>"#
    )));
    insta::assert_snapshot!("run_list", readable(&f, runs));
    // Where Revise lands, before the new run has drafts or a diff.
    let queued = f.get(&format!("/pr/org/repo/7?run={revision}")).await.body;
    assert!(queued.contains("This run is queued"), "{queued}");
    assert!(!queued.contains("diff is gone"), "{queued}");

    // Its drafts say which of the first run's they revise.
    {
        let mut store = f.dashboard.app.store();
        store.claim_run(revision).unwrap();
        let result = ReviewResult {
            summary: "Fine.".into(),
            summary_note: None,
            verdict: Verdict::Comment,
            comments: vec![comment("src/lib.rs", 3, Side::Right, "Why?", false)],
            session_id: Some("sess-7".into()),
            transcript_path: "t".into(),
        };
        let basis = Basis {
            summary: None,
            comments: vec![Some(f.drafts[1])],
        };
        store
            .finish_revision(
                revision,
                &result,
                regeneration.revision.as_ref().unwrap(),
                &basis,
            )
            .unwrap();
    }
    let page = f.get("/pr/org/repo/7").await.body;
    assert!(page.contains("Run 2 of 2"), "{page}");
    assert!(
        page.contains(&format!(
            r##"<a class="edited" href="#draft-{0}">revised from #{0}</a>"##,
            f.drafts[1]
        )),
        "{page}"
    );
}

/// `html` with each `<time>`, which says when the test ran, reduced to
/// `<time>`.
fn without_times(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<time ") {
        let end = start + rest[start..].find("</time>").unwrap() + "</time>".len();
        out.push_str(&rest[..start]);
        out.push_str("<time>");
        rest = &rest[end..];
    }
    out + rest
}

/// A review thread on PR 7 by `author`, placed on `head7` unless `place`
/// says otherwise.
fn review_thread(id: &str, path: &str, line: Option<u32>, author: &str, body: &str) -> Thread {
    Thread {
        id: id.into(),
        path: Some(path.into()),
        line,
        resolved: false,
        place: Placement {
            side: Some(Side::Right),
            head: Some("head7".into()),
            ..Placement::default()
        },
        comments: vec![Comment {
            id: format!("{id}-c1"),
            author: author.into(),
            body: body.into(),
            created_at: "2026-09-21T00:00:00Z".into(),
            url: Some(format!(
                "https://github.com/org/repo/pull/7#discussion_{id}"
            )),
            by_bot: false,
            reacted_at: None,
            reactions: vec![],
        }],
    }
}

/// PR 7 polled again with review threads around the inline draft on
/// `src/lib.rs:3`: `t-range` on 3-4 and `t-outdated`, left on 3 of the
/// reviewed head, overlap it; `t-resolved` on 3 is resolved, `t-moved` is
/// outdated from another commit, `t-other` is on another file, and `t1`,
/// on 2, is in its diff but not on its lines.
fn record_threads(f: &Fixture) {
    // The fixture's own thread, now placed.
    let mut near = review_thread("t1", "src/lib.rs", Some(2), "me", "hm");
    near.comments = fixture_prs()[0].threads[0].comments.clone();
    near.comments[0].url = Some("https://github.com/org/repo/pull/7#discussion_t1".into());
    let mut range = review_thread(
        "t-range",
        "src/lib.rs",
        Some(4),
        "bob",
        "Is `m` used anywhere?\n\nIt looks dead.",
    );
    range.place.start_line = Some(3);
    let mut resolved = review_thread("t-resolved", "src/lib.rs", Some(3), "carol", "Name it?");
    resolved.resolved = true;
    let mut outdated = review_thread("t-outdated", "src/lib.rs", None, "dave", "Why 2?");
    outdated.place.outdated = true;
    outdated.place.original_line = Some(3);
    outdated.place.original_commit = Some("head7".into());
    let mut moved = review_thread("t-moved", "src/lib.rs", None, "erin", "Old point.");
    moved.place.outdated = true;
    moved.place.original_line = Some(3);
    moved.place.original_commit = Some("head6".into());
    let other = review_thread("t-other", "src/other.rs", Some(3), "frank", "Elsewhere.");
    let snap = PrSnapshot {
        threads: vec![near, range, resolved, outdated, moved, other],
        ..snapshot(7, "alice", "Add the thing")
    };
    f.dashboard
        .app
        .store()
        .record(&snap, "me", "default", &[])
        .unwrap();
}

#[tokio::test]
async fn existing_threads_are_summed_up_and_shown_beside_the_drafts_they_overlap() {
    let f = fixture(false).await;
    record_threads(&f);
    let page = f.get("/pr/org/repo/7").await;
    assert_eq!(page.status, StatusCode::OK);
    let body = &page.body;
    let summary =
        &body[body.find(r#"id="existing""#).unwrap()..body.find(r#"id="drafts""#).unwrap()];
    assert!(
        summary.contains("<b>6</b> existing review threads · <b class=\"hot\">2</b> overlap your drafts · 1 resolved"),
        "{summary}"
    );
    // The rest, folded: every thread but the two overlapping ones.
    let rest = &summary[summary.find("<details").unwrap()..];
    assert!(rest.contains("4 threads don't overlap a draft"), "{rest}");
    for id in ["t1", "t-resolved", "t-moved", "t-other"] {
        assert!(
            rest.contains(&format!("#discussion_{id}\"")),
            "{id}: {rest}"
        );
    }
    for id in ["t-range", "t-outdated"] {
        assert!(
            !rest.contains(&format!("#discussion_{id}\"")),
            "{id}: {rest}"
        );
    }
    insta::assert_snapshot!(readable(&f, card_of_draft(body, f.drafts[1])));
}

#[tokio::test]
async fn drafts_and_threads_show_their_markdown_rendered_and_sanitised() {
    let f = fixture(false).await;
    // A thread on the inline draft's line, by someone else.
    let thread = review_thread(
        "t-md",
        "src/lib.rs",
        Some(3),
        "mallory",
        "**Careful** <img src=x onerror=alert(1)> [x](javascript:alert(1))\n\n\
         ```suggestion\n    let m = n;\n```",
    );
    let snap = PrSnapshot {
        threads: vec![thread],
        ..snapshot(7, "alice", "Add the thing")
    };
    f.dashboard
        .app
        .store()
        .record(&snap, "me", "default", &[])
        .unwrap();
    let edit = format!("/drafts/{}/edit", f.drafts[1]);
    let body = "Rename it:\n\n```suggestion\n    let total = n + 1;\n```\n\n\
                - [x] checked\n\n![graph](https://img.example.com/g.png)</div>";
    assert_eq!(
        f.post(&edit, &[("body", body)]).await.status,
        StatusCode::SEE_OTHER
    );
    let page = f.get("/pr/org/repo/7").await.body;
    let card = card_of_draft(&page, f.drafts[1]);
    // The diff's excerpt of the thread is its text, in a title.
    for bad in ["<img", "href=\"javascript", " src=\""] {
        assert!(!card.contains(bad), "{bad}: {card}");
    }
    // The box to edit it in has the text as it is.
    assert!(card.contains("let total = n + 1;\n```"), "{card}");
    insta::assert_snapshot!(readable(&f, card));
}

/// Draft `id`'s card on a PR page.
fn card_of_draft(html: &str, id: i64) -> &str {
    let start = html.find(&format!(r#"id="draft-{id}""#)).unwrap();
    let start = html[..start].rfind("<article").unwrap();
    let end = start + html[start..].find("</article>").unwrap() + "</article>".len();
    &html[start..end]
}

/// Posts draft `i` in `thread`, as the page's thread form does: `react`
/// on `comment`, or `reply`.
async fn choose(f: &Fixture, i: usize, choice: &str, thread: &str, comment: &str) -> Reply {
    let uri = format!("/drafts/{}/thread", f.drafts[i]);
    f.post(
        &uri,
        &[("choice", choice), ("thread", thread), ("comment", comment)],
    )
    .await
}

fn accept(f: &Fixture, i: usize) -> impl Future<Output = Reply> + '_ {
    let uri = format!("/drafts/{}/status", f.drafts[i]);
    async move { f.post(&uri, &[("status", "accepted")]).await }
}

/// A GraphQL mutation named `name` with `variables`, answered `data`.
fn mutation(name: &str, variables: &serde_json::Value) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(name))
        .and(body_partial_json(json!({ "variables": variables })))
}

fn data(data: &serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
}

fn reacted() -> ResponseTemplate {
    data(&json!({ "addReaction": { "reaction": { "content": "THUMBS_UP" } } }))
}

#[tokio::test]
async fn an_overlapping_draft_offers_a_thumbs_up_a_reply_or_posting_separately() {
    let f = fixture(false).await;
    record_threads(&f);
    let card = |page: &str| card_of_draft(page, f.drafts[1]).to_owned();
    let page = f.get("/pr/org/repo/7").await.body;
    let pending = card(&page);
    assert!(pending.contains(">Post separately<"), "{pending}");
    assert_eq!(pending.matches(r#"<form class="choose""#).count(), 2);
    assert!(pending.contains(r#"value="t-range-c1""#), "{pending}");
    // Drafts that overlap nothing are accepted as ever.
    assert!(card_of_draft(&page, f.drafts[2]).contains(">Accept<"));

    // Each choice is saved on the draft, like accepting it, and htmx gets
    // the card back.
    let reply = choose(&f, 1, "react", "t-range", "t-range-c1").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(f.status(1), "accepted");
    let page = f.get("/pr/org/repo/7").await.body;
    assert!(
        card(&page).contains("✓ 👍 on bob's comment instead"),
        "{}",
        card(&page)
    );
    assert!(
        card(&page).contains(r#"class="thread chosen""#),
        "{}",
        card(&page)
    );
    choose(&f, 1, "reply", "t-outdated", "").await;
    let page = f.get("/pr/org/repo/7").await.body;
    assert!(card(&page).contains("✓ reply in dave"), "{}", card(&page));
    accept(&f, 1).await;
    let page = f.get("/pr/org/repo/7").await.body;
    assert!(
        card(&page).contains("✓ posts separately"),
        "{}",
        card(&page)
    );
    // Posted separately, it's an inline comment as before.
    accept(&f, 0).await;
    let payload: serde_json::Value =
        serde_json::from_str(&f.previewed_payload("COMMENT").await).unwrap();
    assert_eq!(payload["review"]["comments"][0]["body"], "Why `m`?");
    assert_eq!(payload["replies"], json!([]));
    assert_eq!(payload["reactions"], json!([]));

    // Not a thread of this PR, or no comment to react to.
    for (choice, thread, comment) in [
        ("reply", "elsewhere", ""),
        ("react", "t-range", "t-other-c1"),
        ("react", "t-range", ""),
        ("vote", "t-range", ""),
    ] {
        let refused = choose(&f, 1, choice, thread, comment).await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{choice} {thread} {comment}"
        );
    }
}

#[tokio::test]
async fn a_thumbs_up_is_sent_after_the_review_in_place_of_the_draft() {
    let f = fixture(false).await;
    record_threads(&f);
    accept(&f, 0).await;
    choose(&f, 1, "react", "t-range", "t-range-c1").await;
    let preview = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(
        preview.contains("GraphQL addReaction (THUMBS_UP), unless you have, with"),
        "{preview}"
    );
    assert!(preview.contains("3 of 3"), "{preview}");
    let payload = hidden_value(&preview, "payload");

    let review = json!({
        "commit_id": "head7", "body": "Mostly fine.", "event": "COMMENT", "comments": [],
    });
    takes_review(&f, &review).await;
    not_reacted(&f, "t-range-c1").await;
    mutation("addReaction", &json!({ "subject": "t-range-c1" }))
        .respond_with(reacted())
        .expect(1)
        .mount(&f.github)
        .await;
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert!(posted.body.contains("1 👍"), "{}", posted.body);
    assert_eq!(f.status(0), "posted");
    assert_eq!(f.status(1), "posted");
}

#[tokio::test]
async fn replies_go_in_the_review_and_the_preview_shows_every_request() {
    let f = fixture(false).await;
    record_threads(&f);
    accept(&f, 0).await;
    choose(&f, 1, "reply", "t-range", "").await;
    // The store takes any thread of the PR; the page offers overlapping ones.
    choose(&f, 2, "react", "t-outdated", "t-outdated-c1").await;
    let preview = f.get(&f.preview_uri("COMMENT")).await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    let body = &preview.body;
    let cols = &body
        [body.find(r#"<div class="cols">"#).unwrap()..body.find("<form class=\"foot\"").unwrap()];
    insta::assert_snapshot!(readable(&f, cols));
    let payload = hidden_value(body, "payload");

    let pending = json!({ "commit_id": "head7", "body": "Mostly fine.", "comments": [] });
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .and(body_json(&pending))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 5, "node_id": "PRR_5",
            "html_url": "https://github.com/org/repo/pull/7#pullrequestreview-5",
        })))
        .expect(1)
        .mount(&f.github)
        .await;
    mutation(
        "addPullRequestReviewThreadReply",
        &json!({ "review": "PRR_5", "thread": "t-range", "body": "Why `m`?" }),
    )
    .respond_with(data(
        &json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "C" } } }),
    ))
    .expect(1)
    .mount(&f.github)
    .await;
    mutation(
        "submitPullRequestReview",
        &json!({ "review": "PRR_5", "event": "COMMENT", "body": "Mostly fine." }),
    )
    .respond_with(data(
        &json!({ "submitPullRequestReview": { "pullRequestReview": { "id": "PRR_5" } } }),
    ))
    .expect(1)
    .mount(&f.github)
    .await;
    not_reacted(&f, "t-outdated-c1").await;
    mutation("addReaction", &json!({ "subject": "t-outdated-c1" }))
        .respond_with(reacted())
        .expect(1)
        .mount(&f.github)
        .await;
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert!(posted.body.contains("1 reply"), "{}", posted.body);
    for i in 0..3 {
        assert_eq!(f.status(i), "posted");
    }
}

#[tokio::test]
async fn a_failed_reply_deletes_the_pending_review_and_marks_nothing() {
    let f = fixture(false).await;
    record_threads(&f);
    accept(&f, 0).await;
    choose(&f, 1, "reply", "t-range", "").await;
    let payload = f.previewed_payload("COMMENT").await;
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 5, "node_id": "PRR_5",
            "html_url": "https://github.com/org/repo/pull/7#pullrequestreview-5",
        })))
        .expect(1)
        .mount(&f.github)
        .await;
    mutation("addPullRequestReviewThreadReply", &json!({}))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null, "errors": [{ "message": "thread is locked" }],
        })))
        .expect(1)
        .mount(&f.github)
        .await;
    mutation("deletePullRequestReview", &json!({ "review": "PRR_5" }))
        .respond_with(data(
            &json!({ "deletePullRequestReview": { "clientMutationId": null } }),
        ))
        .expect(1)
        .mount(&f.github)
        .await;
    mutation("submitPullRequestReview", &json!({}))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.github)
        .await;
    let reply = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.body);
    assert!(reply.body.contains("thread is locked"), "{}", reply.body);
    assert!(
        reply.body.contains("pending review was deleted"),
        "{}",
        reply.body
    );
    assert_eq!(f.status(0), "accepted");
    assert_eq!(f.status(1), "accepted");
}

#[tokio::test]
async fn after_a_failed_thumbs_up_a_retry_sends_only_what_github_lacks() {
    let f = fixture(false).await;
    record_threads(&f);
    accept(&f, 0).await;
    choose(&f, 1, "react", "t-range", "t-range-c1").await;
    // The store takes any thread of the PR; the page offers overlapping ones.
    choose(&f, 2, "react", "t-outdated", "t-outdated-c1").await;
    let payload = f.previewed_payload("COMMENT").await;
    // One review ever, and each thumbs-up once it's taken.
    takes_review(
        &f,
        &json!({ "commit_id": "head7", "body": "Mostly fine.", "event": "COMMENT", "comments": [] }),
    )
    .await;
    not_reacted(&f, "t-range-c1").await;
    // Checked on the first try and the retry.
    not_reacted(&f, "t-outdated-c1").await;
    mutation("addReaction", &json!({ "subject": "t-range-c1" }))
        .respond_with(reacted())
        .expect(1)
        .mount(&f.github)
        .await;
    mutation("addReaction", &json!({ "subject": "t-outdated-c1" }))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .expect(1)
        .mount(&f.github)
        .await;
    let f = &f;
    let confirm = async |payload: String| {
        f.post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await
    };
    let partly = confirm(payload.clone()).await;
    assert_eq!(partly.status, StatusCode::BAD_GATEWAY, "{}", partly.body);
    assert!(partly.body.contains("Partly posted"), "{}", partly.body);
    // The way on is a Comment preview: an approval would be a second review.
    assert!(
        partly
            .body
            .contains(&f.preview_uri("COMMENT").replace('&', "&amp;")),
        "{}",
        partly.body
    );
    assert!(
        partly.body.contains("pullrequestreview-5"),
        "{}",
        partly.body
    );
    // What GitHub took is marked posted: the review and the first 👍.
    assert_eq!(f.status(0), "posted");
    assert_eq!(f.status(1), "posted");
    assert_eq!(f.status(2), "accepted");
    // What GitHub took is fetched again, though not all of it went.
    assert_eq!(*f.serve.refreshed.lock().unwrap(), [key(7)]);

    // The same confirm again sends nothing.
    let again = confirm(payload).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);

    // A new preview has only the second thumbs-up left, with no review.
    let preview = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(preview.contains("NO REVIEW"), "{preview}");
    let payload = hidden_value(&preview, "payload");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
        json!({
            "left": null, "review": null, "replies": [],
            "reactions": [{ "comment_id": "t-outdated-c1" }],
        })
    );
    mutation("addReaction", &json!({ "subject": "t-outdated-c1" }))
        .respond_with(reacted())
        .expect(1)
        .mount(&f.github)
        .await;
    let done = confirm(payload).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(f.status(2), "posted");
    // A thumbs-up alone is a post too.
    assert_eq!(f.serve.refreshed.lock().unwrap().len(), 2);

    // Requesting changes with only thumbs-ups isn't a review GitHub takes.
    let f = fixture(false).await;
    record_threads(&f);
    choose(&f, 1, "react", "t-range", "t-range-c1").await;
    let refused = f.get(&f.preview_uri("REQUEST_CHANGES")).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(
        refused.body.contains("only thumbs-ups are accepted"),
        "{}",
        refused.body
    );
}

/// GitHub taking `review`, a create-review body with its verdict, once,
/// in the one call that creates and submits it, as `PRR_5`.
async fn takes_review(f: &Fixture, review: &serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .and(body_json(review))
        .respond_with(created("PRR_5"))
        .expect(1)
        .mount(&f.github)
        .await;
}

/// GitHub's answer to creating pending review `node_id`.
fn created(node_id: &str) -> ResponseTemplate {
    let n = node_id.trim_start_matches("PRR_");
    ResponseTemplate::new(200).set_body_json(json!({
        "id": n.parse::<u64>().unwrap(),
        "node_id": node_id,
        "html_url": format!("https://github.com/org/repo/pull/7#pullrequestreview-{n}"),
    }))
}

fn submitted() -> ResponseTemplate {
    data(&json!({ "submitPullRequestReview": { "pullRequestReview": { "id": "PRR" } } }))
}

/// PR 7 with drafts `accepted`, after a submit whose one call, creating
/// and submitting the review, failed as a lost answer would.
async fn submit_failed(accepted: &[usize]) -> Fixture {
    let f = fixture(false).await;
    for &i in accepted {
        accept(&f, i).await;
    }
    let payload = f.previewed_payload("COMMENT").await;
    let review = Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .and(body_partial_json(json!({ "event": "COMMENT" })))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount_as_scoped(&f.github)
        .await;
    let failed = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(failed.status, StatusCode::BAD_GATEWAY, "{}", failed.body);
    assert!(
        failed.body.contains("looks for it first"),
        "{}",
        failed.body
    );
    assert_eq!(f.status(0), "accepted");
    drop(review);
    f
}

/// PR 7 after a submit with a reply, so through a pending review, whose
/// last step, the submit of pending review `PRR_5`, failed.
async fn pending_submit_failed() -> Fixture {
    let f = fixture(false).await;
    record_threads(&f);
    accept(&f, 0).await;
    choose(&f, 1, "reply", "t-range", "").await;
    let payload = f.previewed_payload("COMMENT").await;
    let review = Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(created("PRR_5"))
        .expect(1)
        .mount_as_scoped(&f.github)
        .await;
    let reply = replied(&f, "PRR_5").await;
    let submit = mutation("submitPullRequestReview", &json!({ "review": "PRR_5" }))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount_as_scoped(&f.github)
        .await;
    let failed = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(failed.status, StatusCode::BAD_GATEWAY, "{}", failed.body);
    assert!(failed.body.contains("checks it first"), "{}", failed.body);
    assert_eq!(f.status(0), "accepted");
    drop((review, reply, submit));
    f
}

/// GitHub taking the reply in `t-range` in pending review `review`, once.
async fn replied(f: &Fixture, review: &str) -> wiremock::MockGuard {
    mutation(
        "addPullRequestReviewThreadReply",
        &json!({ "review": review, "thread": "t-range" }),
    )
    .respond_with(data(
        &json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "C" } } }),
    ))
    .expect(1)
    .mount_as_scoped(&f.github)
    .await
}

/// Your newest reviews of PR 7, as looking for one sent without an answer
/// finds them: with `found`, the COMMENT review `submit_failed` sent, with
/// inline comments `comments`, posted after all; either way, one just like
/// it from before it was sent, and one since with another comment too.
async fn my_reviews(f: &Fixture, found: bool, comments: &[&str]) {
    let review = |at: &str, n: u32, comments: &[&str]| {
        let nodes: Vec<_> = comments
            .iter()
            .map(|body| json!({ "body": body }))
            .collect();
        json!({
            "url": format!("https://github.com/org/repo/pull/7#pullrequestreview-{n}"),
            "state": "COMMENTED", "body": "Mostly fine.", "submittedAt": at,
            "commit": { "oid": "head7" },
            "comments": { "totalCount": nodes.len(), "nodes": nodes },
        })
    };
    let mut more = comments.to_vec();
    more.push("An earlier point.");
    let mut nodes = vec![
        review("2026-09-23T15:00:00Z", 1, comments),
        review("2026-09-23T16:30:00Z", 2, &more),
    ];
    if found {
        nodes.push(review("2026-09-23T16:33:52Z", 5, comments));
    }
    mutation(
        "reviews(last:",
        &json!({ "owner": "org", "name": "repo", "number": 7, "author": "me" }),
    )
    .respond_with(data(&json!({
        "repository": { "pullRequest": { "reviews": { "nodes": nodes } } },
    })))
    .expect(1)
    .mount(&f.github)
    .await;
}

/// The review state query for `node_id`, answered `state`.
async fn review_is(f: &Fixture, node_id: &str, state: &str) {
    mutation("node(id: $review)", &json!({ "review": node_id }))
        .respond_with(data(&json!({ "node": { "state": state } })))
        .expect(1)
        .mount(&f.github)
        .await;
}

#[tokio::test]
async fn a_review_whose_answer_was_lost_is_found_and_not_posted_again() {
    let f = submit_failed(&[0]).await;
    let preview = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(
        preview.contains("An earlier submit on this PR sent a review GitHub never answered."),
        "{preview}"
    );
    assert!(
        preview.contains("GraphQL repository (your newest reviews of the PR), with"),
        "{preview}"
    );
    let payload = hidden_value(&preview, "payload");
    // Nothing is sent again: GitHub has it.
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.github)
        .await;
    my_reviews(&f, true, &[]).await;
    let found = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(found.status, StatusCode::CONFLICT, "{}", found.body);
    assert!(found.body.contains("submitted after all"), "{}", found.body);
    assert!(found.body.contains("pullrequestreview-5"), "{}", found.body);
    assert_eq!(f.status(0), "posted");
    // And it's forgotten: the next preview has nothing to post.
    assert_eq!(
        f.get(&f.preview_uri("COMMENT")).await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn a_review_github_never_got_is_sent_again_once() {
    let f = submit_failed(&[0]).await;
    let payload = f.previewed_payload("COMMENT").await;
    // Only a like review from before this one was sent, which isn't it.
    my_reviews(&f, false, &[]).await;
    takes_review(
        &f,
        &json!({ "commit_id": "head7", "body": "Mostly fine.", "event": "COMMENT", "comments": [] }),
    )
    .await;
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert_eq!(f.status(0), "posted");
    // Its answer came back, so nothing is left to look for.
    let store = f.dashboard.app.store();
    assert_eq!(store.pending_review(&key(7)).unwrap(), None);
}

#[tokio::test]
async fn a_review_left_pending_that_github_submitted_is_not_posted_again() {
    let f = pending_submit_failed().await;
    let preview = f.get(&f.preview_uri("COMMENT")).await.body;
    assert!(
        preview.contains("An earlier submit on this PR left a review pending."),
        "{preview}"
    );
    assert!(
        preview.contains("GraphQL node (the review's state), with"),
        "{preview}"
    );
    let payload = hidden_value(&preview, "payload");
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.github)
        .await;
    review_is(&f, "PRR_5", "COMMENTED").await;
    let found = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(found.status, StatusCode::CONFLICT, "{}", found.body);
    assert!(found.body.contains("submitted after all"), "{}", found.body);
    assert_eq!(f.status(0), "posted");
    assert_eq!(f.status(1), "posted");
}

#[tokio::test]
async fn a_review_left_pending_is_deleted_before_it_is_posted_afresh() {
    let f = pending_submit_failed().await;
    let payload = f.previewed_payload("COMMENT").await;
    review_is(&f, "PRR_5", "PENDING").await;
    mutation("deletePullRequestReview", &json!({ "review": "PRR_5" }))
        .respond_with(data(
            &json!({ "deletePullRequestReview": { "clientMutationId": null } }),
        ))
        .expect(1)
        .mount(&f.github)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(created("PRR_6"))
        .expect(1)
        .mount(&f.github)
        .await;
    let _reply = replied(&f, "PRR_6").await;
    mutation("submitPullRequestReview", &json!({ "review": "PRR_6" }))
        .respond_with(submitted())
        .expect(1)
        .mount(&f.github)
        .await;
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert!(
        posted.body.contains("pullrequestreview-6"),
        "{}",
        posted.body
    );
    assert_eq!(f.status(0), "posted");
    assert_eq!(f.status(1), "posted");
}

/// GitHub saying you haven't given comment `id` a thumbs-up, however often
/// it's asked.
async fn not_reacted(f: &Fixture, id: &str) {
    mutation("reactionGroups", &json!({ "subject": id }))
        .respond_with(data(&json!({ "node": { "reactionGroups": [
            { "content": "THUMBS_UP", "viewerHasReacted": false },
        ] } })))
        .mount(&f.github)
        .await;
}

#[tokio::test]
async fn a_thumbs_up_github_already_has_is_marked_posted_and_not_sent() {
    let f = fixture(false).await;
    record_threads(&f);
    choose(&f, 1, "react", "t-range", "t-range-c1").await;
    let payload = f.previewed_payload("COMMENT").await;
    // As after a 👍 whose answer was lost: GitHub has it.
    mutation("reactionGroups", &json!({ "subject": "t-range-c1" }))
        .respond_with(data(&json!({ "node": { "reactionGroups": [
            { "content": "THUMBS_UP", "viewerHasReacted": true },
        ] } })))
        .expect(1)
        .mount(&f.github)
        .await;
    mutation("addReaction", &json!({}))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.github)
        .await;
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    assert_eq!(f.status(1), "posted");
}

/// A regeneration of `f`'s run, finished: a copy of its summary and its
/// comment, still as they were decided, and the comment revised. Its
/// drafts, in that order.
fn regenerate(f: &Fixture) -> (i64, Vec<i64>) {
    let mut store = f.dashboard.app.store();
    let sanic_store::Regeneration::Queued(run) = store
        .queue_regeneration(f.run, "Say more.", |_| false)
        .unwrap()
    else {
        panic!("refused");
    };
    store.claim_run(run.id).unwrap();
    let result = ReviewResult {
        summary: "Mostly fine.".into(),
        summary_note: None,
        verdict: Verdict::Comment,
        comments: vec![
            comment("src/lib.rs", 3, Side::Right, "Why `m`?", false),
            comment("src/lib.rs", 3, Side::Right, "Why `m`, really?", false),
        ],
        session_id: Some("sess-7".into()),
        transcript_path: "t".into(),
    };
    let basis = Basis {
        summary: Some(f.drafts[0]),
        comments: vec![Some(f.drafts[1]), Some(f.drafts[1])],
    };
    let revision = run.revision.as_ref().unwrap();
    store
        .finish_revision(run.id, &result, revision, &basis)
        .unwrap();
    let ids = store
        .draft_rows(run.id)
        .unwrap()
        .iter()
        .map(|d| d.id)
        .collect();
    (run.id, ids)
}

/// A regeneration of the fixture's run whose summary and inline comment
/// carry the agent's private notes. Its drafts, summary first.
fn noted_regeneration(f: &Fixture) -> (i64, Vec<i64>) {
    let mut store = f.dashboard.app.store();
    let sanic_store::Regeneration::Queued(run) = store
        .queue_regeneration(f.run, "Say why.", |_| false)
        .unwrap()
    else {
        panic!("refused");
    };
    store.claim_run(run.id).unwrap();
    let mut noted = comment("src/lib.rs", 3, Side::Right, "Why `m`?", false);
    // Markdown, and what the agent may have copied from a hostile PR.
    noted.comment.note = Some(
        "NOTE-C: checked **every** caller; couldn't run the tests.\n\n\
         <img src=x onerror=alert(1)> [x](javascript:alert(1)) <script>alert(1)</script>"
            .into(),
    );
    let result = ReviewResult {
        summary: "Mostly fine.".into(),
        summary_note: Some("NOTE-S: low confidence overall.".into()),
        verdict: Verdict::Comment,
        comments: vec![noted],
        session_id: Some("sess-7".into()),
        transcript_path: "t".into(),
    };
    let basis = Basis {
        summary: None,
        comments: vec![None],
    };
    store
        .finish_revision(run.id, &result, run.revision.as_ref().unwrap(), &basis)
        .unwrap();
    let run_dir = f.data.path().join("runs").join(run.id.to_string());
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("pr.diff"), DIFF).unwrap();
    let ids = store
        .draft_rows(run.id)
        .unwrap()
        .iter()
        .map(|d| d.id)
        .collect();
    (run.id, ids)
}

#[tokio::test]
async fn a_drafts_private_note_is_shown_on_its_card_in_both_views() {
    let f = fixture(false).await;
    let (_, ids) = noted_regeneration(&f);
    for uri in ["/pr/org/repo/7", "/pr/org/repo/7?view=files"] {
        let page = f.get(uri).await.body;
        let summary = card_of_draft(&page, ids[0]);
        assert!(summary.contains("Reviewer note"), "{uri}: {summary}");
        assert!(summary.contains("(not posted)"), "{uri}: {summary}");
        assert!(
            summary.contains("NOTE-S: low confidence overall."),
            "{uri}: {summary}"
        );
        let comment = card_of_draft(&page, ids[1]);
        let note = &comment[comment.find(r#"<aside class="pnote">"#).unwrap()..];
        let note = &note[..note.find("</aside>").unwrap()];
        // Rendered as the draft is, and sanitised the same.
        assert!(
            note.contains("NOTE-C: checked <strong>every</strong> caller; couldn't run the tests."),
            "{uri}: {note}"
        );
        for bad in ["<img", "onerror", "javascript:", "<script"] {
            assert!(!note.contains(bad), "{uri}: {bad}: {note}");
        }
        assert!(note.contains("&lt;script&gt;"), "{uri}: {note}");
        // Set apart from the text that's posted.
        let body = &comment[comment.find(r#"class="body""#).unwrap()..];
        let body = &body[..body.find("</div>").unwrap()];
        assert!(!body.contains("NOTE-C"), "{uri}: {comment}");
    }
}

#[tokio::test]
async fn a_private_note_is_never_in_what_is_posted() {
    let f = fixture(false).await;
    let (run, ids) = noted_regeneration(&f);
    for id in &ids {
        let reply = f
            .post(&format!("/drafts/{id}/status"), &[("status", "accepted")])
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
    }
    let preview = f
        .get(&format!("/pr/org/repo/7/runs/{run}/preview?event=COMMENT"))
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    // Not in the review as it'll read, nor any request's body.
    assert!(!preview.body.contains("NOTE-"), "{}", preview.body);
    let payload = hidden_value(&preview.body, "payload");
    assert!(payload.contains("Why `m`?"), "{payload}");
    assert!(!payload.contains("NOTE-"), "{payload}");

    let expected = json!({
        "commit_id": "head7",
        "body": "Mostly fine.",
        "event": "COMMENT",
        "comments": [
            { "path": "src/lib.rs", "body": "Why `m`?", "line": 3, "side": "RIGHT" },
        ],
    });
    // The submit sends exactly this, with no note in it.
    takes_review(&f, &expected).await;
    let posted = f
        .post(
            &format!("/pr/org/repo/7/runs/{run}/submit"),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    for request in f.github.received_requests().await.unwrap() {
        let body = String::from_utf8(request.body).unwrap();
        assert!(!body.contains("NOTE-"), "{body}");
    }
}

#[tokio::test]
async fn a_regeneration_after_a_review_github_took_anyway_does_not_post_it_again() {
    let f = submit_failed(&[0, 1]).await;
    // Revising the run copies its accepted drafts, still accepted.
    let (regeneration, ids) = regenerate(&f);
    let status = |id: i64| {
        let store = f.dashboard.app.store();
        store.draft_row(id).unwrap().unwrap().status
    };
    assert_eq!(status(ids[0]), "accepted");
    assert_eq!(status(ids[1]), "accepted");
    assert_eq!(status(ids[2]), "pending");

    // Its preview checks the review the first run left, first.
    let uri = format!("/pr/org/repo/7/runs/{regeneration}/preview?event=COMMENT");
    let preview = f.get(&uri).await.body;
    assert!(
        preview.contains("An earlier submit on this PR sent a review GitHub never answered."),
        "{preview}"
    );
    let payload = hidden_value(&preview, "payload");
    my_reviews(&f, true, &["Why `m`?"]).await;
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&f.github)
        .await;
    let found = f
        .post(
            &format!("/pr/org/repo/7/runs/{regeneration}/submit"),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(found.status, StatusCode::CONFLICT, "{}", found.body);
    assert!(found.body.contains("submitted after all"), "{}", found.body);
    // The first run's drafts, and the copies of them, are posted; the one
    // revised from a posted draft is left, named, for you to check.
    assert_eq!(f.status(0), "posted");
    assert_eq!(f.status(1), "posted");
    assert_eq!(status(ids[0]), "posted");
    assert_eq!(status(ids[1]), "posted");
    assert_eq!(status(ids[2]), "pending");
    assert!(
        found.body.contains(&format!(
            "draft {}</a> are revised from drafts it posted",
            ids[2]
        )),
        "{}",
        found.body
    );
}

#[tokio::test]
async fn a_review_github_took_anyway_marks_its_copies_in_a_regeneration_posted() {
    let f = submit_failed(&[0, 1]).await;
    let (_, ids) = regenerate(&f);
    // The run the review is from is submitted again, not the regeneration.
    let preview = f.get(&f.preview_uri("COMMENT")).await.body;
    let payload = hidden_value(&preview, "payload");
    my_reviews(&f, true, &["Why `m`?"]).await;
    let found = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(found.status, StatusCode::CONFLICT, "{}", found.body);
    let status = |id: i64| {
        let store = f.dashboard.app.store();
        store.draft_row(id).unwrap().unwrap().status
    };
    // Submitting the regeneration can't post them again.
    assert_eq!(status(ids[0]), "posted");
    assert_eq!(status(ids[1]), "posted");
    assert_eq!(status(ids[2]), "pending");
}

#[tokio::test]
async fn a_posted_review_lists_the_copies_it_marks_posted_in_other_runs() {
    let f = fixture(false).await;
    accept(&f, 0).await;
    accept(&f, 1).await;
    let payload = f.previewed_payload("COMMENT").await;
    // Regenerated before the post: its copies are accepted.
    let (regeneration, ids) = regenerate(&f);
    takes_review(
        &f,
        &json!({
            "commit_id": "head7", "body": "Mostly fine.", "event": "COMMENT",
            "comments": [{ "path": "src/lib.rs", "body": "Why `m`?", "line": 3, "side": "RIGHT" }],
        }),
    )
    .await;
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    let card = card_of(&posted.body);
    for id in &ids[..2] {
        assert!(
            card.contains(&format!(
                r#"<a href="/pr/org/repo/7?run={regeneration}#draft-{id}">draft {id}</a>"#
            )),
            "{card}"
        );
    }
    assert!(card.contains("are word for word what was posted"), "{card}");
    // The revised one isn't a copy, so it isn't listed.
    assert!(!card.contains(&format!("draft {}<", ids[2])), "{card}");
    let status = |id: i64| {
        let store = f.dashboard.app.store();
        store.draft_row(id).unwrap().unwrap().status
    };
    assert_eq!(status(ids[0]), "posted");
    assert_eq!(status(ids[1]), "posted");
    assert_ne!(status(ids[2]), "posted");
}

#[tokio::test]
async fn once_the_pr_moves_on_a_drafts_lines_link_to_the_file_at_the_reviewed_head() {
    let f = fixture(false).await;
    let moved = PrSnapshot {
        head_sha: "head8".into(),
        ..snapshot(7, "alice", "Add the thing")
    };
    f.dashboard
        .app
        .store()
        .record(&moved, "me", "default", &[])
        .unwrap();
    let page = f.get("/pr/org/repo/7").await;
    let card = card_of_draft(&page.body, f.drafts[1]);
    assert!(
        card.contains(
            r#"<a class="anc" href="https://github.com/org/repo/blob/head7/src/lib.rs?plain=1#L3""#
        ),
        "{card}"
    );
}

/// The PR page's files view, from its tabs to the end of the diff.
fn files_view(html: &str) -> &str {
    let start = html.find(r#"<nav class="views""#).unwrap();
    let end = start + html[start..].find("</section>").unwrap() + "</section>".len();
    &html[start..end]
}

#[tokio::test]
async fn the_files_view_shows_the_diff_with_drafts_and_threads_at_their_lines() {
    let f = fixture(false).await;
    record_threads(&f);
    let page = f.get("/pr/org/repo/7?view=files").await;
    assert_eq!(page.status, StatusCode::OK);
    let view = files_view(&page.body);
    // The summary and the draft that isn't on a line of the diff are over
    // it; the inline draft is under its line, 3.
    let top = &view[view.find("fv-top").unwrap()..view.find("fv-files").unwrap()];
    for (i, shown) in [(0, true), (1, false), (2, true)] {
        assert_eq!(
            top.contains(&format!("id=\"draft-{}\"", f.drafts[i])),
            shown,
            "{i}: {top}"
        );
    }
    insta::assert_snapshot!(readable(&f, view));
}

#[tokio::test]
async fn the_pr_page_links_each_view_keeping_the_run() {
    let f = fixture(false).await;
    let page = f.get("/pr/org/repo/7").await;
    let tabs = &page.body[page.body.find(r#"<nav class="views""#).unwrap()..];
    let tabs = &tabs[..tabs.find("</nav>").unwrap()];
    assert!(
        tabs.contains(r#"<a class="cur" href="/pr/org/repo/7?view=drafts" data-view="drafts">"#),
        "{tabs}"
    );
    assert!(
        tabs.contains(r#"href="/pr/org/repo/7?view=files" data-view="files""#),
        "{tabs}"
    );
    assert!(page.body.contains(r#"<section id="drafts">"#));

    let page = f
        .get(&format!("/pr/org/repo/7?run={}&view=files", f.run))
        .await;
    let tabs = &page.body[page.body.find(r#"<nav class="views""#).unwrap()..];
    let tabs = &tabs[..tabs.find("</nav>").unwrap()];
    assert!(
        tabs.contains(&format!(
            r#"<a class="cur" href="/pr/org/repo/7?run={}&amp;view=files" data-view="files">"#,
            f.run
        )),
        "{tabs}"
    );
    assert!(page.body.contains(r#"<section class="files" id="drafts""#));
    // Anything else is the drafts.
    let page = f.get("/pr/org/repo/7?view=nonsense").await;
    assert!(page.body.contains(r#"<section id="drafts">"#));
}

#[tokio::test]
async fn a_files_diff_loads_on_its_own_for_the_view_to_fetch() {
    let f = fixture(false).await;
    let uri = |path: &str| format!("/pr/org/repo/7/runs/{}/file?path={path}", f.run);
    let file = f.get(&uri("src/lib.rs")).await;
    assert_eq!(file.status, StatusCode::OK);
    assert!(
        file.body.starts_with("<table class=\"d\">"),
        "{}",
        file.body
    );
    assert!(file.body.contains(&format!("id=\"draft-{}\"", f.drafts[1])));
    assert_eq!(
        f.get(&uri("elsewhere.rs")).await.status,
        StatusCode::NOT_FOUND
    );
    let other = f.get("/pr/org/repo/7/runs/999/file?path=src/lib.rs").await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_big_file_with_nothing_on_it_loads_only_when_asked() {
    let f = fixture(false).await;
    let big = (1..=500).fold(String::new(), |big, n| big + &format!("+line {n}\n"));
    let diff = format!(
        "{DIFF}diff --git a/big.txt b/big.txt\nnew file mode 100644\n--- /dev/null\n+++ b/big.txt\n@@ -0,0 +1,500 @@\n{big}"
    );
    let run_dir = f.data.path().join("runs").join(f.run.to_string());
    std::fs::write(run_dir.join("pr.diff"), diff).unwrap();
    let page = f.get("/pr/org/repo/7?view=files").await;
    let view = files_view(&page.body);
    assert!(
        view.contains(&format!(
            r#"hx-get="/pr/org/repo/7/runs/{}/file?path=big.txt&amp;layout=unified""#,
            f.run
        )),
        "{view}"
    );
    assert!(!view.contains("line 250"), "{view}");
    // The file with a draft on it shows as ever.
    assert!(view.contains(&format!("id=\"draft-{}\"", f.drafts[1])));
    let file = f
        .get(&format!("/pr/org/repo/7/runs/{}/file?path=big.txt", f.run))
        .await;
    assert!(file.body.contains("line 250"), "{}", file.body);
}

#[tokio::test]
async fn the_split_layout_puts_the_old_lines_beside_the_new() {
    let f = fixture(false).await;
    record_threads(&f);
    let page = f.get("/pr/org/repo/7?view=files&layout=split").await;
    let body = &page.body;
    let table = &body[body.find("<table class=\"d split\">").unwrap()..];
    // Its end: the draft's own excerpt is a table too, but not the last
    // thing in its box.
    let table = &table[..table.find("</table></div>").unwrap() + "</table>".len()];
    insta::assert_snapshot!(readable(&f, table));
    // Its links keep the layout, and offer the other.
    let head = &body[body.find(r#"id="layouts""#).unwrap()..];
    let head = &head[..head.find("</span>").unwrap()];
    assert!(
        head.contains(
            r#"<a href="/pr/org/repo/7?view=files&amp;layout=unified" data-layout="unified">"#
        ),
        "{head}"
    );
    assert!(body.contains(r#"href="/pr/org/repo/7?view=drafts" data-view="drafts""#));
    // A file loaded on its own is laid out the same.
    let file = f
        .get(&format!(
            "/pr/org/repo/7/runs/{}/file?path=src/lib.rs&layout=split",
            f.run
        ))
        .await;
    assert!(
        file.body.starts_with("<table class=\"d split\">"),
        "{}",
        file.body
    );
}

#[tokio::test]
async fn the_file_list_marks_files_with_drafts_and_threads() {
    let f = fixture(false).await;
    record_threads(&f);
    let page = f.get("/pr/org/repo/7?view=files").await;
    let body = &page.body;
    let tree = &body[body.find(r#"<details class="tree" open>"#).unwrap()..];
    let tree = &tree[..tree.find("</details>").unwrap() + "</details>".len()];
    insta::assert_snapshot!(readable(&f, tree));
}

/// `n` lines of a file, `line 1` to `line n`.
fn numbered(n: u32) -> String {
    (1..=n).fold(String::new(), |text, i| text + &format!("line {i}\n"))
}

/// The expander buttons in `html`, by their `hx-get`, unescaped.
fn expanders(html: &str) -> Vec<String> {
    html.split(r#"<button class="x" type="button" hx-get=""#)
        .skip(1)
        .map(|rest| rest[..rest.find('"').unwrap()].replace("&amp;", "&"))
        .collect()
}

#[tokio::test]
async fn unchanged_lines_expand_from_the_mirror_only_when_it_has_the_head() {
    let f = fixture(false).await;
    let page = f.get("/pr/org/repo/7?view=files").await;
    assert!(expanders(&page.body).is_empty(), "{}", page.body);

    // The hunk is lines 1 to 5 of 35, with all of git's context after its
    // change, so there may be more; old line n is new line n+1 after it,
    // since it adds a line.
    let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@
 line 1
+line 2
 line 3
 line 4
 line 5
";
    let run_dir = f.data.path().join("runs").join(f.run.to_string());
    std::fs::write(run_dir.join("pr.diff"), diff).unwrap();
    let page = f.get("/pr/org/repo/7?view=files").await;
    assert!(expanders(&page.body).is_empty(), "{}", page.body);
    f.mirror.add("head7", "src/lib.rs", &numbered(35));
    let page = f.get("/pr/org/repo/7?view=files").await;
    let run = f.run;
    let down = format!(
        "/pr/org/repo/7/runs/{run}/context?path=src%2Flib.rs&start=6&dir=down&layout=unified"
    );
    assert_eq!(expanders(&page.body), [down.as_str()]);
    let rows = f.get(&down).await;
    assert_eq!(rows.status, StatusCode::OK);
    insta::assert_snapshot!(readable(&f, &rows.body));
    // The rest, to the end of the file, and then nothing more.
    let rest = format!(
        "/pr/org/repo/7/runs/{run}/context?path=src%2Flib.rs&start=26&dir=down&layout=unified"
    );
    assert_eq!(expanders(&rows.body), [rest.as_str()]);
    let rows = f.get(&rest).await;
    assert!(
        rows.body.contains(r#"<td class="n">35</td>"#),
        "{}",
        rows.body
    );
    assert!(expanders(&rows.body).is_empty(), "{}", rows.body);
}

/// A run's diff of `two.txt`, 32 lines long: a hunk at each end, with
/// lines 4 to 29 left out between them.
const TWO_HUNKS: &str = "\
diff --git a/two.txt b/two.txt
--- a/two.txt
+++ b/two.txt
@@ -1,3 +1,3 @@
 line 1
-old 2
+line 2
 line 3
@@ -30,3 +30,3 @@
 line 30
-old 31
+line 31
 line 32
";

#[tokio::test]
async fn a_gap_between_hunks_expands_up_down_or_all_at_once() {
    let f = fixture(false).await;
    let run_dir = f.data.path().join("runs").join(f.run.to_string());
    std::fs::write(run_dir.join("pr.diff"), TWO_HUNKS).unwrap();
    f.mirror.add("head7", "two.txt", &numbered(32));
    let page = f.get("/pr/org/repo/7?view=files&layout=split").await;
    let base = format!("/pr/org/repo/7/runs/{}/context?path=two.txt", f.run);
    // Lines 4 to 29 are left out, more than one click's worth. The last
    // hunk has less context after its change than git gives, so the file
    // ends there.
    assert_eq!(
        expanders(&page.body),
        [
            format!("{base}&start=4&dir=down&layout=split&end=29"),
            format!("{base}&start=4&dir=up&layout=split&end=29"),
        ]
    );
    // Up from the hunk below, whose header moves over what's left.
    let up = f
        .get(&format!("{base}&start=4&dir=up&layout=split&end=29"))
        .await;
    let body = &up.body;
    assert!(body.starts_with(r#"<tr class="hh">"#), "{body}");
    assert!(body.contains("@@ -30,3 +30,3 @@"), "{body}");
    assert!(
        body.contains("line 10") && body.contains("line 29"),
        "{body}"
    );
    assert!(!body.contains("line 9<"), "{body}");
    // What's left fits in one click.
    assert_eq!(
        expanders(body),
        [format!("{base}&start=4&dir=all&layout=split&end=9")]
    );
    let all = f
        .get(&format!("{base}&start=4&dir=all&layout=split&end=9"))
        .await;
    assert!(all.body.contains("line 4") && all.body.contains("line 9"));
    assert!(expanders(&all.body).is_empty(), "{}", all.body);
    // Lines in the diff aren't a gap.
    let inside = f
        .get(&format!("{base}&start=2&dir=all&layout=split&end=3"))
        .await;
    assert_eq!(inside.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_expander_keeps_its_hunk_header_when_the_mirror_loses_the_file() {
    let f = fixture(false).await;
    let run_dir = f.data.path().join("runs").join(f.run.to_string());
    std::fs::write(run_dir.join("pr.diff"), TWO_HUNKS).unwrap();
    // The mirror has the head, but not the file there.
    f.mirror.add("head7", "elsewhere.txt", "");
    let up = f
        .get(&format!(
            "/pr/org/repo/7/runs/{}/context?path=two.txt&start=4&dir=up&layout=unified&end=29",
            f.run
        ))
        .await;
    assert_eq!(up.status, StatusCode::OK);
    let body = &up.body;
    assert!(body.contains("@@ -30,3 +30,3 @@"), "{body}");
    assert!(body.contains("no longer has this file"), "{body}");
    assert!(expanders(body).is_empty(), "{body}");
}

/// The PR page's summary of its review threads.
fn threads_summary(body: &str) -> &str {
    &body[body.find(r#"id="existing""#).unwrap()..body.find(r#"id="drafts""#).unwrap()]
}

/// PR 7 polled again with `threads`, beside the fixture's `t1`, which
/// is on no draft's lines.
fn record_pr7_threads(f: &Fixture, threads: Vec<Thread>) {
    let snap = PrSnapshot {
        threads,
        ..snapshot(7, "alice", "Add the thing")
    };
    f.dashboard
        .app
        .store()
        .record(&snap, "me", "default", &[])
        .unwrap();
}

#[tokio::test]
async fn a_posted_comments_thread_is_its_draft_and_not_an_existing_thread() {
    let f = fixture(false).await;
    accept(&f, 0).await;
    accept(&f, 1).await;
    let payload = f.previewed_payload("COMMENT").await;
    Mock::given(method("POST"))
        .and(path("/repos/org/repo/pulls/7/reviews"))
        .respond_with(created("PRR_5"))
        .expect(1)
        .mount(&f.github)
        .await;
    mutation(
        "PullRequestReview { comments",
        &json!({ "review": "PRR_5" }),
    )
    .respond_with(data(&json!({ "node": { "comments": { "nodes": [
            { "id": "PRRC_9", "path": "src/lib.rs", "body": "Why `m`?", "replyTo": null },
        ] } } })))
    .expect(1)
    .mount(&f.github)
    .await;
    let posted = f
        .post(
            &f.submit_uri(),
            &[("event", "COMMENT"), ("payload", &payload)],
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.body);
    let recorded = f.dashboard.app.store().posted_drafts(&key(7)).unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        (recorded[0].id, recorded[0].comment.as_deref()),
        (f.drafts[1], Some("PRRC_9"))
    );

    // The next fetch has its thread, answered, and someone's on its lines.
    let mut mine = review_thread("t-mine", "src/lib.rs", Some(3), "me", "Why `m`?");
    mine.comments[0].id = "PRRC_9".into();
    let reply = review_thread("t-x", "src/lib.rs", Some(3), "alice", "It's used below.");
    mine.comments.extend(reply.comments);
    let bob = review_thread("t-bob", "src/lib.rs", Some(3), "bob", "Is `m` used?");
    record_pr7_threads(&f, vec![mine, bob]);
    let page = f.get("/pr/org/repo/7").await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains(
            "<b>2</b> existing review threads · <b>0</b> overlap your drafts · <b>1</b> posted from here"
        ),
        "{summary}"
    );
    assert!(!summary.contains("#discussion_t-mine"), "{summary}");
    let card = card_of_draft(&page.body, f.drafts[1]);
    assert!(card.contains("Posted from here ↗"), "{card}");
    assert!(card.contains("#discussion_t-mine"), "{card}");
    // Answered, so the thread's under it.
    assert!(card.contains("used below."), "{card}");
    assert!(!card.contains("overlaps an existing thread"), "{card}");
    assert!(!card.contains("👍 instead"), "{card}");
    assert!(!card.contains("#discussion_t-bob"), "{card}");
}

#[tokio::test]
async fn a_thread_posted_before_its_comment_was_recorded_is_matched_by_its_text() {
    let f = fixture(false).await;
    f.dashboard.app.store().mark_posted(&[f.drafts[1]]).unwrap();
    let mine = review_thread("t-mine", "src/lib.rs", Some(3), "me", "Why `m`?");
    // Word for word too, but not yours.
    let bobs = review_thread("t-bob", "src/lib.rs", Some(3), "bob", "Why `m`?");
    record_pr7_threads(&f, vec![bobs, mine]);
    let page = f.get("/pr/org/repo/7").await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains(
            "<b>2</b> existing review threads · <b>0</b> overlap your drafts · <b>1</b> posted from here"
        ),
        "{summary}"
    );
    assert!(summary.contains("#discussion_t-bob"), "{summary}");
    let card = card_of_draft(&page.body, f.drafts[1]);
    assert!(card.contains("Posted from here ↗"), "{card}");
    assert!(card.contains("#discussion_t-mine"), "{card}");
}

#[tokio::test]
async fn your_own_thread_written_on_github_is_yours_and_still_overlaps() {
    let f = fixture(false).await;
    // Word for word the draft, which isn't posted.
    let mine = review_thread("t-mine", "src/lib.rs", Some(3), "me", "Why `m`?");
    record_pr7_threads(&f, vec![mine]);
    let page = f.get("/pr/org/repo/7").await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains(
            "<b>2</b> existing review threads · <b class=\"hot\">1</b> overlaps your drafts"
        ),
        "{summary}"
    );
    assert!(!summary.contains("posted from here"), "{summary}");
    let card = card_of_draft(&page.body, f.drafts[1]);
    assert!(card.contains("overlaps an existing thread"), "{card}");
    assert!(card.contains(">yours</span>"), "{card}");
    assert!(card.contains("👍 instead"), "{card}");
    assert!(!card.contains("Posted from here"), "{card}");
}

#[tokio::test]
async fn a_rejected_draft_overlaps_nothing() {
    let f = fixture(false).await;
    let bob = review_thread("t-bob", "src/lib.rs", Some(3), "bob", "Is `m` used?");
    record_pr7_threads(&f, vec![bob]);
    f.post(
        &format!("/drafts/{}/status", f.drafts[1]),
        &[("status", "rejected")],
    )
    .await;
    let page = f.get("/pr/org/repo/7").await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains("<b>2</b> existing review threads · <b>0</b> overlap your drafts"),
        "{summary}"
    );
}

#[tokio::test]
async fn a_thread_you_posted_is_its_drafts_and_an_existing_thread_to_the_rest() {
    let f = fixture(false).await;
    let (regeneration, ids) = regenerate(&f);
    {
        let mut store = f.dashboard.app.store();
        // Posted from the first run, and its copy in the regeneration
        // marked posted with it.
        store.mark_posted(&[f.drafts[1], ids[1]]).unwrap();
        store
            .record_posted_comments(&[(f.drafts[1], "PRRC_9".into())])
            .unwrap();
    }
    let mut mine = review_thread("t-mine", "src/lib.rs", Some(3), "me", "Why `m`?");
    mine.comments[0].id = "PRRC_9".into();
    record_pr7_threads(&f, vec![mine]);

    // The regeneration's other draft on those lines overlaps it.
    let page = f.get(&format!("/pr/org/repo/7?run={regeneration}")).await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains(
            "<b>2</b> existing review threads · <b class=\"hot\">1</b> overlaps your drafts"
        ),
        "{summary}"
    );
    let other = card_of_draft(&page.body, ids[2]);
    assert!(other.contains("overlaps an existing thread"), "{other}");
    assert!(
        other.contains(&format!("you posted this from run {}", f.run)),
        "{other}"
    );
    assert!(other.contains("👍 instead"), "{other}");
    let copy = card_of_draft(&page.body, ids[1]);
    assert!(copy.contains("Posted from here ↗"), "{copy}");

    // On its own run, it's only the draft's.
    let page = f.get(&format!("/pr/org/repo/7?run={}", f.run)).await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains(
            "<b>1</b> existing review thread · <b>0</b> overlap your drafts · <b>1</b> posted from here"
        ),
        "{summary}"
    );
    assert!(!summary.contains("#discussion_t-mine"), "{summary}");
    let card = card_of_draft(&page.body, f.drafts[1]);
    assert!(card.contains("Posted from here ↗"), "{card}");
    // Nor does its diff show it as an existing thread.
    let page = f
        .get(&format!("/pr/org/repo/7?run={}&view=files", f.run))
        .await;
    let view = files_view(&page.body);
    assert!(!view.contains("you posted this from run"), "{view}");
}

#[tokio::test]
async fn a_draft_that_chose_a_thread_but_went_inline_is_not_in_it() {
    let f = fixture(false).await;
    let bob = review_thread("t-bob", "src/lib.rs", Some(3), "bob", "Is `m` used?");
    let mut mine = review_thread("t-mine", "src/lib.rs", Some(3), "me", "Why `m`?");
    mine.comments[0].id = "PRRC_9".into();
    record_pr7_threads(&f, vec![bob, mine]);
    // GitHub took it inline but marking it failed, and it chose bob's
    // thread before the next submit found the review and marked it.
    f.dashboard
        .app
        .store()
        .record_posted_comments(&[(f.drafts[1], "PRRC_9".into())])
        .unwrap();
    choose(&f, 1, "reply", "t-bob", "").await;
    f.dashboard.app.store().mark_posted(&[f.drafts[1]]).unwrap();
    let page = f.get("/pr/org/repo/7").await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains(
            "<b>2</b> existing review threads · <b>0</b> overlap your drafts · <b>1</b> posted from here"
        ),
        "{summary}"
    );
    assert!(summary.contains("#discussion_t-bob"), "{summary}");
    let card = card_of_draft(&page.body, f.drafts[1]);
    assert!(card.contains("Posted from here ↗"), "{card}");
    assert!(!card.contains("Posted in this thread:"), "{card}");
    assert!(!card.contains("#discussion_t-bob"), "{card}");
}

#[tokio::test]
async fn a_thread_a_posted_reply_went_in_is_with_its_draft() {
    let f = fixture(false).await;
    let bob = review_thread("t-bob", "src/lib.rs", Some(3), "bob", "Is `m` used?");
    record_pr7_threads(&f, vec![bob]);
    choose(&f, 1, "reply", "t-bob", "").await;
    f.dashboard.app.store().mark_posted(&[f.drafts[1]]).unwrap();
    let page = f.get("/pr/org/repo/7").await;
    let summary = threads_summary(&page.body);
    assert!(
        summary.contains(
            "<b>1</b> existing review thread · <b>0</b> overlap your drafts · <b>1</b> posted from here"
        ),
        "{summary}"
    );
    assert!(!summary.contains("#discussion_t-bob"), "{summary}");
    let card = card_of_draft(&page.body, f.drafts[1]);
    assert!(card.contains("Posted in this thread:"), "{card}");
    assert!(card.contains("#discussion_t-bob"), "{card}");
}
