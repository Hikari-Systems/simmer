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

    pub domain_groups: Vec<DomainGroup>,
    pub senders: Vec<SenderRule>,

    /// §3.2 step 1. Required unless `strict_senders` is true (§4.2).
    #[serde(default)]
    pub default_chain: Option<Vec<String>>,
    #[serde(default)]
    pub strict_senders: bool,

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
    /// §5.1. `host:port`; validated as a `SocketAddr` in §4.2.
    pub listen: String,
    /// Used in the EHLO banner and `Received:` headers (§6.1 step 8).
    pub hostname: String,
    pub max_message_bytes: u64,
    /// §5.5's recipient ceiling. Vestigial since D-047: a transaction may carry
    /// exactly one recipient, so no value above 1 is reachable. Kept because
    /// §4.1 mandates the key; §4.2 warns when it is set above 1.
    pub max_recipients: usize,
    pub max_concurrent_sessions: usize,
    pub allowed_cidrs: Vec<String>,
    pub timeouts: ServerTimeouts,
    pub auth: Auth,
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
    pub required: bool,
    /// §4.2 requires this to be explicitly true: there is no inbound TLS (§5.1),
    /// so AUTH would otherwise be unusable.
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
}

// A derived Debug would put a password hash into any error or trace that
// happens to render the config. §9.5 says message bodies are never logged;
// the same care is owed to credentials.
impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("username", &self.username)
            .field("password_hash", &"<redacted>")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// database / admin / logging
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// A libpq-style URL. §4.1 specifies a URL rather than the discrete fields
    /// `hs_utils::db::DbConfig` wants, so the pool is built directly — see
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
    pub auth_token: String,
}

impl fmt::Debug for Admin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admin")
            .field("listen", &self.listen)
            .field("auth_token", &"<redacted>")
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyRewrite {
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
