//! Synthetic "scanned" filled forms: blank render + handwriting-font values, flattened to an
//! image-only PDF. Used for tests and demos; real scans replace these.

use std::collections::BTreeMap;

use ab_glyph::{FontRef, PxScale};
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use imageproc::drawing::{draw_line_segment_mut, draw_text_mut};
use lopdf::{Dictionary, Document, Object, Stream, dictionary};

use crate::{
    render,
    template::{FieldKind, FormTemplate},
    vision::PxRect,
};

#[derive(Debug, Clone)]
pub struct SynthOptions {
    pub dpi: f32,
    /// Whole-page translation in pixels, simulating scanner misfeed.
    pub shift: (i32, i32),
    pub seed: u64,
    /// Random size/position variation per field.
    pub jitter: bool,
    /// Fraction of pixels turned into gray speckle.
    pub noise: f32,
    pub jpeg_quality: u8,
}

impl Default for SynthOptions {
    fn default() -> Self {
        Self {
            dpi: 200.0,
            shift: (0, 0),
            seed: 1,
            jitter: true,
            noise: 0.0,
            jpeg_quality: 85,
        }
    }
}

/// xorshift64*; deterministic per seed, no external RNG dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }
}

/// Draws `values` (field id → text, or `true` for checkboxes) onto the blank and returns PDF bytes.
pub fn synthesize(
    template: &FormTemplate,
    blank_pdf: &[u8],
    values: &BTreeMap<String, String>,
    font: &[u8],
    opts: &SynthOptions,
) -> anyhow::Result<Vec<u8>> {
    let font = FontRef::try_from_slice(font)?;
    let mut rng = Rng(opts.seed.max(1));
    let mut pages = render::render_rgb(blank_pdf, opts.dpi)?;
    let ink = Rgb([24, 32, 110]);

    for f in &template.fields {
        let Some(v) = values.get(&f.id) else { continue };
        let Some((_, h_pt, img)) = pages.get_mut(f.page as usize - 1) else {
            continue;
        };
        let scale = opts.dpi / 72.0;
        let (w, h) = img.dimensions();
        let r = PxRect::from_pdf(f.rect, *h_pt, scale, w, h);
        match f.kind {
            FieldKind::Checkbox if v == "true" => {
                let (x0, y0, x1, y1) = (
                    r.x0 as f32 + 1.0,
                    r.y0 as f32 + 1.0,
                    r.x1 as f32 - 1.0,
                    r.y1 as f32 - 1.0,
                );
                for d in -1..=1 {
                    let d = d as f32;
                    draw_line_segment_mut(img, (x0 + d, y0), (x1 + d, y1), ink);
                    draw_line_segment_mut(img, (x0 + d, y1), (x1 + d, y0), ink);
                }
            }
            FieldKind::Checkbox => {}
            _ => {
                let bh = r.height() as f32;
                let (size, dx, dy) = if opts.jitter {
                    (
                        rng.range(1.0, 1.2),
                        rng.range(3.0, 12.0),
                        rng.range(-0.2, -0.05),
                    )
                } else {
                    (1.1, 6.0, -0.15)
                };
                draw_text_mut(
                    img,
                    ink,
                    r.x0 as i32 + dx as i32,
                    r.y0 as i32 + (bh * dy) as i32,
                    PxScale::from(bh * size),
                    &font,
                    v,
                );
            }
        }
    }

    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::new();
    for (i, (w_pt, h_pt, img)) in pages.into_iter().enumerate() {
        let mut img = shift(&img, opts.shift);
        if opts.noise > 0.0 {
            speckle(
                &mut img,
                opts.noise,
                &mut Rng(opts.seed.wrapping_add(i as u64 + 7)),
            );
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, opts.jpeg_quality).encode_image(&img)?;
        let image = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => img.width() as i64, "Height" => img.height() as i64,
                "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8, "Filter" => "DCTDecode",
            },
            jpeg,
        );
        let image_id = doc.add_object(image);
        let content = format!("q {w_pt} 0 0 {h_pt} 0 0 cm /Im0 Do Q");
        let content_id = doc.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
        let page = dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), Object::Real(w_pt), Object::Real(h_pt)],
            "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image_id } },
        };
        kids.push(Object::Reference(doc.add_object(page)));
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

fn shift(img: &RgbImage, (dx, dy): (i32, i32)) -> RgbImage {
    if (dx, dy) == (0, 0) {
        return img.clone();
    }
    let (w, h) = img.dimensions();
    let mut out = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (sx, sy) = (x - dx, y - dy);
            if sx >= 0 && sy >= 0 && (sx as u32) < w && (sy as u32) < h {
                out.put_pixel(x as u32, y as u32, *img.get_pixel(sx as u32, sy as u32));
            }
        }
    }
    out
}

fn speckle(img: &mut RgbImage, fraction: f32, rng: &mut Rng) {
    let n = (img.width() as f32 * img.height() as f32 * fraction) as u32;
    for _ in 0..n {
        let x = (rng.next() * img.width() as f32) as u32 % img.width();
        let y = (rng.next() * img.height() as f32) as u32 % img.height();
        let v = rng.range(120.0, 220.0) as u8;
        img.put_pixel(x, y, Rgb([v, v, v]));
    }
}
