//! Renders a [`Letter`] to PDF: US Letter, standard Helvetica fonts (not embedded), WinAnsi text.
//!
//! Output is deterministic for a given letter, so its sha256 identifies the content.

use lopdf::{
    Document, Object, Stream, StringFormat,
    content::{Content, Operation},
    dictionary,
};

use crate::letters::{Block, Letter};

const PAGE_W: f32 = 612.0;
const PAGE_H: f32 = 792.0;
const MARGIN: f32 = 72.0;
const BOTTOM: f32 = 72.0;
const BODY_SIZE: f32 = 10.5;
const LEADING: f32 = 14.0;
const INDENT: f32 = 20.0;
/// Address block origin (top line), positioned for a #10 window envelope.
const ADDRESS_TOP: f32 = PAGE_H - 162.0;
const DETAILS_X: f32 = 360.0;

/// Helvetica advance widths (1/1000 em) for 32..=126.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];
/// Helvetica-Bold advance widths for 32..=126.
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667,
    611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556,
    278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

#[derive(Clone, Copy, PartialEq)]
enum Font {
    Regular,
    Bold,
}

impl Font {
    fn name(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
        }
    }
}

/// WinAnsiEncoding byte for `c`; unmappable characters become `?`.
fn win_ansi(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        '\u{a0}'..='\u{ff}' => c as u32 as u8,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201c}' => 0x93,
        '\u{201d}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{20ac}' => 0x80,
        '\t' | '\n' | '\r' => b' ',
        _ => b'?',
    }
}

fn encode(s: &str) -> Vec<u8> {
    s.chars().map(win_ansi).collect()
}

fn char_width(b: u8, font: Font) -> f32 {
    let table = match font {
        Font::Regular => &HELVETICA,
        Font::Bold => &HELVETICA_BOLD,
    };
    match b {
        32..=126 => table[(b - 32) as usize] as f32,
        0x95 => 350.0,
        0x96 => 556.0,
        0x97 => 1000.0,
        _ => 556.0,
    }
}

fn text_width(s: &str, font: Font, size: f32) -> f32 {
    encode(s).iter().map(|b| char_width(*b, font)).sum::<f32>() * size / 1000.0
}

/// Greedy word wrap to `width` points. Words longer than a line are split.
fn wrap(s: &str, font: Font, size: f32, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if text_width(&candidate, font, size) <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let mut rest = word;
            while text_width(rest, font, size) > width {
                let mut w = 0.0;
                let cut = rest
                    .char_indices()
                    .find(|&(i, c)| {
                        w += char_width(win_ansi(c), font) * size / 1000.0;
                        w > width && i > 0
                    })
                    .map_or(rest.len(), |(i, _)| i);
                lines.push(rest[..cut].to_owned());
                rest = &rest[cut..];
            }
            line = rest.to_owned();
        }
        lines.push(line);
    }
    lines
}

struct Page {
    ops: Vec<Operation>,
}

impl Page {
    fn text(&mut self, x: f32, y: f32, font: Font, size: f32, s: &str) {
        self.ops.extend([
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![font.name().into(), size.into()]),
            Operation::new("Td", vec![x.into(), y.into()]),
            Operation::new("Tj", vec![Object::String(encode(s), StringFormat::Literal)]),
            Operation::new("ET", vec![]),
        ]);
    }

    fn rule(&mut self, x0: f32, x1: f32, y: f32) {
        self.ops.extend([
            Operation::new("w", vec![0.5.into()]),
            Operation::new("m", vec![x0.into(), y.into()]),
            Operation::new("l", vec![x1.into(), y.into()]),
            Operation::new("S", vec![]),
        ]);
    }
}

struct Layout<'a> {
    letter: &'a Letter,
    pages: Vec<Page>,
    y: f32,
}

impl Layout<'_> {
    fn page(&mut self) -> &mut Page {
        self.pages.last_mut().expect("at least one page")
    }

    fn new_page(&mut self) {
        let mut p = Page { ops: Vec::new() };
        let top = PAGE_H - MARGIN + 18.0;
        p.text(MARGIN, top, Font::Bold, 9.0, &self.letter.letterhead[0]);
        p.text(
            DETAILS_X,
            top,
            Font::Regular,
            9.0,
            &format!("Receipt number {}", self.letter.receipt_number),
        );
        p.rule(MARGIN, PAGE_W - MARGIN, top - 6.0);
        self.pages.push(p);
        self.y = top - 30.0;
    }

    /// Starts a new page when fewer than `needed` points remain.
    fn ensure(&mut self, needed: f32) {
        if self.y - needed < BOTTOM {
            self.new_page();
        }
    }

    fn lines(&mut self, x: f32, font: Font, size: f32, lines: &[String]) {
        for l in lines {
            self.ensure(LEADING);
            let y = self.y;
            self.page().text(x, y, font, size, l);
            self.y -= LEADING;
        }
    }

    fn paragraph(&mut self, s: &str) {
        let lines = wrap(s, Font::Regular, BODY_SIZE, PAGE_W - 2.0 * MARGIN);
        self.ensure(LEADING * lines.len().min(2) as f32);
        self.lines(MARGIN, Font::Regular, BODY_SIZE, &lines);
        self.y -= 6.0;
    }

    fn heading(&mut self, s: &str) {
        self.ensure(LEADING * 3.0);
        self.y -= 4.0;
        let y = self.y;
        self.page().text(MARGIN, y, Font::Bold, BODY_SIZE, s);
        self.y -= LEADING + 2.0;
    }

    fn list(&mut self, items: &[String], numbered: bool) {
        let width = PAGE_W - 2.0 * MARGIN - INDENT;
        for (i, item) in items.iter().enumerate() {
            let marker = if numbered {
                format!("{}.", i + 1)
            } else {
                "\u{2022}".to_owned()
            };
            let lines = wrap(item, Font::Regular, BODY_SIZE, width);
            self.ensure(LEADING * lines.len().min(2) as f32);
            let y = self.y;
            self.page()
                .text(MARGIN, y, Font::Regular, BODY_SIZE, &marker);
            self.lines(MARGIN + INDENT, Font::Regular, BODY_SIZE, &lines);
            self.y -= 3.0;
        }
        self.y -= 4.0;
    }

    /// First page: letterhead, case details, address block and title.
    fn first_page(&mut self) {
        let l = self.letter;
        let mut p = Page { ops: Vec::new() };
        let mut y = PAGE_H - MARGIN + 18.0;
        for (i, line) in l.letterhead.iter().enumerate() {
            let (font, size) = if i == 0 {
                (Font::Bold, 12.0)
            } else {
                (Font::Regular, 9.0)
            };
            p.text(MARGIN, y, font, size, line);
            y -= if i == 0 { 14.0 } else { 11.0 };
        }
        let mut dy = PAGE_H - MARGIN + 18.0;
        for (label, value) in &l.details {
            p.text(DETAILS_X, dy, Font::Regular, 7.5, label);
            dy -= 10.0;
            for v in wrap(value, Font::Bold, 9.5, PAGE_W - MARGIN - DETAILS_X) {
                p.text(DETAILS_X, dy, Font::Bold, 9.5, &v);
                dy -= 11.5;
            }
            dy -= 3.0;
        }
        let mut ay = ADDRESS_TOP.min(y - 24.0);
        for line in std::iter::once(&l.recipient.name).chain(&l.recipient.lines) {
            p.text(MARGIN, ay, Font::Regular, 10.5, line);
            ay -= 13.0;
        }
        self.pages.push(p);
        self.y = ay.min(dy) - 28.0;
        let title = wrap(&l.title, Font::Bold, 14.0, PAGE_W - 2.0 * MARGIN);
        for t in &title {
            let y = self.y;
            self.page().text(MARGIN, y, Font::Bold, 14.0, t);
            self.y -= 18.0;
        }
        self.y -= 8.0;
    }

    fn signature(&mut self) {
        if self.letter.signature.is_empty() {
            return;
        }
        self.ensure(LEADING * (self.letter.signature.len() as f32 + 2.0));
        self.y -= LEADING;
        let sig = self.letter.signature.clone();
        self.lines(MARGIN, Font::Regular, BODY_SIZE, &sig);
    }
}

pub fn render(letter: &Letter) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(!letter.letterhead.is_empty(), "letterhead is empty");
    let mut layout = Layout {
        letter,
        pages: Vec::new(),
        y: 0.0,
    };
    layout.first_page();
    for b in &letter.blocks {
        match b {
            Block::Heading(h) => layout.heading(h),
            Block::Paragraph(p) => layout.paragraph(p),
            Block::Numbered(items) => layout.list(items, true),
            Block::Bullets(items) => layout.list(items, false),
        }
    }
    layout.signature();

    let total = layout.pages.len();
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let font = |base: &str| {
        dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => base.to_owned(),
            "Encoding" => "WinAnsiEncoding",
        }
    };
    let f1 = doc.add_object(font("Helvetica"));
    let f2 = doc.add_object(font("Helvetica-Bold"));
    let resources = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => f1, "F2" => f2 },
    });
    let mut kids = Vec::with_capacity(total);
    for (i, mut page) in layout.pages.into_iter().enumerate() {
        page.text(
            MARGIN,
            40.0,
            Font::Regular,
            8.0,
            &format!("{} · {}", letter.title, letter.receipt_number),
        );
        let footer = format!("Page {} of {total}", i + 1);
        page.text(
            PAGE_W - MARGIN - text_width(&footer, Font::Regular, 8.0),
            40.0,
            Font::Regular,
            8.0,
            &footer,
        );
        let content = Content {
            operations: page.ops,
        }
        .encode()?;
        let content_id = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), PAGE_W.into(), PAGE_H.into()],
            "Contents" => content_id,
            "Resources" => resources,
        });
        kids.push(Object::Reference(page_id));
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => total as i64,
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    let info = doc.add_object(dictionary! {
        "Title" => Object::string_literal(encode(&format!("{} {}", letter.title, letter.receipt_number))),
        "Producer" => Object::string_literal(crate::PRODUCER),
    });
    doc.trailer.set("Root", catalog);
    doc.trailer.set("Info", info);
    doc.compress();
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::letters::{Kind, Recipient};

    fn letter(blocks: Vec<Block>) -> Letter {
        Letter {
            kind: Kind::Rfe,
            title: "Request for Evidence".into(),
            receipt_number: "SHP2600000001".into(),
            letter_date: "2026-09-24".into(),
            letterhead: vec!["Office".into(), "P.O. Box 1".into()],
            details: vec![("Receipt number".into(), "SHP2600000001".into())],
            recipient: Recipient {
                name: "José Nguyen".into(),
                lines: vec!["1 Main St".into(), "Portland OR 97205".into()],
            },
            blocks,
            signature: vec!["Director".into()],
        }
    }

    fn page_count(pdf: &[u8]) -> usize {
        Document::load_mem(pdf).unwrap().get_pages().len()
    }

    #[test]
    fn wraps_at_width() {
        let lines = wrap(
            "The quick brown fox jumps over the lazy dog again and again",
            Font::Regular,
            10.0,
            120.0,
        );
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|l| text_width(l, Font::Regular, 10.0) <= 120.0)
        );
        let long = wrap(&"W".repeat(40), Font::Regular, 10.0, 100.0);
        assert!(long.len() > 1 && long.iter().all(|l| !l.is_empty()));
    }

    #[test]
    fn encodes_latin1_and_replaces_others() {
        assert_eq!(encode("José – ok"), b"Jos\xe9 \x96 ok");
        assert_eq!(encode("日本"), b"??");
    }

    #[test]
    fn renders_single_page_deterministically() {
        let l = letter(vec![
            Block::Paragraph("We are reviewing your case.".into()),
            Block::Heading("Evidence requested".into()),
            Block::Numbered(vec!["Passport (a copy)".into(), "Form I-94".into()]),
        ]);
        let a = render(&l).unwrap();
        assert!(a.starts_with(b"%PDF-1.7"));
        assert_eq!(page_count(&a), 1);
        assert_eq!(a, render(&l).unwrap());
    }

    #[test]
    fn long_letters_paginate() {
        let items = (0..80)
            .map(|i| format!("Evidence item {i} with a description long enough to wrap onto a second line of the letter body"))
            .collect();
        let pdf = render(&letter(vec![Block::Numbered(items)])).unwrap();
        assert!(page_count(&pdf) >= 3);
    }
}
