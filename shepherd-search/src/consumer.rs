//! JetStream consumer that keeps `search.cards` in sync with storage events.
//!
//! Binds to the durable `search-indexer` declared in the stream topology. Messages are acked only
//! after the card write commits. Within a batch, events are coalesced per document. Each applied
//! write emits `shepherd.search.card.indexed.<doc_id>`.

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_nats::jetstream::{self, Message};
use futures::StreamExt;
use shepherd_common::{
    consumer::{dead_letter, retry_or_dead_letter},
    events::{CARD_INDEXED, CardIndexed, EVENTS_STREAM, Envelope, SEARCH_CONSUMER},
    topology::bind_consumer,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{SERVICE, backend::SearchBackend, indexer::Indexer};

const FETCH_EXPIRES: Duration = Duration::from_secs(2);

pub struct Consumer<B> {
    pub js: jetstream::Context,
    pub indexer: Arc<Indexer<B>>,
    pub batch: usize,
    pub concurrency: usize,
}

impl<B: SearchBackend> Consumer<B> {
    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let Some(mut consumer) =
            bind_consumer(&self.js, EVENTS_STREAM, SEARCH_CONSUMER, &shutdown).await
        else {
            return Ok(());
        };
        let max_deliver = consumer.info().await?.config.max_deliver;
        tracing::info!(consumer = SEARCH_CONSUMER, max_deliver, "consuming");

        loop {
            let fetch = consumer
                .fetch()
                .max_messages(self.batch)
                .expires(FETCH_EXPIRES)
                .messages();
            let batch = tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                b = fetch => b,
            };
            let mut batch = match batch {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "fetch failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let mut msgs = Vec::new();
            while let Some(m) = batch.next().await {
                match m {
                    Ok(m) => msgs.push(m),
                    Err(e) => tracing::warn!(error = %e, "batch message error"),
                }
            }
            if !msgs.is_empty() {
                self.process(msgs, max_deliver).await;
            }
        }
    }

    async fn process(&self, msgs: Vec<Message>, max_deliver: i64) {
        let mut groups: HashMap<Uuid, (u64, Vec<Message>)> = HashMap::new();
        for m in msgs {
            let seq = m.info().map(|i| i.stream_sequence).unwrap_or(0);
            match serde_json::from_slice::<Envelope<serde_json::Value>>(&m.payload) {
                Ok(ev) => {
                    let g = groups.entry(ev.subject).or_default();
                    g.0 = g.0.max(seq);
                    g.1.push(m);
                }
                Err(e) => {
                    dead_letter(
                        &self.js,
                        SEARCH_CONSUMER,
                        &m,
                        &format!("malformed envelope: {e}"),
                    )
                    .await
                }
            }
        }
        futures::stream::iter(groups)
            .for_each_concurrent(self.concurrency, |(doc_id, (seq, msgs))| async move {
                let started = std::time::Instant::now();
                match self.indexer.index(doc_id, seq, false).await {
                    Ok(outcome) => {
                        metrics::counter!("index_updates_total", "applied" => outcome.applied.to_string()).increment(1);
                        metrics::histogram!("index_update_duration_seconds").record(started.elapsed().as_secs_f64());
                        if outcome.applied {
                            self.announce(doc_id, outcome.schemas, outcome.deleted).await;
                        }
                        for m in &msgs {
                            if let Err(e) = m.ack().await {
                                tracing::warn!(error = %e, %doc_id, "ack failed");
                            }
                        }
                    }
                    Err(e) => {
                        metrics::counter!("index_errors_total").increment(1);
                        let err = format!("{e:#}");
                        tracing::warn!(error = %err, %doc_id, "index failed");
                        for m in &msgs {
                            retry_or_dead_letter(&self.js, SEARCH_CONSUMER, m, &err, max_deliver).await;
                        }
                    }
                }
            })
            .await;
    }

    /// Best effort: the card is already committed, so a failed publish is only logged.
    async fn announce(&self, doc_id: Uuid, schemas: Vec<String>, deleted: bool) {
        let ev = Envelope::new(
            CARD_INDEXED,
            SERVICE,
            doc_id,
            CardIndexed {
                document_id: doc_id,
                schemas,
                deleted,
            },
        );
        let payload = match serde_json::to_vec(&ev) {
            Ok(p) => p,
            Err(e) => return tracing::warn!(error = %e, "serialize card.indexed"),
        };
        match self.js.publish(ev.nats_subject(), payload.into()).await {
            Ok(ack) => {
                if let Err(e) = ack.await {
                    tracing::warn!(error = %e, %doc_id, "card.indexed not acknowledged");
                }
            }
            Err(e) => tracing::warn!(error = %e, %doc_id, "publish card.indexed failed"),
        }
    }
}
