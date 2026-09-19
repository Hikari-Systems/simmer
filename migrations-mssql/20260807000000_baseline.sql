-- SQL Server translation of migrations/20260807000000_baseline.sql (D-084).
-- Same table, same meaning; the Postgres file is the one with the reasoning.
--
-- Idempotent throughout, per the house convention. T-SQL has no
-- CREATE TABLE IF NOT EXISTS, so each object is guarded by OBJECT_ID. Types:
-- TEXT -> NVARCHAR (TEXT is deprecated in T-SQL and cannot be a key),
-- TIMESTAMPTZ -> DATETIME2 holding UTC, now() -> SYSUTCDATETIME(). `key` and
-- `value` are bracketed because KEY is a reserved word.
--
-- Every text key is `COLLATE Latin1_General_100_BIN2`: SQL Server's default
-- collation is case- and accent-insensitive, which would let routes `Warming`
-- and `warming` share one quota row. Postgres compares bytes; so does this.

-- §7.3: "The salt is generated once and persisted."
IF OBJECT_ID(N'dbo.instance_config', N'U') IS NULL
CREATE TABLE dbo.instance_config (
    [key]      NVARCHAR(200) COLLATE Latin1_General_100_BIN2 NOT NULL PRIMARY KEY,
    [value]    NVARCHAR(MAX) NOT NULL,
    created_at DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),
    updated_at DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME()
);
