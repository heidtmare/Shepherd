//! Search engine seam. `postgres` is the only implementation; the HTTP API and indexer depend
//! only on this trait.

pub mod postgres;

use std::future::Future;

use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    mapping::{Card, Page},
    model::{CardView, SearchResponse},
    query::Plan,
};

pub trait SearchBackend: Send + Sync + 'static {
    /// Replaces the card and its pages. Unless `force`, the write is skipped (returns `false`)
    /// when the stored card already reflects a JetStream sequence >= `seq`.
    fn upsert(
        &self,
        card: &Card,
        pages: &[Page],
        seq: u64,
        force: bool,
        indexed_at: OffsetDateTime,
    ) -> impl Future<Output = anyhow::Result<bool>> + Send;

    fn delete(&self, doc_id: Uuid) -> impl Future<Output = anyhow::Result<()>> + Send;

    fn search(&self, plan: &Plan) -> impl Future<Output = anyhow::Result<SearchResponse>> + Send;

    fn get(&self, doc_id: Uuid) -> impl Future<Output = anyhow::Result<Option<CardView>>> + Send;

    fn ping(&self) -> impl Future<Output = bool> + Send;
}
