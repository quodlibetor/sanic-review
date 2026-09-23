-- Whether the PR was open and still turned up in your searches when last
-- polled. Existing rows count as open until the next reconcile says
-- otherwise.
ALTER TABLE prs ADD COLUMN open INTEGER NOT NULL DEFAULT 1;
