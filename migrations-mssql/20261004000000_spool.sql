-- SQL Server translation of migrations/20261004000000_spool.sql (D-116). The
-- Postgres file carries the reasoning. Keys and every compared string are
-- NVARCHAR(128) BIN2, as everywhere else; instants are UTC DATETIME2; the
-- envelope is NVARCHAR(MAX) JSON that SQL never looks inside.
IF OBJECT_ID(N'dbo.spool_message', N'U') IS NULL
CREATE TABLE dbo.spool_message (
    id              UNIQUEIDENTIFIER NOT NULL CONSTRAINT spool_message_pk PRIMARY KEY,
    ramp            NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    domain_group    NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    group_basis     NVARCHAR(512) COLLATE Latin1_General_100_BIN2 NOT NULL,
    state           NVARCHAR(16)  COLLATE Latin1_General_100_BIN2 NOT NULL,
    next_attempt_at DATETIME2     NOT NULL,
    lease_owner     NVARCHAR(256) COLLATE Latin1_General_100_BIN2 NULL,
    lease_until     DATETIME2     NULL,
    lease_token     UNIQUEIDENTIFIER NULL,
    attempts        BIGINT        NOT NULL DEFAULT 0,
    pinned_route    NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NULL,
    booked_route    NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NULL,
    booked_group    NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NULL,
    booked_tat      DATETIME2     NULL,
    received_at     DATETIME2     NOT NULL,
    expires_at      DATETIME2     NOT NULL,
    envelope        NVARCHAR(MAX) NOT NULL,
    body_ref        NVARCHAR(512) COLLATE Latin1_General_100_BIN2 NULL,
    body_bytes      BIGINT        NOT NULL,
    body_sha256     VARBINARY(32) NOT NULL,
    uuid_seed       UNIQUEIDENTIFIER NOT NULL,
    dead_reason     NVARCHAR(16)  COLLATE Latin1_General_100_BIN2 NULL,
    last_code       BIGINT        NULL,
    last_error      NVARCHAR(MAX) NULL,
    dead_at         DATETIME2     NULL
);

IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE name = N'spool_message_due')
CREATE INDEX spool_message_due ON dbo.spool_message (state, next_attempt_at);

-- The claim's scan order. `TOP (n) … ORDER BY next_attempt_at` over any other
-- access path sorts first, and under `UPDLOCK` the sort's input — every
-- candidate row — is locked, so one claimant holds them all and `READPAST`
-- turns every other claimant away empty. Read in this index's order, the scan
-- stops after n qualifying rows and locks only those.
IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE name = N'spool_message_next')
CREATE INDEX spool_message_next ON dbo.spool_message (next_attempt_at, id)
    INCLUDE (state, lease_until, ramp);

IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE name = N'spool_message_lane')
CREATE INDEX spool_message_lane ON dbo.spool_message (ramp, domain_group, state);

IF OBJECT_ID(N'dbo.spool_ramp_state', N'U') IS NULL
CREATE TABLE dbo.spool_ramp_state (
    ramp       NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL
               CONSTRAINT spool_ramp_state_pk PRIMARY KEY,
    paused     BIT       NOT NULL DEFAULT 0,
    draining   BIT       NOT NULL DEFAULT 0,
    updated_at DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME()
);
