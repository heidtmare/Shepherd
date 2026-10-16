//! Deletes blobs that no artifact references.
//!
//! Orphans arise when an upload succeeds but its transaction does not commit (crash, DB error), and
//! as `tmp/` objects left by an interrupted multipart upload. A sweep lists `sha256/` and `tmp/`,
//! skips objects modified within `grace`, and deletes content objects with no `storage.artifacts`
//! row. Each delete runs under an exclusive per-key advisory lock after re-checking references;
//! uploads that reuse an existing object take the same lock shared (`hold`) before inserting their
//! row, so a sweep never deletes an object a committed row points to. One replica sweeps at a time.
//!
//! Incomplete multipart uploads are not objects and are not listed; expire them with a bucket
//! lifecycle rule.

use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use object_store::{ObjectStoreExt, path::Path};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::blob::BlobStore;

const SWEEP_LOCK: i64 = 0x7374_6f72_6167_0001;
const BATCH: usize = 1000;

/// Takes the shared per-key lock for the rest of the transaction and reports whether the object
/// still exists. Call before inserting a row that references an object the upload did not write.
pub async fn hold(conn: &mut PgConnection, blobs: &BlobStore, key: &Path) -> anyhow::Result<bool> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))")
        .bind(key.as_ref())
        .execute(&mut *conn)
        .await?;
    Ok(blobs.exists(key).await?)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub scanned: u64,
    pub deleted: u64,
}

pub struct Gc {
    pub db: PgPool,
    pub blobs: BlobStore,
    pub grace: Duration,
    pub interval: Duration,
}

impl Gc {
    pub async fn run(self, shutdown: CancellationToken) {
        let mut tick = tokio::time::interval(self.interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            match self.sweep().await {
                Ok(Some(s)) => tracing::info!(scanned = s.scanned, deleted = s.deleted, "blob gc"),
                Ok(None) => {}
                Err(e) => tracing::warn!(error = format!("{e:#}"), "blob gc failed"),
            }
        }
    }

    /// `None` when another replica holds the sweep lock.
    pub async fn sweep(&self) -> anyhow::Result<Option<SweepStats>> {
        let mut lock = self.db.acquire().await?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(SWEEP_LOCK)
            .fetch_one(&mut *lock)
            .await?;
        if !locked {
            return Ok(None);
        }
        let res = self.sweep_locked().await;
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(SWEEP_LOCK)
            .execute(&mut *lock)
            .await?;
        res.map(Some)
    }

    async fn sweep_locked(&self) -> anyhow::Result<SweepStats> {
        let cutoff = OffsetDateTime::now_utc() - self.grace;
        let old = |m: &object_store::ObjectMeta| {
            OffsetDateTime::from_unix_timestamp(m.last_modified.timestamp())
                .is_ok_and(|t| t < cutoff)
        };
        let store = self.blobs.inner();
        let mut stats = SweepStats::default();

        let mut listing = store
            .list(Some(&Path::from("sha256")))
            .try_filter(|m| std::future::ready(old(m)))
            .map_ok(|m| m.location.to_string())
            .try_chunks(BATCH);
        while let Some(keys) = listing.next().await {
            let keys = keys.map_err(|e| e.1)?;
            stats.scanned += keys.len() as u64;
            let unreferenced: Vec<String> = sqlx::query_scalar(
                "SELECT k FROM unnest($1::text[]) AS k
                 WHERE NOT EXISTS (SELECT 1 FROM storage.artifacts a WHERE a.object_key = k)",
            )
            .bind(&keys)
            .fetch_all(&self.db)
            .await?;
            for key in unreferenced {
                if self.delete_if_unreferenced(&key).await? {
                    stats.deleted += 1;
                }
            }
        }

        let tmp: Vec<Path> = store
            .list(Some(&Path::from("tmp")))
            .try_filter(|m| std::future::ready(old(m)))
            .map_ok(|m| m.location)
            .try_collect()
            .await?;
        for p in tmp {
            store.delete(&p).await?;
            stats.deleted += 1;
        }
        metrics::counter!("blob_gc_deleted_total").increment(stats.deleted);
        Ok(stats)
    }

    async fn delete_if_unreferenced(&self, key: &str) -> anyhow::Result<bool> {
        let mut tx = self.db.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        let referenced: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM storage.artifacts WHERE object_key = $1)",
        )
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        if !referenced {
            // Delete before commit: a waiting upload then sees the object gone and re-uploads.
            match self.blobs.inner().delete(&Path::from(key)).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        tx.commit().await?;
        Ok(!referenced)
    }
}
