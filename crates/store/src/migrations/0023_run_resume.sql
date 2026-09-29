-- For a push review that resumed an earlier run's agent session: that
-- run, and the run whose worktree path it ran at, where the session
-- lives. A regeneration uses its source's worktree path.
ALTER TABLE runs ADD COLUMN resumed_from INTEGER;
ALTER TABLE runs ADD COLUMN worktree_run INTEGER;
-- For a resumed review that found nothing new since the run it resumed:
-- the agent's word on what it checked. Its drafts are that run's, carried.
ALTER TABLE runs ADD COLUMN no_update TEXT;
