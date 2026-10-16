-- Set and cleared only by the user: the effort a PR's reviews run at when
-- the run has none of its own. NULL follows the profile's.
ALTER TABLE prs ADD COLUMN effort TEXT;
