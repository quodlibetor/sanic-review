-- Where a queued review sits in the queue the worker pulls from: smallest
-- first, with gaps so a reorder rewrites only the rows it moves past. NULL
-- for runs that never queue (regenerations start at once) and for runs that
-- have left the queue.
ALTER TABLE runs ADD COLUMN queue_pos INTEGER;

-- Runs a previous version left queued keep the order they had, by id.
UPDATE runs SET queue_pos = id * 1024 WHERE status = 'queued' AND kind = 'review';

CREATE INDEX runs_queued_order ON runs (queue_pos)
    WHERE status = 'queued' AND queue_pos IS NOT NULL;

-- `runs.status` gains 'cancelled' (0002 lists the rest). It isn't in the set
-- that blocks a head from being queued again, so cancelling is one-shot.
