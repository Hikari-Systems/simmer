-- §7.7 (D-116) — the opt-in spool. Created on every instance, read only when a
-- ramp has `delivery: spool`: an empty table costs nothing, and a migration
-- that depended on configuration would make the schema differ by deployment.
--
-- State only. The body lives in the body store (D-117) under `body_ref`; a
-- 25 MiB body here would share the pool with §7.4's reservations, write twice
-- through WAL/TOAST and bloat every backup. `body_ref` is NULL once the body
-- has been deleted (a dead letter past `keep_body`, D-121).
--
-- A delivered message's row is deleted in the same transaction as its quota
-- commit (`commit_and_complete`); the states a row can be in are `queued`,
-- `leased` and `dead`. `lease_token` fences every write a lease holder makes,
-- so a holder whose lease expired under it cannot overwrite the next one's.
CREATE TABLE IF NOT EXISTS spool_message (
    id              UUID        PRIMARY KEY,
    ramp            TEXT        NOT NULL,
    domain_group    TEXT        NOT NULL,
    group_basis     TEXT        NOT NULL,
    state           TEXT        NOT NULL,
    next_attempt_at TIMESTAMPTZ NOT NULL,
    lease_owner     TEXT        NULL,
    lease_until     TIMESTAMPTZ NULL,
    lease_token     UUID        NULL,
    attempts        BIGINT      NOT NULL DEFAULT 0,
    pinned_route    TEXT        NULL,
    booked_route    TEXT        NULL,
    booked_group    TEXT        NULL,
    booked_tat      TIMESTAMPTZ NULL,
    received_at     TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    envelope        TEXT        NOT NULL,
    body_ref        TEXT        NULL,
    body_bytes      BIGINT      NOT NULL,
    body_sha256     BYTEA       NOT NULL,
    uuid_seed       UUID        NOT NULL,
    dead_reason     TEXT        NULL,
    last_code       BIGINT      NULL,
    last_error      TEXT        NULL,
    dead_at         TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS spool_message_due ON spool_message (state, next_attempt_at);
CREATE INDEX IF NOT EXISTS spool_message_lane ON spool_message (ramp, domain_group, state);

-- §9.3 for the spool (Phase 3): a paused ramp's messages are not claimed; a
-- draining ramp accepts nothing new. In the database rather than in memory so
-- every instance sees one answer (Q5).
CREATE TABLE IF NOT EXISTS spool_ramp_state (
    ramp       TEXT        PRIMARY KEY,
    paused     BOOLEAN     NOT NULL DEFAULT FALSE,
    draining   BOOLEAN     NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
