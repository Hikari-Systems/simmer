//! §5.8 end to end (D-099): which ramp a message is routed in, over a real SMTP
//! session, with two ramps whose routes share a name.
//!
//! Each ramp's `only` route points at its own fake downstream and leaves under
//! its own envelope sender, so where a message landed says which ramp routed
//! it. The first listener has no affinity; the second is tied to `partner`.
//! `cfapp` may name `partner` in `X-Simmer-Ramp`; `crm` may name nothing.

mod support;

use std::sync::Arc;

use support::{Client, FakeDownstream, GrantAllQuota, Script, Simmer};

/// argon2id of "local-dev-password", as in `tests/smtp_ingress.rs`.
const HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$iYj0sVhRvzAWM9kzsFBKyg$V1tnVXTGEO+BD0x9q1uccuo91w84jrz+qDKA3qElLFE";

fn route(port: u16, envelope_from: &str) -> String {
    format!(
        "  - name: only\n    overflow: true\n    downstream:\n      host: \"127.0.0.1\"\n      \
         port: {port}\n      tls: off\n      pool: {{ max_connections: 1, idle_ttl: 60s, \
         max_messages_per_connection: 10 }}\n      timeouts: {{ connect: 2s, command: 2s, data: 2s }}\n    \
         identity:\n      envelope_from: \"{envelope_from}\"\n"
    )
}

fn config(main: u16, partner: u16, partner_listener: &str) -> String {
    format!(
        r#"
server:
  listeners:
    - address: "127.0.0.1:0"
      auth: optional
    - address: "127.0.0.1:0"
      auth: optional
{partner_listener}
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 5
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth:
    allow_insecure_auth: true
    mechanisms: [PLAIN, LOGIN]
    users:
      - username: "cfapp"
        password_hash: "{HASH}"
        grants: {{ send_as: ["oldbrand.com"], ramps: [partner, main] }}
      - username: "crm"
        password_hash: "{HASH}"
        grants: {{ send_as: ["oldbrand.com"] }}
database:
  url: "postgres://u:p@localhost/simmer"
  connect_timeout: 5s
admin:
  listen: "127.0.0.1:0"
  auth_token: "t"
logging: {{ level: warn, format: text }}
default_ramp: main
ramps:
 main:
  domain_groups:
  - {{ name: catchall, domains: ["*"] }}
  senders: []
  default_chain: [only]
  routes:
{main_route} partner:
  domain_groups:
  - {{ name: catchall, domains: ["*"] }}
  senders: []
  default_chain: [only]
  routes:
{partner_route}"#,
        main_route = route(main, "b@main.example"),
        partner_route = route(partner, "b@partner.example"),
    )
}

struct Stack {
    main: FakeDownstream,
    partner: FakeDownstream,
    quota: Arc<GrantAllQuota>,
    simmer: Simmer,
}

/// `partner_listener` is extra YAML for the second listener, at six spaces.
async fn stack(partner_listener: &str) -> Stack {
    let main = FakeDownstream::start(Script::default()).await;
    let partner = FakeDownstream::start(Script::default()).await;
    let quota = Arc::new(GrantAllQuota::new());
    let simmer = Simmer::start_with_quota(
        &config(main.addr.port(), partner.addr.port(), partner_listener),
        quota.clone(),
    )
    .await;
    Stack {
        main,
        partner,
        quota,
        simmer,
    }
}

async fn session(stack: &Stack, listener: usize, user: Option<&str>) -> Client {
    let mut c = Client::connect(stack.simmer.addrs[listener]).await;
    c.hello().await;
    if let Some(user) = user {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::STANDARD
            .encode(format!("\0{user}\0local-dev-password"));
        let r = c.command(&format!("AUTH PLAIN {payload}")).await;
        assert_eq!(r.code, 235, "AUTH as {user}: {r:?}");
    }
    c
}

fn body(ramp_headers: &[&str]) -> String {
    let mut out = String::from("From: jane@oldbrand.com\r\nSubject: hi\r\n");
    for h in ramp_headers {
        out.push_str(&format!("X-Simmer-Ramp: {h}\r\n"));
    }
    out.push_str("\r\nhello\r\n");
    out
}

/// Deliver one message and say which ramp's downstream got it.
async fn landed(
    stack: &Stack,
    listener: usize,
    user: Option<&str>,
    headers: &[&str],
) -> &'static str {
    let before = (stack.main.messages().len(), stack.partner.messages().len());
    let mut c = session(stack, listener, user).await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.net", &body(headers))
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    let after = (stack.main.messages().len(), stack.partner.messages().len());
    match (after.0 - before.0, after.1 - before.1) {
        (1, 0) => "main",
        (0, 1) => "partner",
        other => panic!("expected exactly one delivery, got {other:?}"),
    }
}

const LOCKED: &str = "      ramp: partner\n";
const OVERRIDABLE: &str = "      ramp: partner\n      header_overrides_affinity: true\n";

#[tokio::test]
async fn with_nothing_to_go_on_mail_is_routed_in_the_default_ramp() {
    let s = stack(LOCKED).await;
    assert_eq!(landed(&s, 0, None, &[]).await, "main");
    let got = s.main.last().unwrap();
    assert_eq!(got.mail_from.as_deref(), Some("b@main.example"));
}

#[tokio::test]
async fn a_listener_affinity_routes_in_its_ramp() {
    let s = stack(LOCKED).await;
    assert_eq!(landed(&s, 1, None, &[]).await, "partner");
    assert_eq!(
        s.partner.last().unwrap().mail_from.as_deref(),
        Some("b@partner.example")
    );
}

#[tokio::test]
async fn a_permitted_header_chooses_the_ramp_and_never_reaches_the_downstream() {
    let s = stack(LOCKED).await;
    assert_eq!(landed(&s, 0, Some("cfapp"), &["partner"]).await, "partner");
    let raw = String::from_utf8(s.partner.last().unwrap().body).unwrap();
    assert!(
        !raw.to_ascii_lowercase().contains("x-simmer-ramp"),
        "§6.5 strips the header:\n{raw}"
    );
}

#[tokio::test]
async fn a_header_the_session_may_not_name_is_ignored_not_refused() {
    let s = stack(LOCKED).await;
    // No grant for it.
    assert_eq!(landed(&s, 0, Some("crm"), &["partner"]).await, "main");
    // Not authenticated at all.
    assert_eq!(landed(&s, 0, None, &["partner"]).await, "main");
    // Names no ramp.
    assert_eq!(landed(&s, 0, Some("cfapp"), &["nonesuch"]).await, "main");
    // Two that disagree.
    assert_eq!(
        landed(&s, 0, Some("cfapp"), &["partner", "main"]).await,
        "main"
    );
    // Stripped every time, used or not.
    for m in s.main.messages() {
        let raw = String::from_utf8(m.body).unwrap();
        assert!(!raw.to_ascii_lowercase().contains("x-simmer-ramp"), "{raw}");
    }
}

#[tokio::test]
async fn a_locked_affinity_wins_over_a_permitted_header() {
    let s = stack(LOCKED).await;
    assert_eq!(landed(&s, 1, Some("cfapp"), &["main"]).await, "partner");
}

#[tokio::test]
async fn with_the_override_a_permitted_header_wins_and_a_bad_one_falls_back_to_the_affinity() {
    let s = stack(OVERRIDABLE).await;
    assert_eq!(landed(&s, 1, Some("cfapp"), &["main"]).await, "main");
    // Rule 3, not rule 4: the listener's ramp, not default_ramp.
    assert_eq!(landed(&s, 1, Some("crm"), &["main"]).await, "partner");
}

#[tokio::test]
async fn the_reservation_is_taken_in_the_selected_ramp() {
    // D-099's storage keys: the same route name in two ramps is two rows.
    let s = stack(LOCKED).await;
    landed(&s, 0, None, &[]).await;
    landed(&s, 1, None, &[]).await;
    let committed: Vec<(String, String)> = s
        .quota
        .committed()
        .into_iter()
        .map(|r| (r.ramp, r.route))
        .collect();
    assert_eq!(
        committed,
        [
            ("main".to_string(), "only".to_string()),
            ("partner".to_string(), "only".to_string())
        ]
    );
}

#[tokio::test]
async fn different_ramp_grants_route_the_same_bytes_differently() {
    // The one amended exception to D-071 (§5.3): grants.ramps decides whether
    // the header is honoured, so the same message from two users with
    // different grants can leave under different identities. Pinned here so
    // it is a decision, not a drift. Within a ramp the rule is unchanged —
    // `tests/ingress_tls.rs` still asserts byte-identical output for two users
    // with identical grants.
    let s = stack(LOCKED).await;
    assert_eq!(landed(&s, 0, Some("cfapp"), &["partner"]).await, "partner");
    assert_eq!(landed(&s, 0, Some("crm"), &["partner"]).await, "main");
    assert_ne!(
        s.partner.last().unwrap().mail_from,
        s.main.last().unwrap().mail_from
    );
}

#[cfg(feature = "postgres")]
#[sqlx::test]
async fn the_early_refusal_waits_when_a_header_could_move_the_ramp(pool: sqlx::PgPool) {
    // §5.4 as amended: RCPT TO refuses early only when the ramp is already
    // fixed. `main` has nothing eligible; a session that may name `partner`
    // must not be refused for `main`'s sake before its header arrives.
    use simmer::quota::{PgQuotaStore, QuotaStore};

    let main = FakeDownstream::start(Script::default()).await;
    let partner = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool));
    store.set_paused("main", "only", true).await.unwrap();
    let simmer = Simmer::start_with_quota(
        &config(main.addr.port(), partner.addr.port(), LOCKED),
        store.clone(),
    )
    .await;
    let s = Stack {
        main,
        partner,
        quota: Arc::new(GrantAllQuota::new()),
        simmer,
    };

    // May name nothing: the ramp is fixed at `main`, so it is refused early.
    let mut c = session(&s, 0, Some("crm")).await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    let r = c.command("RCPT TO:<bob@example.net>").await;
    assert_eq!(r.code, 451, "refused at RCPT TO: {r:?}");

    // May name `partner`: RCPT TO is accepted, and the header decides.
    assert_eq!(landed(&s, 0, Some("cfapp"), &["partner"]).await, "partner");
    // Only `partner`'s pause state is untouched; `main`'s stays paused.
    assert!(store.route_states("main").await.unwrap()["only"].paused);
    assert!(store.route_states("partner").await.unwrap().is_empty());
}
