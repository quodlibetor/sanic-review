-- A regeneration of one draft of the run it revises, rather than of the
-- whole review: the new run's other drafts are copies of that run's.
ALTER TABLE runs ADD COLUMN draft_id INTEGER REFERENCES drafts (id);
-- Why the agent dropped a draft it was asked to revise; the draft is kept,
-- rejected, so you can restore it.
ALTER TABLE drafts ADD COLUMN drop_reason TEXT;
