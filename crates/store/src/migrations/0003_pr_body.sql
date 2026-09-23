-- The PR description, for review briefs. Rows from before this migration
-- get theirs on the next poll.
ALTER TABLE prs ADD COLUMN body TEXT NOT NULL DEFAULT '';
