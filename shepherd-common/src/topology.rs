//! Declarative JetStream topology (streams and durable consumers).
//!
//! `shepherd-stream` applies the topology at startup. Other services only bind to the
//! consumers it declares, so settings such as `max_deliver` and `ack_wait` live in one file.

use std::time::Duration;

use async_nats::jetstream::{
    self,
    consumer::{self, PullConsumer, pull},
    stream,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Topology {
    #[serde(rename = "stream")]
    pub streams: Vec<StreamDef>,
    #[serde(default, rename = "consumer")]
    pub consumers: Vec<ConsumerDef>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Storage {
    #[default]
    File,
    Memory,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDef {
    pub name: String,
    pub subjects: Vec<String>,
    #[serde(default)]
    pub storage: Storage,
    /// 0 = unlimited.
    #[serde(default)]
    pub max_age_days: u64,
    #[serde(default = "default_dup_window")]
    pub duplicate_window_secs: u64,
    #[serde(default = "default_replicas")]
    pub replicas: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeliverFrom {
    #[default]
    All,
    New,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerDef {
    pub stream: String,
    pub name: String,
    pub filter_subjects: Vec<String>,
    #[serde(default = "default_ack_wait")]
    pub ack_wait_secs: u64,
    #[serde(default = "default_max_deliver")]
    pub max_deliver: i64,
    #[serde(default = "default_max_ack_pending")]
    pub max_ack_pending: i64,
    #[serde(default)]
    pub deliver_from: DeliverFrom,
}

fn default_dup_window() -> u64 {
    120
}
fn default_replicas() -> usize {
    1
}
fn default_ack_wait() -> u64 {
    30
}
fn default_max_deliver() -> i64 {
    5
}
fn default_max_ack_pending() -> i64 {
    1000
}

impl StreamDef {
    fn config(&self) -> stream::Config {
        stream::Config {
            name: self.name.clone(),
            subjects: self.subjects.clone(),
            storage: match self.storage {
                Storage::File => stream::StorageType::File,
                Storage::Memory => stream::StorageType::Memory,
            },
            max_age: Duration::from_secs(self.max_age_days * 86_400),
            duplicate_window: Duration::from_secs(self.duplicate_window_secs),
            num_replicas: self.replicas,
            allow_direct: true,
            ..Default::default()
        }
    }
}

impl ConsumerDef {
    fn config(&self) -> pull::Config {
        pull::Config {
            durable_name: Some(self.name.clone()),
            filter_subjects: self.filter_subjects.clone(),
            ack_policy: consumer::AckPolicy::Explicit,
            ack_wait: Duration::from_secs(self.ack_wait_secs),
            max_deliver: self.max_deliver,
            max_ack_pending: self.max_ack_pending,
            deliver_policy: match self.deliver_from {
                DeliverFrom::All => consumer::DeliverPolicy::All,
                DeliverFrom::New => consumer::DeliverPolicy::New,
            },
            ..Default::default()
        }
    }
}

impl Topology {
    pub fn parse(src: &str) -> anyhow::Result<Self> {
        let t: Topology = toml::from_str(src)?;
        for c in &t.consumers {
            anyhow::ensure!(
                t.streams.iter().any(|s| s.name == c.stream),
                "consumer {} references unknown stream {}",
                c.name,
                c.stream
            );
            anyhow::ensure!(
                !c.filter_subjects.is_empty(),
                "consumer {} has no filter_subjects",
                c.name
            );
        }
        Ok(t)
    }

    /// Creates or updates every stream and consumer. Idempotent.
    pub async fn apply(&self, js: &jetstream::Context) -> anyhow::Result<()> {
        for s in &self.streams {
            js.create_or_update_stream(s.config())
                .await
                .map_err(|e| anyhow::anyhow!("stream {}: {e}", s.name))?;
        }
        for c in &self.consumers {
            let stream = js.get_stream(&c.stream).await?;
            let exists = stream.consumer_info(&c.name).await.is_ok();
            let res: Result<PullConsumer, String> = if exists {
                stream
                    .update_consumer(c.config())
                    .await
                    .map_err(|e| e.to_string())
            } else {
                stream
                    .create_consumer(c.config())
                    .await
                    .map_err(|e| e.to_string())
            };
            res.map_err(|e| anyhow::anyhow!("consumer {}/{}: {e}", c.stream, c.name))?;
        }
        Ok(())
    }
}

/// Waits until the durable consumer exists (declared by `shepherd-stream`), then returns it.
/// Returns `None` if `shutdown` fires first.
pub async fn bind_consumer(
    js: &jetstream::Context,
    stream: &str,
    name: &str,
    shutdown: &CancellationToken,
) -> Option<PullConsumer> {
    loop {
        let res = match js.get_stream(stream).await {
            Ok(s) => s
                .get_consumer::<pull::Config>(name)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match res {
            Ok(c) => return Some(c),
            Err(e) => {
                tracing::info!(error = %e, "waiting for consumer {stream}/{name}");
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_topology_parses() {
        let t =
            Topology::parse(include_str!("../../shepherd-stream/config/topology.toml")).unwrap();
        let names: Vec<_> = t.consumers.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&crate::events::SEARCH_CONSUMER));
        assert!(names.contains(&crate::events::INTAKE_CONSUMER));
        assert!(names.contains(&crate::events::RECOGNIZER_CONSUMER));
        assert!(names.contains(&crate::events::RECEIPTING_CONSUMER));
        assert!(names.contains(&crate::events::CORRESPONDENCE_CONSUMER));
        assert!(names.contains(&crate::events::CASE_CORRESPONDENCE_CONSUMER));
        assert!(names.contains(&crate::events::INFERENCE_CONSUMER));
        assert!(
            t.streams
                .iter()
                .any(|s| s.name == crate::events::EVENTS_STREAM)
        );
        assert!(
            t.streams
                .iter()
                .any(|s| s.name == crate::events::DLQ_STREAM)
        );
    }

    #[test]
    fn rejects_dangling_consumer() {
        let src = "[[stream]]\nname = \"A\"\nsubjects = [\"a.>\"]\n[[consumer]]\nstream = \"B\"\nname = \"c\"\nfilter_subjects = [\"a.>\"]";
        assert!(Topology::parse(src).is_err());
    }
}
