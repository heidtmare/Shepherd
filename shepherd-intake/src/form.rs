//! `intake.form.v1`: field-level recognition of a known form.

use serde::{Deserialize, Serialize};
use shepherd_forms::recognize::{FieldResult, FieldStats, FormMatch, FormResult, RecognizerConfig};

pub const SCHEMA: &str = "intake.form.v1";
/// Recorded as `produced_by` on every form artifact.
pub const PRODUCER: &str = concat!("shepherd-intake-recognizer/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntakeForm {
    pub schema: String,
    pub recognizer: RecognizerInfo,
    pub source: crate::ir::Source,
    /// `None` when the document matched no template.
    pub form: Option<FormMatch>,
    pub page_count: u32,
    pub stats: FieldStats,
    pub fields: Vec<FieldResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecognizerInfo {
    pub name: String,
    pub version: String,
    pub htr_model: Option<String>,
    pub dpi: f32,
    pub min_token_prob: f32,
    pub min_mean_prob: f32,
}

pub fn build(
    source: crate::ir::Source,
    res: FormResult,
    cfg: &RecognizerConfig,
    htr_model: Option<String>,
) -> IntakeForm {
    IntakeForm {
        schema: SCHEMA.into(),
        recognizer: RecognizerInfo {
            name: "shepherd-forms".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            htr_model,
            dpi: cfg.dpi,
            min_token_prob: cfg.min_token_prob,
            min_mean_prob: cfg.min_mean_prob,
        },
        source,
        form: res.form,
        page_count: res.page_count,
        stats: res.stats,
        fields: res.fields,
    }
}
