//! §7.7's body store (D-117): where a spooled message's bytes wait.
//!
//! **Atomicity comes from ordering, not co-location.** Accept is `put` (durable
//! before it returns) → insert the row → reply `250`. A crash between the first
//! two leaves a body no row names, which [`super::sweeper`] deletes once it is
//! older than [`ORPHAN_AGE`]; nothing can leave a row without a body. Delivery
//! and dead-letter delete the body after the row's transaction commits, best
//! effort, with the sweeper as the backstop.
//!
//! A body ref is the store-relative name — a file name, or an object key under
//! the configured prefix — so a volume can be remounted elsewhere, or a bucket
//! renamed, without rewriting a row.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};
use tracing::field::Empty;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::config::BodyStoreConfig;

/// A body no row names is deleted once it is this old. Long enough that the
/// `put` → insert window of a live accept never reaches it.
pub const ORPHAN_AGE: Duration = Duration::from_secs(10 * 60);

const SUFFIX: &str = ".eml";
const TMP_PREFIX: &str = ".tmp-";

#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("body {0} is missing")]
    NotFound(String),
    /// The bytes read back are not the bytes stored: nothing safe to send.
    #[error("body {0} does not match its digest")]
    Corrupt(String),
    #[error("body store: {0}")]
    Io(String),
}

impl From<std::io::Error> for BodyError {
    fn from(e: std::io::Error) -> Self {
        BodyError::Io(e.to_string())
    }
}

/// What a `put` stored, for the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub body_ref: String,
    pub bytes: i64,
    pub sha256: Vec<u8>,
}

pub fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// One body store, chosen by configuration.
#[derive(Debug, Clone)]
pub enum BodyStore {
    Volume(Volume),
    Object(Box<super::object::ObjectStore>),
}

impl BodyStore {
    /// `volume`, `s3` or `azure`, for spans and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Volume(_) => "volume",
            Self::Object(o) => o.provider(),
        }
    }

    /// §9.6 — one span per body-store operation: which store, which operation,
    /// how many bytes, and how it ended. An object store records its HTTP
    /// status on it too. Never the body, and never a key beyond the spool id
    /// it is named after.
    fn span(&self, op: &'static str, body_ref: Option<&str>) -> tracing::Span {
        tracing::info_span!(
            "simmer.spool.body",
            otel.name = format!("simmer.spool.body.{op}"),
            otel.status_code = Empty,
            body_store = self.kind(),
            op,
            body_ref = body_ref.unwrap_or(""),
            bytes = Empty,
            listed = Empty,
            outcome = Empty,
            http.response.status_code = Empty,
        )
    }

    /// Open the configured store: create a volume's directory (`0700`), or
    /// build an object store's client. Startup refuses on failure.
    pub fn open(cfg: &BodyStoreConfig, tls: rustls::ClientConfig) -> anyhow::Result<Self> {
        match cfg {
            BodyStoreConfig::Volume { path } => Ok(Self::Volume(Volume::open(Path::new(path))?)),
            BodyStoreConfig::Object(o) => Ok(Self::Object(Box::new(
                super::object::ObjectStore::with_tls(o, tls)?,
            ))),
        }
    }

    /// Store `bytes` durably under a name derived from `id`.
    pub async fn put(&self, id: Uuid, bytes: &[u8]) -> Result<Stored, BodyError> {
        let span = self.span("put", None);
        span.record("bytes", bytes.len());
        let sha256 = digest(bytes);
        let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let body_ref = async {
            match self {
                Self::Volume(v) => v.put(id, bytes.to_vec()).await,
                Self::Object(o) => o.put(id, bytes).await,
            }
        }
        .instrument(span.clone())
        .await;
        finish(&span, &body_ref);
        if let Ok(r) = &body_ref {
            span.record("body_ref", r.as_str());
        }
        Ok(Stored {
            body_ref: body_ref?,
            bytes: len,
            sha256,
        })
    }

    /// Read a body back and check it against the digest the row recorded.
    pub async fn get(&self, body_ref: &str, sha256: &[u8]) -> Result<Vec<u8>, BodyError> {
        let span = self.span("get", Some(body_ref));
        let result = async {
            let bytes = match self {
                Self::Volume(v) => v.get(body_ref).await?,
                Self::Object(o) => o.get(body_ref).await?,
            };
            if digest(&bytes) != sha256 {
                return Err(BodyError::Corrupt(body_ref.to_string()));
            }
            Ok(bytes)
        }
        .instrument(span.clone())
        .await;
        if let Ok(b) = &result {
            span.record("bytes", b.len());
        }
        finish(&span, &result);
        result
    }

    /// Delete a body. Already gone is success.
    pub async fn delete(&self, body_ref: &str) -> Result<(), BodyError> {
        let span = self.span("delete", Some(body_ref));
        let result = async {
            match self {
                Self::Volume(v) => v.delete(body_ref).await,
                Self::Object(o) => o.delete(body_ref).await,
            }
        }
        .instrument(span.clone())
        .await;
        finish(&span, &result);
        result
    }

    /// Write, read back and delete one body: the whole of what the store does,
    /// so wrong credentials, a missing bucket or a read-only mount refuse
    /// startup rather than the first `250 queued` (D-117).
    pub async fn probe(&self) -> anyhow::Result<()> {
        let span = tracing::info_span!(
            "simmer.spool.probe",
            otel.name = "simmer.spool.probe",
            body_store = self.kind(),
        );
        self.probe_inner().instrument(span).await
    }

    async fn probe_inner(&self) -> anyhow::Result<()> {
        let id = Uuid::new_v4();
        let bytes = format!("simmer spool probe {id}\r\n").into_bytes();
        let stored = self
            .put(id, &bytes)
            .await
            .map_err(|e| anyhow::anyhow!("spool body store probe: writing: {e}"))?;
        let back = self.get(&stored.body_ref, &stored.sha256).await;
        let deleted = self.delete(&stored.body_ref).await;
        back.map_err(|e| anyhow::anyhow!("spool body store probe: reading back: {e}"))?;
        deleted.map_err(|e| anyhow::anyhow!("spool body store probe: deleting: {e}"))?;
        Ok(())
    }

    /// Every stored body last written before `cutoff`, for the orphan sweep.
    /// A volume's abandoned temporary files are deleted here rather than
    /// listed: no row can name one.
    pub async fn list_older_than(&self, cutoff: SystemTime) -> Result<Vec<String>, BodyError> {
        let span = self.span("list", None);
        let result = async {
            match self {
                Self::Volume(v) => v.list_older_than(cutoff).await,
                Self::Object(o) => o.list_older_than(cutoff).await,
            }
        }
        .instrument(span.clone())
        .await;
        if let Ok(l) = &result {
            span.record("listed", l.len());
        }
        finish(&span, &result);
        result
    }
}

/// `outcome` (`ok`, `not_found`, `corrupt`, `error`) and an error status on
/// failure. A missing body is an error for `get` and `ok` for `delete`, which
/// treats it as success.
fn finish<T>(span: &tracing::Span, result: &Result<T, BodyError>) {
    let outcome = match result {
        Ok(_) => "ok",
        Err(BodyError::NotFound(_)) => "not_found",
        Err(BodyError::Corrupt(_)) => "corrupt",
        Err(BodyError::Io(_)) => "error",
    };
    span.record("outcome", outcome);
    if result.is_err() {
        span.record("otel.status_code", "ERROR");
    }
}

/// One file per message under a directory.
#[derive(Debug, Clone)]
pub struct Volume {
    dir: PathBuf,
}

impl Volume {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        if !dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| {
                    anyhow::anyhow!("creating the spool directory {}: {e}", dir.display())
                })?;
        } else if !dir.is_dir() {
            anyhow::bail!("the spool path {} is not a directory", dir.display());
        } else {
            // Every body is mail; "as sensitive as a mailbox" (capture's rule).
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A ref names a file directly inside the directory, and nothing else: a
    /// row is data, and a `..` in it must not reach a path outside.
    fn path_of(&self, body_ref: &str) -> Result<PathBuf, BodyError> {
        let ok = !body_ref.is_empty()
            && body_ref.ends_with(SUFFIX)
            && !body_ref.starts_with('.')
            && body_ref
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        if !ok {
            return Err(BodyError::Io(format!(
                "not a volume body ref: {body_ref:?}"
            )));
        }
        Ok(self.dir.join(body_ref))
    }

    async fn put(&self, id: Uuid, bytes: Vec<u8>) -> Result<String, BodyError> {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::OpenOptionsExt as _;
            let name = format!("{id}{SUFFIX}");
            let tmp = dir.join(format!("{TMP_PREFIX}{id}"));
            let result = (|| {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)?;
                f.write_all(&bytes)?;
                f.sync_all()?;
                drop(f);
                std::fs::rename(&tmp, dir.join(&name))?;
                // The rename is durable only once the directory is.
                std::fs::File::open(&dir)?.sync_all()?;
                Ok::<_, std::io::Error>(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            result.map(|()| name).map_err(BodyError::from)
        })
        .await
        .map_err(|e| BodyError::Io(format!("body write task: {e}")))?
    }

    async fn get(&self, body_ref: &str) -> Result<Vec<u8>, BodyError> {
        let path = self.path_of(body_ref)?;
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(BodyError::NotFound(body_ref.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, body_ref: &str) -> Result<(), BodyError> {
        let path = self.path_of(body_ref)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_older_than(&self, cutoff: SystemTime) -> Result<Vec<String>, BodyError> {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if !meta.is_file() || meta.modified().map_or(true, |m| m >= cutoff) {
                    continue;
                }
                if name.starts_with(TMP_PREFIX) {
                    // A put that never reached its rename: a crash mid-write.
                    let _ = std::fs::remove_file(entry.path());
                } else if name.ends_with(SUFFIX) {
                    out.push(name);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| BodyError::Io(format!("body list task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BodyStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore::Volume(Volume::open(&dir.path().join("bodies")).unwrap());
        (dir, store)
    }

    #[tokio::test]
    async fn put_get_delete_round_trip() {
        let (_d, s) = store();
        let id = Uuid::new_v4();
        let stored = s.put(id, b"Subject: hi\r\n\r\nbody\r\n").await.unwrap();
        assert_eq!(stored.body_ref, format!("{id}.eml"));
        assert_eq!(stored.bytes, 21);
        let back = s.get(&stored.body_ref, &stored.sha256).await.unwrap();
        assert_eq!(back, b"Subject: hi\r\n\r\nbody\r\n");
        s.delete(&stored.body_ref).await.unwrap();
        s.delete(&stored.body_ref).await.unwrap();
        assert!(matches!(
            s.get(&stored.body_ref, &stored.sha256).await,
            Err(BodyError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn files_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let (d, s) = store();
        let stored = s.put(Uuid::new_v4(), b"x").await.unwrap();
        let dir = d.path().join("bodies");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(&stored.body_ref)), 0o600);
    }

    #[tokio::test]
    async fn a_changed_body_is_corrupt() {
        let (d, s) = store();
        let stored = s.put(Uuid::new_v4(), b"original").await.unwrap();
        std::fs::write(d.path().join("bodies").join(&stored.body_ref), b"tampered").unwrap();
        assert!(matches!(
            s.get(&stored.body_ref, &stored.sha256).await,
            Err(BodyError::Corrupt(_))
        ));
    }

    #[tokio::test]
    async fn a_ref_cannot_leave_the_directory() {
        let (_d, s) = store();
        for bad in ["../x.eml", "/etc/passwd", "a/b.eml", ".tmp-x", "x.txt", ""] {
            assert!(s.get(bad, &[]).await.is_err(), "{bad}");
            assert!(s.delete(bad).await.is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn listing_finds_old_bodies_and_clears_old_temporaries() {
        let (d, s) = store();
        let stored = s.put(Uuid::new_v4(), b"x").await.unwrap();
        let tmp = d.path().join("bodies").join(".tmp-abandoned");
        std::fs::write(&tmp, b"half").unwrap();

        let past = SystemTime::now() - Duration::from_secs(3600);
        assert!(s.list_older_than(past).await.unwrap().is_empty());
        assert!(
            tmp.exists(),
            "a recent temporary may still be being written"
        );

        let future = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(
            s.list_older_than(future).await.unwrap(),
            vec![stored.body_ref]
        );
        assert!(!tmp.exists(), "an old temporary is deleted");
    }
}
