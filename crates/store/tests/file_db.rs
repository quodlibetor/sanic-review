//! Behaviour of an on-disk database across process restarts.

// Integration-test helpers are test code; clippy only exempts `#[test]` fns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::Connection;
use sanic_store::Store;
use tempfile::TempDir;

#[test]
fn reopening_keeps_state_and_creates_parent_dirs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested/state.db");
    Store::open(&path)
        .unwrap()
        .set_poll_state("last_modified", "x")
        .unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.poll_state("last_modified").unwrap().as_deref(),
        Some("x")
    );
}

#[test]
fn newer_schema_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.db");
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 9999)
        .unwrap();
    let err = format!("{:?}", Store::open(&path).err().unwrap());
    assert!(err.contains("newer than this build"), "{err}");
}
