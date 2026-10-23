# The configuration file

Simmer reads one YAML file at startup. It is the whole configuration: there are
no command-line options for the server, and the only environment variable Simmer
itself reads is `SIMMER_CONFIG`.

## Where it is read from

`SIMMER_CONFIG` names the file. Without it, Simmer reads `simmer.yaml` in the
current directory. In the Docker image the current directory is `/app`, which
holds an example configuration for the repository's own development stack
rather than one you can use, so always mount your own file and set
`SIMMER_CONFIG`.

## The top level

```yaml
server:       # SMTP listeners, TLS, users and limits, see 04-server.md (required)
database:     # where counters are kept, see 05-database.md (required)
admin:        # the admin API and /metrics, see 12-admin-api.md (required)
logging:      # stdout log level and format, see 11-observability.md
telemetry:    # OpenTelemetry export, see 11-observability.md
link_proxy:   # the HTTP link forwarder, see 10-link-proxy.md
capture:      # the debugging capture, see 13-capture.md
dot_insensitive_domains: [gmail.com, googlemail.com]   # see 07-routes.md
default_ramp: main   # see 06-routing.md (required)
ramps:        # the routing setups: groups, senders, routes, see 06-routing.md (required)
  main: { ... }
```

[The configuration reference](reference.md) lists every key.

## Environment variables in values

Any value can include `${NAME}`, which is replaced by the environment variable
`NAME` when Simmer starts. Use it for secrets and for anything that differs
between environments:

```yaml
database:
  url: "${DATABASE_URL}"
routes:
  - name: warming
    identity:
      envelope_from: "bounce@${SIMMER_NEW_DOMAIN}"
```

- **Every referenced variable must be set.** An unset one stops startup, and the
  error lists every unset variable and where it is used. There is no default
  syntax such as `${NAME:-x}`. A variable set to the empty string is allowed,
  and is substituted as empty.
- **Only values, never keys.** `${NAME}` in a key is left alone.
- **The result is always a string.** A number or true/false key, such as a port
  or `metrics: true`, cannot come from a variable; write it in the file. Many
  values that look like strings are fine, and the starter configuration takes
  hostnames, domains, the start instant and the password hash this way.
- **It cannot change the document's structure.** Substitution happens after the
  YAML is parsed, so a password containing a newline or `key: value` stays a
  password.
- **`${` with no closing `}`** is left as it is, so a password that happens to
  contain `${` still works.

To switch a feature off in some environments only, a few keys treat an empty
value as off. The main one is `telemetry.endpoint`: `endpoint: "${OTEL_ENDPOINT}"`
with `OTEL_ENDPOINT=` empty exports nothing.

## Validation

Simmer checks the whole configuration before it opens a port, and refuses to
start if anything is wrong. It reports **every** problem in one go, each with
the path to the key, so you fix them all in one pass:

```
3 configuration violations:
  - server.auth.users[0].password_hash: is not an argon2 PHC string (expected it to begin '$argon2')
  - ramps.main.senders[1] (match 'newbrand.com').chain: chain[0] references route 'warmng', which is not defined
  - ramps.main.routes.warming.identity.envelope_from: has a domain that is not a literal: …
```

What it checks, beyond the types:

- **Unknown keys are refused.** A misspelt key would otherwise be ignored
  silently, and "the limit you set was ignored" is the worst failure a quota
  enforcer can have.
- **Keys removed in an earlier version** are refused with a message saying what
  replaced them.
- **References resolve.** Every route a chain names exists; every domain group a
  schedule override or `downstream_overrides` entry names exists; the
  `default_ramp` is declared.
- **Chains have a valid shape.** At most one overflow route, and it is last. The
  `default_chain` ends in an overflow route, unless the ramp says otherwise (see
  [Routing](06-routing.md#ramps-with-no-overflow-route)).
- **Every route's rewrite is safe to apply twice**, and its sending domain is
  fixed (see [Routes](07-routes.md#stability-why-a-rewrite-may-be-refused)).
- **Files exist.** The TLS certificate and key are loaded exactly as the listener
  will load them, and the capture directory must be writable.
- **Numbers are in range**: no negative caps, fractions between 0 and 1, pools of
  at least one connection.

Some problems are **warnings** instead: Simmer starts, and logs a `WARN` line
for each. For example, a TLS certificate that expires within 14 days or does not
cover `server.hostname`, an admin token shorter than 16 characters, a listener
that accepts unauthenticated mail while users exist, or the capture being on.
Read the startup log after every configuration change.

## Durations

Durations are a whole number and a unit: `500ms`, `30s`, `5m`, `2h`, `1d`. A bare
number such as `300` is refused, because whether it meant seconds or
milliseconds would be a guess.

## Changing the configuration

There is no reload. Edit the file and restart Simmer. Counters, pauses and
overrides are in the database and survive the restart. What can change without
a restart is listed in [The admin API](12-admin-api.md): pausing and resuming
routes, graduating them, and overriding a day's allowance.

A restart is cheap but not free: it closes the SMTP connections in progress
after letting them finish (see [Operations](14-operations.md#shutdown)), and
applications see a connection refused for the second or two before it is back.
