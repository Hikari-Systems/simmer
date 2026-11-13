-- D-118 — sticky membership of an auto partial ramp (O-21). Idempotent, per the
-- house convention.
--
-- D-117 made a share LIST sticky by taking the day out of the hash: a list may
-- not decrease, so the set it offers only grows. `share: {mode: auto}` cannot do
-- that — D-097's controller moves the share down as well as up, within a day and
-- across days — so a recipient's draw alone cannot keep them on the route. This
-- table is what does: a recipient a message was COMMITTED to on an auto route is
-- a member of `(ramp, route)` and is offered it whatever the share.
--
--   * `recipient_hash` is §7.3's keyed hash under the salt in `instance_config`,
--     16 bytes, never plaintext — but over a domain-separated input
--     ("{normalised}\0member"), so it does not join against `recipient_event`.
--   * Written only for auto routes, in the commit transaction (§7.4 phase 3), so
--     a delivered message is never missing its row and an undelivered one never
--     has one.
--   * Unlike `recipient_event`, it does not age out per row. A route's rows go
--     together, `member_retention_days` after the route's last commit — which is
--     the newest `last_committed_at` among them, since every commit on an auto
--     route touches one. D-118 records that this is a record of every address
--     mailed on the route for the life of its ramp, accepted by the author.
CREATE TABLE IF NOT EXISTS partial_ramp_member (
    ramp               TEXT        NOT NULL,
    route              TEXT        NOT NULL,
    recipient_hash     BYTEA       NOT NULL,
    first_committed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_committed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (ramp, route, recipient_hash)
);

-- The sweeper's read: the newest commit per route.
CREATE INDEX IF NOT EXISTS partial_ramp_member_last_idx
    ON partial_ramp_member (ramp, route, last_committed_at);
