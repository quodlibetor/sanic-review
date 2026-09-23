-- When you last opened each PR's dashboard page. A PR whose latest review
-- finished after that, or that you've never opened, is unseen.
CREATE TABLE views (
    repo       TEXT    NOT NULL,
    number     INTEGER NOT NULL,
    viewed_at  TEXT    NOT NULL,
    PRIMARY KEY (repo, number),
    FOREIGN KEY (repo, number) REFERENCES prs (repo, number)
);
