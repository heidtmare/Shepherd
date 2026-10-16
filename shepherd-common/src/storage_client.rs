//! Minimal shepherd-storage HTTP client used by downstream services.

use bytes::Bytes;
use reqwest::{StatusCode, header};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("not found")]
    NotFound,
    #[error("storage returned {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocumentDetail {
    pub id: Uuid,
    pub source_filename: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub created_at: Option<time::OffsetDateTime>,
    #[serde(default)]
    pub labels: serde_json::Value,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub id: Uuid,
    pub kind: String,
    pub schema: String,
    pub version: i32,
    pub sha256: String,
    pub size_bytes: i64,
    pub produced_by: Option<String>,
    pub parent_artifact_id: Option<Uuid>,
}

impl DocumentDetail {
    pub fn source(&self) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| a.kind == "source")
            .max_by_key(|a| a.version)
    }

    /// Latest version of a derived schema.
    pub fn latest(&self, schema: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| a.schema == schema)
            .max_by_key(|a| a.version)
    }
}

#[derive(Clone)]
pub struct StorageClient {
    pub http: reqwest::Client,
    pub base_url: String,
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response, StorageError> {
    match resp.status() {
        s if s.is_success() => Ok(resp),
        StatusCode::NOT_FOUND => Err(StorageError::NotFound),
        status => Err(StorageError::Status {
            status,
            body: resp.text().await.unwrap_or_default(),
        }),
    }
}

impl StorageClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    pub async fn document(&self, id: Uuid) -> Result<DocumentDetail, StorageError> {
        let r = self
            .http
            .get(format!("{}/v1/documents/{id}", self.base_url))
            .send()
            .await?;
        Ok(check(r).await?.json().await?)
    }

    pub async fn content(&self, artifact_id: Uuid) -> Result<Bytes, StorageError> {
        let r = self
            .http
            .get(format!(
                "{}/v1/artifacts/{artifact_id}/content",
                self.base_url
            ))
            .send()
            .await?;
        Ok(check(r).await?.bytes().await?)
    }

    /// Stores a derived JSON artifact. `actor` is recorded in the audit log.
    pub async fn create_artifact_as(
        &self,
        document_id: Uuid,
        schema: &str,
        produced_by: &str,
        parent: Uuid,
        actor: &str,
        body: Vec<u8>,
    ) -> Result<Artifact, StorageError> {
        self.create_typed_artifact_as(
            document_id,
            schema,
            produced_by,
            parent,
            actor,
            "application/json",
            body,
        )
        .await
    }

    /// Stores a derived artifact of `content_type` (`application/json` or `application/pdf`).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_typed_artifact_as(
        &self,
        document_id: Uuid,
        schema: &str,
        produced_by: &str,
        parent: Uuid,
        actor: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Artifact, StorageError> {
        let r = self
            .http
            .post(format!(
                "{}/v1/documents/{document_id}/artifacts",
                self.base_url
            ))
            .query(&[
                ("schema", schema),
                ("produced_by", produced_by),
                ("parent_artifact_id", &parent.to_string()),
            ])
            .header(header::CONTENT_TYPE, content_type)
            .header("x-actor", actor)
            .body(body)
            .send()
            .await?;
        Ok(check(r).await?.json().await?)
    }

    pub async fn create_artifact(
        &self,
        document_id: Uuid,
        schema: &str,
        produced_by: &str,
        parent: Uuid,
        body: Vec<u8>,
    ) -> Result<Artifact, StorageError> {
        self.create_artifact_as(document_id, schema, produced_by, parent, produced_by, body)
            .await
    }

    /// Newest documents first.
    pub async fn recent_documents(&self, limit: usize) -> Result<serde_json::Value, StorageError> {
        let r = self
            .http
            .get(format!("{}/v1/documents", self.base_url))
            .query(&[("order", "desc"), ("limit", &limit.to_string())])
            .send()
            .await?;
        Ok(check(r).await?.json().await?)
    }

    pub async fn ready(&self) -> bool {
        self.http
            .get(format!("{}/readyz", self.base_url))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}
