-- What a PR's state (mergeable, approved, changes requested, unanswered
-- comments) is worked out from. NULL until the next poll of older rows.
ALTER TABLE prs ADD COLUMN review_decision TEXT;
ALTER TABLE prs ADD COLUMN merge_state TEXT;
ALTER TABLE prs ADD COLUMN checks TEXT;
ALTER TABLE comments ADD COLUMN by_bot INTEGER NOT NULL DEFAULT 0;
ALTER TABLE comments ADD COLUMN reacted_at TEXT;
