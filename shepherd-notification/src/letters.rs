//! Letter text (`config/letters.toml`) and composition of a letter from case data.
//!
//! Composition is pure: [`compose`] returns the letter, or a [`Hold`] naming what is missing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Receipt,
    Rfe,
    Noid,
    Approval,
    Denial,
    Withdrawal,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Receipt,
        Kind::Rfe,
        Kind::Noid,
        Kind::Approval,
        Kind::Denial,
        Kind::Withdrawal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Receipt => "receipt",
            Kind::Rfe => "rfe",
            Kind::Noid => "noid",
            Kind::Approval => "approval",
            Kind::Denial => "denial",
            Kind::Withdrawal => "withdrawal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Letter kind for a decision outcome (`approved`, `denied`, `withdrawn`).
    pub fn from_outcome(outcome: &str) -> Option<Self> {
        match outcome {
            "approved" => Some(Kind::Approval),
            "denied" => Some(Kind::Denial),
            "withdrawn" => Some(Kind::Withdrawal),
            _ => None,
        }
    }
}

const PLACEHOLDERS: &[&str] = &[
    "receipt_number",
    "form_number",
    "benefit",
    "received_date",
    "notice_date",
    "due_date",
    "name",
    "a_number",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Letterhead {
    pub lines: Vec<String>,
    #[serde(default)]
    pub signature: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindText {
    pub title: String,
    #[serde(default)]
    pub body: Vec<String>,
    #[serde(default)]
    pub closing: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailText {
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LetterConfig {
    pub letterhead: Letterhead,
    pub kinds: BTreeMap<String, KindText>,
    #[serde(default)]
    pub email: BTreeMap<String, EmailText>,
}

impl LetterConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        Self::parse(&std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?)
            .map_err(|e| anyhow::anyhow!("{path}: {e}"))
    }

    pub fn parse(src: &str) -> anyhow::Result<Self> {
        let c: LetterConfig = toml::from_str(src)?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.letterhead.lines.is_empty(),
            "letterhead.lines is empty"
        );
        for k in Kind::ALL {
            anyhow::ensure!(
                self.kinds.contains_key(k.as_str()),
                "kinds.{} missing",
                k.as_str()
            );
        }
        for key in self.kinds.keys().chain(self.email.keys()) {
            anyhow::ensure!(Kind::parse(key).is_some(), "unknown letter kind {key:?}");
        }
        let texts = self
            .kinds
            .values()
            .flat_map(|k| std::iter::once(&k.title).chain(&k.body).chain(&k.closing))
            .chain(self.email.values().flat_map(|e| [&e.subject, &e.body]));
        for t in texts {
            for p in placeholders(t) {
                anyhow::ensure!(
                    PLACEHOLDERS.contains(&p),
                    "unknown placeholder {{{p}}} in {t:?}"
                );
            }
        }
        Ok(())
    }

    fn kind(&self, k: Kind) -> &KindText {
        &self.kinds[k.as_str()]
    }
}

fn placeholders(t: &str) -> impl Iterator<Item = &str> {
    t.split('{')
        .skip(1)
        .filter_map(|s| s.split_once('}').map(|(p, _)| p))
}

fn fill(t: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = t.to_owned();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// `September 24, 2026`.
pub fn long_date(d: Date) -> String {
    format!("{} {}, {}", d.month(), d.day(), d.year())
}

/// Case data from shepherd-workbench at generation time.
#[derive(Debug, Clone)]
pub struct CaseInfo {
    pub case_id: Uuid,
    pub receipt_number: String,
    pub form_type: String,
    pub form_number: String,
    pub benefit: String,
    pub received_date: Date,
    pub facts: BTreeMap<String, String>,
    /// Primary form values are receipted, or recognized with nothing flagged.
    pub form_final: bool,
    /// E-notification (G-1145) address, when one is on the case.
    pub email: Option<String>,
}

/// `workbench.notice.v1`, the fields used here.
#[derive(Debug, Clone, Deserialize)]
pub struct NoticeBody {
    pub notice_id: Uuid,
    pub kind: String,
    pub items: Vec<String>,
    pub message: Option<String>,
    pub due_date: String,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroundRef {
    pub id: String,
    pub label: String,
}

/// `workbench.decision.v1`, the fields used here.
#[derive(Debug, Clone, Deserialize)]
pub struct DecisionBody {
    pub outcome: String,
    #[serde(default)]
    pub grounds: Vec<GroundRef>,
    #[serde(default)]
    pub rationale: String,
    #[serde(with = "time::serde::rfc3339")]
    pub decided_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub enum Source {
    Receipt,
    Notice(NoticeBody),
    Decision(DecisionBody),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Recipient {
    pub name: String,
    /// Address lines after the name.
    pub lines: Vec<String>,
}

impl Recipient {
    pub fn postal(&self) -> String {
        std::iter::once(self.name.as_str())
            .chain(self.lines.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "text", rename_all = "snake_case")]
pub enum Block {
    Heading(String),
    Paragraph(String),
    Numbered(Vec<String>),
    Bullets(Vec<String>),
}

#[derive(Debug, Clone, Serialize)]
pub struct Letter {
    pub kind: Kind,
    pub title: String,
    pub receipt_number: String,
    /// `YYYY-MM-DD`.
    pub letter_date: String,
    pub letterhead: Vec<String>,
    /// Label → value, printed beside the letterhead.
    pub details: Vec<(String, String)>,
    pub recipient: Recipient,
    pub blocks: Vec<Block>,
    pub signature: Vec<String>,
}

/// Why a letter cannot be generated yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold(pub String);

fn fact<'a>(facts: &'a BTreeMap<String, String>, k: &str) -> Option<&'a str> {
    facts.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn person(facts: &BTreeMap<String, String>, prefix: &str) -> Option<String> {
    let parts: Vec<&str> = ["given_name", "family_name"]
        .iter()
        .filter_map(|k| fact(facts, &format!("{prefix}{k}")))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn is_us(country: Option<&str>) -> bool {
    country.is_none_or(|c| {
        matches!(
            c.to_ascii_uppercase().replace('.', "").as_str(),
            "US" | "USA" | "UNITED STATES" | "UNITED STATES OF AMERICA"
        )
    })
}

/// Mailing address from the `mailing_*` case facts.
pub fn recipient(facts: &BTreeMap<String, String>) -> Result<Recipient, Hold> {
    let f = |k: &str| fact(facts, k);
    let country = f("mailing_country");
    let us = is_us(country);
    let mut missing = Vec::new();
    let name = person(facts, "");
    if name.is_none() {
        missing.push("applicant name");
    }
    let street = f("mailing_street");
    let city = f("mailing_city");
    if street.is_none() {
        missing.push("street");
    }
    if city.is_none() {
        missing.push("city");
    }
    if us {
        if f("mailing_state").is_none() {
            missing.push("state");
        }
        if f("mailing_zip").is_none() {
            missing.push("ZIP code");
        }
    }
    if !missing.is_empty() {
        return Err(Hold(format!(
            "mailing address incomplete: missing {}",
            missing.join(", ")
        )));
    }
    let mut lines = Vec::new();
    if let Some(c) = f("mailing_in_care_of") {
        lines.push(format!("C/O {c}"));
    }
    let street = street.unwrap_or_default();
    lines.push(match f("mailing_unit") {
        Some(u) => format!("{street} # {u}"),
        None => street.to_owned(),
    });
    let city = city.unwrap_or_default();
    if us {
        lines.push(format!(
            "{city} {} {}",
            f("mailing_state").unwrap_or_default(),
            f("mailing_zip").unwrap_or_default()
        ));
    } else {
        let tail: Vec<&str> = [f("mailing_province"), f("mailing_postal_code")]
            .into_iter()
            .flatten()
            .collect();
        lines.push(if tail.is_empty() {
            city.to_owned()
        } else {
            format!("{city} {}", tail.join(" "))
        });
        lines.push(country.unwrap_or_default().to_uppercase());
    }
    Ok(Recipient {
        name: name.unwrap_or_default(),
        lines,
    })
}

fn vars(case: &CaseInfo, letter_date: Date, due: Option<&str>) -> BTreeMap<&'static str, String> {
    let mut v = BTreeMap::from([
        ("receipt_number", case.receipt_number.clone()),
        ("form_number", format!("Form {}", case.form_number)),
        ("benefit", case.benefit.clone()),
        ("received_date", long_date(case.received_date)),
        ("notice_date", long_date(letter_date)),
        ("name", person(&case.facts, "").unwrap_or_default()),
        (
            "a_number",
            fact(&case.facts, "a_number")
                .map(|a| format!("A{a}"))
                .unwrap_or_default(),
        ),
    ]);
    if let Some(d) = due {
        let long = parse_date(d).map(long_date).unwrap_or_else(|| d.to_owned());
        v.insert("due_date", long);
    }
    v
}

pub fn parse_date(s: &str) -> Option<Date> {
    Date::parse(s, time::macros::format_description!("[year]-[month]-[day]")).ok()
}

fn iso(d: Date) -> String {
    d.format(time::macros::format_description!("[year]-[month]-[day]"))
        .expect("date formats")
}

/// Kind of letter for a source, checked against the notice or decision it carries.
pub fn kind_of(source: &Source) -> Option<Kind> {
    match source {
        Source::Receipt => Some(Kind::Receipt),
        Source::Notice(n) => match n.kind.as_str() {
            "rfe" => Some(Kind::Rfe),
            "noid" => Some(Kind::Noid),
            _ => None,
        },
        Source::Decision(d) => Kind::from_outcome(&d.outcome),
    }
}

/// Letter date and, for notices, the response due date.
fn dates(source: &Source, today: Date) -> (Date, Option<&str>) {
    match source {
        Source::Receipt => (today, None),
        Source::Notice(n) => (n.issued_at.date(), Some(n.due_date.as_str())),
        Source::Decision(d) => (d.decided_at.date(), None),
    }
}

pub fn compose(
    cfg: &LetterConfig,
    case: &CaseInfo,
    source: &Source,
    today: Date,
) -> Result<Letter, Hold> {
    let kind = kind_of(source).ok_or_else(|| Hold("unsupported notice or outcome".into()))?;
    if kind == Kind::Receipt && !case.form_final {
        return Err(Hold(
            "primary form values not final (receipting pending)".into(),
        ));
    }
    let recipient = recipient(&case.facts)?;
    let (letter_date, due) = dates(source, today);
    let v = vars(case, letter_date, due);
    let text = cfg.kind(kind);
    let mut blocks: Vec<Block> = text
        .body
        .iter()
        .map(|p| Block::Paragraph(fill(p, &v)))
        .collect();
    match source {
        Source::Receipt => {}
        Source::Notice(n) => {
            blocks.push(Block::Heading(match kind {
                Kind::Noid => "Reasons and evidence that may overcome them".into(),
                _ => "Evidence requested".into(),
            }));
            blocks.push(Block::Numbered(n.items.clone()));
            if let Some(m) = n
                .message
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
            {
                blocks.push(Block::Heading("Additional information".into()));
                blocks.push(Block::Paragraph(m.to_owned()));
            }
        }
        Source::Decision(d) => {
            if kind == Kind::Denial {
                if !d.grounds.is_empty() {
                    blocks.push(Block::Heading("Grounds for denial".into()));
                    blocks.push(Block::Bullets(
                        d.grounds.iter().map(|g| g.label.clone()).collect(),
                    ));
                }
                if !d.rationale.trim().is_empty() {
                    blocks.push(Block::Heading("Reasons".into()));
                    blocks.push(Block::Paragraph(d.rationale.trim().to_owned()));
                }
            }
        }
    }
    blocks.extend(text.closing.iter().map(|p| Block::Paragraph(fill(p, &v))));

    let mut details = vec![
        ("Receipt number".to_owned(), case.receipt_number.clone()),
        ("Notice date".to_owned(), long_date(letter_date)),
        (
            "Case type".to_owned(),
            format!("{} - {}", case.form_number, case.benefit),
        ),
        ("Received date".to_owned(), long_date(case.received_date)),
        (
            match case.form_type.as_str() {
                "uscis.i-130" => "Petitioner",
                _ => "Applicant",
            }
            .to_owned(),
            recipient.name.clone(),
        ),
    ];
    if let Some(a) = fact(&case.facts, "a_number") {
        details.push(("A-Number".into(), format!("A{a}")));
    }
    if let Some(b) = person(&case.facts, "beneficiary_") {
        details.push(("Beneficiary".into(), b));
    }
    if let Some(d) = due {
        details.push((
            "Response due".into(),
            v.get("due_date").cloned().unwrap_or_else(|| d.to_owned()),
        ));
    }
    Ok(Letter {
        kind,
        title: fill(&text.title, &v),
        receipt_number: case.receipt_number.clone(),
        letter_date: iso(letter_date),
        letterhead: cfg.letterhead.lines.clone(),
        details,
        recipient,
        blocks,
        signature: cfg.letterhead.signature.clone(),
    })
}

/// E-mail subject and body for the letter `source` produces, when the configuration defines one.
/// Placeholders take the same values as in the letter.
pub fn email(
    cfg: &LetterConfig,
    case: &CaseInfo,
    source: &Source,
    today: Date,
) -> Option<(String, String)> {
    let t = cfg.email.get(kind_of(source)?.as_str())?;
    let (letter_date, due) = dates(source, today);
    let v = vars(case, letter_date, due);
    Some((fill(&t.subject, &v), fill(t.body.trim(), &v)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    fn cfg() -> LetterConfig {
        LetterConfig::parse(include_str!("../config/letters.toml")).unwrap()
    }

    fn case() -> CaseInfo {
        CaseInfo {
            case_id: Uuid::nil(),
            receipt_number: "SHP2600000001".into(),
            form_type: "uscis.i-765".into(),
            form_number: "I-765".into(),
            benefit: "Employment Authorization".into(),
            received_date: date!(2026 - 09 - 01),
            facts: BTreeMap::from(
                [
                    ("family_name", "Nguyen"),
                    ("given_name", "Linh"),
                    ("a_number", "123456789"),
                    ("mailing_street", "1420 Maple Avenue"),
                    ("mailing_unit", "4B"),
                    ("mailing_city", "Portland"),
                    ("mailing_state", "OR"),
                    ("mailing_zip", "97205"),
                ]
                .map(|(k, v)| (k.to_owned(), v.to_owned())),
            ),
            form_final: true,
            email: None,
        }
    }

    #[test]
    fn shipped_config_is_valid() {
        let c = cfg();
        assert_eq!(c.kinds.len(), 6);
        assert!(c.email.contains_key("receipt"));
    }

    #[test]
    fn rejects_unknown_placeholder() {
        let src = include_str!("../config/letters.toml").replace("{benefit}", "{bogus}");
        assert!(LetterConfig::parse(&src).is_err());
    }

    #[test]
    fn us_address() {
        let r = recipient(&case().facts).unwrap();
        assert_eq!(r.name, "Linh Nguyen");
        assert_eq!(r.lines, ["1420 Maple Avenue # 4B", "Portland OR 97205"]);
    }

    #[test]
    fn foreign_address() {
        let mut f = case().facts;
        f.remove("mailing_state");
        f.remove("mailing_zip");
        f.remove("mailing_unit");
        f.insert("mailing_in_care_of".into(), "Ana Silva".into());
        f.insert("mailing_country".into(), "Mexico".into());
        f.insert("mailing_province".into(), "Jalisco".into());
        f.insert("mailing_postal_code".into(), "44100".into());
        let r = recipient(&f).unwrap();
        assert_eq!(
            r.lines,
            [
                "C/O Ana Silva",
                "1420 Maple Avenue",
                "Portland Jalisco 44100",
                "MEXICO"
            ]
        );
    }

    #[test]
    fn incomplete_address_holds() {
        let mut f = case().facts;
        f.remove("mailing_zip");
        f.remove("mailing_city");
        let Hold(reason) = recipient(&f).unwrap_err();
        assert_eq!(reason, "mailing address incomplete: missing city, ZIP code");
    }

    #[test]
    fn receipt_waits_for_final_values() {
        let mut c = case();
        c.form_final = false;
        assert!(compose(&cfg(), &c, &Source::Receipt, date!(2026 - 09 - 24)).is_err());
        c.form_final = true;
        let l = compose(&cfg(), &c, &Source::Receipt, date!(2026 - 09 - 24)).unwrap();
        assert_eq!(l.kind, Kind::Receipt);
        assert_eq!(l.letter_date, "2026-09-24");
        let Block::Paragraph(p) = &l.blocks[0] else {
            panic!()
        };
        assert!(p.contains("Form I-765, Employment Authorization, on September 1, 2026"));
        assert!(
            l.details
                .contains(&("A-Number".into(), "A123456789".into()))
        );
    }

    #[test]
    fn rfe_lists_items_and_due_date() {
        let n = NoticeBody {
            notice_id: Uuid::nil(),
            kind: "rfe".into(),
            items: vec!["Copy of passport".into(), "Form I-94".into()],
            message: Some("Translations must be certified.".into()),
            due_date: "2026-12-20".into(),
            issued_at: datetime!(2026-09-24 15:00 UTC),
        };
        let l = compose(&cfg(), &case(), &Source::Notice(n), date!(2026 - 10 - 01)).unwrap();
        assert_eq!(l.kind, Kind::Rfe);
        assert_eq!(l.letter_date, "2026-09-24");
        assert!(l.blocks.contains(&Block::Numbered(vec![
            "Copy of passport".into(),
            "Form I-94".into()
        ])));
        assert!(
            l.blocks
                .contains(&Block::Paragraph("Translations must be certified.".into()))
        );
        assert!(l.blocks.iter().any(
            |b| matches!(b, Block::Paragraph(p) if p.contains("received by December 20, 2026"))
        ));
    }

    #[test]
    fn denial_states_grounds_and_rationale() {
        let d = DecisionBody {
            outcome: "denied".into(),
            grounds: vec![GroundRef {
                id: "ineligible".into(),
                label: "Eligibility requirements not met".into(),
            }],
            rationale: "The category requires a pending asylum application.".into(),
            decided_at: datetime!(2026-09-24 15:00 UTC),
        };
        let l = compose(&cfg(), &case(), &Source::Decision(d), date!(2026 - 09 - 24)).unwrap();
        assert_eq!(l.kind, Kind::Denial);
        assert!(l.blocks.contains(&Block::Bullets(vec![
            "Eligibility requirements not met".into()
        ])));
    }

    fn rfe() -> Source {
        Source::Notice(NoticeBody {
            notice_id: Uuid::nil(),
            kind: "rfe".into(),
            items: vec!["Copy of passport".into()],
            message: None,
            due_date: "2026-12-20".into(),
            issued_at: datetime!(2026-09-24 15:00 UTC),
        })
    }

    #[test]
    fn receipt_email() {
        let today = date!(2026 - 09 - 24);
        let (subject, body) = email(&cfg(), &case(), &Source::Receipt, today).unwrap();
        assert_eq!(subject, "Case accepted: SHP2600000001");
        assert!(body.contains("Received: September 1, 2026"));
        assert!(!body.contains("Nguyen"));
        assert!(email(&cfg(), &case(), &rfe(), today).is_none());
    }

    #[test]
    fn notice_email_uses_letter_dates() {
        let src = format!(
            "{}\n[email.rfe]\nsubject = \"Evidence requested: {{receipt_number}}\"\nbody = \"Dated {{notice_date}}, due {{due_date}}.\"\n",
            include_str!("../config/letters.toml")
        );
        let (_, body) = email(
            &LetterConfig::parse(&src).unwrap(),
            &case(),
            &rfe(),
            date!(2026 - 10 - 01),
        )
        .unwrap();
        assert_eq!(body, "Dated September 24, 2026, due December 20, 2026.");
    }
}
