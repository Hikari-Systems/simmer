//! D-123's object stores against real implementations. Each test is a no-op
//! unless its environment names a server, so `cargo test` passes anywhere:
//!
//! ```sh
//! # S3 — any S3-compatible server that verifies SigV4 (SeaweedFS with an
//! # identity configured does; see DECISIONS.md D-123):
//! SIMMER_S3_ENDPOINT=http://127.0.0.1:18333 SIMMER_S3_KEY=… SIMMER_S3_SECRET=… \
//!   cargo test --test spool_object
//! # Azure — Azurite, with its published development account:
//! SIMMER_AZURITE=http://127.0.0.1:20000/devstoreaccount1 cargo test --test spool_object
//! ```

use std::time::{Duration, SystemTime};

use simmer::config::ObjectStoreConfig;
use simmer::spool::body::{BodyError, BodyStore};
use simmer::spool::object::ObjectStore;

/// Azurite's well-known development key (public; it is in Microsoft's docs).
const AZURITE_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

fn s3_config(bucket: &str, secret: &str) -> Option<ObjectStoreConfig> {
    let endpoint = std::env::var("SIMMER_S3_ENDPOINT").ok()?;
    let key = std::env::var("SIMMER_S3_KEY").ok()?;
    Some(
        serde_yaml_ng::from_str(&format!(
            "{{ provider: s3, bucket: {bucket}, region: us-east-1, endpoint: \"{endpoint}\", \
               allow_http: true, access_key_id: \"{key}\", secret_access_key: \"{secret}\", \
               prefix: spool }}"
        ))
        .unwrap(),
    )
}

fn azure_config(container: &str, key: &str) -> Option<ObjectStoreConfig> {
    let endpoint = std::env::var("SIMMER_AZURITE").ok()?;
    Some(
        serde_yaml_ng::from_str(&format!(
            "{{ provider: azure, account: devstoreaccount1, container: {container}, \
               access_key: \"{key}\", endpoint: \"{endpoint}\", allow_http: true, prefix: spool }}"
        ))
        .unwrap(),
    )
}

fn tls() -> rustls::ClientConfig {
    let (tls, _) = simmer::downstream::TlsConfigs::load().unwrap();
    (*tls.verifying()).clone()
}

async fn round_trip(cfg: ObjectStoreConfig) {
    let objects = ObjectStore::with_tls(&cfg, tls()).unwrap();
    objects
        .create_container()
        .await
        .expect("create the container");
    let store = BodyStore::Object(Box::new(objects));
    store.probe().await.expect("the startup probe");

    let id = uuid::Uuid::new_v4();
    let body = b"Subject: object\r\n\r\nstored remotely\r\n".repeat(1000);
    let stored = store.put(id, &body).await.expect("put");
    assert_eq!(stored.body_ref, format!("spool/{id}.eml"));
    assert_eq!(
        store
            .get(&stored.body_ref, &stored.sha256)
            .await
            .expect("get"),
        body
    );

    let future = SystemTime::now() + Duration::from_secs(3600);
    let listed = store.list_older_than(future).await.expect("list");
    assert!(listed.contains(&stored.body_ref), "{listed:?}");
    let past = SystemTime::now() - Duration::from_secs(3600);
    assert!(!store
        .list_older_than(past)
        .await
        .expect("list")
        .contains(&stored.body_ref));

    store.delete(&stored.body_ref).await.expect("delete");
    store
        .delete(&stored.body_ref)
        .await
        .expect("deleting twice is fine");
    assert!(matches!(
        store.get(&stored.body_ref, &stored.sha256).await,
        Err(BodyError::NotFound(_))
    ));
}

#[tokio::test]
async fn s3_round_trip() {
    let Some(cfg) = s3_config(
        "simmer-test",
        &std::env::var("SIMMER_S3_SECRET").unwrap_or_default(),
    ) else {
        eprintln!("SIMMER_S3_ENDPOINT not set; skipped");
        return;
    };
    round_trip(cfg).await;
}

#[tokio::test]
async fn s3_refuses_a_wrong_secret() {
    // The signature is checked: a store that accepted anything would prove
    // nothing above.
    let Some(cfg) = s3_config("simmer-test", "not-the-secret") else {
        return;
    };
    let store = BodyStore::Object(Box::new(ObjectStore::with_tls(&cfg, tls()).unwrap()));
    assert!(store.probe().await.is_err());
}

#[tokio::test]
async fn azure_round_trip() {
    let Some(cfg) = azure_config("simmer-test", AZURITE_KEY) else {
        eprintln!("SIMMER_AZURITE not set; skipped");
        return;
    };
    round_trip(cfg).await;
}

#[tokio::test]
async fn azure_refuses_a_wrong_key() {
    use base64::Engine as _;
    let wrong = base64::engine::general_purpose::STANDARD.encode([7u8; 64]);
    let Some(cfg) = azure_config("simmer-test", &wrong) else {
        return;
    };
    let store = BodyStore::Object(Box::new(ObjectStore::with_tls(&cfg, tls()).unwrap()));
    assert!(store.probe().await.is_err());
}
