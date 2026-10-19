-- The effort a run was started at, when not its profile's; NULL follows the
-- profile's at run time.
ALTER TABLE runs ADD COLUMN effort TEXT;
ALTER TABLE start_requests ADD COLUMN effort TEXT;
