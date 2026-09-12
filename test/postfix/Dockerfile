# A real MTA as a downstream for the T2 server matrix (docs/TESTING.md, the test
# programme's step 3). One image, configured entirely by environment by
# entrypoint.sh, so each variant in test/compose/matrix.yml is a service with a
# different environment rather than a different image:
#
#   postfix-plain       no AUTH, no TLS
#   postfix-tls-auth    STARTTLS required, SASL PLAIN+LOGIN, a leaf from the test CA
#   postfix-login-only  SASL LOGIN only (finding F9)
#   postfix-strict      no 8BITMIME, no SMTPUTF8, a small message_size_limit
#   postfix-ratelimit   a per-client message rate limit and a short smtpd_timeout
#
# Everything it accepts is relayed to a Mailpit trap, so arrival is asserted the
# same way for every variant. Cyrus SASL with a sasldb2 file rather than Dovecot:
# the smallest thing that gives Postfix real SASL mechanisms to advertise.
#
# Test tooling only; never part of the shipped image.
FROM debian:bookworm-slim

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        postfix libsasl2-modules sasl2-bin ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY test/postfix/entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh

EXPOSE 25
ENTRYPOINT ["/entrypoint.sh"]
