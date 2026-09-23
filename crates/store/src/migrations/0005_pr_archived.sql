-- Set and cleared only by the user. Archived PRs aren't reviewed
-- automatically, and new activity doesn't clear the flag.
ALTER TABLE prs ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
