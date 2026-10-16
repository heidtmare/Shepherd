//! Image routines: page fingerprints, alignment, and ink isolation by subtracting the blank form.

use image::{GrayImage, Luma, imageops};

/// Pixels this much darker than the blank template count as ink.
pub const INK_DELTA: u8 = 60;

/// Downsampled, mean-centred, unit-norm page signature for template classification.
pub fn fingerprint(img: &GrayImage) -> Vec<f32> {
    let small = imageops::resize(img, 48, 62, imageops::FilterType::Triangle);
    let v: Vec<f32> = small.pixels().map(|p| p[0] as f32).collect();
    let mean = v.iter().sum::<f32>() / v.len() as f32;
    let centred: Vec<f32> = v.iter().map(|x| x - mean).collect();
    let norm = centred.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    centred.into_iter().map(|x| x / norm).collect()
}

/// Cosine similarity of two fingerprints (1.0 = identical layout).
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn ncc_at(a: &GrayImage, b: &GrayImage, dx: i32, dy: i32, step: u32) -> f32 {
    // Correlate darkness (255 - v) over the overlap of `a` and `b` shifted by (dx, dy).
    let (w, h) = (a.width() as i32, a.height() as i32);
    let (mut sab, mut saa, mut sbb) = (0f64, 0f64, 0f64);
    let mut y = h / 10;
    while y < h - h / 10 {
        let by = y + dy;
        if by >= 0 && by < b.height() as i32 {
            let mut x = w / 10;
            while x < w - w / 10 {
                let bx = x + dx;
                if bx >= 0 && bx < b.width() as i32 {
                    let va = 255.0 - a.get_pixel(x as u32, y as u32)[0] as f64;
                    let vb = 255.0 - b.get_pixel(bx as u32, by as u32)[0] as f64;
                    sab += va * vb;
                    saa += va * va;
                    sbb += vb * vb;
                }
                x += step as i32;
            }
        }
        y += step as i32;
    }
    (sab / (saa.sqrt() * sbb.sqrt()).max(1e-9)) as f32
}

/// Translation (dx, dy) in pixels such that `scan(x + dx, y + dy)` ≈ `blank(x, y)`.
/// Coarse search at 1/4 scale within ±`max_shift` px, then ±3 px refinement at full scale.
pub fn align(blank: &GrayImage, scan: &GrayImage, max_shift: i32) -> (i32, i32) {
    let q = 4;
    let bq = imageops::resize(
        blank,
        blank.width() / q,
        blank.height() / q,
        imageops::FilterType::Triangle,
    );
    let sq = imageops::resize(
        scan,
        blank.width() / q,
        blank.height() / q,
        imageops::FilterType::Triangle,
    );
    let r = max_shift / q as i32;
    let mut best = (0, 0, f32::MIN);
    for dy in -r..=r {
        for dx in -r..=r {
            let s = ncc_at(&bq, &sq, dx, dy, 2);
            if s > best.2 {
                best = (dx, dy, s);
            }
        }
    }
    let (cx, cy) = (best.0 * q as i32, best.1 * q as i32);
    // Blur so 1px rules still correlate at off-by-one shifts.
    let (bb, sb) = (imageops::blur(blank, 1.0), imageops::blur(scan, 1.0));
    let mut fine = (cx, cy, f32::MIN);
    for dy in cy - 3..=cy + 3 {
        for dx in cx - 3..=cx + 3 {
            let s = ncc_at(&bb, &sb, dx, dy, 1);
            if s > fine.2 {
                fine = (dx, dy, s);
            }
        }
    }
    (fine.0, fine.1)
}

/// Pixel rectangle `[x0, y0, x1, y1)` (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl PxRect {
    /// From PDF points (bottom-left origin) at `scale` px/pt on a page `page_h_pt` tall.
    pub fn from_pdf(rect: [f32; 4], page_h_pt: f32, scale: f32, w: u32, h: u32) -> Self {
        let c = |v: f32, max: u32| (v.max(0.0) as u32).min(max);
        PxRect {
            x0: c(rect[0] * scale, w),
            y0: c((page_h_pt - rect[3]) * scale, h),
            x1: c(rect[2] * scale, w),
            y1: c((page_h_pt - rect[1]) * scale, h),
        }
    }

    pub fn width(&self) -> u32 {
        self.x1.saturating_sub(self.x0)
    }

    pub fn height(&self) -> u32 {
        self.y1.saturating_sub(self.y0)
    }

    /// Grows by fractions of the height vertically and width horizontally, clamped to `w`×`h`.
    pub fn grow(&self, up: f32, down: f32, side: f32, w: u32, h: u32) -> Self {
        let hh = self.height() as f32;
        let ww = self.width() as f32;
        PxRect {
            x0: (self.x0 as f32 - ww * side).max(0.0) as u32,
            y0: (self.y0 as f32 - hh * up).max(0.0) as u32,
            x1: ((self.x1 as f32 + ww * side) as u32).min(w),
            y1: ((self.y1 as f32 + hh * down) as u32).min(h),
        }
    }

    pub fn shrink(&self, px: u32) -> Self {
        PxRect {
            x0: self.x0 + px.min(self.width() / 2),
            y0: self.y0 + px.min(self.height() / 2),
            x1: self.x1.saturating_sub(px.min(self.width() / 2)),
            y1: self.y1.saturating_sub(px.min(self.height() / 2)),
        }
    }
}

/// Ink found in a region of the scan after removing everything already present on the blank.
#[derive(Debug, Clone)]
pub struct Ink {
    /// Fraction of region pixels that are ink.
    pub ratio: f32,
    pub pixels: u32,
    /// Tight bounds of ink in page pixel coordinates (blank frame).
    pub bbox: Option<PxRect>,
    /// Scan pixels inside `bbox` with non-ink set to white, padded; ready for HTR.
    pub crop: Option<GrayImage>,
}

/// Compares `region` of the blank against the scan (shifted by `shift`) and isolates new ink.
pub fn ink(
    blank: &GrayImage,
    scan: &GrayImage,
    region: PxRect,
    shift: (i32, i32),
    pad: u32,
) -> Ink {
    let (mut minx, mut miny, mut maxx, mut maxy) = (u32::MAX, u32::MAX, 0, 0);
    let mut pixels = 0u32;
    let area = (region.width() * region.height()).max(1);
    let sample = |x: u32, y: u32| -> Option<u8> {
        let sx = x as i32 + shift.0;
        let sy = y as i32 + shift.1;
        (sx >= 0 && sy >= 0 && (sx as u32) < scan.width() && (sy as u32) < scan.height())
            .then(|| scan.get_pixel(sx as u32, sy as u32)[0])
    };
    let is_ink = |x: u32, y: u32| -> bool {
        let Some(s) = sample(x, y) else { return false };
        // Tolerate 1px misalignment of printed lines: compare against the darkest blank neighbour.
        let mut b = blank.get_pixel(x, y)[0];
        for (nx, ny) in [
            (x.wrapping_sub(1), y),
            (x + 1, y),
            (x, y.wrapping_sub(1)),
            (x, y + 1),
        ] {
            if nx < blank.width() && ny < blank.height() {
                b = b.min(blank.get_pixel(nx, ny)[0]);
            }
        }
        b.saturating_sub(s) >= INK_DELTA
    };
    // Raw mask, then despeckle: keep ink pixels with at least 2 ink neighbours (scan noise and
    // JPEG artefacts are isolated; strokes are not).
    let (rw, rh) = (region.width() as usize, region.height() as usize);
    let raw: Vec<bool> = (0..rw * rh)
        .map(|i| is_ink(region.x0 + (i % rw) as u32, region.y0 + (i / rw) as u32))
        .collect();
    let at = |x: isize, y: isize| {
        x >= 0
            && y >= 0
            && (x as usize) < rw
            && (y as usize) < rh
            && raw[y as usize * rw + x as usize]
    };
    let mask: Vec<bool> = (0..rw * rh)
        .map(|i| {
            let (x, y) = ((i % rw) as isize, (i / rw) as isize);
            raw[i]
                && (-1..=1)
                    .flat_map(|dy| (-1..=1).map(move |dx| (dx, dy)))
                    .filter(|&(dx, dy)| (dx, dy) != (0, 0) && at(x + dx, y + dy))
                    .count()
                    >= 2
        })
        .collect();
    for (i, _) in mask.iter().enumerate().filter(|(_, m)| **m) {
        let (x, y) = (region.x0 + (i % rw) as u32, region.y0 + (i / rw) as u32);
        pixels += 1;
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    let ratio = pixels as f32 / area as f32;
    if pixels == 0 {
        return Ink {
            ratio,
            pixels,
            bbox: None,
            crop: None,
        };
    }
    let bbox = PxRect {
        x0: minx,
        y0: miny,
        x1: maxx + 1,
        y1: maxy + 1,
    };
    let mut crop =
        GrayImage::from_pixel(bbox.width() + 2 * pad, bbox.height() + 2 * pad, Luma([255]));
    for y in bbox.y0..bbox.y1 {
        for x in bbox.x0..bbox.x1 {
            let i = (y - region.y0) as usize * rw + (x - region.x0) as usize;
            if mask[i]
                && let Some(s) = sample(x, y)
            {
                crop.put_pixel(x - bbox.x0 + pad, y - bbox.y0 + pad, Luma([s]));
            }
        }
    }
    Ink {
        ratio,
        pixels,
        bbox: Some(bbox),
        crop: Some(crop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_with_box(offset: (u32, u32)) -> GrayImage {
        let mut img = GrayImage::from_pixel(400, 520, Luma([255]));
        for x in 50..350 {
            img.put_pixel(x + offset.0, 100 + offset.1, Luma([0]));
            img.put_pixel(x + offset.0, 140 + offset.1, Luma([0]));
        }
        for y in 100..=140 {
            img.put_pixel(50 + offset.0, y + offset.1, Luma([0]));
            img.put_pixel(349 + offset.0, y + offset.1, Luma([0]));
        }
        for y in 200..480 {
            img.put_pixel(30 + offset.0, y + offset.1, Luma([0]));
        }
        img
    }

    #[test]
    fn aligns_and_isolates_ink() {
        let blank = page_with_box((0, 0));
        let mut scan = page_with_box((7, 5));
        for x in 120..160 {
            for y in 115..125 {
                scan.put_pixel(x + 7, y + 5, Luma([20]));
            }
        }
        assert_eq!(align(&blank, &scan, 24), (7, 5));
        let region = PxRect {
            x0: 50,
            y0: 100,
            x1: 350,
            y1: 141,
        };
        let found = ink(&blank, &scan, region, (7, 5), 4);
        assert_eq!(
            found.bbox,
            Some(PxRect {
                x0: 120,
                y0: 115,
                x1: 160,
                y1: 125
            })
        );
        assert_eq!(found.pixels, 400);
        let mut noisy = page_with_box((7, 5));
        for (x, y) in [(60, 110), (300, 130), (200, 105)] {
            noisy.put_pixel(x + 7, y + 5, Luma([0]));
        }
        let none = ink(&blank, &noisy, region, (7, 5), 4);
        assert_eq!(
            none.pixels, 0,
            "printed box and isolated specks must not count as ink"
        );
    }

    #[test]
    fn fingerprint_similarity() {
        let a = fingerprint(&page_with_box((0, 0)));
        let b = fingerprint(&page_with_box((3, 2)));
        let mut other = GrayImage::from_pixel(400, 520, Luma([255]));
        for y in 0..520 {
            other.put_pixel(200, y, Luma([0]));
        }
        assert!(similarity(&a, &b) > 0.8);
        assert!(similarity(&a, &fingerprint(&other)) < similarity(&a, &b));
    }

    #[test]
    fn pdf_rect_conversion() {
        let r = PxRect::from_pdf([72.0, 700.0, 144.0, 720.0], 792.0, 2.0, 1224, 1584);
        assert_eq!(
            r,
            PxRect {
                x0: 144,
                y0: 144,
                x1: 288,
                y1: 184
            }
        );
    }
}
