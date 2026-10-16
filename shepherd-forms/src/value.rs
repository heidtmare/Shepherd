//! Field value types: inference from labels, normalization and validation.

use serde::{Deserialize, Serialize};
use time::{Date, Month};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    Text,
    Name,
    Date,
    Email,
    Phone,
    Ssn,
    ANumber,
    UscisAccount,
    Zip,
    State,
    Checkbox,
    Choice,
    Signature,
}

const STATES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
    "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM",
    "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA",
    "WV", "WI", "WY", "AS", "GU", "MP", "PR", "VI", "FM", "MH", "PW", "AA", "AE", "AP",
];

/// Heuristic type for a text field from its tooltip and name.
pub fn infer(label: &str, name: &str, max_len: Option<usize>) -> ValueType {
    let l = format!(
        "{} {}",
        label.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    );
    let has = |s: &str| l.contains(s);
    if has("e-mail") || has("email") {
        ValueType::Email
    } else if has("social security") || has("ssn") {
        ValueType::Ssn
    } else if has("a-number")
        || has("alien registration number")
        || has("anumber")
        || has("alien number")
    {
        ValueType::ANumber
    } else if has("uscis online account") || has("onlineacct") {
        ValueType::UscisAccount
    } else if has("telephone") || has("phone") {
        ValueType::Phone
    } else if has("zip code") || has("zipcode") {
        ValueType::Zip
    } else if (has("state") && max_len == Some(2)) || name.ends_with("_State[0]") {
        ValueType::State
    } else if (has("date") && !has("update")) || has("dob") {
        ValueType::Date
    } else if has("family name")
        || has("given name")
        || has("middle name")
        || has("last name")
        || has("first name")
    {
        ValueType::Name
    } else {
        ValueType::Text
    }
}

/// True for types whose values are digits only; HTR output gets O→0 / l→1 repair before validation.
pub fn is_numeric(t: ValueType) -> bool {
    matches!(
        t,
        ValueType::Phone
            | ValueType::Ssn
            | ValueType::ANumber
            | ValueType::UscisAccount
            | ValueType::Zip
    )
}

/// Common handwriting confusions for digit-only fields.
pub fn repair_digits(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            'O' | 'o' | 'D' | 'Q' => '0',
            'l' | 'I' | '|' | 'i' => '1',
            'Z' | 'z' => '2',
            'S' | 's' => '5',
            'B' => '8',
            c => c,
        })
        .collect()
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// Normalizes `raw` to the canonical form for `t`, or explains why it is invalid.
/// Empty input is valid and normalizes to an empty string.
pub fn normalize(
    t: ValueType,
    raw: &str,
    max_len: Option<usize>,
    options: &[String],
) -> Result<String, String> {
    let s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() {
        return Ok(String::new());
    }
    let out = match t {
        ValueType::Text | ValueType::Signature => s,
        ValueType::Name => {
            if !s
                .chars()
                .all(|c| c.is_alphabetic() || matches!(c, ' ' | '-' | '\'' | '.'))
            {
                return Err("name contains digits or symbols".into());
            }
            s
        }
        ValueType::Date => normalize_date(&s)?,
        ValueType::Email => {
            let lower = s.replace(' ', "").to_ascii_lowercase();
            let (user, domain) = lower.split_once('@').ok_or("missing @")?;
            if user.is_empty()
                || !domain.contains('.')
                || domain.starts_with('.')
                || domain.ends_with('.')
                || domain.contains('@')
            {
                return Err("not an email address".into());
            }
            lower
        }
        ValueType::Phone => {
            let d = digits(&s);
            // International numbers are legal on the form but indistinguishable from a misread
            // extra digit, so only NANP numbers pass automatically.
            match d.len() {
                10 => d,
                11 if d.starts_with('1') => d[1..].to_owned(),
                _ => {
                    return Err(
                        "expected a 10-digit US number; verify international numbers".into(),
                    );
                }
            }
        }
        ValueType::Ssn => {
            let d = digits(&s);
            if d.len() != 9 || d.len() != s.chars().filter(|c| !matches!(c, '-' | ' ')).count() {
                return Err("expected 9 digits".into());
            }
            d
        }
        ValueType::ANumber => {
            let body = s.trim_start_matches(['A', 'a']).replace(['-', ' '], "");
            if !(7..=9).contains(&body.len()) || !body.chars().all(|c| c.is_ascii_digit()) {
                return Err("expected A-Number of 7 to 9 digits".into());
            }
            format!("{body:0>9}")
        }
        ValueType::UscisAccount => {
            let d = s.replace(['-', ' '], "");
            if d.len() != 12 || !d.chars().all(|c| c.is_ascii_digit()) {
                return Err("expected 12 digits".into());
            }
            d
        }
        ValueType::Zip => {
            let d = s.replace(['-', ' '], "");
            match d.len() {
                5 if d.chars().all(|c| c.is_ascii_digit()) => d,
                9 if d.chars().all(|c| c.is_ascii_digit()) => format!("{}-{}", &d[..5], &d[5..]),
                _ => return Err("expected 5 or 9 digit ZIP".into()),
            }
        }
        ValueType::State => {
            let u = s.replace('.', "").to_ascii_uppercase();
            if !STATES.contains(&u.as_str()) {
                return Err("not a USPS state code".into());
            }
            u
        }
        ValueType::Checkbox => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "x" | "checked" | "on" | "1" => "true".into(),
            "false" | "no" | "unchecked" | "off" | "0" => "false".into(),
            _ => return Err("expected true or false".into()),
        },
        ValueType::Choice => {
            if options.is_empty() {
                s
            } else {
                options
                    .iter()
                    .find(|o| o.eq_ignore_ascii_case(&s))
                    .cloned()
                    .ok_or_else(|| "not one of the allowed options".to_owned())?
            }
        }
    };
    if let Some(m) = max_len
        && out.chars().count() > m
    {
        return Err(format!("longer than {m} characters"));
    }
    Ok(out)
}

fn normalize_date(s: &str) -> Result<String, String> {
    let parts: Vec<&str> = s
        .split(['/', '-', '.'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let (m, d, y) = match parts.as_slice() {
        [m, d, y] => (m.to_string(), d.to_string(), y.to_string()),
        [one] if one.len() == 8 && one.chars().all(|c| c.is_ascii_digit()) => (
            one[0..2].to_owned(),
            one[2..4].to_owned(),
            one[4..8].to_owned(),
        ),
        _ => return Err("expected MM/DD/YYYY".into()),
    };
    let (m, d, y): (u8, u8, i32) = (
        m.parse().map_err(|_| "invalid month")?,
        d.parse().map_err(|_| "invalid day")?,
        y.parse().map_err(|_| "invalid year")?,
    );
    if y.to_string().len() != 4 || !(1900..=2100).contains(&y) {
        return Err("year must be 4 digits between 1900 and 2100".into());
    }
    let month = Month::try_from(m).map_err(|_| "invalid month")?;
    Date::from_calendar_date(y, month, d).map_err(|_| "no such calendar date")?;
    Ok(format!("{m:02}/{d:02}/{y}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference() {
        assert_eq!(
            infer("Enter E-mail Address.", "Email[0]", None),
            ValueType::Email
        );
        assert_eq!(
            infer(
                "Enter Mobile Telephone Number",
                "MobilePhoneNumber[0]",
                Some(15)
            ),
            ValueType::Phone
        );
        assert_eq!(
            infer(
                "1. A. Enter Family Name, Last Name.",
                "Line1a_FamilyName[0]",
                None
            ),
            ValueType::Name
        );
        assert_eq!(
            infer("Enter Date of Birth (mm/dd/yyyy).", "Line19_DOB[0]", None),
            ValueType::Date
        );
        assert_eq!(
            infer(
                "Enter U.S. Social Security Number, if any.",
                "Pt2Line11_SSN[0]",
                Some(9)
            ),
            ValueType::Ssn
        );
        assert_eq!(
            infer("Enter City or Town.", "CityTown[0]", None),
            ValueType::Text
        );
    }

    #[test]
    fn normalization() {
        let n = |t, s: &str| normalize(t, s, None, &[]);
        assert_eq!(n(ValueType::Date, "4/7/1988").unwrap(), "04/07/1988");
        assert_eq!(n(ValueType::Date, "04071988").unwrap(), "04/07/1988");
        assert!(n(ValueType::Date, "041171988").is_err());
        assert!(n(ValueType::Date, "02/30/2020").is_err());
        assert_eq!(n(ValueType::Phone, "(555) 012-3456").unwrap(), "5550123456");
        assert!(n(ValueType::Phone, "555-0123").is_err());
        assert_eq!(
            n(ValueType::Email, "Maria.G@Example.com").unwrap(),
            "maria.g@example.com"
        );
        assert!(n(ValueType::Email, "mariageexample com").is_err());
        assert_eq!(n(ValueType::ANumber, "A-12345678").unwrap(), "012345678");
        assert_eq!(n(ValueType::Zip, "123456789").unwrap(), "12345-6789");
        assert_eq!(n(ValueType::State, "ca").unwrap(), "CA");
        assert!(n(ValueType::Name, "Mar1a").is_err());
        assert_eq!(n(ValueType::Text, "  ").unwrap(), "");
        assert!(normalize(ValueType::Text, "abcdef", Some(3), &[]).is_err());
        let opts = vec!["Male".to_owned(), "Female".to_owned()];
        assert_eq!(
            normalize(ValueType::Choice, "female", None, &opts).unwrap(),
            "Female"
        );
    }

    #[test]
    fn digit_repair() {
        assert_eq!(repair_digits("5S5 O12-34l6"), "555012-3416");
    }
}
