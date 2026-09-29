//! What runs beside the config editor, wherever it runs: checks, saves
//! and searches of the disk off the UI thread, and counts on the runtime.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::SystemTime,
};

use sanic_core::config::{CheckoutResolver, Config, Keys};

use super::{Checker, ConfigEditor, Find, Found, Outcome, counts::Counter, discover};
use crate::{
    config_doc::{ConfigDoc, Key, Scalar, Setting, Table},
    config_edit::resolve_links,
};

pub struct Host {
    path: PathBuf,
    resolver: Arc<dyn CheckoutResolver + Send + Sync>,
    checker: Checker,
    found_tx: mpsc::Sender<Found>,
    found: mpsc::Receiver<Found>,
}

impl Host {
    /// The editor on the config file at `path`, its first check started,
    /// or why it can't open. Its fields take `keys` where the config
    /// doesn't say: the guess made once where it runs, so it's the same
    /// each time it opens.
    pub fn open(
        path: &Path,
        resolver: Arc<dyn CheckoutResolver + Send + Sync>,
        keys: Keys,
    ) -> Result<(ConfigEditor, Self), String> {
        let opened = Config::read_text(path).and_then(|text| {
            let doc = ConfigDoc::parse(text.as_deref())?;
            let target = resolve_links(path)?;
            Ok(ConfigEditor::new(
                path,
                (target != path).then_some(target),
                doc,
            ))
        });
        let mut editor = opened.map_err(|err| format!("can't edit the config: {err:#}"))?;
        let checker = Checker::new(path.to_owned(), Arc::clone(&resolver));
        if let Outcome::Check { generation, text } = editor.check_now() {
            checker.start(generation, text);
        }
        let model =
            Key::new(Table::Runner, "model").and_then(|key| match editor.doc().scalar(&key) {
                Setting::Set(Scalar::Text(model)) => Some(model),
                _ => None,
            });
        editor.set_models(discover::known_models(model.as_deref()));
        editor.set_guessed_keys(keys);
        let (found_tx, found) = mpsc::channel();
        Ok((
            editor,
            Self {
                path: path.to_owned(),
                resolver,
                checker,
                found_tx,
                found,
            },
        ))
    }

    pub fn check(&self, generation: u64, text: String) {
        self.checker.start(generation, text);
    }

    /// Starts writing the editor's edits, and tells it so.
    pub fn save(&self, editor: &mut ConfigEditor) {
        self.checker.save(editor.doc().clone());
        editor.saving();
    }

    /// Starts finding what the editor suggests.
    pub fn find(&self, find: Find) {
        let path = self.path.clone();
        let resolver = Arc::clone(&self.resolver);
        let found = self.found_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("config find".into())
            .spawn(move || {
                // Only fails once the editor has closed.
                let _ = found.send(discover::find(find, &path, &*resolver));
            });
        if let Err(err) = spawned {
            let _ = self
                .found_tx
                .send(Found::Failed(format!("couldn't look: {err}")));
        }
    }

    /// Hands the editor what's come in, and `counter` what it wants.
    pub fn tend(&self, editor: &mut ConfigEditor, counter: Option<&Counter>, now: SystemTime) {
        if let Some((generation, checked)) = self.checker.finished() {
            editor.checked(generation, checked);
        }
        if let Some(saved) = self.checker.saved() {
            if let Err(err) = &saved {
                tracing::warn!("saving the config failed: {err:?}");
            }
            editor.saved(saved);
        }
        for found in self.found.try_iter() {
            editor.found(found);
        }
        if let Some(counter) = counter {
            editor.counted(counter.counted());
            counter.want(editor.want(now));
        }
    }
}
