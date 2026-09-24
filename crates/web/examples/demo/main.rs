//! The dashboard over an invented store, for the README's screenshots
//! (`mise run screenshots`). It polls nothing and runs no agents, and its
//! GitHub client points at a closed port, so nothing leaves the machine.
//!
//! `cargo run -p sanic-web --example demo [-- --port N]` prints the
//! dashboard's URL, then serves until interrupted. The data is in
//! [`seed`].

mod seed;

use std::{collections::HashMap, path::Path, sync::Arc, time::SystemTime};

use color_eyre::eyre::{Result, WrapErr, eyre};
use sanic_core::{
    clock::{Clock, RecencyWindow, WindowChoice},
    config::{CheckoutResolver, Config, Vcs},
    pr::PrKey,
    repo::RepoName,
};
use sanic_github::{Client, Token};
use sanic_runner::review::{AgentProfile, RunSettings};
use sanic_store::{Refusal, Store};
use sanic_web::{Bound, Context, Control, Dashboard, Sources};
use tempfile::TempDir;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let port = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => 0,
        [flag, port] if flag == "--port" => port.parse().wrap_err("parsing --port")?,
        _ => return Err(eyre!("usage: demo [--port N]")),
    };
    let data = TempDir::new().wrap_err("making the demo's data dir")?;
    let bound = demo(data.path(), port).await?;
    // The screenshot script reads the URL from the first line.
    println!("{}", bound.url());
    let mut term = signal(SignalKind::terminate()).wrap_err("listening for SIGTERM")?;
    tokio::select! {
        served = bound.serve() => served?,
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    // Dropping `data` removes the store.
    Ok(())
}

/// Seeds a store in `dir` and binds the dashboard over it on `port`.
async fn demo(dir: &Path, port: u16) -> Result<Bound> {
    let db = dir.join("sanic-review.db");
    let seeded = {
        let mut store = Store::open(&db)?;
        seed::seed(&mut store)?
    };
    seed::pin_times(&db, &seeded)?;
    for (run, diff) in &seeded.diffs {
        let path = dir.join("runs").join(run.to_string()).join("pr.diff");
        if let Some(run_dir) = path.parent() {
            std::fs::create_dir_all(run_dir)
                .wrap_err_with(|| format!("creating {}", run_dir.display()))?;
        }
        std::fs::write(&path, diff).wrap_err_with(|| format!("writing {}", path.display()))?;
    }
    let config = Config::parse(seed::CONFIG, Path::new("/"), &NoCheckouts)
        .wrap_err("parsing the demo config")?;
    let skips = config.skip_rules();
    let files = seeded
        .files
        .iter()
        .map(|(commit, path, text)| ((commit.clone(), (*path).to_owned()), *text))
        .collect();
    let (window, window_rx) = watch::channel(RecencyWindow {
        configured: Some(seed::WINDOW_DAYS),
        choice: None,
    });
    let dashboard = Dashboard::new(Context {
        me: seed::ME.into(),
        manual_reviews: false,
        data_dir: dir.to_owned(),
        config_path: "~/.config/sanic-review/config.toml".into(),
        store: Store::open(&db)?,
        // Port 9 is discard, and nothing listens there.
        github: Client::new("http://127.0.0.1:9", Token::new("demo".into()))?,
        control: Arc::new(DemoServe { config, window }),
        sources: Arc::new(DemoFiles(files)),
        due: watch::channel(HashMap::new()).1,
        skips: watch::channel(skips).1,
        window: window_rx,
        clock: Arc::new(DemoClock),
    })?;
    dashboard.bind(port).await
}

/// `serve`, as far as the demo needs one: it starts nothing.
struct DemoServe {
    config: Config,
    window: watch::Sender<RecencyWindow>,
}

impl Control for DemoServe {
    fn review_now(&self, _: PrKey) {}

    fn refresh(&self, _: PrKey) {}

    fn add_skip_title(&self, _: &str, _: Option<&str>) -> Result<bool> {
        Err(eyre!("the demo doesn't edit a config"))
    }

    fn run_settings(&self, profile: &str) -> Result<RunSettings> {
        let config = &self.config;
        let profile = config
            .profiles
            .iter()
            .find(|p| p.name == profile)
            .ok_or_else(|| eyre!("no profile `{profile}`"))?;
        Ok(RunSettings::new(
            AgentProfile::from(profile),
            &config.runner,
            &config.github.git_url,
            vec![],
        ))
    }

    fn regenerate(&self, _: i64, _: Option<i64>, _: &str) -> Result<Result<i64, Refusal>> {
        Ok(Err(Refusal::NoSession))
    }

    fn set_window(&self, choice: Option<WindowChoice>) -> Result<()> {
        self.window.send_modify(|window| window.choice = choice);
        Ok(())
    }
}

/// The seeded files, by commit and path, in place of the runner's mirrors.
struct DemoFiles(HashMap<(String, String), &'static str>);

impl Sources for DemoFiles {
    fn has_commit(&self, _: &RepoName, commit: &str) -> bool {
        self.0.keys().any(|(c, _)| c == commit)
    }

    fn file_at(&self, _: &RepoName, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .0
            .get(&(commit.to_owned(), path.to_owned()))
            .map(|text| text.as_bytes().to_vec()))
    }
}

/// Always [`seed::NOW`], so relative times come out the same.
struct DemoClock;

impl Clock for DemoClock {
    fn now(&self) -> SystemTime {
        seed::now()
    }
}

/// The demo config names no local checkouts.
struct NoCheckouts;

impl CheckoutResolver for NoCheckouts {
    fn resolve(&self, path: &Path, _: Option<&str>) -> Result<(Vcs, RepoName)> {
        Err(eyre!("the demo has no checkout at {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// `path`'s status line and body, over plain HTTP/1.1.
    async fn get(url: &str, path: &str) -> String {
        let host = url
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        let mut stream = tokio::net::TcpStream::connect(&host).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn the_demo_seeds_and_serves() {
        let data = TempDir::new().unwrap();
        let bound = demo(data.path(), 0).await.unwrap();
        let url = bound.url();
        tokio::spawn(bound.serve());

        let index = get(&url, "/").await;
        assert!(index.starts_with("HTTP/1.1 200"), "{index}");
        for title in [
            "Teach the frobnicator to count past three",
            "Replace the hamster wheel with a slightly faster hamster wheel",
            "do_the_other_thing",
            "Make flock wait politely instead of tapping its foot",
            "Make the dashboard go fast (sonic, not sanic)",
        ] {
            assert!(index.contains(title), "no {title:?} in the index:\n{index}");
        }

        let (owner, name, number) = seed::SHOWCASE;
        let page = get(&url, &format!("/pr/{owner}/{name}/{number}")).await;
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        assert!(page.contains("saturating_add"), "{page}");
        assert!(page.contains("Asking for a friend"), "{page}");
        let files = get(&url, &format!("/pr/{owner}/{name}/{number}?view=files")).await;
        assert!(files.starts_with("HTTP/1.1 200"), "{files}");
        assert!(files.contains("tests/count.rs"), "{files}");
    }
}
