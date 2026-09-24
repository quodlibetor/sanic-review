-- Your own review pending on GitHub, as last polled, which only you can
-- see: its node id and its comments, as JSON.
CREATE TABLE in_progress_reviews (
    repo TEXT NOT NULL,
    number INTEGER NOT NULL,
    review_id TEXT NOT NULL,
    comments TEXT NOT NULL,
    PRIMARY KEY (repo, number)
);
-- How many of its comments a review's agent was shown.
ALTER TABLE runs ADD COLUMN in_progress_comments INTEGER;
