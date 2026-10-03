//! §7.7's object store backends (D-117, D-123). Placeholder until the S3 and
//! Azure clients land; `open` refuses, so a configuration naming one fails at
//! startup rather than at the first message.

use uuid::Uuid;

use super::body::BodyError;
use crate::config::ObjectStoreConfig;

#[derive(Debug, Clone)]
pub struct ObjectStore {
    _private: (),
}

impl ObjectStore {
    pub fn new(_cfg: &ObjectStoreConfig) -> anyhow::Result<Self> {
        anyhow::bail!("spool.body_store kind: object is not built yet")
    }

    pub async fn put(&self, _id: Uuid, _bytes: &[u8]) -> Result<String, BodyError> {
        Err(BodyError::Io("object store not built".into()))
    }

    pub async fn get(&self, _body_ref: &str) -> Result<Vec<u8>, BodyError> {
        Err(BodyError::Io("object store not built".into()))
    }

    pub async fn delete(&self, _body_ref: &str) -> Result<(), BodyError> {
        Err(BodyError::Io("object store not built".into()))
    }

    pub async fn list_older_than(
        &self,
        _cutoff: std::time::SystemTime,
    ) -> Result<Vec<String>, BodyError> {
        Err(BodyError::Io("object store not built".into()))
    }
}
