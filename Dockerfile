FROM rust:1-bookworm AS builder

WORKDIR /app

# Cache dependencies with a stub crate before copying real source, so subsequent
# builds only recompile src/. No cargo-chef and no BuildKit cache mounts: for a
# single crate the stub achieves the same thing with nothing extra to install.
#
# Not alpine/musl — proc-macro crates need the dynamic linker at build time.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs
RUN cargo build --release --locked
# Remove the stub artifacts, or the real build links the empty binary. The
# leading wildcard matters: this crate produces both `server` (bin) and `simmer`
# (lib), and cargo mangles the lib name into several files.
RUN rm -rf target/release/server \
           target/release/deps/server* \
           target/release/deps/*simmer* \
           target/release/.fingerprint/simmer-*

COPY src ./src
COPY migrations ./migrations
COPY simmer.yaml simmer.acceptance.yaml ./
# COPY preserves source mtimes, which can be older than the cached stub
# artifacts; without this cargo decides nothing changed and ships the stub.
RUN find src -name '*.rs' -exec touch {} + \
    && cargo build --release --locked

# --locked throughout. The original reason was that hs-utils was a git-tag
# dependency, and a git tag is mutable — but D-060 removed it and the flag stays:
# an image is meant to be reproducible from a commit, and a build that is allowed
# to rewrite Cargo.lock is not. Without it the resolver would silently pick up
# whatever a semver-compatible release brought that morning.

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/server ./server
COPY --from=builder /app/migrations ./migrations
COPY --from=builder /app/simmer.yaml ./simmer.yaml
COPY --from=builder /app/simmer.acceptance.yaml ./simmer.acceptance.yaml

RUN useradd -r -u 1000 appuser
USER appuser

# SPEC.md §12.2: "The container must not EXPOSE port 25 to a host interface by
# default." Nothing is EXPOSEd here at all — the SMTP listener is reached over a
# container network, and publishing 25 to a host must be a deliberate act in the
# compose file or the run command. §2.3 governs: Simmer's listener is plaintext
# and accepts plaintext AUTH, so it belongs on a trusted internal segment only.

# Uses the binary's own healthcheck subcommand rather than curl, which this
# runtime image does not have. It probes GET /healthcheck — liveness only, no
# database round-trip.
#
# Deliberately NOT `["/app/server", "healthcheck", "deps"]`. Docker reacts to an
# unhealthy container by restarting it, and a restart does not fix a database
# outage — it would turn a dependency blip into a restart loop on top of an
# outage. §9.2's GET /health does include database reachability and is what
# operators and load balancers should read.
HEALTHCHECK --interval=10s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/app/server", "healthcheck"]

ENTRYPOINT ["/app/server"]

# ---------------------------------------------------------------------------

# The acceptance suite's bulk sender (`docs/ACCEPTANCE.md` §2). A separate stage
# so `runtime` — the image that ships — stays exactly what it was: this binary
# never reaches it, and only the `acceptance` compose profile builds this target.
#
# **It has to be last**, because it builds on `runtime`, and "last" is what an
# unpinned `docker build` produces. Anything building the shipped image must name
# `--target runtime` explicitly — `docker-compose.yml`'s `app` service does, and
# any deployment pipeline must too.
#
# It has to run inside the compose network because §2.3 says port 25 must not be
# published to a host interface, and `docker-compose.yml` deliberately does not
# publish it.
FROM runtime AS acceptance

COPY --from=builder /app/target/release/loadgen /app/loadgen
# The load tiers' counting SMTP sink (src/bin/sink.rs). Test tooling like the
# loadgen: in this stage only, never in `runtime`.
COPY --from=builder /app/target/release/sink /app/sink

# No HEALTHCHECK: the loadgen is a one-shot command, not a service. Compose runs
# it with `run --rm` and reads its stdout.
ENTRYPOINT ["/app/loadgen"]
