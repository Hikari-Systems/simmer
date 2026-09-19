-- SQL Server translation of migrations/20260810000000_recipient_event.sql
-- (D-084). The Postgres file carries the reasoning: a 16-byte keyed hash, never
-- plaintext; per route; append-only with no key, swept by age.
--
-- BYTEA -> VARBINARY(16), which is exactly the truncated HMAC's length.
IF OBJECT_ID(N'dbo.recipient_event', N'U') IS NULL
CREATE TABLE dbo.recipient_event (
    recipient_hash VARBINARY(16) NOT NULL,
    route          NVARCHAR(200) COLLATE Latin1_General_100_BIN2 NOT NULL,
    sent_at        DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME()
);

IF NOT EXISTS (SELECT 1 FROM sys.indexes
               WHERE name = N'recipient_event_lookup_idx'
                 AND object_id = OBJECT_ID(N'dbo.recipient_event'))
CREATE INDEX recipient_event_lookup_idx
    ON dbo.recipient_event (recipient_hash, route, sent_at);

IF NOT EXISTS (SELECT 1 FROM sys.indexes
               WHERE name = N'recipient_event_sent_at_idx'
                 AND object_id = OBJECT_ID(N'dbo.recipient_event'))
CREATE INDEX recipient_event_sent_at_idx
    ON dbo.recipient_event (sent_at);
