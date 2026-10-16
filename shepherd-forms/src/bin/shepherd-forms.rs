//! Form tooling: generate templates from blank fillable PDFs, synthesize filled samples,
//! and run recognition on a PDF.

use std::{collections::BTreeMap, path::PathBuf};

use clap::{Parser, Subcommand};
use shepherd_forms::{
    acroform,
    htr::Trocr,
    recognize::{Recognizer, RecognizerConfig},
    render, synth,
    template::{FormTemplate, TemplateSet},
};

#[derive(Parser)]
struct Cli {
    /// Forms root: contains templates/, the blank PDFs and fonts/.
    #[arg(long, env = "FORMS_DIR", default_value = "forms")]
    forms: PathBuf,
    #[arg(long, env = "PDFIUM_LIB_DIR", default_value = "vendor/pdfium/lib")]
    pdfium: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write templates/<id>.json from a blank fillable PDF under the forms root.
    Template {
        /// Path relative to the forms root, e.g. uscis/i-765.pdf.
        #[arg(long)]
        pdf: String,
        /// Edition id. For a form's second edition use e.g. `uscis.i-765@2026-01-20` with
        /// `--form-type uscis.i-765`, and give the existing edition an edition id too (keeping its
        /// old id in `aliases`).
        #[arg(long)]
        id: String,
        #[arg(long)]
        title: String,
        /// Form type shared by all editions; defaults to the id.
        #[arg(long)]
        form_type: Option<String>,
    },
    /// Render a filled sample from a template and a JSON map of field id → value.
    Synth {
        #[arg(long)]
        template: String,
        #[arg(long)]
        values: PathBuf,
        #[arg(long, default_value = "fonts/Caveat.ttf")]
        font: String,
        #[arg(long, default_value = "0,0", value_parser = parse_pair)]
        shift: (i32, i32),
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 0.0)]
        noise: f32,
        #[arg(long)]
        out: PathBuf,
    },
    /// Recognize a PDF and print the result as JSON.
    Recognize {
        pdf: PathBuf,
        #[arg(long)]
        hint: Option<String>,
        #[arg(
            long,
            env = "HTR_MODEL_DIR",
            default_value = "vendor/models/trocr-base-handwritten"
        )]
        model: PathBuf,
        /// Print only fields that are filled or flagged.
        #[arg(long)]
        brief: bool,
    },
    /// Print the AcroForm values of a PDF.
    Values { pdf: PathBuf },
}

fn parse_pair(s: &str) -> Result<(i32, i32), String> {
    let (a, b) = s.split_once(',').ok_or("expected dx,dy")?;
    Ok((
        a.trim().parse().map_err(|_| "bad dx")?,
        b.trim().parse().map_err(|_| "bad dy")?,
    ))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    render::init(&cli.pdfium)?;
    match cli.cmd {
        Cmd::Template {
            pdf,
            id,
            title,
            form_type,
        } => {
            let bytes = std::fs::read(cli.forms.join(&pdf))?;
            let mut t = acroform::template_from_pdf(&bytes, &id, &title, &pdf)?;
            let out = cli.forms.join("templates").join(format!("{id}.json"));
            // Regenerating keeps hand-edited identity and `required` flags.
            if let Ok(prev) = std::fs::read(&out) {
                let prev: FormTemplate = serde_json::from_slice(&prev)?;
                for f in &mut t.fields {
                    f.required = prev.field(&f.id).is_some_and(|p| p.required);
                }
                t.form_type = prev.form_type;
                t.aliases = prev.aliases;
            }
            if form_type.is_some() {
                t.form_type = form_type.filter(|f| f != &id);
            }
            std::fs::create_dir_all(out.parent().expect("has parent"))?;
            std::fs::write(&out, serde_json::to_string_pretty(&t)? + "\n")?;
            eprintln!(
                "{}: {} pages, {} fields, edition {:?}",
                out.display(),
                t.pages.len(),
                t.fields.len(),
                t.edition
            );
        }
        Cmd::Synth {
            template,
            values,
            font,
            shift,
            seed,
            noise,
            out,
        } => {
            let set = TemplateSet::load(&cli.forms)?;
            let t = set
                .get(&template)
                .ok_or_else(|| anyhow::anyhow!("unknown template {template}"))?;
            let values: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(values)?)?;
            for k in values.keys() {
                anyhow::ensure!(t.field(k).is_some(), "{template} has no field {k}");
            }
            let pdf = synth::synthesize(
                t,
                &std::fs::read(set.blank_path(t))?,
                &values,
                &std::fs::read(cli.forms.join(font))?,
                &synth::SynthOptions {
                    shift,
                    seed,
                    noise,
                    ..Default::default()
                },
            )?;
            std::fs::write(&out, pdf)?;
        }
        Cmd::Recognize {
            pdf,
            hint,
            model,
            brief,
        } => {
            let set = TemplateSet::load(&cli.forms)?;
            let htr = Trocr::load(&model)
                .map_err(|e| eprintln!("HTR disabled: {e}"))
                .ok();
            let r = Recognizer::new(set, htr, RecognizerConfig::default())?;
            let started = std::time::Instant::now();
            let mut res = r.recognize(&std::fs::read(pdf)?, hint.as_deref())?;
            eprintln!("recognized in {:?}: {:?}", started.elapsed(), res.stats);
            if brief {
                res.fields
                    .retain(|f| f.status != shepherd_forms::recognize::FieldStatus::Empty);
            }
            println!("{}", serde_json::to_string_pretty(&res)?);
        }
        Cmd::Values { pdf } => {
            let doc = lopdf::Document::load(pdf)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&acroform::filled_values(&doc))?
            );
        }
    }
    Ok(())
}
