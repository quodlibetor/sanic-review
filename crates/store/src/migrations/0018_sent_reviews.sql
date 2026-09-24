-- A review sent in the one call that creates and submits it, recorded
-- before it's sent and kept until its answer comes back: what it sent, as
-- JSON, so the next submit on the PR can look for it on GitHub before
-- sending it again. Its `node_id` and `html_url` are empty: GitHub hasn't
-- said them.
ALTER TABLE pending_reviews ADD COLUMN sent TEXT;
