-- Baseline schema for simmer (SPEC.md §11).
--
-- Idempotent throughout (IF NOT EXISTS), per the hikari-systems convention, so
-- it is safe to run against a database that already has the schema.
--
-- Phase 1 creates only `instance_config`. The quota tables (`quota_usage`,
-- `quota_reservation`, `recipient_event`, `route_state`) land in phase 3, where
-- the reservation protocol is designed and the open questions about overflow
-- accounting and per-group allowance overrides are settled. Creating them now
-- would bake in answers that have not been given.

-- §7.3: "The salt is generated once and persisted." Also the home for any other
-- singleton this instance needs to remember across restarts.
CREATE TABLE IF NOT EXISTS instance_config (
    key        TEXT        NOT NULL PRIMARY KEY,
    value      TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
