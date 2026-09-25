-- D-099 — named ramps: every persisted key gains `ramp`.
--
-- Existing rows are filled with '' — impossible as a ramp name (§4.2 wants
-- 1–64 characters) — and the default is then DROPPED. That is the fence: a
-- v0.8 binary against this schema finds its `ON CONFLICT (route, domain_group,
-- day_index)` matching no constraint, so every reservation fails and every
-- message gets §7.5's 451. Nothing miscounts.
--
-- The '' rows are moved into `default_ramp` at startup by
-- `QuotaStore::adopt_legacy_rows`, before any listener binds, because only the
-- configuration knows which ramp that is.

ALTER TABLE quota_usage ADD COLUMN IF NOT EXISTS ramp TEXT NOT NULL DEFAULT '';
ALTER TABLE quota_usage ALTER COLUMN ramp DROP DEFAULT;
ALTER TABLE quota_usage DROP CONSTRAINT IF EXISTS quota_usage_pkey;
ALTER TABLE quota_usage ADD CONSTRAINT quota_usage_pkey
    PRIMARY KEY (ramp, route, domain_group, day_index);

ALTER TABLE quota_reservation ADD COLUMN IF NOT EXISTS ramp TEXT NOT NULL DEFAULT '';
ALTER TABLE quota_reservation ALTER COLUMN ramp DROP DEFAULT;
DROP INDEX IF EXISTS quota_reservation_route_idx;
CREATE INDEX IF NOT EXISTS quota_reservation_route_idx
    ON quota_reservation (ramp, route, domain_group, day_index);

ALTER TABLE route_state ADD COLUMN IF NOT EXISTS ramp TEXT NOT NULL DEFAULT '';
ALTER TABLE route_state ALTER COLUMN ramp DROP DEFAULT;
ALTER TABLE route_state DROP CONSTRAINT IF EXISTS route_state_pkey;
ALTER TABLE route_state ADD CONSTRAINT route_state_pkey PRIMARY KEY (ramp, route);

ALTER TABLE recipient_event ADD COLUMN IF NOT EXISTS ramp TEXT NOT NULL DEFAULT '';
ALTER TABLE recipient_event ALTER COLUMN ramp DROP DEFAULT;
DROP INDEX IF EXISTS recipient_event_lookup_idx;
CREATE INDEX IF NOT EXISTS recipient_event_lookup_idx
    ON recipient_event (recipient_hash, ramp, route, sent_at);
