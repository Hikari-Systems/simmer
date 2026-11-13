-- D-115 — volume-gated ramp advancement: a row records the schedule STEP it was
-- created at, beside the `allowance` that step planned.
--
-- Before D-115 the step was the day index, always. Every existing warming row is
-- backfilled with exactly that, so an upgrade moves nothing: a route part-way
-- through its ramp continues from the day it is on, and the threshold rule takes
-- over from the next boundary, measured against that day's row.
--
-- NULL thereafter means "no step": an overflow row (no ceiling, no schedule) or
-- a negative day index. `quota::advance` reads a NULL as the day index, which is
-- what such a row meant when it was written.
--
-- Written once at creation and authoritative afterwards, for the reason
-- `allowance` is (D-026): a restart under a different rule must not re-decide a
-- day that is already being spent.

ALTER TABLE quota_usage ADD COLUMN IF NOT EXISTS step BIGINT;

UPDATE quota_usage
   SET step = day_index
 WHERE step IS NULL
   AND allowance IS NOT NULL
   AND day_index >= 0;

ALTER TABLE quota_usage DROP CONSTRAINT IF EXISTS quota_usage_step_non_negative;
ALTER TABLE quota_usage ADD CONSTRAINT quota_usage_step_non_negative
    CHECK (step IS NULL OR step >= 0);
