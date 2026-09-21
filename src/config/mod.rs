//! Configuration: the `SPEC.md` §4.1 schema, `${ENV_VAR}` interpolation (§4), and
//! startup validation (§4.2).
//!
//! Format is YAML per §4, *not* the hikari-systems `config.json` + `/sandbox`
//! layering used by the data services. See `DECISIONS.md` D-004 for why.
//!
//! Every struct is `deny_unknown_fields`. §2.2 says there is no hot reload, so a
//! typo'd key would otherwise sit silently in the file until someone wondered why
//! a setting had no effect — and for a component whose whole job is to *not*
//! exceed a limit, "the limit you set was ignored" is the worst failure mode
//! available.

pub mod duration;
pub mod interpolate;
pub mod validate;

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

pub use validate::{Violation, ViolationList};

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub database: Database,
    pub admin: Admin,
    #[serde(default)]
    pub logging: Logging,

    /// §5.7 (D-083) — the optional HTTP forwarder for tracking and unsubscribe
    /// links. Absent means no listener and nothing started.
    #[serde(default)]
    pub link_proxy: Option<LinkProxy>,

    /// D-085 — the optional message capture sink. Absent means nothing is
    /// written and no task is started.
    ///
    /// A **debugging mode**, not a spool: see `src/capture/mod.rs` for the four
    /// properties that keep that true, and `DECISIONS.md` D-085 for what it
    /// costs against §7.3.
    #[serde(default)]
    pub capture: Option<Capture>,

    pub domain_groups: Vec<DomainGroup>,
    pub senders: Vec<SenderRule>,

    /// §3.2 step 1. Required unless `strict_senders` is true (§4.2).
    #[serde(default)]
    pub default_chain: Option<Vec<String>>,
    #[serde(default)]
    pub strict_senders: bool,

    /// §3.2 step 2a (D-090) — walk the route that emitted a message ID this
    /// message refers to first. Off by default. When on, §4.2 requires every
    /// route in every chain to set a `Message-ID:` with a literal domain, and
    /// those domains to differ within each chain.
    #[serde(default)]
    pub thread_affinity: bool,

    /// §10.3. Not shown in the §4.1 example; placed at the top level because it
    /// is a policy about the whole engine rather than about one route.
    #[serde(default)]
    pub exhausted_chain_reply: ExhaustedChainReply,

    /// §7.3. The spec defaults this to "the `google` group's domains", which
    /// couples behaviour to a configuration-defined group name that may not
    /// exist. Made explicit instead — see `DECISIONS.md` D-010.
    #[serde(default = "default_dot_insensitive_domains")]
    pub dot_insensitive_domains: Vec<String>,

    pub routes: Vec<Route>,
}

fn default_dot_insensitive_domains() -> Vec<String> {
    ["gmail.com", "googlemail.com"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// §10.3 — `451` by default. See §14.1: Simmer must not emit a reply that causes
/// a client to record permanent state about a recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
pub enum ExhaustedChainReply {
    #[default]
    #[serde(rename = "451")]
    Temporary,
    #[serde(rename = "550")]
    Permanent,
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// §5.1. One entry per port, each with its own TLS and AUTH policy — 25, 465
    /// and 587 have genuinely different rules and one switch cannot express them
    /// (D-070). Replaced `listen`, which `validate::removed_keys` still names.
    pub listeners: Vec<ListenerConfig>,
    /// Used in the EHLO banner and `Received:` headers (§6.1 step 8), and the
    /// name the §5.1 certificate is expected to cover.
    pub hostname: String,
    pub max_message_bytes: u64,
    /// §5.5's recipient ceiling. Vestigial since D-047: a transaction may carry
    /// exactly one recipient, so no value above 1 is reachable. Kept because
    /// §4.1 mandates the key; §4.2 warns when it is set above 1.
    pub max_recipients: usize,
    pub max_concurrent_sessions: usize,
    pub allowed_cidrs: Vec<String>,
    pub timeouts: ServerTimeouts,
    /// §5.1's certificate. Required when any listener's `tls` is not `off`.
    #[serde(default)]
    pub tls: Option<ServerTls>,
    pub auth: Auth,
}

/// One inbound listener (§5.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    /// `host:port`; validated as a `SocketAddr` in §4.2.
    pub address: String,
    /// Absent means the port's RFC default — see [`ListenerConfig::tls_mode`].
    #[serde(default)]
    pub tls: Option<IngressTls>,
    /// Absent means the port's RFC default — see [`ListenerConfig::auth_mode`].
    #[serde(default)]
    pub auth: Option<IngressAuth>,
}

/// §5.1 inbound TLS, per listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressTls {
    /// Plaintext only; `STARTTLS` is not advertised.
    Off,
    /// `STARTTLS` advertised and accepted; a client may decline it.
    Starttls,
    /// `STARTTLS` advertised; nothing but `EHLO`, `NOOP`, `RSET`, `QUIT` and
    /// `STARTTLS` itself is served until the handshake completes (RFC 3207 §4).
    StarttlsRequired,
    /// TLS from the first byte. Never advertises `STARTTLS` (RFC 8314 §3.3).
    Implicit,
}

impl IngressTls {
    pub fn as_str(self) -> &'static str {
        match self {
            IngressTls::Off => "off",
            IngressTls::Starttls => "starttls",
            IngressTls::StarttlsRequired => "starttls_required",
            IngressTls::Implicit => "implicit",
        }
    }

    /// Whether a session on this listener can ever be encrypted.
    pub fn can_encrypt(self) -> bool {
        self != IngressTls::Off
    }
}

/// §5.3, per listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressAuth {
    /// `AUTH` is not advertised, and the command is `503`.
    Disabled,
    /// Advertised when usable; an unauthenticated session may still send.
    Optional,
    /// `MAIL FROM` before a successful `AUTH` is `530 5.7.0`.
    Required,
}

impl IngressAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            IngressAuth::Disabled => "disabled",
            IngressAuth::Optional => "optional",
            IngressAuth::Required => "required",
        }
    }
}

impl ListenerConfig {
    fn port(&self) -> Option<u16> {
        self.address
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|a| a.port())
    }

    /// The effective TLS mode: the configured one, or the port's RFC default.
    ///
    /// 465 is RFC 8314 submissions, implicit TLS. 587 is RFC 6409 submission,
    /// which with a certificate available means `STARTTLS` before anything else.
    /// Everything else — 25 included — defaults to `off`, which is what Simmer
    /// did before listeners existed, so a config naming only an address keeps
    /// its old meaning.
    pub fn tls_mode(&self) -> IngressTls {
        self.tls.unwrap_or(match self.port() {
            Some(465) => IngressTls::Implicit,
            Some(587) => IngressTls::StarttlsRequired,
            _ => IngressTls::Off,
        })
    }

    /// The effective AUTH mode: the configured one, or the port's RFC default.
    /// 465 and 587 are submission ports and require it; 25 is transfer, where
    /// RFC 5321 makes it optional (D-070).
    pub fn auth_mode(&self) -> IngressAuth {
        self.auth.unwrap_or(match self.port() {
            Some(465) | Some(587) => IngressAuth::Required,
            _ => IngressAuth::Optional,
        })
    }
}

/// §5.1's certificate and key, PEM, read once at startup. No ACME, and no
/// reload: rotation is a restart (§2.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerTls {
    /// The chain, leaf first.
    pub certificate: String,
    pub private_key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerTimeouts {
    #[serde(deserialize_with = "duration::deserialize")]
    pub command: Duration,
    #[serde(deserialize_with = "duration::deserialize")]
    pub data: Duration,
    #[serde(deserialize_with = "duration::deserialize")]
    pub session: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// Whether `AUTH` may be used on a session that is not encrypted. Default
    /// false: plaintext credentials are refused unless this says otherwise.
    ///
    /// Before D-070 §4.2 required this to be *true*, because there was no
    /// inbound TLS and AUTH was otherwise unusable. It now means what its name
    /// says. Whether AUTH is required at all is per listener
    /// ([`ListenerConfig::auth_mode`]); the global `required` key it replaced is
    /// named by `validate::removed_keys`.
    #[serde(default)]
    pub allow_insecure_auth: bool,
    #[serde(default = "default_mechanisms")]
    pub mechanisms: Vec<Mechanism>,
    #[serde(default)]
    pub users: Vec<User>,
}

fn default_mechanisms() -> Vec<Mechanism> {
    vec![Mechanism::Plain, Mechanism::Login]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Mechanism {
    #[serde(rename = "PLAIN")]
    Plain,
    #[serde(rename = "LOGIN")]
    Login,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub username: String,
    /// argon2id (§5.3). Never logged — see the `Debug` impl below.
    pub password_hash: String,
    /// §5.3's sender ACL (D-071). Required: a user with no grants could
    /// authenticate and then send nothing, which is a configuration mistake
    /// better caught at startup than at the first refused message.
    pub grants: Grants,
}

// A derived Debug would put a password hash into any error or trace that
// happens to render the config. §9.5 says message bodies are never logged;
// the same care is owed to credentials.
impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("username", &self.username)
            .field("password_hash", &"<redacted>")
            .field("grants", &self.grants)
            .finish()
    }
}

/// What an authenticated user may do (D-071), shaped after Slater's
/// per-resource capability lists. Default deny: anything not granted is refused.
///
/// `deny_unknown_fields` is deliberate and is where this differs from Slater,
/// which ignores an unrecognised capability. Slater's file is reloaded at runtime
/// and a bad edit must not take it down; this one is read once at startup, and a
/// misspelt grant that silently grants nothing is D-013's worst failure mode.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grants {
    /// Sender identities this user may present, in §5.4's pattern grammar:
    /// exact domain, `*.subdomain`, or full address. Checked against the
    /// envelope sender at `MAIL FROM` and the `From:` header at the final dot.
    pub send_as: Vec<String>,
}

// ---------------------------------------------------------------------------
// database / admin / logging
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// A libpq-style URL. §4.1 specifies a URL rather than the discrete fields
    /// the house `DbConfig` wants, so the pool is built directly — see
    /// `DECISIONS.md` D-005.
    pub url: String,
    #[serde(default = "default_db_max_connections")]
    pub max_connections: u32,
    #[serde(deserialize_with = "duration::deserialize")]
    pub connect_timeout: Duration,
    /// §7.5 — default true. A quota enforcer that stops enforcing under failure
    /// provides no guarantee at all.
    #[serde(default = "yes")]
    pub fail_closed: bool,
}

fn default_db_max_connections() -> u32 {
    10
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The URL carries the password.
        f.debug_struct("Database")
            .field("url", &"<redacted>")
            .field("max_connections", &self.max_connections)
            .field("connect_timeout", &self.connect_timeout)
            .field("fail_closed", &self.fail_closed)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    pub listen: String,
    /// §4.1's single shared token. Still the ordinary spelling, and still what
    /// the example configuration uses; it is the token named
    /// [`Admin::DEFAULT_TOKEN_NAME`].
    #[serde(default)]
    pub auth_token: Option<String>,
    /// O-11 — §9.3 logs every mutation "with the acting token's identifier",
    /// which a single scalar cannot supply. Named tokens give that line a name
    /// an operator recognises without changing anything else about the
    /// mechanism. See `DECISIONS.md` D-053.
    #[serde(default)]
    pub tokens: Vec<AdminToken>,
}

/// One named §9.3 credential.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminToken {
    /// What the audit log calls whoever presents this token.
    pub name: String,
    pub token: String,
}

impl fmt::Debug for AdminToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminToken")
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Admin {
    /// The name `auth_token` is logged under. Chosen rather than `admin` so that
    /// an audit trail makes the difference between "the shared token" and a
    /// named one visible at a glance.
    pub const DEFAULT_TOKEN_NAME: &'static str = "default";

    /// Every configured credential as `(name, token)`, `auth_token` first.
    ///
    /// §4.2 guarantees this is non-empty, that the names are unique, and that no
    /// two entries share a secret — without that last rule the audit line would
    /// be a coin flip between two names.
    pub fn credentials(&self) -> Vec<(&str, &str)> {
        let mut out = Vec::with_capacity(self.tokens.len() + 1);
        if let Some(token) = self.auth_token.as_deref() {
            out.push((Self::DEFAULT_TOKEN_NAME, token));
        }
        out.extend(
            self.tokens
                .iter()
                .map(|t| (t.name.as_str(), t.token.as_str())),
        );
        out
    }
}

impl fmt::Debug for Admin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admin")
            .field("listen", &self.listen)
            .field("auth_token", &"<redacted>")
            .field("tokens", &self.tokens)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logging {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub format: LogFormat,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

/// §9.5 requires structured JSON. `text` exists for local development only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Json,
    Text,
}

// ---------------------------------------------------------------------------
// domain groups and senders
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainGroup {
    pub name: String,
    /// Literal recipient domains. Exactly one group must contain `*` (§3.1, §4.2).
    /// No MX-based or heuristic grouping — see §14.3.
    pub domains: Vec<String>,
}

impl DomainGroup {
    pub fn is_catchall(&self) -> bool {
        self.domains.iter().any(|d| d == "*")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SenderRule {
    /// §5.4: exact domain, `*.subdomain` wildcard, or full address.
    #[serde(rename = "match")]
    pub pattern: String,
    pub match_on: MatchOn,
    pub chain: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOn {
    Envelope,
    FromHeader,
    Either,
}

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    /// §3.1. An overflow route carries no warm-up schedule, is never
    /// quota-limited, and must be last in any chain containing it.
    #[serde(default)]
    pub overflow: bool,
    pub downstream: Downstream,
    pub identity: Identity,
    /// §6.7. Absent means disabled — see `DECISIONS.md` D-012. When present,
    /// `enabled` defaults to true.
    #[serde(default)]
    pub preflight: Option<Preflight>,
    /// §4.2: required on a non-overflow route, forbidden on an overflow route.
    #[serde(default)]
    pub warmup: Option<Warmup>,
    #[serde(default)]
    pub recipient_frequency: Option<RecipientFrequency>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Downstream {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub tls: TlsMode,
    #[serde(default)]
    pub auth: Option<DownstreamAuth>,
    pub pool: PoolConfig,
    #[serde(default)]
    pub timeouts: Option<DownstreamTimeouts>,

    /// Does this downstream support `SMTPUTF8` (RFC 6531)?
    ///
    /// §5.2 says `EHLO` advertises `SMTPUTF8` unconditionally, but Simmer cannot
    /// honour that promise for a downstream that does not implement it, and it
    /// cannot discover which downstream it will use until after the client has
    /// committed to an address — with `match_on: from_header` the route is not
    /// known until the final dot. Declaring it here is the only way to answer at
    /// `EHLO` time. Default `false`, so the capability is never claimed by
    /// accident. See `DECISIONS.md` D-018.
    #[serde(default)]
    pub smtputf8: bool,

    /// Is this downstream 8-bit clean even though its `EHLO` does not say
    /// `8BITMIME`? (D-074, finding F13.)
    ///
    /// Postal, the production downstream, advertises no `8BITMIME` and accepts
    /// 8-bit bodies anyway. RFC 6152 lets a relay facing a server without the
    /// extension either convert to 7-bit or refuse; Simmer does neither to a
    /// body that is 7-bit already, and for a genuinely 8-bit one it needs to be
    /// *told* the server is clean, since `EHLO` says otherwise. Default `false`:
    /// an undeclared downstream keeps `451 4.3.5` and the config-error metric,
    /// which is loud, over silently sending 8-bit data to a server that may not
    /// take it.
    #[serde(default)]
    pub assume_8bitmime: bool,
}

/// §8.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Off,
    Opportunistic,
    /// STARTTLS mandatory, certificate not validated (self-signed internal Postal).
    Required,
    #[default]
    RequiredVerify,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownstreamAuth {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for DownstreamAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownstreamAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// §8.3.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolConfig {
    pub max_connections: usize,
    #[serde(deserialize_with = "duration::deserialize")]
    pub idle_ttl: Duration,
    pub max_messages_per_connection: u64,
}

/// §8.4. Per-stage; the sum should sit comfortably below the client's own
/// timeout (documented in the README).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownstreamTimeouts {
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub connect: Option<Duration>,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub command: Option<Duration>,
    #[serde(default, deserialize_with = "duration::deserialize_opt")]
    pub data: Option<Duration>,
}

// ---------------------------------------------------------------------------
// link_proxy (D-083)
// ---------------------------------------------------------------------------

/// D-083. An HTTP/1.x listener that forwards every request, unchanged, to one
/// upstream. It sits behind a TLS-terminating load balancer, which is why it
/// speaks plain HTTP and why `allowed_cidrs` has no default: the only peers it
/// should ever see are that load balancer's addresses.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkProxy {
    pub listen: String,
    /// Scheme, authority and an optional path prefix, e.g.
    /// `https://link.esp.example/tracking`. The request's path is appended to
    /// the prefix and its query kept unchanged, so `/test?abc=123` goes to
    /// `/tracking/test?abc=123`. No query.
    pub upstream: String,
    /// What the load balancer terminates. Sent as `X-Forwarded-Proto` when the
    /// load balancer did not set one, and used as the scheme of a rewritten
    /// `Location`.
    #[serde(default)]
    pub public_scheme: PublicScheme,
    pub allowed_cidrs: Vec<String>,
    #[serde(default = "default_link_proxy_max_request_bytes")]
    pub max_request_bytes: u64,
    #[serde(default = "default_link_proxy_max_connections")]
    pub max_connections: usize,
    #[serde(default)]
    pub timeouts: LinkProxyTimeouts,
}

fn default_link_proxy_max_request_bytes() -> u64 {
    // A tracking click is a GET and a one-click unsubscribe (RFC 8058) is a
    // one-line form POST; 1 MiB is generous for both.
    1024 * 1024
}

fn default_link_proxy_max_connections() -> usize {
    512
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PublicScheme {
    Http,
    #[default]
    Https,
}

impl PublicScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkProxyTimeouts {
    /// A client that has not finished sending request headers by then is
    /// disconnected.
    #[serde(
        default = "default_header_read",
        deserialize_with = "duration::deserialize"
    )]
    pub header_read: Duration,
    #[serde(
        default = "default_upstream_connect",
        deserialize_with = "duration::deserialize"
    )]
    pub upstream_connect: Duration,
    /// Until the upstream's response *headers*; `504` after. Bodies stream.
    #[serde(
        default = "default_upstream_response",
        deserialize_with = "duration::deserialize"
    )]
    pub upstream_response: Duration,
    /// Idle pooled upstream connections are closed after this.
    #[serde(default = "default_idle", deserialize_with = "duration::deserialize")]
    pub idle: Duration,
}

impl Default for LinkProxyTimeouts {
    fn default() -> Self {
        Self {
            header_read: default_header_read(),
            upstream_connect: default_upstream_connect(),
            upstream_response: default_upstream_response(),
            idle: default_idle(),
        }
    }
}

fn default_header_read() -> Duration {
    Duration::from_secs(10)
}
fn default_upstream_connect() -> Duration {
    Duration::from_secs(5)
}
fn default_upstream_response() -> Duration {
    Duration::from_secs(30)
}
fn default_idle() -> Duration {
    Duration::from_secs(60)
}

// ---------------------------------------------------------------------------
// capture (D-085)
// ---------------------------------------------------------------------------

/// D-085. Where received messages are appended, one JSON object per line, in
/// files of ten minutes each.
///
/// Presence is the opt-in, like `link_proxy` and §6.7's `preflight`. Every
/// other key has a default, because the only one an operator must think about
/// is where it goes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    /// Absolute path to the directory holding the bucket files. Created `0700`
    /// if absent; the files inside are `0600`.
    ///
    /// The shipped container has a read-only root filesystem, so this must name
    /// a writable mount owned by UID 1000 — see `docker-compose.yml`.
    pub directory: String,

    /// Above this, the envelope is recorded and the body is not
    /// (`body_omitted: true`). Default 1 MiB, which is §8.1's spill threshold:
    /// an inlined body then never exceeds what Simmer was already holding in
    /// memory for that message anyway.
    #[serde(default = "default_capture_max_body_bytes")]
    pub max_body_bytes: u64,

    /// Bucket files whose window ended longer ago than this are deleted by the
    /// sweeper. Default 24h. It must exceed one bucket, or the sweeper would
    /// delete the file being written.
    #[serde(
        default = "default_capture_retention",
        deserialize_with = "duration::deserialize"
    )]
    pub retention: Duration,

    /// What a failed capture write does to the client's reply. Default
    /// `continue` — §14.1's principle cuts both ways, and a full disk must not
    /// stop mail for a facility that exists to help diagnose it.
    ///
    /// `defer` answers `451` instead, **before anything is relayed**. That is
    /// only coherent because the capture happens before the downstream
    /// conversation: a `451` raised afterwards would defer a message the
    /// downstream had already accepted, and the client's retry would deliver it
    /// twice (§10.2, D-068).
    #[serde(default)]
    pub on_error: CaptureOnError,

    /// How many records may be queued for the writer before further ones are
    /// dropped. The queue is never awaited on the message path — a slow disk
    /// must not become backpressure on the relay — so a full queue drops and
    /// counts rather than waiting.
    #[serde(default = "default_capture_queue_depth")]
    pub queue_depth: usize,

    /// A second bound on the same queue, in bytes of serialised record.
    ///
    /// `queue_depth` alone is not a bound on memory: a 25 MiB message becomes a
    /// ~34 MiB base64 line, and a thousand of those queued is 34 GiB. Default
    /// 64 MiB.
    #[serde(default = "default_capture_queue_bytes")]
    pub max_queue_bytes: u64,
}

fn default_capture_max_body_bytes() -> u64 {
    // crate::smtp::buffer::SPILL_THRESHOLD, not imported to keep `config` free
    // of a dependency on `smtp`. The two are asserted equal in that module.
    1024 * 1024
}

fn default_capture_retention() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn default_capture_queue_depth() -> usize {
    1024
}

fn default_capture_queue_bytes() -> u64 {
    64 * 1024 * 1024
}

/// D-085 — what a failed capture write does to the client's reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOnError {
    /// Log, count, and leave the reply alone. The default.
    #[default]
    Continue,
    /// Answer `451` and relay nothing. A deferral, never a `5xx` (§14.1).
    Defer,
}

impl CaptureOnError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Defer => "defer",
        }
    }
}

// ---------------------------------------------------------------------------
// identity (the rewrite spec)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// §6.2. Templated (§6.3).
    pub envelope_from: String,
    /// Order-preserving: header order is part of the byte-equivalence the §12.3
    /// acceptance test asserts, so a `BTreeMap` or `HashMap` would not do.
    #[serde(default)]
    pub set_headers: OrderedHeaders,
    /// §6.6. Headers whose instability is migration-only and acknowledged.
    /// Naming an identity field here is a fatal error (§4.2).
    #[serde(default)]
    pub unstable_headers: Vec<String>,
    /// Applied *before* `set_headers`, so a header may be replaced by naming it
    /// in both (§6.2).
    #[serde(default)]
    pub remove_headers: Vec<String>,
    /// §6.4. `text/*` parts only, applied in order.
    #[serde(default)]
    pub body_rewrites: Vec<BodyRewrite>,
    /// D-089 — a regex replacement over one named header's value, applied
    /// after `remove_headers` and before `set_headers`, in order. §6.2.
    #[serde(default)]
    pub header_rewrites: Vec<HeaderRewrite>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyRewrite {
    pub pattern: String,
    pub replacement: String,
}

/// One `header_rewrites` entry (D-089): `body_rewrites`' shape plus the header
/// it applies to. Every instance of the header is rewritten; a header no entry
/// names keeps its original bytes (D-039).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRewrite {
    pub header: String,
    pub pattern: String,
    pub replacement: String,
}

/// A YAML mapping kept in document order.
///
/// `serde_yaml_ng::Mapping` would also preserve order, but carrying it around
/// means every consumer re-does the "is this a string?" dance. This gives the
/// rest of the engine `&[(String, String)]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderedHeaders(pub Vec<(String, String)>);

impl OrderedHeaders {
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Case-insensitive lookup — header field names are case-insensitive per
    /// RFC 5322, and a config saying `Reply-To` must match a rule saying `reply-to`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

impl<'de> Deserialize<'de> for OrderedHeaders {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = OrderedHeaders;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a mapping of header name to template string")
            }

            fn visit_map<M>(self, mut map: M) -> Result<OrderedHeaders, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut out = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    out.push((k, v));
                }
                Ok(OrderedHeaders(out))
            }
        }

        d.deserialize_map(V)
    }
}

// ---------------------------------------------------------------------------
// preflight / warmup / recipient frequency
// ---------------------------------------------------------------------------

/// §6.7.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preflight {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub spf_include: Option<String>,
    #[serde(default)]
    pub dkim_selector: Option<String>,
    #[serde(default)]
    pub require_dmarc: bool,
    /// When true a failing check makes the route ineligible, so traffic falls to
    /// the next link in the chain. Default false: a DNS blip must not become an
    /// outage.
    #[serde(default)]
    pub strict: bool,
}

/// §7.2.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Warmup {
    /// RFC 3339 instant, must be explicit. `day_index` is elapsed duration from
    /// here, *not* calendar arithmetic — immune to DST, never a 23- or 25-hour
    /// window.
    pub started: chrono::DateTime<chrono::Utc>,
    pub schedule: Schedule,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// Signed on purpose. §4.2 makes "contains a negative value" a *validation*
    /// violation, and typing this `u64` would instead make it a parse error —
    /// which aborts on the first bad element and so cannot satisfy §4.2's
    /// "report all violations". Validation guarantees non-negative thereafter.
    pub default: Vec<i64>,
    /// Keyed by domain group name. §4.2 rejects a key naming a nonexistent group.
    #[serde(default)]
    pub overrides: BTreeMap<String, Vec<i64>>,
    /// D-091's partial ramp: on day `i`, only `share[i]` of the messages that
    /// reach this route in the walk are offered to it, and the rest skip to the
    /// next link. The cap is then reached later in the day, and the route's
    /// volume is spread across it rather than spent in its first hours.
    ///
    /// Indexed like `default`, for every domain group. Past its end the share is
    /// **1** — not §7.2's "final value repeats", which would leave a list ending
    /// below 1 throttling the route forever. Empty, the default, is no partial
    /// ramp at all. Each value is in `(0, 1]`; §4.2 refuses anything else.
    ///
    /// Which messages are offered is a keyed hash, not a dice roll
    /// (`routing::partial`), so every instance and §9.4's dry run agree.
    #[serde(default)]
    pub share: Vec<f64>,
}

impl Schedule {
    /// The allowance for a group on a given day index. When `day_index` exceeds
    /// the array bounds the **final value repeats indefinitely** (§7.2): routes
    /// do not auto-graduate to uncapped.
    ///
    /// Returns `None` only for an empty series, which §4.2 has already rejected.
    pub fn allowance_for(&self, group: &str, day_index: u64) -> Option<u64> {
        let series = self.overrides.get(group).unwrap_or(&self.default);
        if series.is_empty() {
            return None;
        }
        let idx = usize::try_from(day_index)
            .unwrap_or(usize::MAX)
            .min(series.len() - 1);
        series.get(idx).map(|v| u64::try_from(*v).unwrap_or(0))
    }

    /// D-091: the share of traffic offered on `day_index`, or `None` for all of
    /// it — before the ramp starts, past the end of `share`, or at a share of 1.
    pub fn share_for(&self, day_index: i64) -> Option<f64> {
        let idx = usize::try_from(day_index).ok()?;
        self.share.get(idx).copied().filter(|s| *s < 1.0)
    }
}

/// §7.3.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipientFrequency {
    pub mode: FrequencyMode,
    pub window: Window,
    pub threshold: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrequencyMode {
    ToAddress,
    ToDomain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub unit: WindowUnit,
    pub count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowUnit {
    Hourly,
    Daily,
    Weekly,
}

impl Window {
    pub fn as_duration(&self) -> Duration {
        let unit = match self.unit {
            WindowUnit::Hourly => 3_600,
            WindowUnit::Daily => 86_400,
            WindowUnit::Weekly => 604_800,
        };
        Duration::from_secs(unit * u64::from(self.count))
    }
}

fn yes() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("reading config '{path}': {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("parsing config '{path}': {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    /// §4: "an unresolvable reference is a fatal startup error". Reported as a
    /// group, per §4.2's "report all violations".
    #[error("{}", render_unresolved(.0))]
    Unresolved(Vec<interpolate::Unresolved>),

    #[error("{0}")]
    Invalid(ViolationList),
}

fn render_unresolved(missing: &[interpolate::Unresolved]) -> String {
    use fmt::Write as _;
    let mut s = format!(
        "{} unresolved ${{ENV_VAR}} reference{} in config:",
        missing.len(),
        if missing.len() == 1 { "" } else { "s" }
    );
    for m in missing {
        let _ = write!(
            s,
            "\n  - {} references ${{{}}}, which is not set",
            m.path, m.var
        );
    }
    s
}

/// Read, interpolate, parse and validate. This is the only entry point; there is
/// deliberately no way to obtain an unvalidated `Config`, so §4.2's "the process
/// refuses to start on any violation" cannot be bypassed by a future caller.
pub fn load(path: impl AsRef<Path>) -> Result<Config, LoadError> {
    let path = path.as_ref();
    let display = path.display().to_string();

    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
        path: display.clone(),
        source,
    })?;

    from_str(&text, &display)
}

/// As [`load`], for an already-read document. Split out so tests do not need a
/// temporary file.
pub fn from_str(text: &str, origin: &str) -> Result<Config, LoadError> {
    let mut tree: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(text).map_err(|source| LoadError::Parse {
            path: origin.to_string(),
            source,
        })?;

    // Before anything else: a key this version has removed would otherwise fail
    // as a bare `unknown field`, which says nothing about what replaced it.
    let removed = validate::removed_keys(&tree);
    if !removed.is_empty() {
        return Err(LoadError::Invalid(removed));
    }

    // Interpolate before deserialising: ${VAR} may appear in a field typed as
    // something other than String (a port, say), and substituting into the
    // parsed tree keeps a secret's contents from altering document structure.
    let missing = interpolate::interpolate_from_env(&mut tree);
    if !missing.is_empty() {
        return Err(LoadError::Unresolved(missing));
    }

    let config: Config = serde_yaml_ng::from_value(tree).map_err(|source| LoadError::Parse {
        path: origin.to_string(),
        source,
    })?;

    let violations = validate::validate(&config);
    if !violations.is_empty() {
        return Err(LoadError::Invalid(violations));
    }

    Ok(config)
}

/// One chain, with a path naming where in the document it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedChain<'a> {
    pub path: String,
    pub routes: &'a [String],
}

impl Config {
    pub fn route(&self, name: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.name == name)
    }

    pub fn domain_group(&self, name: &str) -> Option<&DomainGroup> {
        self.domain_groups.iter().find(|g| g.name == name)
    }

    pub fn catchall_group(&self) -> Option<&DomainGroup> {
        self.domain_groups.iter().find(|g| g.is_catchall())
    }

    /// Every chain in the configuration: one per sender rule, plus the default.
    ///
    /// §4.2 uses it to check each chain's shape; §9.3 uses it to work out which
    /// chains a mutation has just left with nothing eligible, which is the
    /// difference between an operator pausing a route and an operator answering
    /// every message `451` without meaning to.
    pub fn chains(&self) -> Vec<NamedChain<'_>> {
        let mut out: Vec<NamedChain<'_>> = self
            .senders
            .iter()
            .enumerate()
            .map(|(i, rule)| NamedChain {
                path: format!("senders[{i}] (match '{}').chain", rule.pattern),
                routes: &rule.chain,
            })
            .collect();

        if let Some(default) = self.default_chain.as_deref() {
            out.push(NamedChain {
                path: "default_chain".to_string(),
                routes: default,
            });
        }

        out
    }

    /// Every route a message could actually be sent through: the union of all
    /// sender-rule chains and `default_chain`.
    ///
    /// A route defined in `routes` but named by no chain is dead configuration
    /// (there is no hot reload, §2.2, so nothing can bring it to life), and it
    /// must not get a vote in [`advertise_smtputf8`](Self::advertise_smtputf8).
    pub fn reachable_routes(&self) -> impl Iterator<Item = &Route> {
        let names: std::collections::BTreeSet<&str> = self
            .senders
            .iter()
            .flat_map(|s| s.chain.iter())
            .chain(self.default_chain.iter().flatten())
            .map(String::as_str)
            .collect();
        self.routes
            .iter()
            .filter(move |r| names.contains(r.name.as_str()))
    }

    /// Whether `EHLO` may advertise `SMTPUTF8` (§5.2, D-018).
    ///
    /// Only when **every** reachable route can carry it. Anything less would let
    /// the quota ramp steer a UTF-8 message onto a route that cannot deliver it,
    /// which is the mid-relay discovery O-10 exists to prevent — and which route
    /// a message takes is not knowable at `EHLO` time.
    pub fn advertise_smtputf8(&self) -> bool {
        let mut any = false;
        for route in self.reachable_routes() {
            any = true;
            if !route.downstream.smtputf8 {
                return false;
            }
        }
        any
    }
}

impl Route {
    /// Whether §6.7 preflight should run for this route. Absent block means
    /// disabled (`DECISIONS.md` D-012).
    pub fn preflight_enabled(&self) -> bool {
        self.preflight.as_ref().is_some_and(|p| p.enabled)
    }
}
