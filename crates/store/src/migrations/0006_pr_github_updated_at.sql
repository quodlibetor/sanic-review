-- When GitHub last saw activity on the PR, as it writes timestamps. NULL
-- until the next poll of rows from before this migration, or when GitHub
-- didn't say; those count as recent.
ALTER TABLE prs ADD COLUMN github_updated_at TEXT;
