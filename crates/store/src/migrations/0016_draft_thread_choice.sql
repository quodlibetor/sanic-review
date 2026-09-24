-- What to post for a draft that overlaps an existing review thread:
-- `react` puts a thumbs-up on `react_to`, one of `thread_id`'s comments,
-- instead of the draft; `reply` posts the draft in `thread_id`. NULL posts
-- it as a comment of its own.
ALTER TABLE drafts ADD COLUMN thread_choice TEXT;  -- react | reply
ALTER TABLE drafts ADD COLUMN thread_id TEXT;
ALTER TABLE drafts ADD COLUMN react_to TEXT;

-- A review created pending on GitHub for a PR and not yet known to be
-- submitted: the run it's from, and the drafts in it as posted. Any submit
-- on the PR checks it first, so a retry, from that run or a regeneration
-- of it, never posts a review GitHub may already have.
CREATE TABLE pending_reviews (
    repo       TEXT    NOT NULL,
    number     INTEGER NOT NULL,
    run_id     INTEGER NOT NULL REFERENCES runs (id),
    node_id    TEXT    NOT NULL,
    html_url   TEXT    NOT NULL,
    drafts     TEXT    NOT NULL,  -- JSON array of [draft id, body as posted]
    PRIMARY KEY (repo, number),
    FOREIGN KEY (repo, number) REFERENCES prs (repo, number)
);
