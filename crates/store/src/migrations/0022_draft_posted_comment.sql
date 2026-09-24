-- For a comment draft posted inline, the node id of the comment GitHub
-- made of it, which starts its thread: the thread shows as the draft's,
-- not as someone's existing thread. NULL for one posted before this was
-- recorded, or whose comments couldn't be listed after posting.
ALTER TABLE drafts ADD COLUMN posted_comment TEXT;
