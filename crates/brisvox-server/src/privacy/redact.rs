//! Best-effort PII redaction of transcript text.
//!
//! [`redact_pii`] replaces the identifiers a caller is most likely to read out in a phone call
//! with a fixed placeholder, so the text that reaches disk does not carry them:
//!
//! | what | rule | placeholder |
//! |---|---|---|
//! | email address | `local@domain.tld` | `[EMAIL]` |
//! | IBAN | two letters, two digits, 11 to 30 more characters (single spaces allowed), ISO 7064 mod 97 checksum | `[IBAN]` |
//! | Turkish national ID (TC kimlik no) | 11 digits, the two official check digits | `[ID]` |
//! | payment card number | 13 to 19 digits (spaces, dashes and dots allowed between groups), Luhn checksum | `[CARD]` |
//! | phone number | 7 to 15 digits (a leading `+`, spaces, dashes, dots and parentheses allowed) | `[PHONE]` |
//! | any other long number | 16 or more digits | `[NUMBER]` |
//!
//! It is a filter, not a guarantee: digits spoken as words ("five five five ...") and names,
//! addresses and health details are not recognised (`docs/PRIVACY.md`, "What redaction does not
//! do"). It errs on the side of redacting: a number of seven or more digits is replaced even
//! when it is not a phone number.
//!
//! The function is pure, allocates only when something is replaced, and never indexes: every
//! access is a checked `get` or an iterator.

use std::borrow::Cow;

use serde::Deserialize;

/// A kind of identifier the filter recognises (the `redact_kinds` of the `[recording]` table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PiiKind {
    /// Email addresses.
    Email,
    /// IBANs.
    Iban,
    /// Turkish national ID numbers.
    Id,
    /// Payment card numbers.
    Card,
    /// Phone numbers.
    Phone,
    /// Any other long digit run.
    Number,
}

impl PiiKind {
    /// Every kind.
    pub const ALL: [Self; 6] = [
        Self::Email,
        Self::Iban,
        Self::Id,
        Self::Card,
        Self::Phone,
        Self::Number,
    ];

    /// The name used in configuration (`email`, `iban`, `id`, `card`, `phone`, `number`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Iban => "iban",
            Self::Id => "id",
            Self::Card => "card",
            Self::Phone => "phone",
            Self::Number => "number",
        }
    }

    /// The kind called `name` (case-insensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(name.trim()))
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// A set of [`PiiKind`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiiKinds(u8);

impl PiiKinds {
    /// Every kind (the default).
    pub const ALL: Self = Self(0b11_1111);
    /// No kind.
    pub const NONE: Self = Self(0);

    /// The set of `kinds`.
    pub fn of(kinds: impl IntoIterator<Item = PiiKind>) -> Self {
        Self(kinds.into_iter().fold(0, |bits, k| bits | k.bit()))
    }

    /// Parses a comma-separated list like `email,iban,phone`.
    ///
    /// # Errors
    /// The first name that is not a kind.
    pub fn parse(list: &str) -> Result<Self, String> {
        let mut bits = 0;
        for name in list.split(',').filter(|n| !n.trim().is_empty()) {
            let kind = PiiKind::from_name(name).ok_or_else(|| name.trim().to_owned())?;
            bits |= kind.bit();
        }
        Ok(Self(bits))
    }

    /// Whether `kind` is in the set.
    pub const fn contains(self, kind: PiiKind) -> bool {
        self.0 & kind.bit() != 0
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl Default for PiiKinds {
    fn default() -> Self {
        Self::ALL
    }
}

/// Placeholder for an email address.
pub const EMAIL: &str = "[EMAIL]";
/// Placeholder for an IBAN.
pub const IBAN: &str = "[IBAN]";
/// Placeholder for a Turkish national ID number.
pub const ID: &str = "[ID]";
/// Placeholder for a payment card number.
pub const CARD: &str = "[CARD]";
/// Placeholder for a phone number.
pub const PHONE: &str = "[PHONE]";
/// Placeholder for any other long digit run.
pub const NUMBER: &str = "[NUMBER]";

/// Longest digit run the scanner looks at as one number; a longer run is cut here and the rest
/// scanned as another number (a bound on the work per run, not a limit on what is redacted).
const MAX_RUN_DIGITS: usize = 40;

/// `text` with emails, IBANs, national IDs, card numbers and phone numbers replaced by
/// placeholders (see the module docs). Borrows `text` when nothing matched.
pub fn redact_pii(text: &str) -> Cow<'_, str> {
    redact_pii_with(text, PiiKinds::ALL)
}

/// Like [`redact_pii`], for the kinds in `kinds` only.
pub fn redact_pii_with(text: &str, kinds: PiiKinds) -> Cow<'_, str> {
    let emails = if kinds.contains(PiiKind::Email) {
        redact_emails(text)
    } else {
        Cow::Borrowed(text)
    };
    let ibans = if kinds.contains(PiiKind::Iban) {
        match redact_ibans(&emails) {
            Cow::Borrowed(_) => emails,
            Cow::Owned(owned) => Cow::Owned(owned),
        }
    } else {
        emails
    };
    match redact_numbers(&ibans, kinds) {
        Cow::Borrowed(_) => ibans,
        Cow::Owned(owned) => Cow::Owned(owned),
    }
}

/// Characters of a pass over `text`: byte offset and character.
fn chars_of(text: &str) -> Vec<(usize, char)> {
    text.char_indices().collect()
}

/// Byte offset of the character at index `i` of `chars`, or the text's length past the end.
fn offset(chars: &[(usize, char)], i: usize, len: usize) -> usize {
    chars.get(i).map_or(len, |&(at, _)| at)
}

fn is_local_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_domain_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-')
}

/// Emails: expands around each `@`.
fn redact_emails(text: &str) -> Cow<'_, str> {
    if !text.contains('@') {
        return Cow::Borrowed(text);
    }
    let chars = chars_of(text);
    let mut out = String::new();
    let mut copied = 0; // byte offset up to which `text` is already in `out` or skipped
    let mut i = 0;
    let mut changed = false;
    while i < chars.len() {
        let Some(&(_, '@')) = chars.get(i) else {
            i += 1;
            continue;
        };
        let mut start = i;
        while start > 0 && chars.get(start - 1).is_some_and(|&(_, c)| is_local_char(c)) {
            start -= 1;
        }
        let mut end = i + 1;
        while chars.get(end).is_some_and(|&(_, c)| is_domain_char(c)) {
            end += 1;
        }
        // A trailing dot belongs to the sentence, not the address.
        while end > i + 1 && chars.get(end - 1).is_some_and(|&(_, c)| c == '.') {
            end -= 1;
        }
        let domain: String = chars
            .get(i + 1..end)
            .unwrap_or_default()
            .iter()
            .map(|&(_, c)| c)
            .collect();
        let tld_ok = domain
            .rsplit_once('.')
            .is_some_and(|(host, tld)| !host.is_empty() && tld.chars().count() >= 2);
        // `start` may reach back before bytes an earlier match already consumed.
        let start_at = offset(&chars, start, text.len()).max(copied);
        if start < i && tld_ok {
            out.push_str(text.get(copied..start_at).unwrap_or_default());
            out.push_str(EMAIL);
            copied = offset(&chars, end, text.len());
            changed = true;
            i = end;
        } else {
            i += 1;
        }
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    out.push_str(text.get(copied..).unwrap_or_default());
    Cow::Owned(out)
}

/// ISO 7064 mod 97-10 over an IBAN's characters (letters count as 10..=35), `true` when the
/// remainder is 1 after moving the first four characters to the end.
fn iban_checksum_ok(iban: &[char]) -> bool {
    if iban.len() < 5 {
        return false;
    }
    let mut remainder: u32 = 0;
    let rotated = iban.iter().skip(4).chain(iban.iter().take(4));
    for &c in rotated {
        let Some(value) = c.to_digit(36) else {
            return false;
        };
        // Digits add one decimal digit, letters two.
        remainder = if value < 10 {
            (remainder * 10 + value) % 97
        } else {
            (remainder * 100 + value) % 97
        };
    }
    remainder == 1
}

fn is_iban_start(chars: &[(usize, char)], i: usize) -> bool {
    let at = |k: usize| chars.get(i + k).map(|&(_, c)| c);
    let boundary = i == 0 || chars.get(i - 1).is_some_and(|&(_, c)| !c.is_alphanumeric());
    boundary
        && at(0).is_some_and(|c| c.is_ascii_alphabetic())
        && at(1).is_some_and(|c| c.is_ascii_alphabetic())
        && at(2).is_some_and(|c| c.is_ascii_digit())
        && at(3).is_some_and(|c| c.is_ascii_digit())
}

/// The longest valid IBAN starting at character index `i`: the index one past its last
/// character, if any.
fn iban_end(chars: &[(usize, char)], i: usize) -> Option<usize> {
    // Gather up to 34 alphanumerics, allowing a single space between groups.
    let mut alnum: Vec<char> = Vec::with_capacity(34);
    let mut ends: Vec<usize> = Vec::with_capacity(34); // index after each alphanumeric
    let mut k = i;
    while alnum.len() < 34 {
        match chars.get(k) {
            Some(&(_, c)) if c.is_ascii_alphanumeric() => {
                alnum.push(c.to_ascii_uppercase());
                k += 1;
                ends.push(k);
            }
            Some(&(_, ' '))
                if chars
                    .get(k + 1)
                    .is_some_and(|&(_, c)| c.is_ascii_alphanumeric()) =>
            {
                k += 1;
            }
            _ => break,
        }
    }
    for len in (15..=alnum.len()).rev() {
        let end = *ends.get(len - 1)?;
        let next_is_alnum = chars.get(end).is_some_and(|&(_, c)| c.is_alphanumeric());
        if next_is_alnum {
            continue;
        }
        if iban_checksum_ok(alnum.get(..len).unwrap_or_default()) {
            return Some(end);
        }
    }
    None
}

/// IBANs.
fn redact_ibans(text: &str) -> Cow<'_, str> {
    let chars = chars_of(text);
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    let mut changed = false;
    while i < chars.len() {
        if is_iban_start(&chars, i)
            && let Some(end) = iban_end(&chars, i)
        {
            out.push_str(
                text.get(copied..offset(&chars, i, text.len()))
                    .unwrap_or_default(),
            );
            out.push_str(IBAN);
            copied = offset(&chars, end, text.len());
            changed = true;
            i = end;
            continue;
        }
        i += 1;
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    out.push_str(text.get(copied..).unwrap_or_default());
    Cow::Owned(out)
}

/// Luhn checksum over decimal digits.
fn luhn_ok(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (n, &d) in digits.iter().rev().enumerate() {
        let mut v = u32::from(d);
        if n % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

/// The Turkish national ID rule: 11 digits, the first not zero, and the 10th and 11th digits
/// computed from the others.
fn tckn_ok(digits: &[u8]) -> bool {
    let d: Vec<i32> = digits.iter().map(|&d| i32::from(d)).collect();
    let (Some(&first), 11) = (d.first(), d.len()) else {
        return false;
    };
    if first == 0 {
        return false;
    }
    let at = |i: usize| d.get(i).copied().unwrap_or(0);
    let odd = at(0) + at(2) + at(4) + at(6) + at(8);
    let even = at(1) + at(3) + at(5) + at(7);
    let tenth = (odd * 7 - even).rem_euclid(10);
    let eleventh = (d.iter().take(10).sum::<i32>()).rem_euclid(10);
    tenth == at(9) && eleventh == at(10)
}

/// The placeholder for a digit run, `None` when it is too short to be an identifier or its kind
/// is not in `kinds`.
fn classify(digits: &[u8], kinds: PiiKinds) -> Option<&'static str> {
    let n = digits.len();
    let (kind, placeholder) = if n == 11 && tckn_ok(digits) {
        (PiiKind::Id, ID)
    } else if (13..=19).contains(&n) && luhn_ok(digits) {
        (PiiKind::Card, CARD)
    } else if (7..=15).contains(&n) {
        (PiiKind::Phone, PHONE)
    } else if n >= 16 {
        (PiiKind::Number, NUMBER)
    } else {
        return None;
    };
    kinds.contains(kind).then_some(placeholder)
}

/// A digit run's shape.
#[derive(Default)]
struct Run {
    /// The digits.
    digits: Vec<u8>,
    /// Length of each group of digits between separators.
    groups: Vec<usize>,
    /// The separators between the groups.
    seps: Vec<char>,
    /// Index one past the last digit in the character list.
    end: usize,
}

impl Run {
    /// An ISO (`2026-10-13`) or Turkish (`13.10.2026`) date: not an identifier.
    fn is_date(&self) -> bool {
        let same_sep = self.seps.len() == 2
            && self.seps.first() == self.seps.get(1)
            && matches!(self.seps.first(), Some('-' | '.'));
        same_sep && matches!(self.groups.as_slice(), [4, 2, 2] | [2, 2, 4])
    }
}

/// Scans a run starting at character index `begin` (a digit, or a `+`/`(` before one).
fn scan_run(chars: &[(usize, char)], begin: usize) -> Run {
    let mut run = Run {
        end: begin,
        ..Run::default()
    };
    let mut j = begin;
    let mut open = false;
    let mut group = 0usize;
    // The lead.
    match chars.get(j) {
        Some(&(_, '+')) => j += 1,
        Some(&(_, '(')) => {
            open = true;
            j += 1;
        }
        _ => {}
    }
    while let Some(&(_, ch)) = chars.get(j) {
        if ch.is_ascii_digit() {
            if run.digits.len() >= MAX_RUN_DIGITS {
                break;
            }
            run.digits
                .push(u8::try_from(ch.to_digit(10).unwrap_or(0)).unwrap_or(0));
            group += 1;
            j += 1;
            run.end = j;
            continue;
        }
        let next = chars.get(j + 1).map(|&(_, n)| n);
        match ch {
            // One separator between groups of digits.
            ' ' | '-' | '.'
                if group > 0 && next.is_some_and(|n| n.is_ascii_digit() || n == '(') =>
            {
                run.groups.push(group);
                run.seps.push(ch);
                group = 0;
                j += 1;
            }
            // An area code in parentheses after a separator ("+90 (532) 123").
            '(' if group == 0
                && !run.digits.is_empty()
                && !open
                && next.is_some_and(|n| n.is_ascii_digit()) =>
            {
                open = true;
                j += 1;
            }
            ')' if open && group > 0 => {
                open = false;
                j += 1;
                // The next character must be a separator or the number ends here.
                if chars.get(j).is_some_and(|&(_, c)| c.is_ascii_digit()) {
                    run.groups.push(group);
                    run.seps.push(' ');
                    group = 0;
                }
            }
            _ => break,
        }
    }
    if group > 0 {
        run.groups.push(group);
    }
    run
}

/// Numbers: maximal runs of digits joined by single separators.
fn redact_numbers(text: &str, kinds: PiiKinds) -> Cow<'_, str> {
    if kinds.contains(PiiKind::Id)
        || kinds.contains(PiiKind::Card)
        || kinds.contains(PiiKind::Phone)
        || kinds.contains(PiiKind::Number)
    {
        redact_numbers_of(text, kinds)
    } else {
        Cow::Borrowed(text)
    }
}

fn redact_numbers_of(text: &str, kinds: PiiKinds) -> Cow<'_, str> {
    let chars = chars_of(text);
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    let mut changed = false;
    while i < chars.len() {
        let Some(&(_, c)) = chars.get(i) else { break };
        let next_is_digit = chars.get(i + 1).is_some_and(|&(_, d)| d.is_ascii_digit());
        let starts = c.is_ascii_digit() || (matches!(c, '+' | '(') && next_is_digit);
        if !starts {
            i += 1;
            continue;
        }
        // A digit glued to a letter is part of a word or code ("A12"): skip the whole token.
        let glued_before = c.is_ascii_digit()
            && i > 0
            && chars.get(i - 1).is_some_and(|&(_, p)| p.is_alphabetic());
        if glued_before {
            while chars.get(i).is_some_and(|&(_, d)| d.is_alphanumeric()) {
                i += 1;
            }
            continue;
        }
        let run = scan_run(&chars, i);
        let glued_after = chars.get(run.end).is_some_and(|&(_, a)| a.is_alphabetic());
        let placeholder = if run.is_date() || glued_after {
            None
        } else {
            classify(&run.digits, kinds)
        };
        if let Some(placeholder) = placeholder {
            out.push_str(
                text.get(copied..offset(&chars, i, text.len()))
                    .unwrap_or_default(),
            );
            out.push_str(placeholder);
            copied = offset(&chars, run.end, text.len());
            changed = true;
        }
        // Always make progress, whatever the run turned out to be.
        i = run.end.max(i + 1);
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    out.push_str(text.get(copied..).unwrap_or_default());
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact_pii(s).into_owned()
    }

    #[test]
    fn text_without_identifiers_is_borrowed_unchanged() {
        for text in [
            "",
            "hello there, how can I help?",
            "my appointment is at 3 pm on the 14th",
            "room 204, floor 2",
            "version 1.2.3 released in 2026",
            "call me back in 15 minutes",
        ] {
            assert!(matches!(redact_pii(text), Cow::Borrowed(_)), "{text}");
            assert_eq!(r(text), text);
        }
    }

    #[test]
    fn emails_are_replaced_and_the_sentence_survives() {
        assert_eq!(
            r("write to jane.k+test@example.com, thanks"),
            "write to [EMAIL], thanks"
        );
        assert_eq!(r("it is a@b.io."), "it is [EMAIL].");
        assert_eq!(r("x@y.co and z@w.org"), "[EMAIL] and [EMAIL]");
        // Not addresses: nothing before the @, or no top-level domain.
        assert_eq!(r("meet @ noon"), "meet @ noon");
        assert_eq!(r("user@localhost"), "user@localhost");
        assert_eq!(r("@handle"), "@handle");
    }

    #[test]
    fn phone_numbers_in_common_shapes_are_replaced() {
        assert_eq!(r("call 0532 123 45 67 now"), "call [PHONE] now");
        assert_eq!(r("call +90 532 123 4567."), "call [PHONE].");
        assert_eq!(r("call (212) 555-0147 please"), "call [PHONE] please");
        assert_eq!(r("call 555.0147!"), "call [PHONE]!");
        assert_eq!(r("5321234567"), "[PHONE]");
    }

    #[test]
    fn short_numbers_and_codes_are_left_alone() {
        assert_eq!(r("order 12345"), "order 12345");
        assert_eq!(r("at 10:30 on 2026-10-13"), "at 10:30 on 2026-10-13");
        assert_eq!(r("code A1234567890"), "code A1234567890");
    }

    #[test]
    fn card_numbers_need_a_valid_luhn_checksum() {
        assert_eq!(r("card 4111 1111 1111 1111 ok"), "card [CARD] ok");
        assert_eq!(r("card 4111-1111-1111-1111"), "card [CARD]");
        assert_eq!(r("amex 378282246310005"), "amex [CARD]");
        // 16 digits that fail Luhn are still a long number.
        assert_eq!(r("4111 1111 1111 1112"), "[NUMBER]");
    }

    #[test]
    fn turkish_national_ids_follow_the_official_checksum() {
        // 10000000146 is the commonly used valid example number.
        assert_eq!(r("tc 10000000146 dir"), "tc [ID] dir");
        // The same digits with a broken check digit are an 11-digit number: a phone shape.
        assert_eq!(r("tc 10000000147 dir"), "tc [PHONE] dir");
    }

    #[test]
    fn ibans_are_replaced_with_and_without_spaces() {
        assert_eq!(
            r("iban TR33 0006 1005 1978 6457 8413 26 thanks"),
            "iban [IBAN] thanks"
        );
        assert_eq!(r("TR330006100519786457841326"), "[IBAN]");
        assert_eq!(r("DE89 3704 0044 0532 0130 00."), "[IBAN].");
        // A wrong checksum is not an IBAN.
        assert!(!r("TR35 0006 1005 1978 6457 8413 26").contains(IBAN));
    }

    #[test]
    fn several_kinds_in_one_text() {
        assert_eq!(
            r("mail a@b.io or call 0532 123 45 67, card 4111 1111 1111 1111"),
            "mail [EMAIL] or call [PHONE], card [CARD]"
        );
    }

    #[test]
    fn multibyte_text_around_matches_is_kept_whole() {
        assert_eq!(
            r("Merhaba, numaram 0532 123 45 67 \u{015f}\u{00e7}\u{011f}\u{00fc}"),
            "Merhaba, numaram [PHONE] \u{015f}\u{00e7}\u{011f}\u{00fc}"
        );
        assert_eq!(
            r("\u{4f60}\u{597d} x@y.io \u{4f60}\u{597d}"),
            "\u{4f60}\u{597d} [EMAIL] \u{4f60}\u{597d}"
        );
    }

    #[test]
    fn hostile_inputs_do_not_panic() {
        for text in [
            "@",
            "@@@@",
            "+",
            "(",
            "((((",
            "+++1",
            "1-",
            "1 -",
            "----",
            "TR",
            "TR33",
            "a@b.",
            ".@.",
            "00000000000000000000000000000000000000000000000000000000",
            "\u{0}",
            "\u{1f600}\u{1f600}@\u{1f600}.\u{1f600}\u{1f600}",
        ] {
            let _ = redact_pii(text);
        }
        let long = "9".repeat(10_000);
        let _ = redact_pii(&long);
        let spaced = "1 ".repeat(5_000);
        let _ = redact_pii(&spaced);
    }
    #[test]
    fn only_the_chosen_kinds_are_redacted() {
        let text = "mail jane@example.com or call +90 532 123 45 67";
        let only_email = redact_pii_with(text, PiiKinds::of([PiiKind::Email]));
        assert_eq!(only_email, "mail [EMAIL] or call +90 532 123 45 67");
        let only_phone = redact_pii_with(text, PiiKinds::of([PiiKind::Phone]));
        assert_eq!(only_phone, "mail jane@example.com or call [PHONE]");
        assert_eq!(redact_pii_with(text, PiiKinds::NONE), text);
        assert_eq!(redact_pii_with(text, PiiKinds::ALL), redact_pii(text));
        // A card number is not a phone number: with only `phone` selected it stays.
        let card = "card 4111 1111 1111 1111";
        assert_eq!(redact_pii_with(card, PiiKinds::of([PiiKind::Phone])), card);
        assert_eq!(
            redact_pii_with(card, PiiKinds::of([PiiKind::Card])),
            "card [CARD]"
        );
    }

    #[test]
    fn kind_lists_parse_and_name_the_bad_entry() {
        assert_eq!(
            PiiKinds::parse("Email, iban,PHONE").unwrap(),
            PiiKinds::of([PiiKind::Email, PiiKind::Iban, PiiKind::Phone])
        );
        assert_eq!(PiiKinds::parse("").unwrap(), PiiKinds::NONE);
        assert_eq!(PiiKinds::parse("email,name").unwrap_err(), "name");
        for kind in PiiKind::ALL {
            assert_eq!(PiiKind::from_name(kind.name()), Some(kind));
            assert!(PiiKinds::ALL.contains(kind));
        }
    }
}
