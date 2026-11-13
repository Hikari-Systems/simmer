-- D-115 — volume-gated ramp advancement: a row records the schedule STEP it was
-- created at. The Postgres counterpart is migrations/20261113000000_quota_step.sql;
-- read its header for the backfill (every existing warming row gets
-- step = day_index, so an upgrade moves nothing) and what NULL means.
--
-- One batch, no GO. The statements naming the new column go through EXEC, which
-- compiles when it runs: T-SQL compiles a whole batch before running any of it.

IF COL_LENGTH(N'dbo.quota_usage', N'step') IS NULL
BEGIN
    ALTER TABLE dbo.quota_usage ADD step BIGINT NULL;

    EXEC (N'UPDATE dbo.quota_usage
               SET step = day_index
             WHERE step IS NULL
               AND allowance IS NOT NULL
               AND day_index >= 0');

    EXEC (N'ALTER TABLE dbo.quota_usage ADD CONSTRAINT quota_usage_step_non_negative
              CHECK (step IS NULL OR step >= 0)');
END
