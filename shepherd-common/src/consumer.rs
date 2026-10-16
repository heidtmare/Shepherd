//! Redelivery and dead-letter handling shared by JetStream consumers.

use std::time::Duration;

use async_nats::{
    HeaderMap,
    jetstream::{self, AckKind, Message},
};

use crate::events::DLQ_PREFIX;

pub const ERROR_HEADER: &str = "Shepherd-Error";
pub const ORIGINAL_SUBJECT_HEADER: &str = "Shepherd-Original-Subject";
pub const ORIGINAL_SEQUENCE_HEADER: &str = "Shepherd-Stream-Sequence";
pub const CONSUMER_HEADER: &str = "Shepherd-Consumer";
pub const REPLAYED_FROM_HEADER: &str = "Shepherd-Replayed-From";

const MAX_BACKOFF: Duration = Duration::from_secs(60);

pub fn delivered(m: &Message) -> i64 {
    m.info().map(|i| i.delivered).unwrap_or(1)
}

/// Naks with exponential backoff (2^attempt s, capped at 60 s), or dead-letters once
/// `max_deliver` attempts are used up.
pub async fn retry_or_dead_letter(
    js: &jetstream::Context,
    consumer: &str,
    m: &Message,
    err: &str,
    max_deliver: i64,
) {
    let attempt = delivered(m);
    if max_deliver > 0 && attempt >= max_deliver {
        dead_letter(js, consumer, m, err).await;
    } else {
        let delay =
            Duration::from_secs(2u64.saturating_pow(attempt.clamp(0, 16) as u32)).min(MAX_BACKOFF);
        if let Err(e) = m.ack_with(AckKind::Nak(Some(delay))).await {
            tracing::warn!(error = %e, "nak failed");
        }
    }
}

/// Publishes the original payload to `shepherd.dlq.<consumer>` with error context, then terminates
/// redelivery. If the DLQ publish fails the message is left unacked for redelivery after `ack_wait`.
pub async fn dead_letter(js: &jetstream::Context, consumer: &str, m: &Message, err: &str) {
    let mut headers = HeaderMap::new();
    headers.insert(
        ERROR_HEADER,
        err.chars().take(1024).collect::<String>().as_str(),
    );
    headers.insert(ORIGINAL_SUBJECT_HEADER, m.subject.as_str());
    headers.insert(CONSUMER_HEADER, consumer);
    if let Ok(info) = m.info() {
        headers.insert(
            ORIGINAL_SEQUENCE_HEADER,
            info.stream_sequence.to_string().as_str(),
        );
    }
    let subject = format!("{DLQ_PREFIX}.{consumer}");
    let published = match js
        .publish_with_headers(subject, headers, m.payload.clone())
        .await
    {
        Ok(ack) => ack.await.map(|_| ()).map_err(anyhow::Error::from),
        Err(e) => Err(e.into()),
    };
    match published {
        Ok(()) => {
            metrics::counter!("dead_letters_total", "consumer" => consumer.to_owned()).increment(1);
            tracing::error!(error = err, subject = %m.subject, consumer, "dead-lettered");
            if let Err(e) = m.ack_with(AckKind::Term).await {
                tracing::warn!(error = %e, "term failed");
            }
        }
        Err(e) => tracing::error!(error = %e, "dead-letter publish failed"),
    }
}
