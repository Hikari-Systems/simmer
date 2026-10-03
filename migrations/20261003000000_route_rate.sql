-- D-111 — per-segment sending rates. One row per (ramp, route, domain_group),
-- the quota's own key (§7.1) without a day index: a rate is per hour, and an
-- hour does not care which ramp day it falls in.
--
-- `tat` is GCRA's theoretical arrival time — the whole of a bucket's state.
-- Booking a slot creates the row if absent and locks it (`ON CONFLICT DO
-- UPDATE`, as `quota_usage`), decides in Rust, and writes the new `tat`, in one
-- short transaction that is never held across a send. Global across instances
-- for the same reason the quota row is: the limit is the provider's, not ours.
CREATE TABLE IF NOT EXISTS route_rate (
    ramp         TEXT        NOT NULL,
    route        TEXT        NOT NULL,
    domain_group TEXT        NOT NULL,
    tat          TIMESTAMPTZ NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (ramp, route, domain_group)
);
