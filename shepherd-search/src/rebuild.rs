//! Full reindex from the storage schema without taking the index offline.
//!
//! State lives in `search.rebuilds` / `search.rebuild_shards` (`migrations/0004_rebuilds.sql`).
//! Starting a rebuild splits the document id space into shards at sampled boundaries. Every search
//! process runs a worker that claims one shard at a time (`FOR UPDATE SKIP LOCKED`), re-projects
//! its documents with `force`, and records the cursor and a heartbeat after each page. A shard
//! whose heartbeat is older than `STALE` is reclaimed and resumes from its cursor, so restarts lose
//! at most one page. When every shard is done, one worker deletes cards not re-stamped since the
//! rebuild began (documents no longer in storage) and closes the rebuild.

use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{backend::SearchBackend, indexer::Indexer};

const PAGE: i64 = 200;
const POLL: Duration = Duration::from_secs(5);
/// A claimed shard with no progress for this long is reclaimable.
const STALE: Duration = Duration::from_secs(120);
/// Cards stamped up to this long before the rebuild started are kept, absorbing clock skew
/// between search processes.
const ORPHAN_MARGIN: Duration = Duration::from_secs(60);
/// Target sampled rows when choosing shard boundaries.
const SAMPLE_ROWS: f64 = 50_000.0;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RebuildState {
    Idle,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RebuildStatus {
    pub state: RebuildState,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub indexed: u64,
    pub failed: u64,
    pub removed: u64,
    pub shards: u32,
    pub shards_done: u32,
    pub error: Option<String>,
}

impl RebuildStatus {
    fn idle() -> Self {
        Self {
            state: RebuildState::Idle,
            started_at: None,
            finished_at: None,
            indexed: 0,
            failed: 0,
            removed: 0,
            shards: 0,
            shards_done: 0,
            error: None,
        }
    }
}

pub struct Rebuilder {
    db: PgPool,
    shards: u32,
    /// Documents in flight per process; 0 disables this process's worker.
    concurrency: usize,
    owner: Uuid,
    wake: Notify,
}

struct Claim {
    rebuild_id: i64,
    shard: i32,
    upto: Option<Uuid>,
    cursor: Option<Uuid>,
}

impl Rebuilder {
    pub fn new(db: PgPool, shards: u32, concurrency: usize) -> Self {
        Self {
            db,
            shards: shards.max(1),
            concurrency,
            owner: Uuid::now_v7(),
            wake: Notify::new(),
        }
    }

    /// Latest rebuild, or `Idle` if none has run.
    pub async fn status(&self) -> anyhow::Result<RebuildStatus> {
        type Row = (
            String,
            OffsetDateTime,
            Option<OffsetDateTime>,
            i64,
            Option<String>,
            i64,
            i64,
            i64,
            i64,
        );
        let row: Option<Row> = sqlx::query_as(
            "SELECT r.state, r.started_at, r.finished_at, r.removed, r.error,
                    count(s.shard), count(s.shard) FILTER (WHERE s.done),
                    COALESCE(sum(s.indexed), 0)::bigint, COALESCE(sum(s.failed), 0)::bigint
             FROM search.rebuilds r LEFT JOIN search.rebuild_shards s ON s.rebuild_id = r.id
             WHERE r.id = (SELECT max(id) FROM search.rebuilds)
             GROUP BY r.id",
        )
        .fetch_optional(&self.db)
        .await?;
        let Some((state, started, finished, removed, error, shards, done, indexed, failed)) = row
        else {
            return Ok(RebuildStatus::idle());
        };
        Ok(RebuildStatus {
            state: match state.as_str() {
                "running" => RebuildState::Running,
                "succeeded" => RebuildState::Succeeded,
                _ => RebuildState::Failed,
            },
            started_at: Some(started),
            finished_at: finished,
            indexed: indexed as u64,
            failed: failed as u64,
            removed: removed as u64,
            shards: shards as u32,
            shards_done: done as u32,
            error,
        })
    }

    /// Creates a rebuild and its shards. Returns `false` if one is already running.
    pub async fn start(&self) -> anyhow::Result<bool> {
        let mut tx = self.db.begin().await?;
        let id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO search.rebuilds (state) VALUES ('running') ON CONFLICT DO NOTHING RETURNING id",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Ok(false);
        };
        let bounds = shard_bounds(&mut tx, self.shards).await?;
        // Shard k covers (bounds[k-1], bounds[k]]; the first and last are open-ended.
        let mut cursors: Vec<Option<Uuid>> = vec![None];
        cursors.extend(bounds.iter().copied().map(Some));
        let mut uptos: Vec<Option<Uuid>> = bounds.iter().copied().map(Some).collect();
        uptos.push(None);
        sqlx::query(
            "INSERT INTO search.rebuild_shards (rebuild_id, shard, cursor, upto)
             SELECT $1, (ord - 1)::int, c, u FROM unnest($2::uuid[], $3::uuid[]) WITH ORDINALITY AS t(c, u, ord)",
        )
        .bind(id)
        .bind(&cursors)
        .bind(&uptos)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(rebuild_id = id, shards = cursors.len(), "rebuild started");
        self.wake.notify_one();
        Ok(true)
    }

    /// Claims and processes shards of any running rebuild until `shutdown`.
    pub async fn run_worker<B: SearchBackend>(
        self: Arc<Self>,
        indexer: Arc<Indexer<B>>,
        shutdown: CancellationToken,
    ) {
        if self.concurrency == 0 {
            return;
        }
        loop {
            match self.step(&indexer, &shutdown).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = format!("{e:#}"), "rebuild worker"),
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(POLL) => {}
            }
        }
    }

    /// Processes one shard or finalizes one rebuild. Returns whether there was work.
    async fn step<B: SearchBackend>(
        &self,
        indexer: &Indexer<B>,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<bool> {
        if let Some(claim) = self.claim().await? {
            self.process(indexer, claim, shutdown).await?;
            return Ok(!shutdown.is_cancelled());
        }
        self.finalize().await
    }

    async fn claim(&self) -> sqlx::Result<Option<Claim>> {
        let row: Option<(i64, i32, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "UPDATE search.rebuild_shards s SET owner = $1, heartbeat_at = now()
             WHERE (s.rebuild_id, s.shard) = (
                 SELECT c.rebuild_id, c.shard FROM search.rebuild_shards c
                 JOIN search.rebuilds r ON r.id = c.rebuild_id
                 WHERE r.state = 'running' AND NOT c.done
                   AND (c.owner IS NULL OR c.heartbeat_at < now() - make_interval(secs => $2))
                 ORDER BY c.rebuild_id, c.shard LIMIT 1
                 FOR UPDATE OF c SKIP LOCKED)
             RETURNING s.rebuild_id, s.shard, s.upto, s.cursor",
        )
        .bind(self.owner)
        .bind(STALE.as_secs_f64())
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|(rebuild_id, shard, upto, cursor)| Claim {
            rebuild_id,
            shard,
            upto,
            cursor,
        }))
    }

    async fn process<B: SearchBackend>(
        &self,
        indexer: &Indexer<B>,
        mut c: Claim,
        shutdown: &CancellationToken,
    ) -> anyhow::Result<()> {
        loop {
            if shutdown.is_cancelled() {
                // Hand the shard back so another process can resume it without waiting for STALE.
                sqlx::query(
                    "UPDATE search.rebuild_shards SET owner = NULL, heartbeat_at = NULL
                     WHERE rebuild_id = $1 AND shard = $2 AND owner = $3",
                )
                .bind(c.rebuild_id)
                .bind(c.shard)
                .bind(self.owner)
                .execute(&self.db)
                .await?;
                return Ok(());
            }
            let ids = indexer.document_ids_in(c.cursor, c.upto, PAGE).await?;
            let done = (ids.len() as i64) < PAGE;
            let last = ids.last().copied().or(c.cursor);
            let (mut indexed, mut failed) = (0i64, 0i64);
            let results: Vec<_> = futures::stream::iter(ids)
                .map(|id| async move { (id, indexer.index(id, 0, true).await) })
                .buffer_unordered(self.concurrency)
                .collect()
                .await;
            for (id, r) in results {
                match r {
                    Ok(_) => indexed += 1,
                    Err(e) => {
                        failed += 1;
                        tracing::warn!(error = format!("{e:#}"), doc_id = %id, "rebuild: index failed");
                    }
                }
            }
            let owned = sqlx::query(
                "UPDATE search.rebuild_shards
                 SET cursor = $4, indexed = indexed + $5, failed = failed + $6, done = $7,
                     heartbeat_at = now()
                 WHERE rebuild_id = $1 AND shard = $2 AND owner = $3",
            )
            .bind(c.rebuild_id)
            .bind(c.shard)
            .bind(self.owner)
            .bind(last)
            .bind(indexed)
            .bind(failed)
            .bind(done)
            .execute(&self.db)
            .await?
            .rows_affected()
                == 1;
            if !owned {
                tracing::warn!(
                    rebuild_id = c.rebuild_id,
                    shard = c.shard,
                    "rebuild shard reclaimed by another worker"
                );
                return Ok(());
            }
            if done {
                return Ok(());
            }
            c.cursor = last;
        }
    }

    /// Closes a running rebuild whose shards are all done. Orphan removal and the state change
    /// commit together, so a crash leaves the rebuild running for another worker to finalize.
    async fn finalize(&self) -> anyhow::Result<bool> {
        let mut tx = self.db.begin().await?;
        let row: Option<(i64, OffsetDateTime)> = sqlx::query_as(
            "SELECT r.id, r.started_at FROM search.rebuilds r
             WHERE r.state = 'running'
               AND NOT EXISTS (SELECT 1 FROM search.rebuild_shards s WHERE s.rebuild_id = r.id AND NOT s.done)
             FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, started_at)) = row else {
            return Ok(false);
        };
        let failed: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(failed), 0)::bigint FROM search.rebuild_shards WHERE rebuild_id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if failed > 0 {
            // Skip orphan removal: failed documents were not re-stamped and would be deleted.
            sqlx::query(
                "UPDATE search.rebuilds SET state = 'failed', finished_at = now(), error = $2 WHERE id = $1",
            )
            .bind(id)
            .bind(format!("{failed} documents failed; orphan cleanup skipped"))
            .execute(&mut *tx)
            .await?;
        } else {
            let removed = sqlx::query("DELETE FROM search.cards WHERE indexed_at < $1")
                .bind(started_at - ORPHAN_MARGIN)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            sqlx::query(
                "UPDATE search.rebuilds SET state = 'succeeded', finished_at = now(), removed = $2 WHERE id = $1",
            )
            .bind(id)
            .bind(removed as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::info!(rebuild_id = id, failed, "rebuild finished");
        Ok(true)
    }
}

/// Up to `shards - 1` ascending, distinct ids splitting `storage.documents` into roughly equal
/// parts, from a block sample of about `SAMPLE_ROWS` rows. Empty for small or empty tables.
async fn shard_bounds(conn: &mut sqlx::PgConnection, shards: u32) -> sqlx::Result<Vec<Uuid>> {
    if shards < 2 {
        return Ok(Vec::new());
    }
    let estimate: f64 = sqlx::query_scalar(
        "SELECT GREATEST(reltuples, 0)::float8 FROM pg_class WHERE oid = 'storage.documents'::regclass",
    )
    .fetch_one(&mut *conn)
    .await?;
    let percent = if estimate <= SAMPLE_ROWS {
        100.0
    } else {
        (100.0 * SAMPLE_ROWS / estimate).max(0.01)
    };
    let fractions: Vec<f64> = (1..shards).map(|k| k as f64 / shards as f64).collect();
    let bounds: Option<Vec<Option<Uuid>>> = sqlx::query_scalar(
        "SELECT percentile_disc($1::float8[]) WITHIN GROUP (ORDER BY id)
         FROM storage.documents TABLESAMPLE SYSTEM ($2)",
    )
    .bind(&fractions)
    .bind(percent)
    .fetch_one(&mut *conn)
    .await?;
    let mut bounds: Vec<Uuid> = bounds.unwrap_or_default().into_iter().flatten().collect();
    bounds.dedup();
    Ok(bounds)
}
