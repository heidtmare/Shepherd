//! End-to-end recognition on synthetic scans. Needs vendored pdfium and TrOCR
//! (`scripts/fetch-deps.sh`): `cargo test -p shepherd-forms --release -- --ignored`.

use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock};

use lopdf::{Document, Object};
use shepherd_forms::{
    htr::Trocr,
    recognize::{FieldStatus, MatchMethod, Recognizer, RecognizerConfig},
    render,
    synth::{SynthOptions, synthesize},
    template::TemplateSet,
    value::ValueType,
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn recognizer() -> &'static Recognizer {
    static R: OnceLock<Recognizer> = OnceLock::new();
    R.get_or_init(|| {
        render::init(root().join("vendor/pdfium/lib")).expect("pdfium (run scripts/fetch-deps.sh)");
        let htr =
            Trocr::load(root().join("vendor/models/trocr-base-handwritten")).expect("TrOCR model");
        let set = TemplateSet::load(root().join("forms")).unwrap();
        Recognizer::new(set, Some(htr), RecognizerConfig::default()).unwrap()
    })
}

fn values(name: &str) -> BTreeMap<String, String> {
    serde_json::from_slice(
        &std::fs::read(root().join(format!("forms/samples/{name}.values.json"))).unwrap(),
    )
    .unwrap()
}

/// Recognizes a synthetic scan and checks the receipting contract: no false accepts, no missed
/// fields, nothing invented. Returns (filled, flagged).
fn check(
    template: &str,
    truth: &BTreeMap<String, String>,
    font: &str,
    opts: SynthOptions,
) -> (usize, usize) {
    let r = recognizer();
    let t = r.set.get(template).unwrap();
    let blank = std::fs::read(r.set.blank_path(t)).unwrap();
    let font_bytes = std::fs::read(root().join("forms/fonts").join(font)).unwrap();
    let pdf = synthesize(t, &blank, truth, &font_bytes, &opts).unwrap();
    let res = r.recognize(&pdf, None).unwrap();
    let m = res.form.as_ref().expect("template matched");
    assert_eq!(m.template_id, template);
    assert_eq!(m.method, MatchMethod::Visual);

    let (mut filled, mut flagged) = (0, 0);
    for f in &res.fields {
        let tf = t.field(&f.id).unwrap();
        let expected = truth.get(&f.id).map(|v| {
            shepherd_forms::value::normalize(tf.value_type, v, tf.max_len, &tf.options).unwrap()
        });
        match (&expected, f.status) {
            (Some(e), FieldStatus::Filled) => {
                // Letter case carries no meaning in names and free text.
                let case_free = matches!(tf.value_type, ValueType::Name | ValueType::Text);
                let got = f.value.clone().unwrap_or_default();
                let same = if case_free {
                    got.eq_ignore_ascii_case(e)
                } else {
                    &got == e
                };
                assert!(
                    same,
                    "{font}: false accept on {}: {got:?} != {e:?} (raw {:?})",
                    f.id, f.raw
                );
                filled += 1;
            }
            (Some(e), FieldStatus::RequiresReceipting) => {
                eprintln!(
                    "  flagged {} want {e:?} got {:?} conf {:?}/{:?} {:?}",
                    f.id.rsplit('.').next().unwrap_or(&f.id),
                    f.raw,
                    f.confidence,
                    f.min_token_prob,
                    f.reasons
                        .iter()
                        .map(|r| r.code.as_str())
                        .collect::<Vec<_>>()
                );
                flagged += 1
            }
            (Some(_), FieldStatus::Empty) => {
                panic!("{font}: written field {} reported empty", f.id)
            }
            (None, FieldStatus::Empty) => {}
            (None, s) => panic!("{font}: blank field {} reported {s:?} ({:?})", f.id, f.raw),
        }
    }
    (filled, flagged)
}

#[test]
#[ignore = "requires vendored pdfium and TrOCR"]
fn g1145_across_handwriting_styles() {
    let truth = values("g-1145");
    for (font, shift) in [
        ("Caveat.ttf", (9, 6)),
        ("HomemadeApple.ttf", (-4, 3)),
        ("NothingYouCouldDo.ttf", (0, -7)),
    ] {
        let (filled, flagged) = check(
            "uscis.g-1145",
            &truth,
            font,
            SynthOptions {
                shift,
                seed: 11,
                noise: 0.002,
                ..Default::default()
            },
        );
        eprintln!(
            "g-1145 {font}: {filled} filled, {flagged} flagged of {}",
            truth.len()
        );
    }
}

#[test]
#[ignore = "requires vendored pdfium and TrOCR"]
fn i765_multi_page_with_checkboxes() {
    let truth = values("i-765");
    for (font, shift) in [("HomemadeApple.ttf", (-5, 8)), ("Caveat.ttf", (6, -3))] {
        let (filled, flagged) = check(
            "uscis.i-765",
            &truth,
            font,
            SynthOptions {
                shift,
                seed: 3,
                ..Default::default()
            },
        );
        eprintln!(
            "i-765 {font}: {filled} filled, {flagged} flagged of {}",
            truth.len()
        );
        assert!(
            filled * 2 >= truth.len(),
            "{font}: fewer than half the fields recognized"
        );
    }
}

/// Sets `/V` on the blank's fields, as a PDF typed into in a viewer would have.
fn fill_acroform(blank: &[u8], values: &BTreeMap<String, String>) -> Vec<u8> {
    let mut doc = Document::load_mem(blank).unwrap();
    let raw = shepherd_forms::acroform::read_fields(&doc);
    let ids = shepherd_forms::acroform::widget_ids(&raw);
    let targets: Vec<(String, String)> = raw
        .iter()
        .zip(&ids)
        .filter_map(|(f, id)| values.get(id).map(|v| (f.name.clone(), v.clone())))
        .collect();
    let objects: Vec<_> = doc.objects.keys().copied().collect();
    for oid in objects {
        let Ok(d) = doc.get_dictionary(oid) else {
            continue;
        };
        let Some(t) = d
            .get(b"T")
            .ok()
            .and_then(|o| lopdf::decode_text_string(o).ok())
        else {
            continue;
        };
        if let Some((_, v)) = targets
            .iter()
            .find(|(name, _)| name.ends_with(&t) && name.rsplit('.').next() == Some(t.as_str()))
        {
            let d = doc.get_dictionary_mut(oid).unwrap();
            d.set("V", Object::string_literal(v.as_str()));
        }
    }
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[test]
#[ignore = "requires vendored pdfium and TrOCR"]
fn typed_acroform_is_exact() {
    let r = recognizer();
    let t = r.set.get("uscis.g-1145").unwrap();
    let truth = values("g-1145");
    let pdf = fill_acroform(&std::fs::read(r.set.blank_path(t)).unwrap(), &truth);
    let res = r.recognize(&pdf, None).unwrap();
    assert_eq!(res.form.unwrap().method, MatchMethod::Acroform);
    for f in &res.fields {
        let tf = t.field(&f.id).unwrap();
        let want = shepherd_forms::value::normalize(tf.value_type, &truth[&f.id], tf.max_len, &[])
            .unwrap();
        assert_eq!(
            (f.status, f.value.as_deref()),
            (FieldStatus::Filled, Some(want.as_str())),
            "{}",
            f.id
        );
    }
}

#[test]
#[ignore = "requires vendored pdfium and TrOCR"]
fn unrelated_pdf_is_not_matched() {
    let r = recognizer();
    let res = r
        .recognize(include_bytes!("../../fixtures/multipage.pdf"), None)
        .unwrap();
    assert!(res.form.is_none());
}
