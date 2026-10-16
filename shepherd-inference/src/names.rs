//! Person-name comparison tolerant of form-filling and handwriting-recognition variation.

/// Letters folded to ASCII before comparison. Unlisted letters are kept as they are.
const FOLD: &[(&str, &str)] = &[
    ("ÀÁÂÃÄÅĀĂĄ", "A"),
    ("ÇĆĈĊČ", "C"),
    ("ĎĐ", "D"),
    ("ÈÉÊËĒĔĖĘĚ", "E"),
    ("ĜĞĠĢ", "G"),
    ("ĤĦ", "H"),
    ("ÌÍÎÏĨĪĬĮİ", "I"),
    ("Ĵ", "J"),
    ("Ķ", "K"),
    ("ĹĻĽĿŁ", "L"),
    ("ÑŃŅŇ", "N"),
    ("ÒÓÔÕÖØŌŎŐ", "O"),
    ("ŔŖŘ", "R"),
    ("ŚŜŞŠ", "S"),
    ("ŢŤŦ", "T"),
    ("ÙÚÛÜŨŪŬŮŰŲ", "U"),
    ("Ŵ", "W"),
    ("ÝŸŶ", "Y"),
    ("ŹŻŽ", "Z"),
    ("Æ", "AE"),
    ("Œ", "OE"),
    ("ẞ", "SS"),
    ("Þ", "TH"),
];

fn fold(c: char, out: &mut String) {
    match FOLD.iter().find(|(from, _)| from.contains(c)) {
        Some((_, to)) => out.push_str(to),
        None => out.push(c),
    }
}

/// Upper-cased, folded name tokens, family name first. Apostrophes and periods are dropped
/// (`O'Brien` → `OBRIEN`); spaces and hyphens separate tokens.
pub fn tokens(family: Option<&str>, given: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for part in [family, given].into_iter().flatten() {
        for raw in part.split([' ', '-']) {
            let mut t = String::new();
            for c in raw.chars().flat_map(char::to_uppercase) {
                if c.is_alphabetic() {
                    fold(c, &mut t);
                }
            }
            if !t.is_empty() {
                out.push(t);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NameMatch {
    /// Same tokens, in any order.
    Same,
    /// Plausibly the same person: every token of the shorter name matches one of the other's,
    /// exactly, as an initial, or within the edit allowance.
    Variant,
    Different,
}

/// Edits allowed between two tokens: `edit_ratio` of the longer one's length, rounded down.
fn allowance(a: &str, b: &str, edit_ratio: f64) -> usize {
    (a.chars().count().max(b.chars().count()) as f64 * edit_ratio).floor() as usize
}

fn token_matches(a: &str, b: &str, edit_ratio: f64) -> bool {
    let initial = |x: &str, y: &str| x.chars().count() == 1 && y.starts_with(x);
    a == b || initial(a, b) || initial(b, a) || edit_distance(a, b) <= allowance(a, b, edit_ratio)
}

/// Compares two token lists from [`tokens`]. Either list empty compares as `Variant` (no evidence).
pub fn compare(a: &[String], b: &[String], edit_ratio: f64) -> NameMatch {
    if a.is_empty() || b.is_empty() {
        return NameMatch::Variant;
    }
    let (mut sa, mut sb) = (a.to_vec(), b.to_vec());
    sa.sort();
    sb.sort();
    if sa == sb {
        return NameMatch::Same;
    }
    // Spacing differences in compound names: `DE LA CRUZ` / `DELACRUZ`.
    if a.concat() == b.concat() {
        return NameMatch::Same;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut used = vec![false; long.len()];
    // Exact matches first so a fuzzy match does not take a token another token equals.
    let mut pending = Vec::new();
    for s in short {
        match (0..long.len()).find(|&i| !used[i] && long[i] == *s) {
            Some(i) => used[i] = true,
            None => pending.push(s),
        }
    }
    for s in pending {
        match (0..long.len()).find(|&i| !used[i] && token_matches(s, &long[i], edit_ratio)) {
            Some(i) => used[i] = true,
            None => return NameMatch::Different,
        }
    }
    NameMatch::Variant
}

/// Optimal string alignment distance: insertions, deletions, substitutions and adjacent
/// transpositions each cost one.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let w = b.len() + 1;
    let mut d: Vec<usize> = vec![0; (a.len() + 1) * w];
    for i in 0..=a.len() {
        d[i * w] = i;
    }
    for (j, v) in d.iter_mut().enumerate().take(w) {
        *v = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[(i - 1) * w + j - 1] + cost)
                .min(d[(i - 1) * w + j] + 1)
                .min(d[i * w + j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[(i - 2) * w + j - 2] + 1);
            }
            d[i * w + j] = v;
        }
    }
    d[a.len() * w + b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: (&str, &str), b: (&str, &str)) -> NameMatch {
        compare(
            &tokens(Some(a.0), Some(a.1)),
            &tokens(Some(b.0), Some(b.1)),
            0.25,
        )
    }

    #[test]
    fn normalizes_tokens() {
        assert_eq!(
            tokens(Some("O'Brien-Núñez"), Some("José  M.")),
            ["OBRIEN", "NUNEZ", "JOSE", "M"]
        );
        assert!(tokens(None, Some(" ")).is_empty());
    }

    #[test]
    fn same_person() {
        use NameMatch::*;
        assert_eq!(cmp(("Nguyen", "Linh"), ("NGUYEN", "LINH")), Same);
        assert_eq!(cmp(("Nguyen", "Linh"), ("Linh", "Nguyen")), Same);
        assert_eq!(cmp(("García", "José"), ("Garcia", "Jose")), Same);
        assert_eq!(cmp(("De La Cruz", "Ana"), ("Delacruz", "Ana")), Same);
        assert_eq!(cmp(("Garcia Lopez", "Maria"), ("Garcia", "Maria")), Variant);
        assert_eq!(
            cmp(("Garcia", "Maria Elena"), ("Garcia", "Maria E")),
            Variant
        );
        assert_eq!(cmp(("Garsia", "Maria"), ("Garcia", "Maria")), Variant);
        assert_eq!(cmp(("Nguyen", "Linh"), ("Nquyen", "Lihn")), Variant);
        assert_eq!(cmp(("Kowalczyk", "Anna"), ("Kowalcyzk", "Ana")), Variant);
    }

    #[test]
    fn different_people() {
        use NameMatch::*;
        assert_eq!(cmp(("Garcia", "Maria"), ("Garcia", "Jose")), Different);
        assert_eq!(cmp(("Nguyen", "Linh"), ("Okafor", "Chinedu")), Different);
        assert_eq!(cmp(("Lopez", "Maria"), ("Garcia", "Maria")), Different);
        assert_eq!(cmp(("Li", "Wei"), ("Lu", "Wei")), Different);
    }

    #[test]
    fn missing_name_is_no_evidence() {
        assert_eq!(
            compare(&[], &tokens(Some("Garcia"), None), 0.25),
            NameMatch::Variant
        );
    }

    #[test]
    fn distances() {
        assert_eq!(edit_distance("KITTEN", "SITTING"), 3);
        assert_eq!(edit_distance("", "ABC"), 3);
        assert_eq!(edit_distance("LINH", "LIHN"), 1);
        assert_eq!(edit_distance("ÅB", "AB"), 1);
    }
}
