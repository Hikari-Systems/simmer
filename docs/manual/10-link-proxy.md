# The link proxy

When a route rewrites the tracking and unsubscribe links in your mail to a name
on the new domain, such as `click.newbrand.com`, something has to answer at that
name. If those links belong to a click-tracking service that only knows its own
hostname, the link proxy is that something: an HTTP forwarder that sends every
request to the service and relays the answer back.

```
browser ──https──▶ load balancer (TLS) ──http──▶ simmer link_proxy ──https──▶ tracking service
                   click.newbrand.com                                          link.esp.com/tracking
```

```yaml
link_proxy:
  listen: "0.0.0.0:80"
  upstream: "https://link.esp.com/tracking"   # a path prefix is optional
  allowed_cidrs: ["10.0.0.0/8"]               # the load balancer's subnets
  public_scheme: https                        # default
  max_request_bytes: 1048576                  # default, 1 MiB
  max_connections: 512                        # default
  timeouts:
    header_read: 10s          # default
    upstream_connect: 5s      # default
    upstream_response: 30s    # default
    idle: 60s                 # default
```

Without a `link_proxy` block, nothing is started.

| Key | Meaning |
|---|---|
| `listen` | Where to listen for plain HTTP. Must not clash with an SMTP listener or `admin.listen` |
| `upstream` | The service to forward to: scheme, host and optionally a path prefix. No query string |
| `allowed_cidrs` | Required. The addresses allowed to connect, which should be only your load balancer's. Others are disconnected before anything is read |
| `public_scheme` | What the load balancer terminates, `https` or `http`. Used for `X-Forwarded-Proto` and rewritten redirects |
| `max_request_bytes` | The largest request body accepted; more is refused `413` |
| `max_connections` | Client connections open at once |
| `timeouts.header_read` | A client must have sent its request headers within this |
| `timeouts.upstream_connect` | Connecting to the upstream |
| `timeouts.upstream_response` | Waiting for the upstream's response headers; `504` after this. Bodies then stream without a limit |
| `timeouts.idle` | How long an unused connection to the upstream is kept |

## What is forwarded

With the example above, `https://click.newbrand.com/test?abc=123` arrives at the
service as `GET /tracking/test?abc=123` with `Host: link.esp.com`.

- **Unchanged:** the method, path, query, body, cookies and every end-to-end
  header.
- **Added:** `X-Forwarded-For` (the load balancer's value, with the load
  balancer's own address appended), and `X-Forwarded-Host` and
  `X-Forwarded-Proto` where the load balancer did not set them; and
  `Via: 1.1 simmer`.
- **Rewritten on the way back**, to point at the name the browser used: a
  `Location` or `Content-Location` naming the upstream, a `Set-Cookie` whose
  `Domain=` is the upstream or a parent of it, and with a path prefix, a
  `Location: /tracking/…` or cookie `Path=/tracking…`. Anything naming another
  host, such as the redirect to the link's real destination, passes through
  untouched.
- **Redirects are not followed** and nothing is retried, since a one-click
  unsubscribe is a `POST`.

## What is refused

| Request | Answer |
|---|---|
| From outside `allowed_cidrs` | the connection is closed |
| HTTP/2 | not accepted; HTTP/1.0 and 1.1 only |
| `CONNECT` | `501` |
| A body over `max_request_bytes` | `413` |
| The upstream unreachable, or its certificate not trusted | `502` |
| No response headers within `upstream_response` | `504` |

WebSocket upgrades are forwarded as plain requests, never tunnelled. Every answer
Simmer gives itself carries `Cache-Control: no-store`. The upstream's
certificate is checked against the system's trusted roots. An `http://`
upstream is allowed, with a startup warning, since clicks, their cookies and
their tracking tokens then travel unencrypted.

## Privacy

Requests are logged with their method, path, status and timing only. The query
string, cookies and body are never logged, because a tracking token identifies a
recipient.

## Deploying it

- Put **only** the link proxy's port in the load balancer's target group, never
  an SMTP port.
- Point the load balancer's health check at the admin port's `/health`: the
  proxy has no path of its own, so everything it receives is forwarded.
- At the end of the warm-up, point the public name at the service directly (or
  at whatever serves it after the migration) and remove the block.

## Metrics

- `simmer_link_proxy_requests_total{status_class,origin}`: `origin` is `upstream`
  for the service's own answers, `proxy` for ones Simmer gave itself.
- `simmer_link_proxy_duration_seconds`: time to the response headers.
- `simmer_link_proxy_connections`: open client connections.
- `simmer_link_proxy_connections_refused_total{reason}`: `cidr` or `limit`.
