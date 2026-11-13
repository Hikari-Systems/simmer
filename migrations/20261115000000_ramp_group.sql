-- D-116 — ramp groups: one shared daily ceiling over the warming routes of
-- several ramps. The one piece of state member ramps share (§3.4).
--
-- A group's row is a quota row with the route and domain group taken out: one
-- per (ramp_group, day_index), the day counted from the group's own `started`
-- by §7.2's elapsed-duration arithmetic. `allowance` and `step` are written
-- when the row is created and authoritative afterwards, for D-026's reason —
-- a restart under an edited schedule must not raise a day already being spent.
-- Neither is ever NULL: a group has no unlimited day, and a group not yet
-- started writes no row at all.
--
-- §7.4's protocol is unchanged in shape. A reservation on a grouped warming
-- route locks the GROUP row first, then the route row — in that order
-- everywhere (reserve, commit, release, the sweeper), so two contenders can
-- never each hold the lock the other wants.

CREATE TABLE IF NOT EXISTS ramp_group_usage (
    ramp_group TEXT        NOT NULL,
    day_index  BIGINT      NOT NULL,
    allowance  BIGINT      NOT NULL,
    step       BIGINT      NOT NULL,
    committed  BIGINT      NOT NULL DEFAULT 0,
    reserved   BIGINT      NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (ramp_group, day_index),

    CONSTRAINT ramp_group_usage_committed_non_negative CHECK (committed >= 0),
    CONSTRAINT ramp_group_usage_reserved_non_negative  CHECK (reserved  >= 0),
    CONSTRAINT ramp_group_usage_allowance_non_negative CHECK (allowance >= 0),
    CONSTRAINT ramp_group_usage_step_non_negative      CHECK (step      >= 0)
);

-- The reservation names the group row it holds headroom on, so commit, release,
-- the expiry sweeper and §10.4's shutdown release resolve both rows from the
-- reservation alone. NULL for a reservation outside any group — every
-- reservation before D-116, and every one on an overflow route after it.
ALTER TABLE quota_reservation ADD COLUMN IF NOT EXISTS ramp_group TEXT;
ALTER TABLE quota_reservation ADD COLUMN IF NOT EXISTS group_day_index BIGINT;

ALTER TABLE quota_reservation DROP CONSTRAINT IF EXISTS quota_reservation_group_pair;
ALTER TABLE quota_reservation ADD CONSTRAINT quota_reservation_group_pair
    CHECK ((ramp_group IS NULL) = (group_day_index IS NULL));
