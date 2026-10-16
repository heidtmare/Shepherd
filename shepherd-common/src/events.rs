//! Event contract published on NATS JetStream.
//!
//! Subjects are `<type>.<document_id>`, where `<type>` is `shepherd.<service>.<entity>.<action>`.
//! The fixed token count allows per-document filtering across services with
//! [`document_filter`].

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Stream holding all domain events (`shepherd.{storage,intake,search,workbench,notification,inference}.>`).
pub const EVENTS_STREAM: &str = "SHEPHERD_EVENTS";
/// Stream holding dead-lettered messages (`shepherd.dlq.<consumer>`).
pub const DLQ_STREAM: &str = "SHEPHERD_DLQ";
pub const DLQ_PREFIX: &str = "shepherd.dlq";

pub const DOCUMENT_CREATED: &str = "shepherd.storage.document.created";
pub const ARTIFACT_CREATED: &str = "shepherd.storage.artifact.created";
pub const EXTRACTION_REQUESTED: &str = "shepherd.intake.extraction.requested";
pub const EXTRACTION_STARTED: &str = "shepherd.intake.extraction.started";
pub const EXTRACTION_COMPLETED: &str = "shepherd.intake.extraction.completed";
pub const EXTRACTION_FAILED: &str = "shepherd.intake.extraction.failed";
pub const RECOGNITION_REQUESTED: &str = "shepherd.intake.recognition.requested";
pub const RECOGNITION_STARTED: &str = "shepherd.intake.recognition.started";
pub const RECOGNITION_COMPLETED: &str = "shepherd.intake.recognition.completed";
pub const RECOGNITION_FAILED: &str = "shepherd.intake.recognition.failed";
pub const CARD_INDEXED: &str = "shepherd.search.card.indexed";
pub const RECEIPTING_REQUIRED: &str = "shepherd.workbench.receipting.required";
pub const RECEIPTING_COMPLETED: &str = "shepherd.workbench.receipting.completed";
pub const CASE_OPENED: &str = "shepherd.workbench.case.opened";
pub const CASE_DECIDED: &str = "shepherd.workbench.case.decided";
pub const CASE_REOPENED: &str = "shepherd.workbench.case.reopened";
pub const CASE_ASSIGNED: &str = "shepherd.workbench.case.assigned";
pub const NOTICE_ISSUED: &str = "shepherd.workbench.notice.issued";
pub const NOTICE_EXTENDED: &str = "shepherd.workbench.notice.extended";
pub const NOTICE_RESCINDED: &str = "shepherd.workbench.notice.rescinded";
pub const LETTER_GENERATED: &str = "shepherd.notification.letter.generated";
pub const LETTER_HELD: &str = "shepherd.notification.letter.held";
pub const DELIVERY_SENT: &str = "shepherd.notification.delivery.sent";
pub const DELIVERY_FAILED: &str = "shepherd.notification.delivery.failed";
pub const FINDING_RAISED: &str = "shepherd.inference.finding.raised";
pub const FINDING_RESOLVED: &str = "shepherd.inference.finding.resolved";

/// Durable consumer names; definitions live in `shepherd-stream/config/topology.toml`.
pub const SEARCH_CONSUMER: &str = "search-indexer";
pub const INTAKE_CONSUMER: &str = "intake-extractor";
pub const RECOGNIZER_CONSUMER: &str = "intake-recognizer";
pub const RECEIPTING_CONSUMER: &str = "workbench-receipting";
pub const CORRESPONDENCE_CONSUMER: &str = "notification-correspondence";
pub const CASE_CORRESPONDENCE_CONSUMER: &str = "workbench-correspondence";
pub const INFERENCE_CONSUMER: &str = "inference-rules";

pub fn subject(kind: &str, document_id: Uuid) -> String {
    format!("{kind}.{document_id}")
}

/// Matches every domain event for one document.
pub fn document_filter(document_id: Uuid) -> String {
    format!("shepherd.*.*.*.{document_id}")
}

/// CloudEvents-style envelope. `subject` is always the document id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope<T> {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub kind: String,
    pub source: String,
    #[serde(with = "time::serde::rfc3339")]
    pub time: OffsetDateTime,
    pub subject: Uuid,
    pub data: T,
}

impl<T> Envelope<T> {
    pub fn new(kind: &str, source: &str, subject: Uuid, data: T) -> Self {
        Self {
            id: Uuid::now_v7(),
            kind: kind.to_owned(),
            source: source.to_owned(),
            time: OffsetDateTime::now_utc(),
            subject,
            data,
        }
    }

    /// NATS subject for this event.
    pub fn nats_subject(&self) -> String {
        subject(&self.kind, self.subject)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentCreated {
    pub document_id: Uuid,
    pub artifact_id: Uuid,
    pub sha256: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactCreated {
    pub document_id: Uuid,
    pub artifact_id: Uuid,
    pub schema: String,
    pub version: i32,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionRequested {
    pub document_id: Uuid,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionStarted {
    pub document_id: Uuid,
    pub source_artifact_id: Uuid,
    pub extractor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionCompleted {
    pub document_id: Uuid,
    pub source_artifact_id: Uuid,
    pub artifact_id: Uuid,
    pub schema: String,
    pub version: i32,
    pub pages: u32,
    pub text_pages: u32,
    pub needs_ocr: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractionFailed {
    pub document_id: Uuid,
    pub error: String,
    pub attempt: i64,
    /// No further retries will be made.
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecognitionRequested {
    pub document_id: Uuid,
    /// Form type (classify among its editions) or template id, instead of automatic classification.
    pub form_type: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecognitionStarted {
    pub document_id: Uuid,
    pub source_artifact_id: Uuid,
    pub recognizer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecognitionCompleted {
    pub document_id: Uuid,
    pub source_artifact_id: Uuid,
    pub artifact_id: Uuid,
    pub version: i32,
    /// Form type of the matched template; `None` when the document is not a known form. Events
    /// recorded before form editions carry the template id here, which equalled the form type.
    pub form_type: Option<String>,
    /// Matched template (edition) id.
    #[serde(default)]
    pub template_id: Option<String>,
    pub fields: usize,
    pub filled: usize,
    pub requires_receipting: usize,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecognitionFailed {
    pub document_id: Uuid,
    pub error: String,
    pub attempt: i64,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReceiptingRequired {
    pub document_id: Uuid,
    pub task_id: Uuid,
    pub form_type: Option<String>,
    pub flagged_fields: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReceiptingCompleted {
    pub document_id: Uuid,
    pub task_id: Uuid,
    pub artifact_id: Uuid,
    pub version: i32,
    pub form_type: String,
    /// `review` (flagged fields resolved) or `full` (every value entered by a person).
    pub mode: String,
    pub receipted_by: String,
    pub changed_fields: usize,
}

/// Case events use the case's primary document as the subject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseOpened {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub document_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NoticeIssued {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub notice_id: Uuid,
    /// `rfe` (request for evidence) or `noid` (notice of intent to deny).
    pub kind: String,
    /// `workbench.notice.v1` artifact.
    pub artifact_id: Uuid,
    /// `YYYY-MM-DD`.
    pub due_date: String,
}

/// The response window of an outstanding notice was extended.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NoticeExtended {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub notice_id: Uuid,
    pub kind: String,
    /// `YYYY-MM-DD`.
    pub previous_due_date: String,
    /// `YYYY-MM-DD`.
    pub due_date: String,
    pub extended_by: String,
    pub reason: String,
}

/// An outstanding notice was withdrawn; the case returned to review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NoticeRescinded {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub notice_id: Uuid,
    pub kind: String,
    pub rescinded_by: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseDecided {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    /// `approved`, `denied`, `withdrawn` or `closed` (administrative closure).
    pub outcome: String,
    pub decided_by: String,
    /// `workbench.decision.v1` artifact.
    pub artifact_id: Uuid,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseReopened {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub reopened_by: String,
    pub reason: String,
    /// Decision artifact being reconsidered.
    pub supersedes: Option<Uuid>,
}

/// Assignee changed (assigned, reassigned or unassigned).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseAssigned {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    /// `None` when unassigned.
    pub assignee: Option<String>,
    pub previous: Option<String>,
    /// Acting user; `None` for rule assignments.
    pub assigned_by: Option<String>,
    /// `rule`, `manager`, `request` or `reopen`.
    pub via: String,
    pub rule_id: Option<Uuid>,
}

/// Letter events use the case's primary document as the subject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LetterGenerated {
    pub letter_id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    /// `receipt`, `rfe`, `noid`, `approval`, `denial` or `withdrawal`.
    pub kind: String,
    /// `notification.letter.v1` artifact.
    pub artifact_id: Uuid,
    /// `notification.letter.pdf` artifact.
    pub pdf_artifact_id: Uuid,
    /// Delivery channels queued: `mail`, `email`.
    pub channels: Vec<String>,
}

/// Letter cannot be generated yet (e.g. incomplete mailing address); retried until resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LetterHeld {
    pub letter_id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub kind: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliverySent {
    pub letter_id: Uuid,
    pub delivery_id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub kind: String,
    /// `mail` or `email`.
    pub channel: String,
    /// User who recorded a mailing; `None` for automatic sends.
    pub sent_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveryFailed {
    pub letter_id: Uuid,
    pub delivery_id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub kind: String,
    pub channel: String,
    pub error: String,
    pub attempt: i32,
    /// No further retries will be made.
    pub terminal: bool,
}

/// Finding events use the case's primary document as the subject. Payloads carry no party data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FindingRaised {
    pub finding_id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub rule: String,
    /// `low`, `medium` or `high`.
    pub severity: String,
    /// Other cases the finding involves.
    pub related_cases: Vec<Uuid>,
}

/// The rule no longer fires for the finding (data corrected, rule disabled).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FindingResolved {
    pub finding_id: Uuid,
    pub case_id: Uuid,
    pub receipt_number: String,
    pub rule: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CardIndexed {
    pub document_id: Uuid,
    pub schemas: Vec<String>,
    /// Card removed because the document no longer exists.
    pub deleted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let doc = Uuid::now_v7();
        let ev = Envelope::new(
            ARTIFACT_CREATED,
            "shepherd-storage",
            doc,
            ArtifactCreated {
                document_id: doc,
                artifact_id: Uuid::now_v7(),
                schema: "intake.ir.v1".into(),
                version: 2,
                sha256: "ab".repeat(32),
            },
        );
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], ARTIFACT_CREATED);
        let back: Envelope<ArtifactCreated> = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, ev.id);
        assert_eq!(back.data, ev.data);
        assert_eq!(ev.nats_subject(), format!("{ARTIFACT_CREATED}.{doc}"));
    }

    #[test]
    fn subjects_have_uniform_depth() {
        for kind in [
            DOCUMENT_CREATED,
            ARTIFACT_CREATED,
            EXTRACTION_REQUESTED,
            EXTRACTION_STARTED,
            EXTRACTION_COMPLETED,
            EXTRACTION_FAILED,
            RECOGNITION_REQUESTED,
            RECOGNITION_STARTED,
            RECOGNITION_COMPLETED,
            RECOGNITION_FAILED,
            CARD_INDEXED,
            RECEIPTING_REQUIRED,
            RECEIPTING_COMPLETED,
            CASE_OPENED,
            CASE_DECIDED,
            CASE_REOPENED,
            CASE_ASSIGNED,
            NOTICE_ISSUED,
            NOTICE_EXTENDED,
            NOTICE_RESCINDED,
            LETTER_GENERATED,
            LETTER_HELD,
            DELIVERY_SENT,
            DELIVERY_FAILED,
            FINDING_RAISED,
            FINDING_RESOLVED,
        ] {
            assert_eq!(kind.split('.').count(), 4, "{kind}");
        }
    }
}
