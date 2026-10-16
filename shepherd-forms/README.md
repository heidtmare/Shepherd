# shepherd-forms

On-prem recognition of handwritten values in known forms. Used by intake (recognition) and workbench (templates, page rendering).

## Pipeline

1. Templates: `forms/templates/<id>.json` is generated from the blank fillable PDF. Each AcroForm widget becomes a field with its name, tooltip label, short label, section, page, rect in PDF points, kind (`text`, `checkbox`, `choice`, `signature`), value type, max length, options and checkbox group.
2. Typed-in PDFs: if the upload carries AcroForm values (`/V`) matching a template, values are taken exactly (`source: acroform`).
3. Classification: the page-1 fingerprint (48×62 grayscale, cosine similarity) is compared against each blank, with a minimum score of 0.75. An upload's `form_type` label bypasses classification.
4. Alignment: each scanned page is aligned to the rendered blank by translation-only normalized cross-correlation (coarse at 1/4 scale, then refined at full scale, ±60 px at 200 DPI). Rotation and scale are not corrected.
5. Ink isolation: the blank is subtracted per field region. The region is grown upward and outward for handwriting overflow, and pixels at least 60 levels darker than the blank neighbourhood count as ink. The mask is then despeckled, keeping ink pixels with 2 or more ink neighbours. This removes printed boxes, labels and scan noise.
6. Checkboxes: an ink ratio of 12% or more means checked, 4% or less means unchecked, and anything in between is flagged `ambiguous_mark`.
7. Handwriting: TrOCR (`microsoft/trocr-base-handwritten`, ONNX export `Xenova/trocr-base-handwritten`, fp32) runs through ONNX Runtime with greedy decoding.
   - Date, email and digit fields decode with the model restricted to their allowed characters. Probabilities stay those of the unconstrained model.
   - For free text, the low-probability continuation that TrOCR appends is cut (see `htr::trim`). All types lose edge punctuation.
8. Validation: values are normalized per value type: date `MM/DD/YYYY`, 10-digit US phone, SSN, A-Number (9 digits), USCIS account (12), ZIP, USPS state, email, name, choice options and max length. Digit fields get `O→0` / `l→1` repair first.

A field requires receipting when any of these holds:

| Reason | Condition |
|---|---|
| `low_confidence` | Mean token probability < 0.85, any token < 0.6, or a mixed letter/digit value (passport, receipt numbers) with any token < 0.9 |
| `invalid_format` | Normalization fails |
| `illegible` | Ink present but nothing stable decoded (empty output or a repetition loop) |
| `ambiguous_mark` | Checkbox ink ratio between the thresholds |
| `page_missing` | Scan has fewer pages than the template |
| `htr_unavailable` | Model not loaded |

The thresholds are in `RecognizerConfig`. Quantized TrOCR models lose too much accuracy, and `trocr-small` is not usable on this input.

## Measured on synthetic scans

`tests/recognition.rs` fills forms with three handwriting fonts, shifts them and adds noise. It then asserts zero false accepts (every value marked filled matches the truth, case-insensitive for names), no written field reported empty, and no blank field reported filled.

| Sample | Filled correctly | Flagged for receipting |
|---|---|---|
| G-1145, 3 styles (5 fields each) | 11 | 4 |
| I-765 p1–3, 2 styles (15 fields each) | 18 | 12 |

Emails and dense digit strings are flagged most often. Synthetic fonts are cleaner than real handwriting, so expect more flags on real scans. Calibrate the thresholds on real samples before relying on auto-accept.

## CLI

```
cargo run --release -p shepherd-forms -- template --pdf uscis/i-765.pdf --id uscis.i-765 --title "Application for Employment Authorization"
cargo run --release -p shepherd-forms -- synth --template uscis.i-765 --values forms/samples/i-765.values.json --font fonts/Caveat.ttf --shift=-5,8 --out scan.pdf
cargo run --release -p shepherd-forms -- recognize scan.pdf --brief
```

`FORMS_DEBUG_DIR=/tmp/crops` writes each field crop and prints per-token probabilities during recognition.

## Runtime dependencies

- pdfium: `libpdfium.{so,dylib}` from bblanchon/pdfium-binaries, loaded from `PDFIUM_LIB_DIR`.
- TrOCR: `encoder_model.onnx`, `decoder_model.onnx` and `tokenizer.json` (1.3 GB) in `HTR_MODEL_DIR`.
- `scripts/fetch-deps.sh` downloads both into `vendor/` for local runs and tests. Compose downloads the model into the `models` volume, and the images bundle pdfium.

## Editions

A template is one edition of a form. `id` identifies the edition and is recorded in every artifact read with it; `form_type` (default: `id`) groups editions. With one edition, `id` is the form type (`uscis.i-765`).

- References: a form type means "any of its editions" (upload `form_type` label, recognition hints, workbench benefits). A template id or alias means that edition exactly.
- Classification: page 1 selects the form. When the form has several editions, each is scored by the page-count-weighted mean fingerprint similarity over all pages. The best edition is used, and the others are listed with their scores in `form.alternatives`. For AcroForm PDFs, the edition whose fields cover the most filled values wins (ties: newest).
- `intake.form.v1` records `form.template_id`, `form.form_type` and `form.blank_sha256`. Readers use the template the artifact names, not the newest edition.
- `TemplateSet` rejects duplicate ids or aliases, and a form type with several editions whose template id is the bare form type.

Adding an edition of a form that has one template:

1. Rename the current template's `id` to an edition id (`uscis.i-765@2025-08-21`), set `"form_type": "uscis.i-765"` and `"aliases": ["uscis.i-765"]` so existing artifacts still resolve. Rename its file to match.
2. Place the new blank PDF under `forms/` and run `template --id uscis.i-765@2026-01-20 --form-type uscis.i-765`. Regenerating an existing template keeps `form_type`, `aliases` and `required`.
3. Start workbench: benefit validation names every fact whose field id is missing from the new edition. Add the new ids as candidates in `benefits.toml` (`family_name = ["<new id>", "<old id>"]`).

Never change a template's fields or blank in place once artifacts reference it; add an edition instead.

## Forms included

`forms/uscis/` holds the blank G-1145, I-765, I-90 and I-130 from uscis.gov, with generated templates. To add a form, place the blank fillable PDF under `forms/`, run `template`, review the value types in the JSON, and set `required` where needed. For a new edition of an existing form, see Editions.
