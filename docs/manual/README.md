# The simmer manual

Simmer is an SMTP relay that warms up a new sending domain's reputation. Your
application sends mail to Simmer instead of to its SMTP provider. Simmer decides
per message whether the new domain still has allowance today. If it does, Simmer
rewrites the message to the new domain's identity and forwards it to that
domain's provider; if not, the message goes out under your established domain
instead. The new domain's daily allowance grows on a schedule you set, until it
carries all of your mail. Then you point the application at the provider
directly and remove Simmer.

This manual is for people running Simmer. It starts from nothing and builds up
to every configuration option.

## Start here

1. [Getting started](01-getting-started.md): run Simmer with Docker or from the
   command line, with the starter configuration, and send a first message.
2. [How Simmer works](02-concepts.md): ramps, routes, chains, domain groups and
   the warm-up day. Read this before you change the starter configuration.
3. [The configuration file](03-configuration.md): its format, environment
   variables, and how startup validation reports mistakes.

## Configuring

| Chapter | Covers |
|---|---|
| [Listeners, TLS and SMTP users](04-server.md) | `server`: ports, TLS, AUTH, users and what they may send as, limits, timeouts |
| [The database](05-database.md) | `database`: PostgreSQL or SQL Server, and what happens when it is unreachable |
| [Routing: ramps, senders and domain groups](06-routing.md) | `default_ramp`, `ramps`, `domain_groups`, `senders`, `default_chain`, and what happens when no route can take a message |
| [Routes](07-routes.md) | `routes`: the downstream provider, the identity rewrite, templates, DNS preflight, recipient frequency |
| [The warm-up schedule](08-warmup.md) | `warmup`: daily caps, per-provider overrides, and spreading a route's traffic across the day |
| [Thread affinity](09-thread-affinity.md) | keeping a conversation on the route that started it |
| [The link proxy](10-link-proxy.md) | `link_proxy`: forwarding rewritten tracking and unsubscribe links |
| [Logs, metrics and OpenTelemetry](11-observability.md) | `logging`, `/metrics`, `telemetry`, and what to alert on |
| [The admin API](12-admin-api.md) | `admin`: health, route and quota state, pausing, overrides, and the dry run |
| [Capture and replay](13-capture.md) | `capture`: recording traffic for debugging, and `server replay` |

## Running it

- [Operations](14-operations.md): deploying, sizing, alerts, upgrades and
  shutdown.
- [Configuration reference](reference.md): every key, its type, its default,
  and the chapter that explains it.

## Elsewhere in the repository

- [`docs/SPEC.md`](../SPEC.md) is the authoritative specification. Where this
  manual and the spec disagree, the spec is right and the manual is a bug.
- [`DECISIONS.md`](../../DECISIONS.md) records why things are the way they are.
  The manual cites an entry (`D-047`) where the reason is worth knowing.
- [`docs/DEVELOPMENT.md`](../DEVELOPMENT.md) is for building, testing and
  releasing Simmer itself.
