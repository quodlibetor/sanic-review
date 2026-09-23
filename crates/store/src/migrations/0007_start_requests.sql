-- PRs `sanic-review review` asked the running `serve` to review now. serve
-- takes and deletes them as it handles them.
CREATE TABLE start_requests (
    id            INTEGER PRIMARY KEY,
    repo          TEXT    NOT NULL,
    number        INTEGER NOT NULL,
    requested_at  TEXT    NOT NULL
);
