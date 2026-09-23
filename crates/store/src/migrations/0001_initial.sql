-- Timestamps are RFC 3339 UTC text: GitHub's own values are stored as given,
-- local ones come from `now()` below.

CREATE TABLE prs (
    repo              TEXT    NOT NULL,  -- owner/name, lowercase
    number            INTEGER NOT NULL,
    title             TEXT    NOT NULL,
    url               TEXT    NOT NULL,
    author            TEXT    NOT NULL,
    head_sha          TEXT    NOT NULL,
    base_sha          TEXT    NOT NULL,
    is_draft          INTEGER NOT NULL,
    review_requested  INTEGER NOT NULL,
    profile           TEXT    NOT NULL,
    first_seen_at     TEXT    NOT NULL,
    updated_at        TEXT    NOT NULL,
    PRIMARY KEY (repo, number)
);

CREATE TABLE revisions (
    repo      TEXT    NOT NULL,
    number    INTEGER NOT NULL,
    head_sha  TEXT    NOT NULL,
    base_sha  TEXT    NOT NULL,
    seen_at   TEXT    NOT NULL,
    PRIMARY KEY (repo, number, head_sha),
    FOREIGN KEY (repo, number) REFERENCES prs (repo, number)
);

CREATE TABLE threads (
    repo       TEXT    NOT NULL,
    number     INTEGER NOT NULL,
    thread_id  TEXT    NOT NULL,
    path       TEXT,
    line       INTEGER,
    resolved   INTEGER NOT NULL,
    PRIMARY KEY (repo, number, thread_id),
    FOREIGN KEY (repo, number) REFERENCES prs (repo, number)
);

CREATE TABLE comments (
    id          TEXT    PRIMARY KEY,  -- GitHub node id
    repo        TEXT    NOT NULL,
    number      INTEGER NOT NULL,
    thread_id   TEXT    NOT NULL,
    author      TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    created_at  TEXT    NOT NULL,
    FOREIGN KEY (repo, number, thread_id) REFERENCES threads (repo, number, thread_id)
);

CREATE TABLE reviews (
    id            TEXT    PRIMARY KEY,  -- GitHub node id
    repo          TEXT    NOT NULL,
    number        INTEGER NOT NULL,
    author        TEXT    NOT NULL,
    state         TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    submitted_at  TEXT    NOT NULL,
    FOREIGN KEY (repo, number) REFERENCES prs (repo, number)
);

-- Detected triggers, oldest first.
CREATE TABLE events (
    id      INTEGER PRIMARY KEY,
    at      TEXT    NOT NULL,
    repo    TEXT    NOT NULL,
    number  INTEGER NOT NULL,
    kind    TEXT    NOT NULL,
    detail  TEXT    NOT NULL  -- JSON
);

-- Opaque values the pollers carry between runs, e.g. Last-Modified.
CREATE TABLE poll_state (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);
