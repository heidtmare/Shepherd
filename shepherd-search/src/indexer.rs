//! Loads a document's current state from the storage schema and writes its card.

use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt, path::Path};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    artifact_cache::ArtifactCache,
    backend::SearchBackend,
    mapping::{self, DerivedArtifact, DocumentSource, Mapping},
};

pub enum ContentSource {
    Blob(Arc<dyn ObjectStore>),
    Http {
        client: reqwest::Client,
        base_url: String,
    },
}

impl ContentSource {
    async fn fetch(&self, artifact_id: Uuid, object_key: &str) -> anyhow::Result<Bytes> {
        match self {
            Self::Blob(store) => Ok(store.get(&Path::from(object_key)).await?.bytes().await?),
            Self::Http { client, base_url } => Ok(client
                .get(format!("{base_url}/v1/artifacts/{artifact_id}/content"))
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?),
        }
    }
}

pub struct IndexOutcome {
    /// False when skipped because the card already reflects a newer event.
    pub applied: bool,
    pub deleted: bool,
    pub schemas: Vec<String>,
}

pub struct Indexer<B> {
    /// Needs SELECT on `storage.documents` and `storage.artifacts`.
    pub db: PgPool,
    pub content: ContentSource,
    pub mapping: Mapping,
    pub backend: Arc<B>,
    pub cache: ArtifactCache,
}

impl<B: SearchBackend> Indexer<B> {
    /// Rebuilds the card for `doc_id` from current storage state. Idempotent; event payloads are
    /// not trusted for content, so redelivery and reordering are harmless.
    pub async fn index(&self, doc_id: Uuid, seq: u64, force: bool) -> anyhow::Result<IndexOutcome> {
        let Some(src) = self.load(doc_id).await? else {
            self.backend.delete(doc_id).await?;
            return Ok(IndexOutcome {
                applied: true,
                deleted: true,
                schemas: Vec::new(),
            });
        };
        let (card, pages) = mapping::build(&self.mapping, &src);
        let applied = self
            .backend
            .upsert(&card, &pages, seq, force, OffsetDateTime::now_utc())
            .await?;
        Ok(IndexOutcome {
            applied,
            deleted: false,
            schemas: card.schemas,
        })
    }

    async fn load(&self, doc_id: Uuid) -> anyhow::Result<Option<DocumentSource>> {
        let doc: Option<(Option<String>, OffsetDateTime)> = sqlx::query_as(
            "SELECT source_filename, created_at FROM storage.documents WHERE id = $1",
        )
        .bind(doc_id)
        .fetch_optional(&self.db)
        .await?;
        let Some((filename, created_at)) = doc else {
            return Ok(None);
        };

        let rows: Vec<(Uuid, String, String, OffsetDateTime)> = sqlx::query_as(
            "SELECT DISTINCT ON (schema) id, schema, object_key, created_at
             FROM storage.artifacts WHERE document_id = $1 AND kind = 'derived'
               AND content_type = 'application/json'
             ORDER BY schema, version DESC",
        )
        .bind(doc_id)
        .fetch_all(&self.db)
        .await?;

        let artifacts = futures::future::try_join_all(rows.into_iter().map(
            |(id, schema, key, created_at)| async move {
                let body = if self.mapping.ignored(&schema) {
                    Arc::new(serde_json::Value::Null)
                } else {
                    self.body(id, &key).await?
                };
                anyhow::Ok(DerivedArtifact {
                    schema,
                    created_at,
                    body,
                })
            },
        ))
        .await?;
        Ok(Some(DocumentSource {
            doc_id,
            filename,
            created_at,
            artifacts,
        }))
    }

    async fn body(&self, id: Uuid, key: &str) -> anyhow::Result<Arc<serde_json::Value>> {
        if let Some(b) = self.cache.get(id) {
            metrics::counter!("artifact_cache_total", "result" => "hit").increment(1);
            return Ok(b);
        }
        metrics::counter!("artifact_cache_total", "result" => "miss").increment(1);
        let bytes = self
            .content
            .fetch(id, key)
            .await
            .with_context(|| format!("fetch artifact {id}"))?;
        let body: Arc<serde_json::Value> = Arc::new(
            serde_json::from_slice(&bytes).with_context(|| format!("parse artifact {id}"))?,
        );
        self.cache.put(id, bytes.len(), body.clone());
        Ok(body)
    }

    /// Ids in `(after, upto]` in id order; `None` leaves that side unbounded.
    pub async fn document_ids_in(
        &self,
        after: Option<Uuid>,
        upto: Option<Uuid>,
        limit: i64,
    ) -> sqlx::Result<Vec<Uuid>> {
        // Separate statements: `$1 IS NULL OR id > $1` makes a cached generic plan rescan from the start.
        let sql = match (after.is_some(), upto.is_some()) {
            (false, false) => "SELECT id FROM storage.documents ORDER BY id LIMIT $1",
            (true, false) => "SELECT id FROM storage.documents WHERE id > $2 ORDER BY id LIMIT $1",
            (false, true) => "SELECT id FROM storage.documents WHERE id <= $2 ORDER BY id LIMIT $1",
            (true, true) => {
                "SELECT id FROM storage.documents WHERE id > $2 AND id <= $3 ORDER BY id LIMIT $1"
            }
        };
        let mut q = sqlx::query_scalar(sql).bind(limit);
        for b in [after, upto].into_iter().flatten() {
            q = q.bind(b);
        }
        q.fetch_all(&self.db).await
    }
}
