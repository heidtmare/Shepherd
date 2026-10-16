//! `workbench.case.v1`: the searchable projection of a case, stored as a derived artifact of
//! its primary document so shepherd-search indexes it (see `shepherd-search/config/mapping.toml`).
//!
//! Written after each change; a background loop retries cases whose `indexed_revision` trails
//! `revision`, so a failed write only delays discovery. When the primary document changes, the
//! former primary gets a [`Retraction`] so its search card no longer carries the case.

use std::{sync::Arc, time::Duration};

use serde::Serialize;
use shepherd_common::storage_client::StorageClient;
use shepherd_forms::template::TemplateSet;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    PRODUCER, SERVICE,
    benefits::Benefits,
    case::CASE_SCHEMA,
    case_db::{self, Case, CaseDocument},
};

#[derive(Clone)]
pub struct Projector {
    pub db: PgPool,
    pub storage: StorageClient,
    pub benefits: Arc<Benefits>,
    pub templates: Arc<TemplateSet>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Party {
    pub family_name: Option<String>,
    pub given_name: Option<String>,
    pub a_number: Option<String>,
    pub date_of_birth: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ProjectedDocument {
    pub document_id: Uuid,
    pub role: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ProjectedDecision {
    pub outcome: String,
    pub decided_by: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub decided_at: Option<time::OffsetDateTime>,
    pub artifact_id: Option<Uuid>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CaseArtifact {
    pub schema: &'static str,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub form_number: Option<String>,
    pub benefit: Option<String>,
    pub status: String,
    /// `YYYY-MM-DD`.
    pub received_at: String,
    pub response_due: Option<String>,
    pub applicant: Party,
    pub beneficiary: Option<Party>,
    pub documents: Vec<ProjectedDocument>,
    pub decision: Option<ProjectedDecision>,
    pub revision: i32,
}

/// `workbench.case.v1` on a former primary document: the case is projected on
/// `primary_document_id` instead. Carries no indexed fields.
#[derive(Debug, Serialize)]
pub struct Retraction {
    pub schema: &'static str,
    pub case_id: Uuid,
    pub retracted: bool,
    pub primary_document_id: Uuid,
}

impl CaseArtifact {
    pub fn build(
        c: &Case,
        docs: &[CaseDocument],
        benefits: &Benefits,
        templates: &TemplateSet,
    ) -> Self {
        let fact = |k: &str| c.facts.0.get(k).cloned();
        let party = |prefix: &str| Party {
            family_name: fact(&format!("{prefix}family_name")),
            given_name: fact(&format!("{prefix}given_name")),
            a_number: fact(&format!("{prefix}a_number")),
            date_of_birth: fact(&format!("{prefix}date_of_birth")),
        };
        let beneficiary = party("beneficiary_");
        Self {
            schema: CASE_SCHEMA,
            case_id: c.id,
            receipt_number: c.receipt_number.clone(),
            form_type: c.form_type.clone(),
            form_number: templates
                .latest(&c.form_type)
                .map(|t| t.form_number.clone()),
            benefit: benefits.get(&c.form_type).map(|b| b.name.clone()),
            status: c.status.clone(),
            received_at: c.received_date.clone(),
            response_due: c.response_due.clone(),
            applicant: party(""),
            beneficiary: (beneficiary
                != Party {
                    family_name: None,
                    given_name: None,
                    a_number: None,
                    date_of_birth: None,
                })
            .then_some(beneficiary),
            documents: docs
                .iter()
                .map(|d| ProjectedDocument {
                    document_id: d.document_id,
                    role: d.role.clone(),
                })
                .collect(),
            decision: c.decision.clone().map(|outcome| ProjectedDecision {
                outcome,
                decided_by: c.decided_by.clone(),
                decided_at: c.decided_at,
                artifact_id: c.decision_artifact_id,
            }),
            revision: c.revision,
        }
    }
}

impl Projector {
    /// Writes the projection if the stored one is stale.
    pub async fn sync(&self, id: Uuid) -> anyhow::Result<()> {
        let Some(c) = case_db::get(&self.db, id).await? else {
            return Ok(());
        };
        if c.indexed_revision >= c.revision {
            return Ok(());
        }
        let docs = case_db::documents(&self.db, id).await?;
        let body = serde_json::to_vec(&CaseArtifact::build(
            &c,
            &docs,
            &self.benefits,
            &self.templates,
        ))?;
        let parent = self
            .storage
            .document(c.primary_document_id)
            .await?
            .source()
            .map(|a| a.id)
            .ok_or_else(|| anyhow::anyhow!("document {} has no source", c.primary_document_id))?;
        let art = self
            .storage
            .create_artifact_as(
                c.primary_document_id,
                CASE_SCHEMA,
                PRODUCER,
                parent,
                SERVICE,
                body,
            )
            .await?;
        case_db::mark_indexed(&self.db, id, c.revision, art.id).await?;
        metrics::counter!("case_projections_total").increment(1);
        Ok(())
    }

    /// Writes a [`Retraction`] on the former primary `document_id`.
    pub async fn retract(
        &self,
        case_id: Uuid,
        document_id: Uuid,
        primary_document_id: Uuid,
    ) -> anyhow::Result<()> {
        let body = serde_json::to_vec(&Retraction {
            schema: CASE_SCHEMA,
            case_id,
            retracted: true,
            primary_document_id,
        })?;
        let parent = self
            .storage
            .document(document_id)
            .await?
            .source()
            .map(|a| a.id)
            .ok_or_else(|| anyhow::anyhow!("document {document_id} has no source"))?;
        self.storage
            .create_artifact_as(document_id, CASE_SCHEMA, PRODUCER, parent, SERVICE, body)
            .await?;
        Ok(())
    }

    pub fn spawn_sync(&self, id: Uuid) {
        let p = self.clone();
        tokio::spawn(async move {
            if let Err(e) = p.sync(id).await {
                tracing::warn!(error = format!("{e:#}"), case_id = %id, "case projection deferred");
            }
        });
    }

    pub async fn run(self, every: Duration, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
            let ids = match case_db::unindexed(&self.db, 100).await {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(error = %e, "unindexed case scan failed");
                    continue;
                }
            };
            for id in ids {
                if let Err(e) = self.sync(id).await {
                    tracing::warn!(error = format!("{e:#}"), case_id = %id, "case projection failed");
                }
            }
        }
    }
}
