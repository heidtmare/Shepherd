//! Edition choice among templates of one form type. Needs vendored pdfium (no TrOCR):
//! `cargo test -p shepherd-forms --test editions -- --ignored`.
//!
//! Two editions are built from the I-765 blank: the original (7 pages) and a revision without
//! page 3 (6 pages). Page 1 is identical, as between real editions, so the choice rests on the
//! remaining pages and the page count.

use std::{collections::BTreeMap, path::PathBuf};

use shepherd_forms::{
    recognize::{MatchMethod, Recognizer, RecognizerConfig},
    render,
    synth::{SynthOptions, synthesize},
    template::{FormTemplate, TemplateSet},
};

const FORM_TYPE: &str = "test.i-765";
const FULL: &str = "test.i-765@2025-08-21";
const REVISED: &str = "test.i-765@2026-01-20";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Forms root in a temp dir holding both editions.
fn editions() -> TemplateSet {
    let dir = std::env::temp_dir().join(format!("shepherd-editions-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let base = TemplateSet::load(root().join("forms")).unwrap();
    let orig = base.get("uscis.i-765").unwrap().clone();
    let blank = std::fs::read(base.blank_path(&orig)).unwrap();

    let mut doc = lopdf::Document::load_mem(&blank).unwrap();
    doc.delete_pages(&[3]);
    let mut revised_pdf = Vec::new();
    doc.save_to(&mut revised_pdf).unwrap();
    std::fs::write(dir.join("full.pdf"), &blank).unwrap();
    std::fs::write(dir.join("revised.pdf"), &revised_pdf).unwrap();

    let full = FormTemplate {
        id: FULL.into(),
        form_type: Some(FORM_TYPE.into()),
        aliases: vec![FORM_TYPE.into()],
        blank_pdf: "full.pdf".into(),
        ..orig.clone()
    };
    let mut revised = FormTemplate {
        id: REVISED.into(),
        form_type: Some(FORM_TYPE.into()),
        aliases: vec![],
        edition: Some("01/20/26".into()),
        blank_pdf: "revised.pdf".into(),
        ..orig
    };
    revised.pages.retain(|p| p.number != 3);
    for (i, p) in revised.pages.iter_mut().enumerate() {
        p.number = i as u32 + 1;
    }
    revised.fields.retain(|f| f.page != 3);
    for f in &mut revised.fields {
        if f.page > 3 {
            f.page -= 1;
        }
    }
    TemplateSet::new(dir, vec![full, revised]).unwrap()
}

fn scan(set: &TemplateSet, id: &str) -> Vec<u8> {
    let t = set.get(id).unwrap();
    let blank = std::fs::read(set.blank_path(t)).unwrap();
    let font = std::fs::read(root().join("forms/fonts/Caveat.ttf")).unwrap();
    synthesize(
        t,
        &blank,
        &BTreeMap::new(),
        &font,
        &SynthOptions {
            shift: (-4, 6),
            noise: 0.02,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
#[ignore = "requires vendored pdfium"]
fn chooses_edition_by_layout() {
    render::init(root().join("vendor/pdfium/lib")).expect("pdfium (run scripts/fetch-deps.sh)");
    let set = editions();
    assert_eq!(set.resolve(FORM_TYPE).unwrap().id, REVISED);
    let r = Recognizer::new(set, None, RecognizerConfig::default()).unwrap();

    for (truth, other) in [(FULL, REVISED), (REVISED, FULL)] {
        let pdf = scan(&r.set, truth);
        for hint in [None, Some(FORM_TYPE)] {
            let res = r.recognize(&pdf, hint).unwrap();
            let m = res.form.expect("form matched");
            assert_eq!(m.template_id, truth, "hint {hint:?}");
            assert_eq!(m.form_type(), FORM_TYPE);
            assert_eq!(
                m.method,
                if hint.is_some() {
                    MatchMethod::Hint
                } else {
                    MatchMethod::Visual
                }
            );
            assert_eq!(m.alternatives.len(), 1);
            assert_eq!(m.alternatives[0].template_id, other);
            assert!(m.score > m.alternatives[0].score + 0.05, "{m:?}");
            assert_eq!(
                res.fields.len(),
                r.set.get(truth).unwrap().fields.len(),
                "fields come from the chosen edition"
            );
        }
        // An exact template id is honoured even when it is the wrong edition.
        let forced = r.recognize(&pdf, Some(other)).unwrap().form.unwrap();
        assert_eq!(
            (forced.template_id.as_str(), forced.alternatives.len()),
            (other, 0)
        );
    }
}
