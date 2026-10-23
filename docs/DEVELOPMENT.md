# Developing simmer

This is for working on Simmer itself. To run it, see [the manual](manual/README.md).

`docs/SPEC.md` is the specification, and `DECISIONS.md` records where and why the
code departs from it. `CLAUDE.md` is a guide to the codebase written for AI
assistants, and it is a decent orientation for people too.

## Building and testing

```sh
docker compose up -d simmer-db   # the quota tests need a real Postgres
export DATABASE_URL=postgres://simmer:simmer@127.0.0.1:5433/simmer

cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check                 # licences, advisories, sources
docker compose up -d --build     # do this before pushing
```

For the local stack, copy `.env.example` to `.env` and fill it in; it lists every
variable the repository's `simmer.yaml` uses.

`DATABASE_URL` is needed only by `tests/quota*.rs`, which get a fresh database
per test. They use a real Postgres on purpose: the overshoot test is about what
two transactions do to one row at the same moment, which a fake cannot show.
Nothing in the Docker build needs a database, because queries are checked at
runtime rather than with `query_as!`.

**Every dependency comes from crates.io.** There are no git dependencies, and
`deny.toml` rejects them. If you need something from another internal crate,
copy it and record why in `DECISIONS.md` rather than adding the dependency
(D-060).

## The SQL Server build

Add `--no-default-features --features mssql` to any cargo command (D-084). Its
storage tests need a SQL Server, which the compose file has behind a profile:

```sh
docker compose --profile mssql up -d simmer-mssql-db
export MSSQL_URL='server=tcp:127.0.0.1,1434;user id=sa;password=Simmer-dev-1!;TrustServerCertificate=true'

cargo test --no-default-features --features mssql
cargo clippy --all-targets --no-default-features --features mssql -- -D warnings
cargo deny --no-default-features --features mssql check
docker build --target runtime --build-arg CARGO_FEATURES="--no-default-features --features mssql" .
```

`tests/store_conformance/` is the storage contract both backends must meet. A
change to either store belongs in that suite first. Race tests there warm their
connection pools and start behind a barrier, because contenders that each open a
fresh connection never actually overlap, and such a test passes even with the
lock removed.

## The acceptance suite

The §12.3 acceptance suite runs against its own stack of real mail servers and
is not part of `cargo test`. It takes Docker and a couple of minutes of
container restarts:

```sh
docker compose -f docker-compose.yml -f test/compose/acceptance.yml --profile acceptance up -d --build
cargo test --test acceptance -- --ignored --test-threads=1
```

It walks a warm-up across simulated days by moving `warmup.started` and
recreating the container, and it is the only test that proves Simmer's output is
byte for byte what an application configured to match it would send. It also
submits over port 587 with `STARTTLS`, verifying a certificate minted for each
run. Run it after any change to `src/rewrite/`, the chain walk or the
configuration schema. `docs/ACCEPTANCE.md` describes the setup, and
`DECISIONS.md` D-042 covers what tends to go wrong.

The same stack runs a dummy OpenTelemetry collector that writes everything it
receives to the `otel-out` volume, and one more test reads it back:

```sh
cargo test --test telemetry_compose -- --ignored --test-threads=1
docker compose --profile acceptance logs otel-collector
```

`SIMMER_CAPTURE=on` and `SIMMER_OTEL=on` run any compose tier with the capture
or the export switched on; see `CLAUDE.md`.

## CI

The `check` job runs `fmt`, `clippy`, `cargo test` and `cargo deny` on every push,
for both builds. The compose tiers do not run on push: the acceptance suite is
the manually started `acceptance.yml` workflow, so run `docker compose up -d
--build` yourself before pushing. It is the only thing that exercises binding
port 25 as a non-root user, the tmpfs that large messages spill to, and the
system certificate store that `required_verify` uses.

Anything that builds the shipped image must pass `--target runtime`. The
Dockerfile's last stage is the acceptance suite's load generator, and an
unpinned build produces that instead of the server.

## Releasing

The version is written in three places: the "Current version" line in
`README.md`, the same line in `DOCKERHUB.md`, and `Cargo.toml`. That is because
`git tag` sorts alphabetically and hides the newest tag in the middle of the
list. A `vX.Y.Z` tag releases the commit it points at:

```sh
git config core.hooksPath .githooks        # once per clone
# set all three to X.Y.Z, commit, push to main, and wait for build.yml
git tag -a vX.Y.Z -m "what this release is"
git push origin vX.Y.Z
```

`.githooks/pre-push` refuses the tag push if any of the three disagree with it,
and `release.yml` checks again. The release does not rebuild: it promotes the
images `build.yml` already built and tested for that commit on `main`, so the
tagged commit must be on `main`, and the release waits for that build if it is
still running. It tags both images on Docker Hub (`hikarisystems/simmer`) as
`:vX.Y.Z` and `:latest` for Postgres, and `:vX.Y.Z-mssql` and `:latest-mssql`
for SQL Server, does the same on GHCR, then creates the GitHub release from the
tag's message and the generated changelog. A release gets both images or
neither.

Docker Hub only carries releases. GHCR also gets `:<sha>`, `:<branch>` and
`:latest` (and the same with `-mssql`) from every branch push. `DOCKERHUB.md` is
pushed to the Docker Hub page on every push to `main`, so its links must be
absolute.

## Documentation

- `docs/manual/` is the user manual. Its example configurations in
  `docs/manual/examples/` are loaded by `tests/manual_examples.rs`, and so are
  the recipient hash test vectors, so a schema change that breaks either fails
  the tests.
- `README.md` and `DOCKERHUB.md` are short on purpose, and link to the manual.
- `docs/SPEC.md` changes only when its author asks; see `CLAUDE.md`.

## Layout

```
src/config/      the configuration schema, ${ENV_VAR} interpolation, startup validation
src/routing/     sender matching, domain groups, the chain walk, thread affinity, partial ramps
src/smtp/        listeners, the SMTP state machine, AUTH, the DATA buffer, every reply
src/downstream/  the outbound SMTP client, connection pools, the reply mapping
src/rewrite/     templates, headers, MIME, encodings, stability checking
src/quota/       the day index and the reserve/commit protocol, for both databases
src/frequency/   recipient normalisation, keyed hashing, the rolling window
src/preflight/   the DNS preflight
src/link_proxy/  the HTTP link forwarder
src/capture/     the debugging capture and `server replay`
src/telemetry/   the OpenTelemetry export
src/admin/       the admin API, dry run and /metrics
src/db/          one storage backend per build
src/relay.rs     decide, reserve, rewrite, relay, commit or release
src/metrics.rs   every metric and its help text
migrations/      Postgres schema; migrations-mssql/ is the same in T-SQL
tests/           integration tests; tests/support/ is a scripted fake provider
```

`CLAUDE.md` has a longer list, file by file.
