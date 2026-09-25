-- D-099 — named ramps: every persisted key gains `ramp`. The Postgres
-- counterpart is migrations/20260925000000_ramp.sql; read its header for the
-- '' fill and why the default is dropped (it is the fence against a v0.8
-- binary, which here inserts rows with no ramp and fails).
--
-- Also D-099: route, domain group and ramp shrink to NVARCHAR(128), so the
-- four-column clustered key is 776 bytes, under SQL Server's 900. A stored
-- name longer than 128 fails the migration loudly rather than truncating.
--
-- One batch, no GO, like every file here. T-SQL compiles a whole batch before
-- running any of it, so every statement that names the new `ramp` column goes
-- through EXEC, which compiles when it runs.

IF COL_LENGTH(N'dbo.quota_usage', N'ramp') IS NULL
BEGIN
    IF EXISTS (SELECT 1 FROM dbo.quota_usage
               WHERE LEN(route) > 128 OR LEN(domain_group) > 128)
       OR EXISTS (SELECT 1 FROM dbo.quota_reservation
                  WHERE LEN(route) > 128 OR LEN(domain_group) > 128)
       OR EXISTS (SELECT 1 FROM dbo.route_state WHERE LEN(route) > 128)
       OR EXISTS (SELECT 1 FROM dbo.recipient_event WHERE LEN(route) > 128)
        THROW 50099, N'D-099: a stored route or domain group name is longer than 128 characters. Named ramps shrink key names to 128 so the clustered key fits; rename it in the config and the database, or stay on v0.8.', 1;

    -- The keys and indexes over the columns being narrowed have to go first.
    DECLARE @sql NVARCHAR(400);
    ALTER TABLE dbo.quota_usage DROP CONSTRAINT quota_usage_pk;
    DROP INDEX quota_reservation_route_idx ON dbo.quota_reservation;
    -- route_state's primary key was declared inline, so its name is generated.
    SELECT @sql = N'ALTER TABLE dbo.route_state DROP CONSTRAINT ' + QUOTENAME(name)
    FROM sys.key_constraints
    WHERE parent_object_id = OBJECT_ID(N'dbo.route_state') AND type = 'PK';
    EXEC (@sql);
    DROP INDEX recipient_event_lookup_idx ON dbo.recipient_event;

    ALTER TABLE dbo.quota_usage ALTER COLUMN route
        NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL;
    ALTER TABLE dbo.quota_usage ALTER COLUMN domain_group
        NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL;
    ALTER TABLE dbo.quota_reservation ALTER COLUMN route
        NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL;
    ALTER TABLE dbo.quota_reservation ALTER COLUMN domain_group
        NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL;
    ALTER TABLE dbo.route_state ALTER COLUMN route
        NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL;
    ALTER TABLE dbo.recipient_event ALTER COLUMN route
        NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL;

    -- Filled with '' through a named default, which is then dropped: the fence.
    ALTER TABLE dbo.quota_usage ADD ramp NVARCHAR(128) COLLATE Latin1_General_100_BIN2
        NOT NULL CONSTRAINT quota_usage_ramp_legacy DEFAULT N'';
    ALTER TABLE dbo.quota_usage DROP CONSTRAINT quota_usage_ramp_legacy;
    ALTER TABLE dbo.quota_reservation ADD ramp NVARCHAR(128) COLLATE Latin1_General_100_BIN2
        NOT NULL CONSTRAINT quota_reservation_ramp_legacy DEFAULT N'';
    ALTER TABLE dbo.quota_reservation DROP CONSTRAINT quota_reservation_ramp_legacy;
    ALTER TABLE dbo.route_state ADD ramp NVARCHAR(128) COLLATE Latin1_General_100_BIN2
        NOT NULL CONSTRAINT route_state_ramp_legacy DEFAULT N'';
    ALTER TABLE dbo.route_state DROP CONSTRAINT route_state_ramp_legacy;
    ALTER TABLE dbo.recipient_event ADD ramp NVARCHAR(128) COLLATE Latin1_General_100_BIN2
        NOT NULL CONSTRAINT recipient_event_ramp_legacy DEFAULT N'';
    ALTER TABLE dbo.recipient_event DROP CONSTRAINT recipient_event_ramp_legacy;

    EXEC (N'ALTER TABLE dbo.quota_usage ADD CONSTRAINT quota_usage_pk
              PRIMARY KEY (ramp, route, domain_group, day_index)');
    EXEC (N'CREATE INDEX quota_reservation_route_idx
              ON dbo.quota_reservation (ramp, route, domain_group, day_index)');
    EXEC (N'ALTER TABLE dbo.route_state ADD CONSTRAINT route_state_pk
              PRIMARY KEY (ramp, route)');
    EXEC (N'CREATE INDEX recipient_event_lookup_idx
              ON dbo.recipient_event (recipient_hash, ramp, route, sent_at)');
END
