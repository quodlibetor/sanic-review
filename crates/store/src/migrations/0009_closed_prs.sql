-- PRs a refresh found closed or not visible, and when. A notification
-- about one is skipped unless it's newer than that; tracked or not, a PR
-- found open again leaves this table.
CREATE TABLE closed_prs (
    repo        TEXT    NOT NULL,
    number      INTEGER NOT NULL,
    checked_at  TEXT    NOT NULL,  -- as GitHub writes timestamps
    PRIMARY KEY (repo, number)
);
