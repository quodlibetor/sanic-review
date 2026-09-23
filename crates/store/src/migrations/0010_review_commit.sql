-- The commit each review was left on, and whether a bot left it: a
-- person's review of a PR's current head means it's already reviewed.
ALTER TABLE reviews ADD COLUMN commit_sha TEXT;
ALTER TABLE reviews ADD COLUMN by_bot INTEGER NOT NULL DEFAULT 0;
