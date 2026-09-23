-- A `regenerate` run revises the review of `source_run`, resuming its
-- agent session with your `instruction`.
ALTER TABLE runs ADD COLUMN source_run INTEGER REFERENCES runs (id);
ALTER TABLE runs ADD COLUMN instruction TEXT;
