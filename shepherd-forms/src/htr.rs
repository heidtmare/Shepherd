//! Handwritten text recognition with TrOCR (ONNX encoder + decoder, greedy decoding).

use std::{collections::HashMap, path::Path, sync::Mutex};

use image::{GrayImage, imageops};
use ort::{session::Session, value::Tensor};

const INPUT: u32 = 384;
const DECODER_START: i64 = 2;
const EOS: i64 = 2;
const MAX_TOKENS: usize = 48;

#[derive(Debug, Clone, PartialEq)]
pub struct Recognized {
    /// Reading after [`trim`]: edge punctuation and low-probability continuation removed.
    pub text: String,
    /// Untrimmed reading.
    pub full_text: String,
    /// Mean and minimum probability of the kept tokens.
    pub mean_prob: f32,
    pub min_prob: f32,
    /// Tokens were removed by [`trim`].
    pub trimmed: bool,
    /// Decoding stopped on a repetition loop or the token limit rather than EOS.
    pub degenerate: bool,
    /// Decoded pieces with their probabilities (EOS excluded), for diagnostics.
    pub tokens: Vec<(String, f32)>,
}

/// Character set a reading may use. Decoding picks the best token within the set; reported
/// probabilities stay relative to the unconstrained distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Charset {
    Digits,
    Date,
    Email,
}

impl Charset {
    fn allows(self, c: char) -> bool {
        match self {
            Charset::Digits => c.is_ascii_digit() || matches!(c, ' ' | '-' | '(' | ')' | 'A' | 'a'),
            Charset::Date => c.is_ascii_digit() || matches!(c, '/' | '-' | '.' | ' '),
            Charset::Email => c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '+'),
        }
    }
}

pub struct Trocr {
    encoder: Mutex<Session>,
    decoder: Mutex<Session>,
    vocab: Vec<String>,
    special: Vec<bool>,
    byte_of: HashMap<char, u8>,
    masks: Mutex<HashMap<Charset, std::sync::Arc<Vec<bool>>>>,
}

/// GPT-2 byte-level BPE alphabet: printable bytes map to themselves, the rest to U+0100+.
fn byte_decoder() -> HashMap<char, u8> {
    let mut bytes: Vec<u8> = (b'!'..=b'~')
        .chain(0xA1..=0xAC)
        .chain(0xAE..=0xFF)
        .collect();
    let mut chars: Vec<u32> = bytes.iter().map(|&b| b as u32).collect();
    let mut n = 0;
    for b in 0..=255u8 {
        if !bytes.contains(&b) {
            bytes.push(b);
            chars.push(256 + n);
            n += 1;
        }
    }
    bytes
        .into_iter()
        .zip(chars)
        .map(|(b, c)| (char::from_u32(c).expect("valid code point"), b))
        .collect()
}

impl Trocr {
    /// Loads `encoder_model.onnx`, `decoder_model.onnx` and `tokenizer.json` from `dir`.
    pub fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref();
        let session = |name: &str| -> anyhow::Result<Session> {
            let path = dir.join(name);
            Session::builder()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .commit_from_file(&path)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        };
        let tok: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("tokenizer.json"))?)?;
        let map = tok["model"]["vocab"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("tokenizer.json: no model.vocab"))?;
        let size = map.values().filter_map(|v| v.as_u64()).max().unwrap_or(0) as usize + 1;
        let mut vocab = vec![String::new(); size];
        for (k, v) in map {
            if let Some(i) = v.as_u64() {
                vocab[i as usize] = k.clone();
            }
        }
        let mut special = vec![false; size];
        for t in tok["added_tokens"].as_array().into_iter().flatten() {
            if let (Some(id), Some(true)) = (t["id"].as_u64(), t["special"].as_bool())
                && (id as usize) < size
            {
                special[id as usize] = true;
            }
        }
        Ok(Self {
            encoder: Mutex::new(session("encoder_model.onnx")?),
            decoder: Mutex::new(session("decoder_model.onnx")?),
            vocab,
            special,
            byte_of: byte_decoder(),
            masks: Mutex::new(HashMap::new()),
        })
    }

    /// Token ids whose text consists only of characters in `cs` (EOS always allowed).
    fn mask(&self, cs: Charset) -> std::sync::Arc<Vec<bool>> {
        let mut masks = self.masks.lock().expect("mask lock");
        masks
            .entry(cs)
            .or_insert_with(|| {
                std::sync::Arc::new(
                    (0..self.vocab.len())
                        .map(|i| {
                            i as i64 == EOS || {
                                let t = self.decode_raw(&[i as i64]);
                                !self.special[i] && !t.is_empty() && t.chars().all(|c| cs.allows(c))
                            }
                        })
                        .collect(),
                )
            })
            .clone()
    }

    fn decode_raw(&self, ids: &[i64]) -> String {
        let bytes: Vec<u8> = ids
            .iter()
            .filter_map(|&i| self.vocab.get(i as usize))
            .flat_map(|t| t.chars().filter_map(|c| self.byte_of.get(&c).copied()))
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn decode(&self, ids: &[i64]) -> String {
        let mut bytes = Vec::new();
        for &id in ids {
            let i = id as usize;
            if i >= self.vocab.len() || self.special[i] {
                continue;
            }
            bytes.extend(self.vocab[i].chars().filter_map(|c| self.byte_of.get(&c)));
        }
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }

    /// Recognizes one line of text. The image should be a tight crop of the ink on white.
    /// `cut_tail` enables continuation trimming (free text); structured values only lose edge punctuation.
    pub fn recognize(
        &self,
        img: &GrayImage,
        cut_tail: bool,
        charset: Option<Charset>,
    ) -> anyhow::Result<Recognized> {
        let mask = charset.map(|c| self.mask(c));
        let resized = imageops::resize(img, INPUT, INPUT, imageops::FilterType::CatmullRom);
        let plane = (INPUT * INPUT) as usize;
        let mut pixels = vec![0f32; 3 * plane];
        for (i, p) in resized.pixels().enumerate() {
            let v = (p[0] as f32 / 255.0 - 0.5) / 0.5;
            pixels[i] = v;
            pixels[plane + i] = v;
            pixels[2 * plane + i] = v;
        }
        let input = Tensor::from_array(([1usize, 3, INPUT as usize, INPUT as usize], pixels))?;
        let (hs_shape, hs) = {
            let mut enc = self.encoder.lock().expect("encoder lock");
            let out = enc.run(ort::inputs!["pixel_values" => input])?;
            let (shape, data) = out[0].try_extract_tensor::<f32>()?;
            (
                shape.iter().map(|&d| d as usize).collect::<Vec<_>>(),
                data.to_vec(),
            )
        };

        let mut ids = vec![DECODER_START];
        let mut probs = Vec::new();
        let mut degenerate = true;
        let mut dec = self.decoder.lock().expect("decoder lock");
        for _ in 0..MAX_TOKENS {
            let input_ids = Tensor::from_array(([1usize, ids.len()], ids.clone()))?;
            let hidden = Tensor::from_array((hs_shape.clone(), hs.clone()))?;
            let out =
                dec.run(ort::inputs!["input_ids" => input_ids, "encoder_hidden_states" => hidden])?;
            let (shape, logits) = out[0].try_extract_tensor::<f32>()?;
            let v = shape[2] as usize;
            let last = &logits[(ids.len() - 1) * v..ids.len() * v];
            let max = last.iter().copied().fold(f32::MIN, f32::max);
            let sum: f32 = last.iter().map(|l| (l - max).exp()).sum();
            let (best, &bl) = last
                .iter()
                .enumerate()
                .filter(|(i, _)| mask.as_ref().is_none_or(|m| m[*i]))
                .max_by(|a, b| a.1.total_cmp(b.1))
                .expect("non-empty vocabulary");
            probs.push((bl - max).exp() / sum);
            if best as i64 == EOS {
                degenerate = false;
                break;
            }
            ids.push(best as i64);
            // Stop on a loop with period <= 4 (e.g. "0 0 0 0" or "5 0000 5 0000").
            let n = ids.len();
            if n > 9
                && (1..=4).any(|p| {
                    ids[n - 2 * p..n - p] == ids[n - p..]
                        && (p > 1 || ids[n - 6..].iter().all(|&t| t == ids[n - 1]))
                })
            {
                break;
            }
        }
        let tokens: Vec<(String, f32)> = ids[1..]
            .iter()
            .zip(&probs)
            .map(|(&id, &p)| (self.decode(&[id]), p))
            .collect();
        let keep = trim(&tokens, cut_tail);
        let kept: Vec<f32> = tokens[keep.clone()].iter().map(|t| t.1).collect();
        let mean_prob = kept.iter().sum::<f32>() / kept.len().max(1) as f32;
        let min_prob = kept.iter().copied().fold(1.0, f32::min);
        Ok(Recognized {
            text: self.decode(&ids[1..][keep.clone()]),
            full_text: self.decode(&ids[1..]),
            mean_prob: if kept.is_empty() { 0.0 } else { mean_prob },
            min_prob: if kept.is_empty() { 0.0 } else { min_prob },
            trimmed: keep.len() != tokens.len(),
            degenerate,
            tokens,
        })
    }
}

/// Token below this probability may start a hallucinated continuation.
const TAIL_START_PROB: f32 = 0.35;
/// ...if the tokens from there on average below this.
const TAIL_MEAN_PROB: f32 = 0.5;

fn is_punct(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.chars().all(|c| {
            matches!(
                c,
                '.' | ',' | '"' | '\'' | ':' | ';' | '-' | '_' | '!' | '?' | '#'
            )
        })
}

/// Range of tokens to keep. TrOCR (trained on IAM lines, which end in punctuation) tends to append
/// a period or a low-probability continuation ("Gonzalez's son of the"). Cuts the tail at the first
/// token below [`TAIL_START_PROB`] whose remainder averages below [`TAIL_MEAN_PROB`], then strips
/// punctuation-only tokens at both ends.
pub fn trim(tokens: &[(String, f32)], cut_tail: bool) -> std::ops::Range<usize> {
    let mut end = tokens.len();
    for i in (1..tokens.len()).filter(|_| cut_tail) {
        let tail = &tokens[i..];
        let mean = tail.iter().map(|t| t.1).sum::<f32>() / tail.len() as f32;
        if tokens[i].1 < TAIL_START_PROB && mean < TAIL_MEAN_PROB {
            end = i;
            break;
        }
    }
    let mut start = 0;
    while start < end && is_punct(&tokens[start].0) {
        start += 1;
    }
    while end > start && is_punct(&tokens[end - 1].0) {
        end -= 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::{byte_decoder, trim};

    fn t(v: &[(&str, f32)]) -> Vec<(String, f32)> {
        v.iter().map(|(s, p)| (s.to_string(), *p)).collect()
    }

    #[test]
    fn trims_continuations_and_punctuation() {
        let g = t(&[
            ("G", 0.97),
            ("onz", 0.99),
            ("alez", 0.99),
            ("'s", 0.23),
            ("son", 0.03),
            ("of", 0.22),
        ]);
        assert_eq!(trim(&g, true), 0..3);
        let m = t(&[("Maria", 0.99), (".", 0.39), ("\"", 0.70), ("...", 0.28)]);
        assert_eq!(trim(&m, true), 0..1);
        let q = t(&[("\"", 0.91), ("than", 0.22), ("h", 0.98), (".", 0.75)]);
        assert_eq!(trim(&q, true), 1..3);
        let clean = t(&[("97", 0.99), ("205", 0.98)]);
        assert_eq!(trim(&clean, true), 0..2);
        assert!(trim(&t(&[(".", 0.9)]), true).is_empty());
        let date = t(&[
            ("03", 0.91),
            ("/", 0.9),
            ("14", 0.47),
            (".", 0.29),
            ("1991", 0.64),
            (".", 0.5),
        ]);
        assert_eq!(trim(&date, false), 0..5);
    }

    #[test]
    fn byte_alphabet_is_a_bijection() {
        let m = byte_decoder();
        assert_eq!(m.len(), 256);
        assert_eq!(m[&'Ġ'], b' ');
        assert_eq!(m[&'a'], b'a');
    }
}
