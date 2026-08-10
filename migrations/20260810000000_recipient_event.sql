-- §7.3's recipient-frequency events, and §11's "high-cardinality table".
-- Idempotent throughout, per the house convention.
--
-- Deferred out of phase 3 by D-029 so that its row shape could be decided
-- alongside §7.3's hashing and normalisation rather than guessed ahead of them.
-- The three decisions the shape encodes:
--
--   * `recipient_hash` is a **keyed hash, truncated to 16 bytes** — HMAC-SHA256
--     under the salt in `instance_config`. §7.3: "The stored key is a salted hash
--     of the normalised value, never plaintext … This bounds row size and avoids
--     the container accumulating a plaintext record of every address mailed."
--     BYTEA rather than hex text for the same row-size reason.
--   * `route` is part of the key because §7.3's constraint is per route: a
--     threshold is declared on one route and counts that route's own sends.
--   * There is no primary key and no id. The table is append-only and swept by
--     age; a surrogate key would be an index to maintain for nobody's benefit.
--
-- Rows are written **only** for routes that declare a `recipient_frequency`, and
-- only on a downstream 2xx (§7.4 phase 3). Storing events nothing will read would
-- be the opposite of §7.3's reason for hashing in the first place.
CREATE TABLE IF NOT EXISTS recipient_event (
    recipient_hash BYTEA       NOT NULL,
    route          TEXT        NOT NULL,
    sent_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- §11 asks for an index on `(recipient_hash, sent_at)`. `route` is included
-- because every read is "this route, this recipient, since this instant" — the
-- §7.3 window count — and it keeps that a pure index scan.
CREATE INDEX IF NOT EXISTS recipient_event_lookup_idx
    ON recipient_event (recipient_hash, route, sent_at);

-- §11's second index: "and on `sent_at` for the sweeper".
CREATE INDEX IF NOT EXISTS recipient_event_sent_at_idx
    ON recipient_event (sent_at);
