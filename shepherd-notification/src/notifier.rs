//! Letter generation and e-mail delivery.
//!
//! A letter is recorded once per triggering event (`(kind, source_id)`), then generated when its
//! data is complete: the PDF (`notification.letter.pdf`) and a JSON record
//! (`notification.letter.v1`) are stored on the case's primary document, and deliveries are
//! queued. Incomplete letters stay `held` and are retried by [`Notifier::run`].

use std::{sync::Arc, time::Duration};

use serde::Serialize;
use sha2::{Digest, Sha256};
use shepherd_common::{
    events::{
        DELIVERY_FAILED, DELIVERY_SENT, DeliveryFailed, DeliverySent, LETTER_GENERATED,
        LETTER_HELD, LetterGenerated, LetterHeld,
    },
    storage_client::StorageClient,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    PRODUCER, SERVICE,
    db::{self, Delivery, Letter},
    email::Mailer,
    letters::{self, Hold, Kind, LetterConfig, Source},
    pdf,
    workbench::Workbench,
};

pub const LETTER_SCHEMA: &str = "notification.letter.v1";
pub const PDF_SCHEMA: &str = "notification.letter.pdf";

#[derive(Clone)]
pub struct Notifier {
    pub db: PgPool,
    pub storage: StorageClient,
    pub workbench: Workbench,
    pub js: async_nats::jetstream::Context,
    pub letters: Arc<LetterConfig>,
    pub mailer: Option<Mailer>,
    pub email_max_attempts: i32,
}

/// Result of one generation attempt.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Generated,
    Held(String),
    /// Already generated, or the letter does not exist.
    Unchanged,
}

#[derive(Serialize)]
struct LetterArtifact<'a> {
    schema: &'static str,
    letter_id: Uuid,
    case_id: Uuid,
    source_id: Uuid,
    source_artifact_id: Option<Uuid>,
    pdf_artifact_id: Uuid,
    pdf_sha256: &'a str,
    channels: &'a [&'a str],
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    #[serde(flatten)]
    letter: &'a letters::Letter,
}

impl Notifier {
    /// Records the letter for an event and tries to generate it.
    pub async fn request(&self, l: &db::NewLetter<'_>) -> anyhow::Result<Outcome> {
        let id = db::upsert_letter(&self.db, l).await?;
        self.process(id).await
    }

    async fn source(&self, l: &Letter) -> anyhow::Result<Source> {
        let kind = Kind::parse(&l.kind).ok_or_else(|| anyhow::anyhow!("kind {}", l.kind))?;
        if kind == Kind::Receipt {
            return Ok(Source::Receipt);
        }
        let art = l
            .source_artifact_id
            .ok_or_else(|| anyhow::anyhow!("letter {} has no source artifact", l.id))?;
        let body = self.storage.content(art).await?;
        Ok(match kind {
            Kind::Rfe | Kind::Noid => Source::Notice(serde_json::from_slice(&body)?),
            _ => Source::Decision(serde_json::from_slice(&body)?),
        })
    }

    /// Generates a held letter if its data is now complete. Serialized per letter by a row lock.
    /// A failed attempt is recorded on the letter (`last_error`) so the retry scan moves past it.
    pub async fn process(&self, id: Uuid) -> anyhow::Result<Outcome> {
        let res = self.generate(id).await;
        if let Err(e) = &res
            && let Err(re) = db::record_error(&self.db, id, &format!("{e:#}")).await
        {
            tracing::warn!(error = %re, letter_id = %id, "letter error not recorded");
        }
        res
    }

    async fn generate(&self, id: Uuid) -> anyhow::Result<Outcome> {
        let mut tx = self.db.begin().await?;
        let Some(l) = db::lock_letter(&mut tx, id).await? else {
            return Ok(Outcome::Unchanged);
        };
        if l.status == "generated" {
            return Ok(Outcome::Unchanged);
        }
        let today = OffsetDateTime::now_utc().date();
        let composed = match self.workbench.case(l.case_id).await? {
            None => Err(Hold("case not found in workbench".into())),
            Some(case) => {
                let source = self.source(&l).await?;
                letters::compose(&self.letters, &case, &source, today)
                    .map(|letter| (case, source, letter))
            }
        };
        let (case, source, letter) = match composed {
            Ok(v) => v,
            Err(Hold(reason)) => {
                let changed = db::hold(&mut tx, &l, &reason).await?;
                tx.commit().await?;
                metrics::counter!("letters_held_total", "kind" => l.kind.clone()).increment(1);
                if changed {
                    tracing::info!(letter_id = %l.id, case_id = %l.case_id, reason, "letter held");
                    crate::publish(
                        &self.js,
                        LETTER_HELD,
                        l.document_id,
                        LetterHeld {
                            letter_id: l.id,
                            case_id: l.case_id,
                            receipt_number: l.receipt_number.clone(),
                            kind: l.kind.clone(),
                            reason: reason.clone(),
                        },
                    )
                    .await;
                }
                return Ok(Outcome::Held(reason));
            }
        };

        let email = match (
            &case.email,
            letters::email(&self.letters, &case, &source, today),
        ) {
            (Some(to), Some(content)) => Some((to.clone(), content)),
            _ => None,
        };
        let mut channels = vec!["mail"];
        if email.is_some() {
            channels.push("email");
        }

        let pdf_bytes = pdf::render(&letter)?;
        let pdf_sha = hex::encode(Sha256::digest(&pdf_bytes));
        let parent = match l.source_artifact_id {
            Some(a) => a,
            None => self
                .storage
                .document(l.document_id)
                .await?
                .source()
                .map(|a| a.id)
                .ok_or_else(|| anyhow::anyhow!("document {} has no source", l.document_id))?,
        };
        let pdf_art = self
            .storage
            .create_typed_artifact_as(
                l.document_id,
                PDF_SCHEMA,
                PRODUCER,
                parent,
                SERVICE,
                "application/pdf",
                pdf_bytes,
            )
            .await?;
        let body = serde_json::to_vec(&LetterArtifact {
            schema: LETTER_SCHEMA,
            letter_id: l.id,
            case_id: l.case_id,
            source_id: l.source_id,
            source_artifact_id: l.source_artifact_id,
            pdf_artifact_id: pdf_art.id,
            pdf_sha256: &pdf_sha,
            channels: &channels,
            generated_at: OffsetDateTime::now_utc(),
            letter: &letter,
        })?;
        let art = self
            .storage
            .create_artifact_as(
                l.document_id,
                LETTER_SCHEMA,
                PRODUCER,
                pdf_art.id,
                SERVICE,
                body,
            )
            .await?;

        let recipient = serde_json::to_value(&letter.recipient)?;
        db::mark_generated(&mut tx, l.id, art.id, pdf_art.id, &recipient).await?;
        db::insert_delivery(
            &mut tx,
            &db::NewDelivery {
                letter_id: l.id,
                channel: "mail",
                address: &letter.recipient.postal(),
                subject: None,
                body: None,
            },
        )
        .await?;
        if let Some((to, (subject, body))) = &email {
            db::insert_delivery(
                &mut tx,
                &db::NewDelivery {
                    letter_id: l.id,
                    channel: "email",
                    address: to,
                    subject: Some(subject),
                    body: Some(body),
                },
            )
            .await?;
        }
        tx.commit().await?;
        metrics::counter!("letters_generated_total", "kind" => l.kind.clone()).increment(1);
        tracing::info!(letter_id = %l.id, case_id = %l.case_id, kind = l.kind, "letter generated");
        crate::publish(
            &self.js,
            LETTER_GENERATED,
            l.document_id,
            LetterGenerated {
                letter_id: l.id,
                case_id: l.case_id,
                receipt_number: l.receipt_number.clone(),
                kind: l.kind.clone(),
                artifact_id: art.id,
                pdf_artifact_id: pdf_art.id,
                channels: channels.iter().map(|c| c.to_string()).collect(),
            },
        )
        .await;
        if email.is_some() {
            self.spawn_send();
        }
        Ok(Outcome::Generated)
    }

    pub fn spawn_send(&self) {
        let n = self.clone();
        tokio::spawn(async move {
            if let Err(e) = n.send_due().await {
                tracing::warn!(error = format!("{e:#}"), "e-mail send pass failed");
            }
        });
    }

    /// Sends every due e-mail delivery. No-op without an SMTP server.
    pub async fn send_due(&self) -> anyhow::Result<usize> {
        let Some(mailer) = &self.mailer else {
            return Ok(0);
        };
        let mut sent = 0;
        loop {
            let mut tx = self.db.begin().await?;
            let Some(d) = db::claim_due_email(&mut tx).await? else {
                return Ok(sent);
            };
            let result = mailer
                .send(
                    d.id,
                    &d.address,
                    d.subject.as_deref().unwrap_or_default(),
                    d.body.as_deref().unwrap_or_default(),
                )
                .await;
            let updated = match result {
                Ok(message_id) => {
                    sent += 1;
                    db::mark_sent(&mut tx, d.id, None, Some(&message_id)).await?
                }
                Err(e) => {
                    let attempt = d.attempts + 1;
                    let terminal = attempt >= self.email_max_attempts;
                    let backoff =
                        time::Duration::seconds(30 * 2i64.pow(attempt.clamp(0, 10) as u32))
                            .min(time::Duration::hours(1));
                    tracing::warn!(error = format!("{e:#}"), delivery_id = %d.id, attempt, terminal, "e-mail failed");
                    db::mark_attempt_failed(&mut tx, d.id, &format!("{e:#}"), terminal, backoff)
                        .await?
                }
            };
            tx.commit().await?;
            self.delivery_event(&updated, None).await?;
        }
    }

    /// Publishes `delivery.sent`, or `delivery.failed` for a failed attempt.
    pub async fn delivery_event(&self, d: &Delivery, sent_by: Option<&str>) -> anyhow::Result<()> {
        metrics::counter!("deliveries_total", "channel" => d.channel.clone(), "status" => d.status.clone())
            .increment(1);
        let Some(l) = db::get_letter(&self.db, d.letter_id).await? else {
            return Ok(());
        };
        if d.status == "sent" {
            crate::publish(
                &self.js,
                DELIVERY_SENT,
                l.document_id,
                DeliverySent {
                    letter_id: l.id,
                    delivery_id: d.id,
                    case_id: l.case_id,
                    receipt_number: l.receipt_number,
                    kind: l.kind,
                    channel: d.channel.clone(),
                    sent_by: sent_by.map(str::to_owned),
                },
            )
            .await;
        } else if let Some(err) = &d.last_error {
            crate::publish(
                &self.js,
                DELIVERY_FAILED,
                l.document_id,
                DeliveryFailed {
                    letter_id: l.id,
                    delivery_id: d.id,
                    case_id: l.case_id,
                    receipt_number: l.receipt_number,
                    kind: l.kind,
                    channel: d.channel.clone(),
                    error: err.clone(),
                    attempt: d.attempts,
                    terminal: d.status == "failed",
                },
            )
            .await;
        }
        Ok(())
    }

    /// Retries held letters and sends due e-mail every `every`.
    pub async fn run(self, every: Duration, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
            match db::held(&self.db, 100).await {
                Ok(ids) => {
                    for id in ids {
                        if let Err(e) = self.process(id).await {
                            tracing::warn!(error = format!("{e:#}"), letter_id = %id, "letter retry failed");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "held letter scan failed"),
            }
            if let Err(e) = self.send_due().await {
                tracing::warn!(error = format!("{e:#}"), "e-mail send pass failed");
            }
        }
    }
}
