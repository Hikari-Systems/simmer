-- D-116 — ramp groups. The Postgres counterpart is
-- migrations/20261115000000_ramp_group.sql, which carries the reasoning: one
-- row per (ramp_group, day_index), allowance and step written once, and the
-- GROUP row locked before the route row everywhere.
--
-- One batch, no GO: statements naming the new reservation columns go through
-- EXEC, which compiles when it runs.

IF OBJECT_ID(N'dbo.ramp_group_usage', N'U') IS NULL
CREATE TABLE dbo.ramp_group_usage (
    ramp_group NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    day_index  BIGINT        NOT NULL,
    allowance  BIGINT        NOT NULL,
    step       BIGINT        NOT NULL,
    committed  BIGINT        NOT NULL DEFAULT 0,
    reserved   BIGINT        NOT NULL DEFAULT 0,
    created_at DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),
    updated_at DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),

    CONSTRAINT ramp_group_usage_pk PRIMARY KEY (ramp_group, day_index),
    CONSTRAINT ramp_group_usage_committed_non_negative CHECK (committed >= 0),
    CONSTRAINT ramp_group_usage_reserved_non_negative  CHECK (reserved  >= 0),
    CONSTRAINT ramp_group_usage_allowance_non_negative CHECK (allowance >= 0),
    CONSTRAINT ramp_group_usage_step_non_negative      CHECK (step      >= 0)
);

IF COL_LENGTH(N'dbo.quota_reservation', N'ramp_group') IS NULL
BEGIN
    ALTER TABLE dbo.quota_reservation
        ADD ramp_group NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NULL,
            group_day_index BIGINT NULL;

    EXEC (N'ALTER TABLE dbo.quota_reservation ADD CONSTRAINT quota_reservation_group_pair
              CHECK ((ramp_group IS NULL AND group_day_index IS NULL)
                  OR (ramp_group IS NOT NULL AND group_day_index IS NOT NULL))');
END
