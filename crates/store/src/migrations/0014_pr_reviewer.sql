-- Whether you review the PR, by `PrSnapshot::is_reviewer`, as last polled.
-- Existing rows start from the review request alone; the next poll of each
-- adds the ones you've reviewed.
ALTER TABLE prs ADD COLUMN reviewer INTEGER NOT NULL DEFAULT 0;
UPDATE prs SET reviewer = review_requested;
