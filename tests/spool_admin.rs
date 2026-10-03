//! §9 for the spool (D-125): the endpoints against the real router, sharing
//! the engine of a spooled Simmer over real Postgres — so a drain, a retry and
//! a delete act on the same spool the dispatcher is delivering from.

#![cfg(feature = "postgres")]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use simmer::admin::{self, AdminState};
use simmer::quota::PgQuotaStore;
use simmer::spool::SpoolStore;
use sqlx::PgPool;
use support::{config_for, Act, FakeDownstream, Script, Simmer, Turn};
use tower::ServiceExt;

const BODY: &str = "From: App <app@oldbrand.com>\r\nSubject: s\r\n\r\nhello\r\n";

fn spooled(addr: std::net::SocketAddr, dir: &Path, spool: &str) -> String {
    config_for(
        addr,
        &format!(
            "  delivery: spool\nspool:\n  body_store: {{ kind: volume, path: \"{}\" }}\n  \
             dispatch: {{ poll_interval: 50ms, batch: 8 }}\n  \
             retry: {{ initial: 200ms, max: 400ms, factor: 2.0 }}\n{spool}",
            dir.display()
        ),
    )
}

fn admin_state(simmer: &Simmer) -> AdminState {
    AdminState {
        engine: simmer.engine().clone(),
        metrics: None,
        sessions: None,
        db: None,
    }
}

struct Resp {
    status: StatusCode,
    body: String,
}

impl Resp {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

async fn call(state: &AdminState, method: &str, uri: &str, body: &str) -> Resp {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer t")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = admin::router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn send(simmer: &Simmer) -> support::Reply {
    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver("app@oldbrand.com", "bob@example.com", BODY).await
}

async fn wait_until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[sqlx::test]
async fn without_a_spool_every_endpoint_is_404_and_every_one_needs_a_token(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start_with_quota(
        &config_for(fake.addr, ""),
        Arc::new(PgQuotaStore::new(pool)),
    )
    .await;
    let state = admin_state(&simmer);
    let r = call(&state, "GET", "/spool", "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
    assert_eq!(r.json()["error"], "spool_disabled", "{}", r.body);

    let req = Request::get("/spool/dead").body(Body::empty()).unwrap();
    let resp = admin::router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn the_read_endpoints_carry_no_address(pool: PgPool) {
    // The downstream quotes the recipient, as real ones do.
    let fake = FakeDownstream::start(Script::with(|s| {
        s.rcpt_to = Act::Reply(550, "5.1.1 <bob@example.com>... User unknown");
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(PgQuotaStore::new(pool));
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), ""), store.clone()).await;
    let state = admin_state(&simmer);

    assert_eq!(send(&simmer).await.code, 250);
    wait_until("the dead letter", || async {
        !store.dead_entries(1).await.unwrap().is_empty()
    })
    .await;

    for uri in ["/spool", "/spool/dead"] {
        let r = call(&state, "GET", uri, "").await;
        assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.body);
        assert!(!r.body.contains('@'), "{uri} leaked an address: {}", r.body);
    }
    let dead = call(&state, "GET", "/spool/dead", "").await.json();
    let entry = &dead["dead"][0];
    assert_eq!(entry["reason"], "rejected");
    assert_eq!(entry["code"], 550);
    assert!(entry["text"].as_str().unwrap().contains("<redacted>"));
    assert_eq!(entry["retryable"], false);
}

#[sqlx::test]
async fn a_dead_letter_with_its_body_kept_can_be_retried(pool: PgPool) {
    // One connection: the first transaction is refused for good, the retry
    // (the connection's second) goes through.
    let fake = FakeDownstream::start(Script::with(|s| {
        s.transactions = vec![Turn {
            rcpt_to: Some(Act::Reply(550, "5.1.1 unknown")),
            ..Turn::default()
        }];
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(PgQuotaStore::new(pool));
    let yaml = spooled(
        fake.addr,
        dir.path(),
        "  dead_letter: { retention: 7d, keep_body: 1h }\n",
    );
    let simmer = Simmer::start_spooled(&yaml, store.clone()).await;
    let state = admin_state(&simmer);

    assert_eq!(send(&simmer).await.code, 250);
    wait_until("the dead letter", || async {
        !store.dead_entries(1).await.unwrap().is_empty()
    })
    .await;
    let id = store.dead_entries(1).await.unwrap()[0].id;
    assert!(store.dead_entries(1).await.unwrap()[0].body_retained);

    let r = call(&state, "POST", &format!("/spool/dead/{id}/retry"), "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["actor"], "default");
    wait_until("the retry's delivery", || async {
        fake.messages().len() == 1
    })
    .await;

    let r = call(&state, "POST", &format!("/spool/dead/{id}/retry"), "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "it is not dead any more");
    let r = call(&state, "POST", "/spool/dead/not-a-uuid/retry", "").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn a_dead_letter_without_its_body_cannot_be_retried(pool: PgPool) {
    let fake = FakeDownstream::start(Script::with(|s| {
        s.rcpt_to = Act::Reply(550, "5.1.1 unknown");
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(PgQuotaStore::new(pool));
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), ""), store.clone()).await;
    let state = admin_state(&simmer);

    assert_eq!(send(&simmer).await.code, 250);
    wait_until("the dead letter", || async {
        !store.dead_entries(1).await.unwrap().is_empty()
    })
    .await;
    let id = store.dead_entries(1).await.unwrap()[0].id;
    let r = call(&state, "POST", &format!("/spool/dead/{id}/retry"), "").await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert_eq!(r.json()["error"], "no_body");
}

#[sqlx::test]
async fn drain_refuses_new_mail_and_reports_empty_once_delivered(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(PgQuotaStore::new(pool));
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), ""), store.clone()).await;
    let state = admin_state(&simmer);

    let r = call(&state, "POST", "/ramps/main/spool/pause", "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.json()["warnings"].as_array().unwrap().is_empty());
    for _ in 0..2 {
        assert_eq!(send(&simmer).await.code, 250);
    }

    let r = call(&state, "POST", "/ramps/main/spool/drain", "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["remaining"], 2);
    assert_eq!(r.json()["drained"], false);
    let refused = send(&simmer).await;
    assert_eq!(refused.code, 451, "{refused:?}");

    call(&state, "POST", "/ramps/main/spool/resume", "").await;
    wait_until("the backlog to deliver", || async {
        fake.messages().len() == 2
    })
    .await;
    wait_until("drained", || async {
        call(&state, "GET", "/spool", "").await.json()["ramps"]["main"]["drained"] == true
    })
    .await;

    // Reversed, it accepts again.
    let r = call(
        &state,
        "POST",
        "/ramps/main/spool/drain",
        r#"{"draining": false}"#,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(send(&simmer).await.code, 250);
}

#[sqlx::test]
async fn a_spooled_message_can_be_deleted(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(PgQuotaStore::new(pool));
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), ""), store.clone()).await;
    let state = admin_state(&simmer);
    call(&state, "POST", "/ramps/main/spool/pause", "").await;

    let reply = send(&simmer).await;
    let id = reply.text().rsplit(' ').next().unwrap().trim().to_string();
    let r = call(&state, "DELETE", &format!("/spool/{id}"), "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(store.totals().await.unwrap().messages, 0);
    let bodies = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(bodies, 0, "the body went with the row");
    let r = call(&state, "DELETE", &format!("/spool/{id}"), "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn the_dry_run_reports_admission_for_a_spooling_ramp(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(PgQuotaStore::new(pool));
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), ""), store.clone()).await;
    let state = admin_state(&simmer);

    let body = r#"{"envelope_from": "app@oldbrand.com", "recipients": ["bob@example.com"]}"#;
    let r = call(&state, "POST", "/dryrun", body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let spool = &r.json()["recipients"][0]["spool"];
    assert_eq!(spool["admission"], "admit", "{}", r.body);
    assert!(spool["would_reply"]
        .as_str()
        .unwrap()
        .starts_with("250 2.0.0 queued as"));

    call(&state, "POST", "/ramps/main/spool/drain", "").await;
    let r = call(&state, "POST", "/dryrun", body).await;
    assert_eq!(r.json()["recipients"][0]["spool"]["admission"], "draining");
}
