-- Quota state (SPEC.md §7, §11). Idempotent throughout, per the house
-- convention.
--
-- `recipient_event` is deliberately absent: its row shape depends on §7.3's
-- hashing and normalisation, which is phase 6's subject. Creating it now would
-- bake in an answer that has not been given — the same reasoning that deferred
-- these tables out of the phase 1 baseline (DECISIONS.md D-014).

-- §7.1 — the quota key is (route, domain_group), bucketed by §7.2's day index.
--
-- §11 puts the admin allowance override on `route_state`, which is keyed on
-- route alone and so cannot represent §9.3's *per domain group* override. It
-- lives here instead (DECISIONS.md D-025), which also makes §9.3's "expires at
-- the next day boundary" automatic: the override is a property of one day's row,
-- and tomorrow gets a different row.
CREATE TABLE IF NOT EXISTS quota_usage (
    route              TEXT        NOT NULL,
    domain_group       TEXT        NOT NULL,
    -- §7.2. Signed: a route whose `warmup.started` is in the future has a
    -- negative index and is ineligible until it arrives.
    day_index          BIGINT      NOT NULL,

    -- NULL means *no ceiling* — an overflow route, which §3.1 says is never
    -- quota-limited but which still accounts, so that "how much is spilling to
    -- overflow" is answerable (O-2, D-024).
    --
    -- Written once when the row is created and authoritative thereafter: a
    -- config change must not retroactively raise today's ceiling, because a
    -- restart would then authorise a burst (O-4, D-026).
    allowance          BIGINT,
    -- §9.3. NULL when unset; takes precedence over `allowance` when set. Kept
    -- as a separate column rather than overwriting so the admin mutation is
    -- still visible after the fact.
    allowance_override BIGINT,

    committed          BIGINT      NOT NULL DEFAULT 0,
    reserved           BIGINT      NOT NULL DEFAULT 0,

    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (route, domain_group, day_index),

    -- The whole point of the component is that these never go wrong. A negative
    -- counter means the reserve/commit protocol has a bug, and it should fail
    -- loudly at the write rather than silently grant headroom that does not
    -- exist.
    CONSTRAINT quota_usage_committed_non_negative CHECK (committed >= 0),
    CONSTRAINT quota_usage_reserved_non_negative  CHECK (reserved  >= 0),
    CONSTRAINT quota_usage_allowance_non_negative CHECK (allowance IS NULL OR allowance >= 0),
    CONSTRAINT quota_usage_override_non_negative  CHECK (allowance_override IS NULL OR allowance_override >= 0)
);

-- §7.4 phase 1 of the reserve/send/commit protocol.
--
-- Carries its own `day_index` rather than recomputing it at commit time: a
-- reservation taken at 23:59:59 must commit against the day it reserved from,
-- not the day it happens to land in.
CREATE TABLE IF NOT EXISTS quota_reservation (
    id             UUID        NOT NULL PRIMARY KEY,
    route          TEXT        NOT NULL,
    domain_group   TEXT        NOT NULL,
    day_index      BIGINT      NOT NULL,
    count          BIGINT      NOT NULL,
    -- §9.5 — ties a stranded reservation back to the message that took it.
    correlation_id TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- §7.4: "downstream timeout budget + 60s". The sweeper covers a process
    -- crash mid-send.
    expires_at     TIMESTAMPTZ NOT NULL,

    CONSTRAINT quota_reservation_count_positive CHECK (count > 0)
);

CREATE INDEX IF NOT EXISTS quota_reservation_expires_at_idx
    ON quota_reservation (expires_at);

-- Locating a route's live reservations, for §10.4 shutdown release and for
-- §9.2's per-route view.
CREATE INDEX IF NOT EXISTS quota_reservation_route_idx
    ON quota_reservation (route, domain_group, day_index);

-- §9.3 admin mutations that must survive a restart. The allowance override is
-- *not* here — see the note on `quota_usage` above.
CREATE TABLE IF NOT EXISTS route_state (
    route      TEXT        NOT NULL PRIMARY KEY,
    -- §3.2 step 3a — makes a route ineligible without a restart.
    paused     BOOLEAN     NOT NULL DEFAULT false,
    -- §9.3 — pins the route to the final schedule value immediately. §7.2 is
    -- explicit that routes do not auto-graduate; this is the manual act.
    graduated  BOOLEAN     NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
