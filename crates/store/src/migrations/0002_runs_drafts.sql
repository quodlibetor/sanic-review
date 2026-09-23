-- Agent runs and the drafts they produce.

CREATE TABLE runs (
    id                 INTEGER PRIMARY KEY,
    repo               TEXT    NOT NULL,
    number             INTEGER NOT NULL,
    kind               TEXT    NOT NULL,  -- review
    trigger            TEXT    NOT NULL,  -- review_requested | push
    idem_key           TEXT    NOT NULL,  -- review: head_sha
    profile            TEXT    NOT NULL,
    head_sha           TEXT    NOT NULL,
    base_sha           TEXT    NOT NULL,
    from_sha           TEXT,              -- push: the head last seen
    -- queued | running | succeeded | failed | superseded
    status             TEXT    NOT NULL,
    error              TEXT,
    suggested_verdict  TEXT,              -- comment | request_changes | none
    session_id         TEXT,
    transcript_path    TEXT,
    queued_at          TEXT    NOT NULL,
    started_at         TEXT,
    finished_at        TEXT,
    UNIQUE (repo, number, kind, idem_key),
    FOREIGN KEY (repo, number) REFERENCES prs (repo, number)
);

CREATE INDEX runs_by_status ON runs (status);

CREATE TABLE drafts (
    id             INTEGER PRIMARY KEY,
    run_id         INTEGER NOT NULL REFERENCES runs (id),
    kind           TEXT    NOT NULL,  -- summary | comment | reply
    -- Anchor, for inline comments.
    path           TEXT,
    line           INTEGER,
    start_line     INTEGER,
    side           TEXT,              -- LEFT | RIGHT
    severity       TEXT,              -- blocker | major | minor | nit
    confidence     TEXT,              -- high | medium | low
    original_body  TEXT    NOT NULL,
    edited_body    TEXT,              -- NULL until you edit it
    -- pending | accepted | rejected | stale | posted
    status         TEXT    NOT NULL,
    unanchored     INTEGER NOT NULL,
    created_at     TEXT    NOT NULL,
    updated_at     TEXT    NOT NULL
);

CREATE INDEX drafts_by_run ON drafts (run_id);
