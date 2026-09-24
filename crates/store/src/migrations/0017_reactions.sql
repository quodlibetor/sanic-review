-- Everyone's latest reaction to each comment, as last polled: the PR
-- author's reaction to your comment answers it. Filled in as PRs are
-- polled again.
CREATE TABLE reactions (
    comment_id  TEXT NOT NULL REFERENCES comments (id),
    login       TEXT NOT NULL,
    reacted_at  TEXT NOT NULL,
    PRIMARY KEY (comment_id, login)
);
