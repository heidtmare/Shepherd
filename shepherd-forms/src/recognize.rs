//! Field-level recognition of a filled form against its template.
//!
//! Visual path (scans, handwriting): classify page 1 against template fingerprints, align each
//! page to the rendered blank, isolate new ink per field by subtraction, then read text with
//! TrOCR or measure checkbox fill. AcroForm path: PDFs typed into the fillable form carry exact
//! values in `/V`.
//!
//! A field `requires_receipting` when its value cannot be determined with confidence.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use crate::{
    acroform,
    htr::{Charset, Trocr},
    render::{self, RenderedPage},
    template::{FieldKind, FieldTemplate, FormTemplate, TemplateSet},
    value::{self, ValueType},
    vision::{self, PxRect},
};

#[derive(Debug, Clone)]
pub struct RecognizerConfig {
    pub dpi: f32,
    /// Any decoded token below this probability flags the field.
    pub min_token_prob: f32,
    /// Mean token probability below this flags the field.
    pub min_mean_prob: f32,
    /// Stricter per-token minimum for mixed letter/digit readings (passport, receipt numbers),
    /// which format validation cannot check.
    pub min_identifier_token_prob: f32,
    /// Minimum page-1 fingerprint similarity to accept a template.
    pub min_match: f32,
    pub max_shift_px: i32,
    /// Checkbox ink ratio at or above `checkbox_on` = checked; at or below `checkbox_off` = empty.
    pub checkbox_on: f32,
    pub checkbox_off: f32,
    /// Fewer ink pixels than this in a text region = empty.
    pub min_ink_px: u32,
    /// Budget for blanks rendered at `dpi` (8-bit gray pixels); least recently used evicted first.
    /// About 3.7 MB per letter page at 200 dpi.
    pub blank_cache_bytes: usize,
}

impl Default for RecognizerConfig {
    fn default() -> Self {
        Self {
            dpi: 200.0,
            min_token_prob: 0.6,
            min_mean_prob: 0.85,
            min_identifier_token_prob: 0.9,
            min_match: 0.75,
            max_shift_px: 60,
            checkbox_on: 0.12,
            checkbox_off: 0.04,
            min_ink_px: 40,
            blank_cache_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchMethod {
    Acroform,
    Visual,
    Hint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormMatch {
    /// Template (edition) the fields were read with.
    pub template_id: String,
    /// Form type shared by all editions. Absent in artifacts recorded before editions existed;
    /// use [`FormMatch::form_type`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_type: Option<String>,
    pub form_number: String,
    pub title: String,
    pub edition: Option<String>,
    /// sha256 of the template's blank PDF, pinning the exact layout used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blank_sha256: Option<String>,
    pub method: MatchMethod,
    /// Fingerprint similarity: page 1, or the page-count-weighted mean over pages when the
    /// edition was chosen among several.
    pub score: f32,
    /// Other editions of the form that were scored, best first. A small margin to the chosen
    /// edition means the edition is uncertain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<EditionScore>,
    /// Per scan page `[dx, dy]` in PDF points (scan = blank + shift).
    pub page_shifts: Vec<[f32; 2]>,
}

impl FormMatch {
    pub fn form_type(&self) -> &str {
        self.form_type.as_deref().unwrap_or(&self.template_id)
    }

    fn new(t: &FormTemplate, method: MatchMethod, score: f32) -> Self {
        Self {
            template_id: t.id.clone(),
            form_type: Some(t.form_type().to_owned()),
            form_number: t.form_number.clone(),
            title: t.title.clone(),
            edition: t.edition.clone(),
            blank_sha256: Some(t.blank_sha256.clone()),
            method,
            score,
            alternatives: vec![],
            page_shifts: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditionScore {
    pub template_id: String,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FieldStatus {
    Filled,
    Empty,
    RequiresReceipting,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    Acroform,
    Htr,
    Mark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reason {
    /// `low_confidence`, `invalid_format`, `illegible`, `ambiguous_mark`, `page_missing`, `htr_unavailable`.
    pub code: String,
    pub message: String,
}

impl Reason {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldResult {
    pub id: String,
    pub page: u32,
    /// Normalized value when valid, otherwise the best raw reading.
    pub value: Option<String>,
    /// Recognizer output before normalization.
    pub raw: Option<String>,
    /// Mean token probability (HTR), fill ratio certainty (marks), 1.0 (AcroForm).
    pub confidence: Option<f32>,
    pub min_token_prob: Option<f32>,
    pub status: FieldStatus,
    pub reasons: Vec<Reason>,
    pub source: Option<ValueSource>,
    /// Detected ink bounds `[x0, y0, x1, y1]` in template PDF points.
    pub ink_bbox: Option<[f32; 4]>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FieldStats {
    pub total: usize,
    pub filled: usize,
    pub empty: usize,
    pub requires_receipting: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FormResult {
    pub form: Option<FormMatch>,
    pub page_count: u32,
    pub fields: Vec<FieldResult>,
    pub stats: FieldStats,
}

pub struct Recognizer {
    pub set: TemplateSet,
    pub cfg: RecognizerConfig,
    htr: Option<Trocr>,
    /// Page-1 fingerprint per template.
    prints: Vec<(String, Vec<f32>)>,
    /// Every page's fingerprint, for templates whose form type has several editions.
    page_prints: HashMap<String, Vec<Vec<f32>>>,
    blanks: Mutex<BlankCache>,
}

/// Rendered blanks by template id, bounded by pixel bytes, least recently used evicted first.
#[derive(Default)]
struct BlankCache {
    bytes: usize,
    entries: HashMap<String, (usize, Arc<Vec<RenderedPage>>)>,
    /// Least recently used first.
    order: std::collections::VecDeque<String>,
}

impl BlankCache {
    fn get(&mut self, id: &str) -> Option<Arc<Vec<RenderedPage>>> {
        let v = self.entries.get(id)?.1.clone();
        if let Some(i) = self.order.iter().position(|k| k == id) {
            let k = self.order.remove(i).expect("position in range");
            self.order.push_back(k);
        }
        Some(v)
    }

    /// A blank larger than `max` is returned uncached.
    fn put(&mut self, id: &str, pages: Arc<Vec<RenderedPage>>, max: usize) {
        let size: usize = pages.iter().map(|p| p.image.as_raw().len()).sum();
        if size > max || self.entries.contains_key(id) {
            return;
        }
        while self.bytes + size > max {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some((n, _)) = self.entries.remove(&old) {
                self.bytes -= n;
            }
        }
        self.bytes += size;
        self.entries.insert(id.to_owned(), (size, pages));
        self.order.push_back(id.to_owned());
    }
}

fn stats(fields: &[FieldResult]) -> FieldStats {
    let mut s = FieldStats {
        total: fields.len(),
        ..Default::default()
    };
    for f in fields {
        match f.status {
            FieldStatus::Filled => s.filled += 1,
            FieldStatus::Empty => s.empty += 1,
            FieldStatus::RequiresReceipting => s.requires_receipting += 1,
        }
    }
    s
}

fn empty_field(t: &FieldTemplate) -> FieldResult {
    FieldResult {
        id: t.id.clone(),
        page: t.page,
        value: None,
        raw: None,
        confidence: None,
        min_token_prob: None,
        status: FieldStatus::Empty,
        reasons: vec![],
        source: None,
        ink_bbox: None,
    }
}

/// Reading mixes letters and digits (document or receipt numbers).
fn is_identifier(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| c.is_alphabetic())
}

/// Validates a candidate value; invalid values keep their raw reading and flag the field.
fn validated(t: &FieldTemplate, mut f: FieldResult, candidate: &str) -> FieldResult {
    let prepared = if value::is_numeric(t.value_type) {
        value::repair_digits(candidate)
    } else {
        candidate.to_owned()
    };
    match value::normalize(t.value_type, &prepared, t.max_len, &t.options) {
        Ok(v) if v.is_empty() => f.value = None,
        Ok(v) => f.value = Some(v),
        Err(e) => {
            f.value = Some(candidate.to_owned());
            f.reasons.push(Reason::new("invalid_format", e));
        }
    }
    if !f.reasons.is_empty() {
        f.status = FieldStatus::RequiresReceipting;
    } else if f.value.is_some() {
        f.status = FieldStatus::Filled;
    }
    f
}

/// Template whose fields cover the most filled AcroForm values (at least half). Ties keep
/// `candidates` order, so among editions sharing field names the newest wins.
fn acroform_match<'a>(
    filled: &std::collections::BTreeMap<String, String>,
    candidates: &[&'a FormTemplate],
) -> Option<&'a FormTemplate> {
    let mut best: Option<(&FormTemplate, usize)> = None;
    for t in candidates {
        let n = filled.keys().filter(|k| t.field(k).is_some()).count();
        if n > 0 && best.is_none_or(|(_, b)| n > b) {
            best = Some((t, n));
        }
    }
    best.filter(|(_, n)| *n * 2 >= filled.len()).map(|(t, _)| t)
}

impl Recognizer {
    /// `htr` may be `None`; inked text fields are then flagged `htr_unavailable`.
    pub fn new(
        set: TemplateSet,
        htr: Option<Trocr>,
        cfg: RecognizerConfig,
    ) -> anyhow::Result<Self> {
        let mut prints = Vec::new();
        let mut page_prints = HashMap::new();
        for t in &set.templates {
            let blank = std::fs::read(set.blank_path(t))?;
            let multi = set.editions(t.form_type()).len() > 1;
            let pages = render::render(&blank, 50.0, (!multi).then_some(1))?;
            let fps: Vec<Vec<f32>> = pages
                .iter()
                .map(|p| vision::fingerprint(&p.image))
                .collect();
            let first = fps
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{}: blank has no pages", t.id))?;
            prints.push((t.id.clone(), first));
            if multi {
                page_prints.insert(t.id.clone(), fps);
            }
        }
        Ok(Self {
            set,
            cfg,
            htr,
            prints,
            page_prints,
            blanks: Mutex::new(BlankCache::default()),
        })
    }

    pub fn has_htr(&self) -> bool {
        self.htr.is_some()
    }

    fn blank(&self, t: &FormTemplate) -> anyhow::Result<Arc<Vec<RenderedPage>>> {
        if let Some(b) = self.blanks.lock().expect("blank cache").get(&t.id) {
            return Ok(b);
        }
        let pages = Arc::new(render::render(
            &std::fs::read(self.set.blank_path(t))?,
            self.cfg.dpi,
            None,
        )?);
        self.blanks.lock().expect("blank cache").put(
            &t.id,
            pages.clone(),
            self.cfg.blank_cache_bytes,
        );
        Ok(pages)
    }

    /// Best template for page 1 and its similarity.
    pub fn classify(&self, first_page: &RenderedPage) -> Option<(&FormTemplate, f32)> {
        let fp = vision::fingerprint(&first_page.image);
        self.prints
            .iter()
            .map(|(id, p)| (id, vision::similarity(&fp, p)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .and_then(|(id, s)| self.set.get(id).map(|t| (t, s)))
    }

    /// Page-count-weighted mean fingerprint similarity of a scan to every page of `t`.
    fn edition_score(&self, scan: &[Vec<f32>], t: &FormTemplate) -> f32 {
        let Some(prints) = self.page_prints.get(&t.id) else {
            return 0.0;
        };
        let n = scan.len().min(prints.len());
        if n == 0 {
            return 0.0;
        }
        let mean = scan
            .iter()
            .zip(prints)
            .map(|(a, b)| vision::similarity(a, b))
            .sum::<f32>()
            / n as f32;
        mean * n as f32 / scan.len().max(prints.len()) as f32
    }

    /// Best of several editions by [`Self::edition_score`]; ties keep `editions` order (newest first).
    fn pick_edition<'a>(
        &self,
        pages: &[RenderedPage],
        editions: &[&'a FormTemplate],
    ) -> (&'a FormTemplate, f32, Vec<EditionScore>) {
        let scan: Vec<Vec<f32>> = pages
            .iter()
            .map(|p| vision::fingerprint(&p.image))
            .collect();
        let mut scored: Vec<(&FormTemplate, f32)> = editions
            .iter()
            .map(|t| (*t, self.edition_score(&scan, t)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let alternatives = scored[1..]
            .iter()
            .map(|(t, s)| EditionScore {
                template_id: t.id.clone(),
                score: *s,
            })
            .collect();
        (scored[0].0, scored[0].1, alternatives)
    }

    /// `hint` is a form type (its editions are candidates) or a template id or alias. Unknown
    /// hints are ignored.
    pub fn recognize(&self, pdf: &[u8], hint: Option<&str>) -> anyhow::Result<FormResult> {
        let hinted = hint
            .map(|h| self.set.candidates(h))
            .filter(|c| !c.is_empty());
        // Typed-in fillable PDF: exact values, no imaging needed.
        if let Ok(doc) = lopdf::Document::load_mem(pdf) {
            let filled = acroform::filled_values(&doc);
            let all: Vec<&FormTemplate>;
            let candidates = match &hinted {
                Some(c) => c.as_slice(),
                None => {
                    all = self.set.templates.iter().collect();
                    &all
                }
            };
            if !filled.is_empty()
                && let Some(t) = acroform_match(&filled, candidates)
            {
                return Ok(self.acroform_result(t, &filled, doc.get_pages().len() as u32));
            }
        }

        let pages = render::render(pdf, self.cfg.dpi, None)?;
        let page_count = pages.len() as u32;
        let Some(first) = pages.first() else {
            return Ok(FormResult {
                form: None,
                page_count,
                fields: vec![],
                stats: FieldStats::default(),
            });
        };
        let (template, score, method, alternatives) = match hinted.as_deref() {
            Some([t]) => {
                let fp = vision::fingerprint(&first.image);
                let s = self
                    .prints
                    .iter()
                    .find(|(id, _)| *id == t.id)
                    .map_or(0.0, |(_, p)| vision::similarity(&fp, p));
                (*t, s, MatchMethod::Hint, vec![])
            }
            Some(editions) => {
                let (t, s, alts) = self.pick_edition(&pages, editions);
                (t, s, MatchMethod::Hint, alts)
            }
            None => match self.classify(first) {
                Some((t, s)) if s >= self.cfg.min_match => {
                    let editions = self.set.editions(t.form_type());
                    if editions.len() > 1 {
                        let (t, s, alts) = self.pick_edition(&pages, &editions);
                        (t, s, MatchMethod::Visual, alts)
                    } else {
                        (t, s, MatchMethod::Visual, vec![])
                    }
                }
                _ => {
                    return Ok(FormResult {
                        form: None,
                        page_count,
                        fields: vec![],
                        stats: FieldStats::default(),
                    });
                }
            },
        };

        let blanks = self.blank(template)?;
        let mut shifts_px = Vec::new();
        for (i, scan) in pages.iter().enumerate() {
            let shift = match blanks.get(i) {
                Some(b) if b.image.dimensions() == scan.image.dimensions() => {
                    vision::align(&b.image, &scan.image, self.cfg.max_shift_px)
                }
                _ => (0, 0),
            };
            shifts_px.push(shift);
        }
        let scale = self.cfg.dpi / 72.0;
        let fields: Vec<FieldResult> = template
            .fields
            .iter()
            .map(|f| {
                let idx = f.page as usize - 1;
                match (blanks.get(idx), pages.get(idx)) {
                    (Some(b), Some(s)) => self.read_field(f, b, s, shifts_px[idx]),
                    _ => FieldResult {
                        status: FieldStatus::RequiresReceipting,
                        reasons: vec![Reason::new(
                            "page_missing",
                            format!("page {} not in scan", f.page),
                        )],
                        ..empty_field(f)
                    },
                }
            })
            .collect();
        Ok(FormResult {
            form: Some(FormMatch {
                alternatives,
                page_shifts: shifts_px
                    .iter()
                    .map(|&(x, y)| [x as f32 / scale, -(y as f32) / scale])
                    .collect(),
                ..FormMatch::new(template, method, score)
            }),
            page_count,
            stats: stats(&fields),
            fields,
        })
    }

    fn acroform_result(
        &self,
        t: &FormTemplate,
        filled: &std::collections::BTreeMap<String, String>,
        page_count: u32,
    ) -> FormResult {
        let fields: Vec<FieldResult> = t
            .fields
            .iter()
            .map(|f| match filled.get(&f.id) {
                None => empty_field(f),
                Some(v) => {
                    let base = FieldResult {
                        raw: Some(v.clone()),
                        confidence: Some(1.0),
                        source: Some(ValueSource::Acroform),
                        ..empty_field(f)
                    };
                    validated(f, base, v)
                }
            })
            .collect();
        FormResult {
            form: Some(FormMatch::new(t, MatchMethod::Acroform, 1.0)),
            page_count,
            stats: stats(&fields),
            fields,
        }
    }

    fn read_field(
        &self,
        f: &FieldTemplate,
        blank: &RenderedPage,
        scan: &RenderedPage,
        shift: (i32, i32),
    ) -> FieldResult {
        let (w, h) = blank.image.dimensions();
        let scale = blank.scale();
        let rect = PxRect::from_pdf(f.rect, blank.height_pt, scale, w, h);
        let to_pdf = |r: PxRect| {
            [
                r.x0 as f32 / scale,
                blank.height_pt - r.y1 as f32 / scale,
                r.x1 as f32 / scale,
                blank.height_pt - r.y0 as f32 / scale,
            ]
        };
        let base = empty_field(f);
        match f.kind {
            FieldKind::Checkbox => {
                let region = rect.shrink(1);
                let ink = vision::ink(&blank.image, &scan.image, region, shift, 0);
                let (on, off) = (self.cfg.checkbox_on, self.cfg.checkbox_off);
                let bbox = ink.bbox.map(to_pdf);
                if ink.ratio >= on {
                    FieldResult {
                        value: Some("true".into()),
                        raw: Some(format!("{:.3}", ink.ratio)),
                        confidence: Some(((ink.ratio - on) / on + 0.5).min(1.0)),
                        status: FieldStatus::Filled,
                        source: Some(ValueSource::Mark),
                        ink_bbox: bbox,
                        ..base
                    }
                } else if ink.ratio <= off {
                    base
                } else {
                    FieldResult {
                        raw: Some(format!("{:.3}", ink.ratio)),
                        confidence: Some(0.5),
                        status: FieldStatus::RequiresReceipting,
                        reasons: vec![Reason::new(
                            "ambiguous_mark",
                            format!("{:.0}% of box marked", ink.ratio * 100.0),
                        )],
                        source: Some(ValueSource::Mark),
                        ink_bbox: bbox,
                        ..base
                    }
                }
            }
            FieldKind::Text | FieldKind::Choice | FieldKind::Signature => {
                // Handwriting routinely rises above the box and overhangs its edges.
                let region = rect.grow(0.45, 0.2, 0.02, w, h);
                let ink = vision::ink(&blank.image, &scan.image, region, shift, 10);
                if ink.pixels < self.cfg.min_ink_px {
                    return base;
                }
                let bbox = ink.bbox.map(to_pdf);
                if f.kind == FieldKind::Signature {
                    return FieldResult {
                        value: Some("signed".into()),
                        confidence: Some(1.0),
                        status: FieldStatus::Filled,
                        source: Some(ValueSource::Mark),
                        ink_bbox: bbox,
                        ..base
                    };
                }
                let Some(htr) = &self.htr else {
                    return FieldResult {
                        status: FieldStatus::RequiresReceipting,
                        reasons: vec![Reason::new(
                            "htr_unavailable",
                            "handwriting present; recognizer not loaded",
                        )],
                        ink_bbox: bbox,
                        ..base
                    };
                };
                let crop = ink.crop.expect("crop present when ink found");
                if let Ok(dir) = std::env::var("FORMS_DEBUG_DIR") {
                    let _ = crop.save(format!(
                        "{dir}/{}.png",
                        f.id.replace(['[', ']', '#', '.'], "_")
                    ));
                }
                let free_text = matches!(
                    f.value_type,
                    ValueType::Text | ValueType::Name | ValueType::Choice
                );
                let charset = match f.value_type {
                    ValueType::Date => Some(Charset::Date),
                    ValueType::Email => Some(Charset::Email),
                    t if value::is_numeric(t) => Some(Charset::Digits),
                    _ => None,
                };
                let rec = match htr.recognize(&crop, free_text, charset) {
                    Ok(r) => r,
                    Err(e) => {
                        return FieldResult {
                            status: FieldStatus::RequiresReceipting,
                            reasons: vec![Reason::new(
                                "illegible",
                                format!("recognizer error: {e}"),
                            )],
                            ink_bbox: bbox,
                            ..base
                        };
                    }
                };
                if std::env::var("FORMS_DEBUG_DIR").is_ok() {
                    eprintln!("{}: {:?}", f.id, rec.tokens);
                }
                let mut out = FieldResult {
                    raw: Some(rec.full_text.clone()),
                    confidence: Some(rec.mean_prob),
                    min_token_prob: Some(rec.min_prob),
                    source: Some(ValueSource::Htr),
                    ink_bbox: bbox,
                    ..base
                };
                if rec.text.is_empty() || rec.degenerate {
                    out.reasons
                        .push(Reason::new("illegible", "no stable reading"));
                } else if rec.min_prob < self.cfg.min_token_prob
                    || rec.mean_prob < self.cfg.min_mean_prob
                    || (is_identifier(&rec.text)
                        && rec.min_prob < self.cfg.min_identifier_token_prob)
                {
                    out.reasons.push(Reason::new(
                        "low_confidence",
                        format!(
                            "confidence {:.0}% (lowest character {:.0}%)",
                            rec.mean_prob * 100.0,
                            rec.min_prob * 100.0
                        ),
                    ));
                }
                validated(f, out, &rec.text)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank(bytes: u32) -> Arc<Vec<RenderedPage>> {
        Arc::new(vec![RenderedPage {
            width_pt: 1.0,
            height_pt: 1.0,
            image: image::GrayImage::new(bytes, 1),
        }])
    }

    #[test]
    fn blank_cache_lru_by_bytes() {
        let mut c = BlankCache::default();
        c.put("a", blank(40), 100);
        c.put("b", blank(40), 100);
        assert!(c.get("a").is_some()); // b is now least recently used
        c.put("c", blank(40), 100);
        assert!(c.get("b").is_none());
        assert!(c.get("a").is_some() && c.get("c").is_some());
        assert_eq!(c.bytes, 80);
        c.put("huge", blank(101), 100);
        assert!(c.get("huge").is_none());
        assert_eq!(c.bytes, 80);
    }
    use crate::template::FieldTemplate;

    fn template(id: &str, fields: &[&str]) -> FormTemplate {
        FormTemplate {
            id: id.into(),
            form_type: Some("f".into()),
            aliases: vec![],
            form_number: "F-1".into(),
            title: "f".into(),
            edition: None,
            blank_pdf: String::new(),
            blank_sha256: String::new(),
            pages: vec![],
            fields: fields
                .iter()
                .map(|f| FieldTemplate {
                    id: (*f).into(),
                    label: String::new(),
                    short_label: String::new(),
                    group: None,
                    section: None,
                    page: 1,
                    rect: [0.0; 4],
                    kind: FieldKind::Text,
                    value_type: ValueType::Text,
                    max_len: None,
                    options: vec![],
                    required: false,
                })
                .collect(),
        }
    }

    #[test]
    fn acroform_match_prefers_coverage_then_order() {
        let new = template("f@2", &["a", "b", "c"]);
        let old = template("f@1", &["a", "b"]);
        let filled = |keys: &[&str]| {
            keys.iter()
                .map(|k| (k.to_string(), "v".to_string()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        // Shared fields only: tie, first candidate (newest) wins.
        assert_eq!(
            acroform_match(&filled(&["a", "b"]), &[&new, &old])
                .unwrap()
                .id,
            "f@2"
        );
        // A field only the old edition has decides for it.
        let old2 = template("f@1", &["a", "b", "z"]);
        assert_eq!(
            acroform_match(&filled(&["a", "z"]), &[&new, &old2])
                .unwrap()
                .id,
            "f@1"
        );
        // Less than half the values covered: no match.
        assert!(acroform_match(&filled(&["a", "x", "y"]), &[&new]).is_none());
    }
}
