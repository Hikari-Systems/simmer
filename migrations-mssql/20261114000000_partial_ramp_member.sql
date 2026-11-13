-- D-118 — sticky membership of an auto partial ramp (O-21). The Postgres
-- counterpart is migrations/20261114000000_partial_ramp_member.sql; read its
-- header for what the table is and why it is shaped this way.

IF OBJECT_ID(N'dbo.partial_ramp_member', N'U') IS NULL
CREATE TABLE dbo.partial_ramp_member (
    ramp               NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    route              NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    recipient_hash     VARBINARY(16) NOT NULL,
    first_committed_at DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),
    last_committed_at  DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),
    CONSTRAINT partial_ramp_member_pk PRIMARY KEY (ramp, route, recipient_hash)
);

IF NOT EXISTS (SELECT 1 FROM sys.indexes
               WHERE name = N'partial_ramp_member_last_idx'
                 AND object_id = OBJECT_ID(N'dbo.partial_ramp_member'))
CREATE INDEX partial_ramp_member_last_idx
    ON dbo.partial_ramp_member (ramp, route, last_committed_at);
