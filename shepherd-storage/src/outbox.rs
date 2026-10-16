//! Transactional outbox relay: `storage.outbox` → JetStream.
//!
//! The target stream is declared by shepherd-stream; until it exists, publishes fail and are retried.
//! Rows are claimed with `FOR UPDATE SKIP LOCKED`, so multiple replicas can relay concurrently.
//! Each message carries `Nats-Msg-Id` = envelope id; JetStream drops duplicates within the stream's
//! duplicate window if a crash occurs between publish-ack and the `published_at` update.

use std::{sync::Arc, time::Duration};

use async_nats::{HeaderMap, header::NATS_MESSAGE_ID, jetstream};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const PRUNE_BATCH: i64 = 5000;

pub struct Relay {
    pub db: PgPool,
    pub js: jetstream::Context,
    pub wake: Arc<Notify>,
    pub batch: i64,
    pub poll: Duration,
    pub retention_days: i32,
}

impl Relay {
    pub async fn run(self, shutdown: CancellationToken) {
        let mut prune_every = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.poll) => {}
                _ = prune_every.tick() => {
                    if let Err(e) = prune(&self.db, self.retention_days).await {
                        tracing::warn!(error = %e, "outbox prune failed");
                    }
                    continue;
                }
            }
            loop {
                match self.publish_batch().await {
                    Ok(n) if n as i64 == self.batch => continue,
                    Ok(_) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "outbox publish failed; retrying");
                        metrics::counter!("outbox_publish_errors_total").increment(1);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        break;
                    }
                }
            }
        }
    }

    async fn publish_batch(&self) -> anyhow::Result<usize> {
        let mut tx = self.db.begin().await?;
        let rows: Vec<(i64, String, Value)> = sqlx::query_as(
            "SELECT id, subject, payload FROM storage.outbox
             WHERE published_at IS NULL ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind(self.batch)
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            return Ok(0);
        }

        let mut acks = Vec::with_capacity(rows.len());
        for (_, subject, payload) in &rows {
            let mut headers = HeaderMap::new();
            if let Some(id) = payload.get("id").and_then(Value::as_str) {
                headers.insert(NATS_MESSAGE_ID, id);
            }
            let body = serde_json::to_vec(payload)?;
            acks.push(
                self.js
                    .publish_with_headers(subject.clone(), headers, body.into())
                    .await?,
            );
        }
        futures::future::try_join_all(acks.into_iter().map(std::future::IntoFuture::into_future))
            .await?;

        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        sqlx::query("UPDATE storage.outbox SET published_at = now() WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        metrics::counter!("outbox_published_total").increment(ids.len() as u64);
        Ok(ids.len())
    }
}

/// Deletes published rows past retention in short transactions, oldest first, so a large
/// backlog neither holds locks for long nor produces one huge WAL burst.
pub async fn prune(db: &PgPool, retention_days: i32) -> sqlx::Result<u64> {
    let mut total = 0;
    loop {
        let n = sqlx::query(
            "DELETE FROM storage.outbox WHERE id IN (
                 SELECT id FROM storage.outbox
                 WHERE published_at < now() - make_interval(days => $1)
                 ORDER BY published_at LIMIT $2)",
        )
        .bind(retention_days)
        .bind(PRUNE_BATCH)
        .execute(db)
        .await?
        .rows_affected();
        total += n;
        if n < PRUNE_BATCH as u64 {
            break;
        }
    }
    metrics::counter!("outbox_pruned_total").increment(total);
    Ok(total)
}
