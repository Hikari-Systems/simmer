-- SQL Server translation of migrations/20260807000001_quota.sql (D-084). Same
-- tables, columns, keys and CHECK constraints; the Postgres file carries the
-- reasoning for each.
--
-- `route` and `domain_group` are NVARCHAR(200): a clustered primary key is
-- capped at 900 bytes, and (200 + 200) × 2 + 8 fits. Both are configuration
-- names, and §4.2 refuses anything longer at startup under this build.

-- §7.1 — the quota key is (route, domain_group), bucketed by §7.2's day index.
IF OBJECT_ID(N'dbo.quota_usage', N'U') IS NULL
CREATE TABLE dbo.quota_usage (
    route              NVARCHAR(200) COLLATE Latin1_General_100_BIN2 NOT NULL,
    domain_group       NVARCHAR(200) COLLATE Latin1_General_100_BIN2 NOT NULL,
    day_index          BIGINT        NOT NULL,
    allowance          BIGINT        NULL,
    allowance_override BIGINT        NULL,
    committed          BIGINT        NOT NULL DEFAULT 0,
    reserved           BIGINT        NOT NULL DEFAULT 0,
    created_at         DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),
    updated_at         DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),

    CONSTRAINT quota_usage_pk PRIMARY KEY (route, domain_group, day_index),
    CONSTRAINT quota_usage_committed_non_negative CHECK (committed >= 0),
    CONSTRAINT quota_usage_reserved_non_negative  CHECK (reserved  >= 0),
    CONSTRAINT quota_usage_allowance_non_negative CHECK (allowance IS NULL OR allowance >= 0),
    CONSTRAINT quota_usage_override_non_negative  CHECK (allowance_override IS NULL OR allowance_override >= 0)
);

-- §7.4 phase 1 of the reserve/send/commit protocol.
IF OBJECT_ID(N'dbo.quota_reservation', N'U') IS NULL
CREATE TABLE dbo.quota_reservation (
    id             UNIQUEIDENTIFIER NOT NULL PRIMARY KEY,
    route          NVARCHAR(200) COLLATE Latin1_General_100_BIN2    NOT NULL,
    domain_group   NVARCHAR(200) COLLATE Latin1_General_100_BIN2    NOT NULL,
    day_index      BIGINT           NOT NULL,
    [count]        BIGINT           NOT NULL,
    correlation_id NVARCHAR(200) COLLATE Latin1_General_100_BIN2    NOT NULL,
    created_at     DATETIME2        NOT NULL DEFAULT SYSUTCDATETIME(),
    expires_at     DATETIME2        NOT NULL,

    CONSTRAINT quota_reservation_count_positive CHECK ([count] > 0)
);

IF NOT EXISTS (SELECT 1 FROM sys.indexes
               WHERE name = N'quota_reservation_expires_at_idx'
                 AND object_id = OBJECT_ID(N'dbo.quota_reservation'))
CREATE INDEX quota_reservation_expires_at_idx
    ON dbo.quota_reservation (expires_at);

IF NOT EXISTS (SELECT 1 FROM sys.indexes
               WHERE name = N'quota_reservation_route_idx'
                 AND object_id = OBJECT_ID(N'dbo.quota_reservation'))
CREATE INDEX quota_reservation_route_idx
    ON dbo.quota_reservation (route, domain_group, day_index);

-- §9.3 admin mutations that must survive a restart.
IF OBJECT_ID(N'dbo.route_state', N'U') IS NULL
CREATE TABLE dbo.route_state (
    route      NVARCHAR(200) COLLATE Latin1_General_100_BIN2 NOT NULL PRIMARY KEY,
    paused     BIT           NOT NULL DEFAULT 0,
    graduated  BIT           NOT NULL DEFAULT 0,
    created_at DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),
    updated_at DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME()
);
