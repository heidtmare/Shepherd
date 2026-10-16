//! PDF rasterization via pdfium (dynamically loaded).

use std::{path::Path, sync::OnceLock};

use image::GrayImage;
use pdfium_render::prelude::*;

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

/// Binds pdfium from `dir` (containing `libpdfium.{so,dylib}`) on first call; later calls reuse it.
pub fn init(dir: impl AsRef<Path>) -> anyhow::Result<&'static Pdfium> {
    let dir = dir.as_ref().to_path_buf();
    PDFIUM
        .get_or_init(|| {
            Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir))
                .map(Pdfium::new)
                .map_err(|e| format!("load pdfium from {}: {e}", dir.display()))
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn pdfium() -> anyhow::Result<&'static Pdfium> {
    PDFIUM
        .get()
        .ok_or_else(|| anyhow::anyhow!("pdfium not initialized"))?
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[derive(Debug, Clone)]
pub struct RenderedPage {
    /// PDF points.
    pub width_pt: f32,
    pub height_pt: f32,
    pub image: GrayImage,
}

impl RenderedPage {
    /// Pixels per PDF point.
    pub fn scale(&self) -> f32 {
        self.image.width() as f32 / self.width_pt
    }
}

/// Renders every page (or the first `max_pages`) to grayscale at `dpi`. Form widgets are drawn
/// so blank templates show their printed boxes.
pub fn render(
    bytes: &[u8],
    dpi: f32,
    max_pages: Option<usize>,
) -> anyhow::Result<Vec<RenderedPage>> {
    let doc = pdfium()?.load_pdf_from_byte_slice(bytes, None)?;
    let scale = dpi / 72.0;
    let cfg = PdfRenderConfig::new()
        .scale_page_by_factor(scale)
        .render_form_data(true);
    let mut out = Vec::new();
    for (i, page) in doc.pages().iter().enumerate() {
        if max_pages.is_some_and(|m| i >= m) {
            break;
        }
        let image = page.render_with_config(&cfg)?.as_image()?.to_luma8();
        out.push(RenderedPage {
            width_pt: page.width().value,
            height_pt: page.height().value,
            image,
        });
    }
    Ok(out)
}

pub fn page_count(bytes: &[u8]) -> anyhow::Result<usize> {
    Ok(pdfium()?
        .load_pdf_from_byte_slice(bytes, None)?
        .pages()
        .len() as usize)
}

/// Renders one page to PNG (RGB) at `dpi`; used by review UIs.
pub fn render_png(bytes: &[u8], page_index: usize, dpi: f32) -> anyhow::Result<Vec<u8>> {
    let doc = pdfium()?.load_pdf_from_byte_slice(bytes, None)?;
    let page = doc.pages().get(page_index as i32)?;
    let cfg = PdfRenderConfig::new()
        .scale_page_by_factor(dpi / 72.0)
        .render_form_data(true);
    let img = page.render_with_config(&cfg)?.as_image()?.to_rgb8();
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)?;
    Ok(buf.into_inner())
}

/// Renders every page in RGB at `dpi` (for synthesis).
pub fn render_rgb(bytes: &[u8], dpi: f32) -> anyhow::Result<Vec<(f32, f32, image::RgbImage)>> {
    let doc = pdfium()?.load_pdf_from_byte_slice(bytes, None)?;
    let cfg = PdfRenderConfig::new()
        .scale_page_by_factor(dpi / 72.0)
        .render_form_data(true);
    doc.pages()
        .iter()
        .map(|p| {
            Ok((
                p.width().value,
                p.height().value,
                p.render_with_config(&cfg)?.as_image()?.to_rgb8(),
            ))
        })
        .collect()
}
