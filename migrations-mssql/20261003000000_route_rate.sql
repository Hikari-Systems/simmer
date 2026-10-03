-- SQL Server translation of migrations/20261003000000_route_rate.sql (D-111).
-- The Postgres file carries the reasoning. Keys are NVARCHAR(128) BIN2, as
-- D-099 left every other key: (128 × 3) × 2 = 768 bytes, under the 900-byte
-- clustered-key limit. `tat` is DATETIME2, UTC, bound as NaiveDateTime.
IF OBJECT_ID(N'dbo.route_rate', N'U') IS NULL
CREATE TABLE dbo.route_rate (
    ramp         NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    route        NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    domain_group NVARCHAR(128) COLLATE Latin1_General_100_BIN2 NOT NULL,
    tat          DATETIME2     NULL,
    updated_at   DATETIME2     NOT NULL DEFAULT SYSUTCDATETIME(),

    CONSTRAINT route_rate_pk PRIMARY KEY (ramp, route, domain_group)
);
