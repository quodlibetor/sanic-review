-- A whole review's procedure check, as JSON: the agent's run report, the
-- misses the transcript showed, and the attempts; NULL when none ran.
ALTER TABLE runs ADD COLUMN procedure_check TEXT;
