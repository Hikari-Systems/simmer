#!/bin/sh
# Configure Postfix from the environment, then run it in the foreground.
# See test/postfix/Dockerfile for the variants this produces.
#
#   PF_HOSTNAME        myhostname (and the name the TLS leaf covers)
#   PF_RELAYHOST       where everything is delivered, e.g. [trap-matrix]:1025
#   PF_MYNETWORKS      clients trusted to relay without AUTH (default loopback only)
#   PF_TLS             off | may | encrypt              (smtpd_tls_security_level)
#   PF_TLS_CERT        PEM chain, leaf first            (required unless PF_TLS=off)
#   PF_TLS_KEY         PEM key
#   PF_SASL_MECHS      e.g. "PLAIN LOGIN", or "LOGIN"   (unset: no SASL at all)
#   PF_SASL_USER       the one SASL account
#   PF_SASL_PASS       its password
#   PF_NO_8BITMIME     1: do not advertise 8BITMIME
#   PF_SMTPUTF8        yes | no                         (default yes)
#   PF_SIZE            message_size_limit in bytes
#   PF_RATE            smtpd_client_message_rate_limit per minute
#   PF_SMTPD_TIMEOUT   smtpd_timeout, e.g. 10s
#
# The outbound side, for a variant that submits to Simmer rather than receiving
# from it:
#
#   PF_CLIENT_TLS        none | secure                  (smtp_tls_security_level)
#   PF_CLIENT_TLS_MATCH  smtp_tls_secure_cert_match     (default nexthop)
#   PF_CLIENT_SASL_USER  log in to PF_RELAYHOST as this user
#   PF_CLIENT_SASL_PASS  with this password
set -eu

pc() { postconf -e "$@"; }

pc "myhostname = ${PF_HOSTNAME:-postfix.test}"
pc "mydestination ="
pc "relayhost = ${PF_RELAYHOST:?PF_RELAYHOST is required}"
pc "mynetworks = ${PF_MYNETWORKS:-127.0.0.0/8}"
pc "inet_interfaces = all"
pc "inet_protocols = ipv4"
pc "compatibility_level = 3.6"
pc "maillog_file = /dev/stdout"
case "${PF_CLIENT_TLS:-none}" in
  none)
    pc "smtp_tls_security_level = none"
    ;;
  secure)
    pc "smtp_tls_security_level = secure"
    pc "smtp_tls_secure_cert_match = ${PF_CLIENT_TLS_MATCH:-nexthop}"
    # The OS trust store — whatever update-ca-certificates put there, which in
    # the test stacks includes the per-run CA (test-trust). Both settings, and
    # named here rather than left to Debian's main.cf: its CApath is also
    # /etc/ssl/certs, so a negative control that only moved the CAfile would
    # still verify.
    pc "smtp_tls_CAfile = /etc/ssl/certs/ca-certificates.crt"
    pc "smtp_tls_CApath = /etc/ssl/certs"
    # No session cache: a resumed session reports the verification it inherited,
    # and a test reading "Verified" wants this connection's.
    pc "smtp_tls_session_cache_database ="
    pc "smtp_tls_loglevel = 1"
    ;;
  *) echo "PF_CLIENT_TLS must be none or secure" >&2; exit 1 ;;
esac
if [ -n "${PF_CLIENT_SASL_USER:-}" ]; then
  # texthash: read as-is, so no postmap step and no database type to pick.
  printf '%s %s:%s\n' "${PF_RELAYHOST}" "$PF_CLIENT_SASL_USER" \
    "${PF_CLIENT_SASL_PASS:?PF_CLIENT_SASL_PASS is required with PF_CLIENT_SASL_USER}" \
    > /etc/postfix/sasl_passwd
  chmod 0600 /etc/postfix/sasl_passwd
  pc "smtp_sasl_auth_enable = yes"
  pc "smtp_sasl_password_maps = texthash:/etc/postfix/sasl_passwd"
  pc "smtp_sasl_security_options = noanonymous"
  pc "smtp_sasl_tls_security_options = noanonymous"
fi
pc "smtpd_relay_restrictions = permit_mynetworks permit_sasl_authenticated reject_unauth_destination"
pc "smtputf8_enable = ${PF_SMTPUTF8:-yes}"
# No chroot: the sasldb and the TLS files then live where the config says, and a
# test image gains nothing from the isolation.
postconf -F '*/*/chroot = n'

case "${PF_TLS:-off}" in
  off)
    pc "smtpd_tls_security_level = none"
    ;;
  may|encrypt)
    pc "smtpd_tls_security_level = ${PF_TLS}"
    pc "smtpd_tls_chain_files = ${PF_TLS_KEY:?PF_TLS_KEY is required with TLS}, ${PF_TLS_CERT:?PF_TLS_CERT is required with TLS}"
    # The negotiated protocol in the Received: header, so a test reads the
    # encryption off the message rather than trusting a log level.
    pc "smtpd_tls_received_header = yes"
    ;;
  *) echo "PF_TLS must be off, may or encrypt" >&2; exit 1 ;;
esac

if [ -n "${PF_SASL_MECHS:-}" ]; then
  mkdir -p /etc/postfix/sasl
  printf 'pwcheck_method: auxprop\nauxprop_plugin: sasldb\nmech_list: %s\n' "$PF_SASL_MECHS" \
    > /etc/postfix/sasl/smtpd.conf
  echo "${PF_SASL_PASS:?PF_SASL_PASS is required with SASL}" \
    | saslpasswd2 -p -c -u "${PF_HOSTNAME:-postfix.test}" "${PF_SASL_USER:?PF_SASL_USER is required with SASL}"
  chown root:postfix /etc/sasldb2
  chmod 0640 /etc/sasldb2
  pc "smtpd_sasl_auth_enable = yes"
  pc "smtpd_sasl_type = cyrus"
  pc "smtpd_sasl_path = smtpd"
  pc "smtpd_sasl_local_domain = ${PF_HOSTNAME:-postfix.test}"
  pc "smtpd_sasl_security_options = noanonymous"
  # AUTH over plaintext is allowed only where TLS is not required: the
  # login-only variant speaks plaintext so F9 is about mechanisms alone.
  if [ "${PF_TLS:-off}" = "encrypt" ]; then pc "smtpd_tls_auth_only = yes"; fi
fi

if [ "${PF_NO_8BITMIME:-0}" = "1" ]; then
  pc "smtpd_discard_ehlo_keywords = 8bitmime"
fi
if [ -n "${PF_SIZE:-}" ]; then pc "message_size_limit = ${PF_SIZE}"; fi
if [ -n "${PF_RATE:-}" ]; then
  pc "smtpd_client_message_rate_limit = ${PF_RATE}"
  pc "anvil_rate_time_unit = 60s"
  # Rate limits are not applied to mynetworks clients by default.
  pc "smtpd_client_event_limit_exceptions ="
fi
if [ -n "${PF_SMTPD_TIMEOUT:-}" ]; then pc "smtpd_timeout = ${PF_SMTPD_TIMEOUT}"; fi

postfix check
exec postfix start-fg
