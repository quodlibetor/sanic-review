-- The agent's private note on a draft, for you and never posted: why it
-- matters, how sure it is, what it checked and what it couldn't.
ALTER TABLE drafts ADD COLUMN note TEXT;
