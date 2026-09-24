-- Where each review thread sits beyond its last line, and each comment's
-- link, so the dashboard can match drafts to threads on the same lines.
ALTER TABLE threads ADD COLUMN start_line INTEGER;
ALTER TABLE threads ADD COLUMN side TEXT;                -- LEFT | RIGHT
ALTER TABLE threads ADD COLUMN head_sha TEXT;            -- the head line and start_line are on
ALTER TABLE threads ADD COLUMN outdated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE threads ADD COLUMN original_start_line INTEGER;
ALTER TABLE threads ADD COLUMN original_line INTEGER;
ALTER TABLE threads ADD COLUMN original_commit TEXT;     -- where it was left
ALTER TABLE comments ADD COLUMN url TEXT;
