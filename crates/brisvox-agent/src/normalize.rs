//! TTS text normalisation for Turkish and English.
//!
//! LLM text is written for eyes: `%15`, `1.250.000`, `150 TL`, `12.03.2026`, `14:30`, `Dr.`.
//! A TTS engine reads such tokens badly or inconsistently (and a Turkish engine fed `3,5` may say
//! "three comma five" or "thirty-five"). [`Normalizer`] rewrites them into the words a person
//! would say, **before** the text goes to the TTS provider, so the spoken form is the same for
//! every provider. The input is the chunk the [`crate::segment::Segmenter`] emits; the
//! normaliser never changes the segmenter's behaviour (it is a separate, opt-in step applied per
//! chunk: [`Normalizer::normalize`], or [`Normalizer::normalize_in_place`] for a `String` the
//! caller owns).
//!
//! Everything here is a pure function of the text and the [`Language`]. It never panics, never
//! blocks, never reads a clock, and costs time linear in the input. A chunk with no digit,
//! currency symbol or code, percent sign or period is returned borrowed, without an allocation;
//! so is a chunk that comes out unchanged (a sentence-ending period costs one scan and a small
//! buffer, no more).
//!
//! # Cost contract for the caller
//!
//! The input is meant to be one segmenter chunk: [`INLINE_CHUNK_CHARS`] (256) chars or fewer, the
//! segmenter's default `max_chunk_chars` of 250. Measured in release on the dev machine
//! (2026-10-08): about 4 us for 250 plain chars, 8-10 us for 330 chars of mixed Turkish text
//! with numbers, dates, times and a phone number, 0.7 us for a 14-char chunk; about 40 us for a
//! 1,230-char mixed chunk; a pathological digit-dense text (a number every 6 chars) about 0.1 ms
//! per 1,250 chars. So an inline call on a tokio worker stays within the ~50 us rule for chunks
//! up to [`INLINE_CHUNK_CHARS`] even when digit-dense; a chunk the segmenter had to hard-cut
//! (up to 5 x `max_chunk_chars`) should be split again by the caller or normalised off the
//! worker. The function has no input cap of its own: it builds a `Vec<char>` (4 bytes per char)
//! and an output `String` per call, so memory is proportional to the input, and
//! `tests/normalize.rs` (`cost_grows_linearly_with_the_input`) checks that 16 times the input
//! costs well under 64 times the time.
//!
//! # What is rewritten
//!
//! Output is written with the real Turkish letters (`ç ğ ı ö ş ü`; capital `İ` is not needed
//! because the output words are lower case apart from month names and expanded titles).
//!
//! | Kind | Turkish | English |
//! |---|---|---|
//! | Integers | `1.250.000` -> `bir milyon iki yüz elli bin` (the dot is the thousands separator) | `1,250,000` -> `one million two hundred fifty thousand` |
//! | Decimals | `3,5` -> `üç virgül beş`, `0,05` -> `sıfır virgül sıfır beş`, `3,14159` -> `üç virgül bir dört bir beş dokuz` | `3.5` -> `three point five` |
//! | Negative | `-5` -> `eksi beş` | `-5` -> `minus five` |
//! | Ordinals | `3. sınıf` -> `üçüncü sınıf` (up to 4 digits, a period, a space, a lowercase word or a capitalised noun from a short list: `5. Cadde`, `2. Sınıf`) | `3rd` -> `third`, `1000th` -> `one thousandth` |
//! | Percent | `%15`, `15%` -> `yüzde on beş` | `15%` -> `fifteen percent` |
//! | Currency | `150 TL`, `150TL`, `TL 150`, `₺150` -> `yüz elli lira`, `150,50 TL` -> `yüz elli lira elli kuruş`, `20 $`, `US$20`, `USD 20` -> `yirmi dolar`, `€20` -> `yirmi avro`, `5 milyar $` -> `beş milyar dolar` | `$1.50` -> `one dollar fifty cents`, `£5` -> `five pounds` |
//! | Dates | `12.03.2026`, `2026-03-12`, `12/03/2026` -> `on iki Mart iki bin yirmi altı` | `March 12, 2026` -> `March twelfth, twenty twenty-six`; `in 1999` -> `in nineteen ninety-nine`; `the 1990s` -> `the nineteen nineties` |
//! | Times | `14:30` -> `on dört otuz`, `09:05` -> `dokuz sıfır beş`, `saat 14.30` and `10.30'da` -> `on dört otuz`, `on otuzda` | `9:05` -> `nine oh five`, `9:30 PM` -> `nine thirty p m` |
//! | Phones | `+90 532 123 45 67`, `0532 123 45 67`, `5321234567`, `444 12 34` -> digits in groups, `artı dokuz sıfır, beş üç iki, ...` | `555-123-4567` -> `five five five, one two three, ...` |
//! | Abbreviations | `Dr.` -> `Doktor`, `Prof.` -> `Profesör`, `vb.` -> `ve benzeri`, `vs.` -> `vesaire` | `Mr.` -> `Mister`, `etc.` -> `et cetera` |
//! | Suffixes (TR) | `15'inde` -> `on beşinde`, `3'üncü` -> `üçüncü`, `1990'larda` -> `bin dokuz yüz doksanlarda` | not applied |
//!
//! # Rules and limits (documented guesses, see `docs/TURKISH.md`)
//!
//! - **Separators follow the language.** Turkish: `.` thousands, `,` decimal; a lone `.` that is
//!   not followed by exactly three digits is read as a decimal point (`3.5`). English: `,`
//!   thousands, `.` decimal. A comma list without spaces (`1,2,3`) is ambiguous in Turkish and is
//!   read as `1,2` (a decimal) then `3`.
//! - **Fractions.** English reads the fraction digit by digit. Turkish reads a fraction of one or
//!   two digits as a number (`3,25` -> `üç virgül yirmi beş`, leading zeros spoken) and a longer
//!   one digit by digit.
//! - **Numbers glued to letters are left alone** (`A4`, `mp3`, `COVID19`); a number followed by
//!   letters gets a space (`5km` -> `beş km`). Numbers with a leading zero (`007`) or more than 18
//!   digits are read digit by digit.
//! - **Turkish apostrophe suffixes** after a number, date, time or percent are attached to the
//!   spoken form (the suffix was inflected for the spoken numeral). After a currency the suffix is
//!   dropped (`150 TL'ye` -> `yüz elli lira`): `TL'ye` is inflected for the letters "te-le", and
//!   re-inflecting "lira" correctly is out of scope.
//! - **Dates.** Turkish numeric dates are day-month-year. English `.` is day-month-year; English
//!   `/` and `-` are US month-day-year unless the first number is above 12. A two-digit year is
//!   not a date. `12 Mart 2026` needs no special rule (a cardinal, a month name, a cardinal).
//! - **Times** need a two-digit minute and `0..=23` hours (`1..=12` with `AM`/`PM`); `16:9` is
//!   not a time. The Turkish dotted form (`14.30`) is also a decimal point (`10.30` TL), so it is
//!   a time only after the word `saat` or with a time case suffix (`10.30'da`, `'dan`, `'a`,
//!   `'ye` ...); `Fiyat 12.50'ye` is read as a time, a documented ambiguity.
//! - **Phone numbers** are recognised by shape only: a `+` followed by 8-15 digits, a leading `0`
//!   with 10-11 digits (Turkish national format), in Turkish a 10-digit mobile number starting
//!   with `5` (bare, `532 123 45 67` or `532 123 4567`), in Turkish a 7-digit `444` call-centre
//!   number (`444 12 34`, `444 0 123`, `444 1234`), or 10-11 digits grouped like `3-3-4` or
//!   `1-3-3-4` (US). Any other bare digit run is a number (`1234567890` is a billion ...), as
//!   are 7-digit local numbers (`123 45 67`).
//! - **Currency** forms: `150 TL`, `150TL`, `TL 150`, `₺150`, `USD 20`, `US$20`, `5 milyar $`.
//!   A dollar sign after a letter other than `US` (`R$ 5`, `A$5`) is another currency and is
//!   left as written with its amount.
//! - **Space-grouped thousands** (`100 000 TL`, `1 000 000`, no-break space too): a first group of
//!   1-3 digits (no leading zero) followed by groups of exactly three digits is one number. Two
//!   quantities written side by side (`2 150 TL` for two items at 150) cannot be told apart and
//!   are read as 2150.
//! - **English years**: a four-digit number 1100-2099 after `in`, `since`, `by`, `until`,
//!   `till`, `from`, `during`, `before`, `after` (not before a unit, percent or currency) is read
//!   as a year (`in 1999` -> `in nineteen ninety-nine`); `1990s` / `1990's` is a decade. Other
//!   four-digit numbers are cardinals.
//! - **Dates** must exist on the calendar: `31.02.2026` and `31.04.2026` are not dates (read as
//!   dotted numbers), `29.02.2024` is (leap year).
//! - **Abbreviations** are small, case sensitive lists (`Sn.` is "Sayın", `sn.` is "saniye").
//!   Because many are also words or end a sentence (`Elm Dr.`, `Bul.` "find!", `Av.` "hunt"),
//!   titles (`Dr.`, `Prof.`, `Av.`, `Mr.`) are expanded only when a capitalised name follows;
//!   `Sok.`, `Bul.`, `Tel.` only after a capitalised word or a number (`Gazi Bul.`, `12. Sok.`)
//!   or before a digit (`Tel. 0212`); `approx.`, `e.g.`, `vs.` (English) only when a word follows.
//!   Otherwise the text is left as written. `yak.` is not an abbreviation here.
//! - **Capitalised ordinal nouns** (`5. Cadde`) are read as ordinals, but the segmenter cuts
//!   `digit. Capital` as a sentence end, so when a chunk boundary falls between the `5.` and
//!   `Cadde` the digit is read as a cardinal (`beş.`). See `docs/TURKISH.md`.

use std::borrow::Cow;

use crate::segment::Language;

/// Chunk length in chars up to which [`normalize`] fits the ~50 us per-call budget for CPU work
/// on a tokio worker, even for digit-dense text (see the module docs). The segmenter's default
/// `max_chunk_chars` is 250, so ordinary chunks are inside it.
pub const INLINE_CHUNK_CHARS: usize = 256;

/// Largest number of digits read as a cardinal; longer digit runs are read digit by digit.
const MAX_CARDINAL_DIGITS: usize = 18;

/// Rewrites written-form text into the words to speak, for one [`Language`].
///
/// Cheap to create and copy; holds no state between calls, so one value can serve a whole
/// session (or be shared across sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Normalizer {
    language: Language,
}

impl Normalizer {
    /// A normaliser for `language`.
    #[must_use]
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    /// The language of this normaliser.
    #[must_use]
    pub fn language(&self) -> Language {
        self.language
    }

    /// The spoken form of `text`, borrowed when nothing needed rewriting. See the module docs
    /// for the rules; never panics, on any input.
    #[must_use]
    pub fn normalize<'a>(&self, text: &'a str) -> Cow<'a, str> {
        normalize(text, self.language)
    }

    /// Replaces `chunk` with its spoken form (a no-op, without an allocation, when nothing
    /// needed rewriting).
    pub fn normalize_in_place(&self, chunk: &mut String) {
        if let Cow::Owned(spoken) = normalize(chunk, self.language) {
            *chunk = spoken;
        }
    }
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new(Language::default())
    }
}

/// The spoken form of `text` in `language`, borrowed when nothing needed rewriting.
///
/// Same as [`Normalizer::normalize`].
#[must_use]
pub fn normalize(text: &str, language: Language) -> Cow<'_, str> {
    if !needs_work(text) {
        return Cow::Borrowed(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let cx = Cx {
        chars: &chars,
        lang: language,
    };
    let mut out = String::with_capacity(text.len().saturating_add(text.len() / 2));
    let mut i = 0usize;
    while i < chars.len() {
        let next = step(&cx, i, &mut out);
        // Every branch consumes at least one char; the max keeps the loop finite regardless.
        i = next.max(i.saturating_add(1));
    }
    if out == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(out)
    }
}

/// Whether `text` holds anything that could be rewritten. Everything that is rewritten has a
/// digit, a symbol, a period (abbreviations) or a currency code in it.
fn needs_work(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_ascii_digit() || matches!(c, '.' | '%' | '₺' | '$' | '€' | '£'))
        || ["TL", "TRY", "USD", "EUR", "GBP"]
            .iter()
            .any(|code| text.contains(code))
}

// ---------------------------------------------------------------------------------------------
// Spelling numbers
// ---------------------------------------------------------------------------------------------

const TR_ONES: [&str; 10] = [
    "sıfır", "bir", "iki", "üç", "dört", "beş", "altı", "yedi", "sekiz", "dokuz",
];
const TR_TENS: [&str; 10] = [
    "", "on", "yirmi", "otuz", "kırk", "elli", "altmış", "yetmiş", "seksen", "doksan",
];
/// Scale words by group index (thousands group 1, millions 2, ...); `u64::MAX` has 7 groups.
const TR_SCALES: [&str; 7] = [
    "",
    "bin",
    "milyon",
    "milyar",
    "trilyon",
    "katrilyon",
    "kentilyon",
];

const EN_ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const EN_TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];
const EN_SCALES: [&str; 7] = [
    "",
    "thousand",
    "million",
    "billion",
    "trillion",
    "quadrillion",
    "quintillion",
];

/// `table[index]`, or the empty string when the index is out of range (never panics).
fn pick(table: &[&'static str], index: u64) -> &'static str {
    usize::try_from(index)
        .ok()
        .and_then(|i| table.get(i))
        .copied()
        .unwrap_or("")
}

/// The three-digit groups of `n`, least significant first.
fn groups_of(n: u64) -> [u64; 7] {
    let mut groups = [0u64; 7];
    let mut rest = n;
    for group in &mut groups {
        *group = rest % 1000;
        rest /= 1000;
    }
    groups
}

/// Turkish words for `n` (`1..=999` below the scale), appended to `out`.
fn tr_below_1000(n: u64, out: &mut Vec<&'static str>) {
    let (h, t, o) = (n / 100, (n / 10) % 10, n % 10);
    if h > 0 {
        if h > 1 {
            out.push(pick(&TR_ONES, h));
        }
        out.push("yüz");
    }
    if t > 0 {
        out.push(pick(&TR_TENS, t));
    }
    if o > 0 {
        out.push(pick(&TR_ONES, o));
    }
}

/// Turkish words of the cardinal `n`: `bir` is dropped before `yüz` and `bin` only.
fn tr_words(n: u64) -> Vec<&'static str> {
    let mut out = Vec::new();
    if n == 0 {
        out.push("sıfır");
        return out;
    }
    for (idx, &group) in groups_of(n).iter().enumerate().rev() {
        if group == 0 {
            continue;
        }
        if idx == 1 && group == 1 {
            out.push("bin");
            continue;
        }
        tr_below_1000(group, &mut out);
        if idx > 0 {
            out.push(pick(&TR_SCALES, idx as u64));
        }
    }
    out
}

/// English words (hyphenated tens) of `n` below 1000, `n > 0`, appended to `out`.
fn en_below_1000(n: u64, out: &mut Vec<String>) {
    let (h, rest) = (n / 100, n % 100);
    if h > 0 {
        out.push(format!("{} hundred", pick(&EN_ONES, h)));
    }
    let (t, o) = (rest / 10, rest % 10);
    match rest {
        0 => {}
        1..=19 => out.push(pick(&EN_ONES, rest).to_owned()),
        _ if o == 0 => out.push(pick(&EN_TENS, t).to_owned()),
        _ => out.push(format!("{}-{}", pick(&EN_TENS, t), pick(&EN_ONES, o))),
    }
}

/// English words of the cardinal `n`.
fn en_words(n: u64) -> Vec<String> {
    let mut out = Vec::new();
    if n == 0 {
        out.push("zero".to_owned());
        return out;
    }
    for (idx, &group) in groups_of(n).iter().enumerate().rev() {
        if group == 0 {
            continue;
        }
        en_below_1000(group, &mut out);
        if idx > 0 {
            out.push(pick(&EN_SCALES, idx as u64).to_owned());
        }
    }
    out
}

/// The cardinal number `n` in words: `1250000` -> `bir milyon iki yüz elli bin` (Turkish),
/// `one million two hundred fifty thousand` (English).
#[must_use]
pub fn spell_cardinal(n: u64, language: Language) -> String {
    match language {
        Language::Tr => tr_words(n).join(" "),
        Language::En => en_words(n).join(" "),
    }
}

/// Turkish ordinal form of the last word of a spelled cardinal (vowel harmony included).
fn tr_ordinal_word(word: &str) -> &str {
    match word {
        "sıfır" => "sıfırıncı",
        "bir" => "birinci",
        "iki" => "ikinci",
        "üç" => "üçüncü",
        "dört" => "dördüncü",
        "beş" => "beşinci",
        "altı" => "altıncı",
        "yedi" => "yedinci",
        "sekiz" => "sekizinci",
        "dokuz" => "dokuzuncu",
        "on" => "onuncu",
        "yirmi" => "yirminci",
        "otuz" => "otuzuncu",
        "kırk" => "kırkıncı",
        "elli" => "ellinci",
        "altmış" => "altmışıncı",
        "yetmiş" => "yetmişinci",
        "seksen" => "sekseninci",
        "doksan" => "doksanıncı",
        "yüz" => "yüzüncü",
        "bin" => "bininci",
        "milyon" => "milyonuncu",
        "milyar" => "milyarıncı",
        "trilyon" => "trilyonuncu",
        "katrilyon" => "katrilyonuncu",
        "kentilyon" => "kentilyonuncu",
        other => other,
    }
}

/// English ordinal form of a word that ends a spelled cardinal.
fn en_ordinal_word(word: &str) -> String {
    match word {
        "one" => "first".to_owned(),
        "two" => "second".to_owned(),
        "three" => "third".to_owned(),
        "five" => "fifth".to_owned(),
        "eight" => "eighth".to_owned(),
        "nine" => "ninth".to_owned(),
        "twelve" => "twelfth".to_owned(),
        w => match w.strip_suffix('y') {
            Some(stem) => format!("{stem}ieth"),
            None => format!("{w}th"),
        },
    }
}

/// The ordinal number `n` in words: `3` -> `üçüncü` (Turkish), `third` (English),
/// `21` -> `yirmi birinci`, `twenty-first`.
#[must_use]
pub fn spell_ordinal(n: u64, language: Language) -> String {
    match language {
        Language::Tr => {
            let mut words = tr_words(n);
            let last = words.pop().unwrap_or("sıfır");
            words.push(tr_ordinal_word(last));
            words.join(" ")
        }
        Language::En => {
            let mut words = en_words(n);
            let last = words.pop().unwrap_or_default();
            // The last word may be hyphenated ("twenty-one"): only its final part changes.
            let spoken = match last.rsplit_once('-') {
                Some((head, tail)) => format!("{head}-{}", en_ordinal_word(tail)),
                None => en_ordinal_word(&last),
            };
            words.push(spoken);
            words.join(" ")
        }
    }
}

/// The digits of `digits` read one by one (`305` -> `üç sıfır beş`); other characters are
/// skipped.
#[must_use]
pub fn spell_digits(digits: &str, language: Language) -> String {
    let table: &[&str] = match language {
        Language::Tr => &TR_ONES,
        Language::En => &EN_ONES,
    };
    let mut out = String::new();
    for d in digits.chars().filter_map(|c| c.to_digit(10)) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(pick(table, u64::from(d)));
    }
    out
}

/// English reading of a year: `2026` -> `twenty twenty-six`, `1905` -> `nineteen oh five`,
/// `2005` -> `two thousand five`.
fn year_en(year: u64) -> String {
    if !(1100..=9999).contains(&year) || (2000..=2009).contains(&year) {
        return spell_cardinal(year, Language::En);
    }
    let (hi, lo) = (year / 100, year % 100);
    let hi_words = spell_cardinal(hi, Language::En);
    match lo {
        0 => format!("{hi_words} hundred"),
        1..=9 => format!("{hi_words} oh {}", spell_cardinal(lo, Language::En)),
        _ => format!("{hi_words} {}", spell_cardinal(lo, Language::En)),
    }
}

// ---------------------------------------------------------------------------------------------
// Scanning helpers
// ---------------------------------------------------------------------------------------------

/// The text being rewritten, as chars, with the language.
struct Cx<'a> {
    chars: &'a [char],
    lang: Language,
}

impl Cx<'_> {
    fn ch(&self, i: usize) -> Option<char> {
        self.chars.get(i).copied()
    }

    fn is_tr(&self) -> bool {
        self.lang == Language::Tr
    }

    /// End of the run of ASCII digits starting at `i` (`i` itself when there is none).
    fn digits_end(&self, mut i: usize) -> usize {
        while self.ch(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        i
    }

    /// The chars `a..b` as a string (empty when out of range).
    fn text(&self, a: usize, b: usize) -> String {
        self.chars
            .get(a..b)
            .map(|s| s.iter().collect())
            .unwrap_or_default()
    }

    /// The value of the digits `a..b`, `None` when empty or too large.
    fn value(&self, a: usize, b: usize) -> Option<u64> {
        if b <= a || b - a > MAX_CARDINAL_DIGITS {
            return None;
        }
        self.chars.get(a..b)?.iter().try_fold(0u64, |acc, c| {
            acc.checked_mul(10)?.checked_add(u64::from(c.to_digit(10)?))
        })
    }

    /// Whether the text at `i` starts with `pattern`.
    fn starts_with(&self, i: usize, pattern: &str) -> bool {
        pattern
            .chars()
            .enumerate()
            .all(|(k, p)| self.ch(i + k) == Some(p))
    }

    /// Whether the char before `i` is a digit joined to this position by `.` `,` `/` `:`
    /// (the middle of a longer numeric token such as `1.2.3.4`).
    fn glued_to_number(&self, i: usize) -> bool {
        let Some(prev) = i.checked_sub(1).and_then(|p| self.ch(p)) else {
            return false;
        };
        matches!(prev, '.' | ',' | '/' | ':')
            && i.checked_sub(2)
                .and_then(|p| self.ch(p))
                .is_some_and(|c| c.is_ascii_digit())
    }

    /// Whether the numeric token ending at `end` continues (`.5`, `,5`, `/5`, `-5`, `:5`) or runs
    /// into letters, which makes it not a standalone date, time or phone number.
    fn token_continues(&self, end: usize) -> bool {
        self.token_continues_after(end, '-')
    }

    /// Like [`Cx::token_continues`] for a token that was written with the separator `sep`. A
    /// `-` followed by a digit only continues a token that itself uses `-` (`12-03-2026-5`); after
    /// a `:`, `.` or `/` token it is the dash of a range (`09:00-18:00`, `12.03.2026-15.03.2026`),
    /// so the first half is still a standalone time or date.
    fn token_continues_after(&self, end: usize, sep: char) -> bool {
        match self.ch(end) {
            Some(c) if c.is_alphanumeric() => true,
            Some('-') if sep != '-' => false,
            Some('.' | ',' | '/' | '-' | ':') => {
                self.ch(end + 1).is_some_and(|c| c.is_ascii_digit())
            }
            _ => false,
        }
    }

    /// A Turkish apostrophe suffix at `at` (`'inde`, `’ler`): its letters and the end index.
    fn suffix_at(&self, at: usize) -> Option<(String, usize)> {
        if !self.is_tr() || !matches!(self.ch(at)?, '\'' | '’') {
            return None;
        }
        let start = at + 1;
        let mut end = start;
        while self.ch(end).is_some_and(char::is_alphabetic) {
            end += 1;
        }
        if end == start || end - start > 12 {
            return None;
        }
        Some((self.text(start, end), end))
    }

    /// Appends `spoken` to `out` and the suffix that follows `end`; a letter that follows
    /// directly (a unit: `5km`) is separated by a space. Returns the index after everything
    /// consumed.
    fn emit(&self, out: &mut String, spoken: &str, end: usize) -> usize {
        out.push_str(spoken);
        let mut end = end;
        if let Some((suffix, after)) = self.suffix_at(end) {
            out.push_str(&suffix);
            end = after;
        }
        if self.ch(end).is_some_and(char::is_alphabetic) {
            out.push(' ');
        }
        end
    }

    /// As [`Self::emit`] for a currency: the apostrophe suffix is consumed and dropped.
    fn emit_dropping_suffix(&self, out: &mut String, spoken: &str, end: usize) -> usize {
        out.push_str(spoken);
        let mut end = end;
        if let Some((_, after)) = self.suffix_at(end) {
            end = after;
        }
        if self.ch(end).is_some_and(char::is_alphabetic) {
            out.push(' ');
        }
        end
    }
}

/// A number token: integer digits, optional fraction digits, and where it ends.
struct Num {
    int: String,
    frac: Option<String>,
    end: usize,
    /// No separator at all (a plain digit run).
    plain: bool,
}

/// Scans the number at `i` (which must be a digit) by the separator rules of the language.
fn scan_number(cx: &Cx<'_>, i: usize) -> Option<Num> {
    let d0_end = cx.digits_end(i);
    if d0_end == i {
        return None;
    }
    let d0 = cx.text(i, d0_end);
    let (thousands, decimal) = if cx.is_tr() { ('.', ',') } else { (',', '.') };
    let mut int = d0.clone();
    let mut frac: Option<String> = None;
    let mut grouped = false;
    let mut end = d0_end;
    // Thousands grouped by a single space or no-break space (`100 000`, `1 000 000`): a first
    // group of 1-3 digits (no leading zero) and groups of exactly three digits.
    if d0.len() <= 3 && !d0.starts_with('0') {
        while cx.ch(end).is_some_and(is_group_space) {
            let group_end = cx.digits_end(end + 1);
            if group_end - (end + 1) != 3 {
                break;
            }
            int.push_str(&cx.text(end + 1, group_end));
            grouped = true;
            end = group_end;
        }
    }
    loop {
        if frac.is_some() {
            break;
        }
        let Some(sep) = cx.ch(end).filter(|c| matches!(c, '.' | ',')) else {
            break;
        };
        let group_start = end + 1;
        let group_end = cx.digits_end(group_start);
        if group_end == group_start {
            break;
        }
        let group = cx.text(group_start, group_end);
        if sep == thousands {
            let first_ok = d0.len() <= 3 && !d0.starts_with('0');
            if group.len() == 3 && (grouped || first_ok) {
                int.push_str(&group);
                grouped = true;
                end = group_end;
            } else if cx.is_tr() && sep == '.' && !grouped {
                // A lone period that is not a thousands group is a decimal point (`3.5`).
                frac = Some(group);
                end = group_end;
            } else {
                break;
            }
        } else if sep == decimal {
            frac = Some(group);
            end = group_end;
        } else {
            break;
        }
    }
    Some(Num {
        plain: end == d0_end,
        int,
        frac,
        end,
    })
}

/// A space that separates thousands groups: space, no-break space, narrow no-break space.
fn is_group_space(c: char) -> bool {
    matches!(c, ' ' | '\u{a0}' | '\u{202f}')
}

/// The integer digits spoken: a cardinal, or digit by digit when that is the better reading.
fn int_words(int: &str, lang: Language) -> String {
    let digit_by_digit = int.len() > MAX_CARDINAL_DIGITS || (int.len() > 1 && int.starts_with('0'));
    match int.parse::<u64>() {
        Ok(n) if !digit_by_digit => spell_cardinal(n, lang),
        _ => spell_digits(int, lang),
    }
}

/// The fraction digits spoken (see the module docs).
fn frac_words(frac: &str, lang: Language) -> String {
    if lang == Language::Tr && frac.len() <= 2 {
        let zeros = frac.chars().take_while(|c| *c == '0').count();
        let mut parts: Vec<String> = vec!["sıfır".to_owned(); zeros];
        if let Some(n) = frac.get(zeros..).and_then(|r| r.parse::<u64>().ok()) {
            parts.push(spell_cardinal(n, lang));
        }
        parts.join(" ")
    } else {
        spell_digits(frac, lang)
    }
}

/// The words of a whole number token, decimal point included.
fn num_words(num: &Num, lang: Language) -> String {
    let int = int_words(&num.int, lang);
    match &num.frac {
        None => int,
        Some(frac) => {
            let point = if lang == Language::Tr {
                "virgül"
            } else {
                "point"
            };
            format!("{int} {point} {}", frac_words(frac, lang))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The scanner
// ---------------------------------------------------------------------------------------------

/// Handles the text at `i`: appends the output for the construct starting there and returns the
/// index after it.
fn step(cx: &Cx<'_>, i: usize, out: &mut String) -> usize {
    let Some(c) = cx.ch(i) else {
        return i + 1;
    };
    let prev = i.checked_sub(1).and_then(|p| cx.ch(p));
    let after_word = prev.is_some_and(char::is_alphanumeric);
    if c.is_alphabetic() {
        if !after_word
            && let Some(end) = abbreviation(cx, i, out)
                .or_else(|| us_dollar(cx, i, out))
                .or_else(|| currency_code_alone(cx, i, out))
                .or_else(|| month_day(cx, i, out))
        {
            return end;
        }
        return copy_word(cx, i, out);
    }
    if c.is_ascii_digit() {
        if after_word {
            return copy_word(cx, i, out);
        }
        return digit_construct(cx, i, out);
    }
    match c {
        '+' | '(' => phone(cx, i, out).unwrap_or_else(|| copy_char(c, i, out)),
        '-' | '−'
            if cx.ch(i + 1).is_some_and(|d| d.is_ascii_digit())
                && prev.is_none_or(|p| p.is_whitespace() || p == '(') =>
        {
            out.push_str(if cx.is_tr() { "eksi " } else { "minus " });
            i + 1
        }
        '%' => percent_before(cx, i, out).unwrap_or_else(|| copy_char(c, i, out)),
        '₺' | '$' | '€' | '£' => {
            currency_before(cx, i, out).unwrap_or_else(|| copy_char(c, i, out))
        }
        _ => copy_char(c, i, out),
    }
}

fn copy_char(c: char, i: usize, out: &mut String) -> usize {
    out.push(c);
    i + 1
}

/// Copies the alphanumeric run starting at `i` unchanged. A run that holds a digit continues
/// through `.digit` pieces, so identifiers such as `v2.0.1` stay as they are.
fn copy_word(cx: &Cx<'_>, i: usize, out: &mut String) -> usize {
    let mut end = i;
    let mut has_digit = false;
    while let Some(c) = cx.ch(end) {
        if c.is_alphanumeric() || c == '_' {
            has_digit |= c.is_ascii_digit();
            out.push(c);
            end += 1;
        } else if c == '.' && has_digit && cx.ch(end + 1).is_some_and(|d| d.is_ascii_digit()) {
            out.push(c);
            end += 1;
        } else {
            break;
        }
    }
    end.max(i + 1)
}

/// A construct that starts with a digit (not glued to a letter before it).
fn digit_construct(cx: &Cx<'_>, i: usize, out: &mut String) -> usize {
    let glued = cx.glued_to_number(i);
    if !glued
        && let Some(end) = numeric_date(cx, i, out)
            .or_else(|| time(cx, i, out))
            .or_else(|| dot_time(cx, i, out))
            .or_else(|| phone(cx, i, out))
    {
        return end;
    }
    if cx.lang == Language::En
        && let Some(end) = day_month(cx, i, out).or_else(|| english_year(cx, i, out))
    {
        return end;
    }
    if let Some(end) = dotted_sequence(cx, i, out) {
        return end;
    }
    let Some(num) = scan_number(cx, i) else {
        return copy_word(cx, i, out);
    };
    if let Some(end) = currency_after(cx, &num, out) {
        return end;
    }
    if let Some(end) = percent_after(cx, &num, out) {
        return end;
    }
    if let Some(end) = ordinal(cx, &num, out) {
        return end;
    }
    let spoken = num_words(&num, cx.lang);
    cx.emit(out, &spoken, num.end)
}

/// Three or more digit groups joined by periods that are not a thousands-grouped number
/// (`192.168.1.1`, `2.0.1`): each group is read as a number, joined with "nokta" / "dot". In
/// Turkish `1.250.000` is a grouped number and is left to the number rule.
fn dotted_sequence(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    let mut groups: Vec<String> = Vec::new();
    let mut j = i;
    loop {
        let stop = cx.digits_end(j);
        if stop == j {
            break;
        }
        groups.push(cx.text(j, stop));
        j = stop;
        if cx.ch(j) == Some('.') && cx.ch(j + 1).is_some_and(|d| d.is_ascii_digit()) {
            j += 1;
        } else {
            break;
        }
    }
    if groups.len() < 3 {
        return None;
    }
    let grouped_thousands = groups
        .first()
        .is_some_and(|g| g.len() <= 3 && !g.starts_with('0'))
        && groups.iter().skip(1).all(|g| g.len() == 3);
    if grouped_thousands && cx.is_tr() {
        return None;
    }
    let dot = if cx.is_tr() { " nokta " } else { " dot " };
    let spoken: Vec<String> = groups.iter().map(|g| int_words(g, cx.lang)).collect();
    Some(cx.emit(out, &spoken.join(dot), j))
}

/// Words after which a four-digit number is read as a year (English).
const YEAR_WORDS: [&str; 9] = [
    "in", "since", "by", "until", "till", "from", "during", "before", "after",
];

/// `the 1990s` -> `the nineteen nineties`, `1900s` -> `nineteen hundreds`, `2000s` ->
/// `two thousands`.
fn decade_en(year: u64) -> String {
    let hi = year / 100;
    let tens = (year % 100) / 10;
    if year.is_multiple_of(100) {
        return if hi == 20 {
            "two thousands".to_owned()
        } else {
            format!("{} hundreds", spell_cardinal(hi, Language::En))
        };
    }
    let plural = match tens {
        1 => "tens".to_owned(),
        t => {
            let word = pick(&EN_TENS, t);
            match word.strip_suffix('y') {
                Some(stem) => format!("{stem}ies"),
                None => format!("{word}s"),
            }
        }
    };
    format!("{} {plural}", spell_cardinal(hi, Language::En))
}

/// English years outside a full date: `in 1999` -> `in nineteen ninety-nine`, `the 1990s` ->
/// `the nineteen nineties`. A four-digit number from 1100 to 2099 is a year after `in`, `since`,
/// `by`, `until`, `till`, `from`, `during`, `before` or `after` (and not before a unit,
/// percent sign or currency), or when an `s` follows and it ends in 0 (a decade).
fn english_year(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    if cx.lang != Language::En {
        return None;
    }
    let end = cx.digits_end(i);
    if end - i != 4 {
        return None;
    }
    let year = cx.value(i, end)?;
    if !(1100..=2099).contains(&year) {
        return None;
    }
    // `1990s`, `1990's`
    let s_at = if matches!(cx.ch(end), Some('\'' | '’')) {
        end + 1
    } else {
        end
    };
    if year.is_multiple_of(10)
        && cx.ch(s_at) == Some('s')
        && !cx.ch(s_at + 1).is_some_and(char::is_alphanumeric)
    {
        out.push_str(&decade_en(year));
        return Some(s_at + 1);
    }
    if cx.token_continues(end) || cx.ch(end).is_some_and(char::is_alphanumeric) {
        return None;
    }
    if !YEAR_WORDS.iter().any(|w| word_before_is(cx, i, w)) {
        return None;
    }
    let next = if cx.ch(end) == Some(' ') {
        end + 1
    } else {
        end
    };
    if cx.ch(next) == Some('%')
        || cx.ch(next).and_then(Currency::from_symbol).is_some()
        || code_at(cx, next).is_some()
    {
        return None;
    }
    out.push_str(&year_en(year));
    Some(end)
}

// ---------------------------------------------------------------------------------------------
// Ordinals and percent
// ---------------------------------------------------------------------------------------------

/// `3. sınıf` (Turkish), `3rd` (English).
fn ordinal(cx: &Cx<'_>, num: &Num, out: &mut String) -> Option<usize> {
    // Turkish: up to four digits (`2026. yılında`); English: any suffixed number.
    let max_digits = if cx.is_tr() { 4 } else { MAX_CARDINAL_DIGITS };
    if !num.plain || num.int.len() > max_digits {
        return None;
    }
    let n: u64 = num.int.parse().ok()?;
    let end = num.end;
    match cx.lang {
        Language::Tr => {
            if cx.ch(end)? != '.' || cx.ch(end + 1)? != ' ' {
                return None;
            }
            let mut next = end + 2;
            while cx.ch(next) == Some(' ') {
                next += 1;
            }
            // A lowercase word follows, an abbreviated noun (`12. Sok.`) or a capitalised noun
            // that takes an ordinal (`5. Cadde`, `2. Sınıf`).
            if !cx.ch(next)?.is_lowercase()
                && find_abbreviation(cx, next).is_none()
                && !ordinal_noun_at(cx, next)
            {
                return None;
            }
            out.push_str(&spell_ordinal(n, cx.lang));
            Some(end + 1)
        }
        Language::En => {
            let suffix = cx.text(end, end + 2).to_ascii_lowercase();
            if !matches!(suffix.as_str(), "st" | "nd" | "rd" | "th")
                || cx.ch(end + 2).is_some_and(char::is_alphanumeric)
            {
                return None;
            }
            out.push_str(&spell_ordinal(n, cx.lang));
            Some(end + 2)
        }
    }
}

/// Capitalised nouns that follow an ordinal in an address, school or sports context
/// (`5. Cadde`, `3. Kat`, `2. Sınıf`, `1. Lig`). Other capitalised words after `N. ` are a new
/// sentence (`Ben 3. Sonra gittik.`). Compared after lower-casing the Turkish way.
const ORDINAL_NOUNS: [&str; 17] = [
    "cadde", "sokak", "kat", "sınıf", "bölge", "bölüm", "lig", "sezon", "daire", "blok", "sayfa",
    "madde", "dönem", "yüzyıl", "asır", "etap", "kısım",
];

/// Whether a word of `ORDINAL_NOUNS` (any case) starts at `i` and ends there.
fn ordinal_noun_at(cx: &Cx<'_>, i: usize) -> bool {
    let mut end = i;
    while cx.ch(end).is_some_and(char::is_alphabetic) {
        end += 1;
        if end - i > 12 {
            return false;
        }
    }
    let word: String = cx
        .text(i, end)
        .chars()
        .flat_map(|c| match c {
            'İ' => 'i'.to_lowercase().collect::<Vec<_>>(),
            'I' => 'ı'.to_lowercase().collect::<Vec<_>>(),
            c => c.to_lowercase().collect::<Vec<_>>(),
        })
        .collect();
    ORDINAL_NOUNS.contains(&word.as_str())
}

/// The spoken percent: `yüzde on beş` / `fifteen percent`.
fn percent_words(spoken: &str, lang: Language) -> String {
    match lang {
        Language::Tr => format!("yüzde {spoken}"),
        Language::En => format!("{spoken} percent"),
    }
}

/// `%15` at `i` (the percent sign).
fn percent_before(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    if !cx.ch(i + 1)?.is_ascii_digit() {
        return None;
    }
    let num = scan_number(cx, i + 1)?;
    let spoken = percent_words(&num_words(&num, cx.lang), cx.lang);
    Some(cx.emit(out, &spoken, num.end))
}

/// `15%` / `15 %` right after a number token.
fn percent_after(cx: &Cx<'_>, num: &Num, out: &mut String) -> Option<usize> {
    let mut j = num.end;
    if cx.ch(j) == Some(' ') {
        j += 1;
    }
    if cx.ch(j)? != '%' {
        return None;
    }
    let spoken = percent_words(&num_words(num, cx.lang), cx.lang);
    Some(cx.emit(out, &spoken, j + 1))
}

// ---------------------------------------------------------------------------------------------
// Currency
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Currency {
    Try,
    Usd,
    Eur,
    Gbp,
}

impl Currency {
    fn from_symbol(c: char) -> Option<Self> {
        match c {
            '₺' => Some(Self::Try),
            '$' => Some(Self::Usd),
            '€' => Some(Self::Eur),
            '£' => Some(Self::Gbp),
            _ => None,
        }
    }

    /// The major unit; `plural` only matters in English.
    fn major(self, lang: Language, plural: bool) -> &'static str {
        match (lang, self, plural) {
            (Language::Tr, Self::Try, _) => "lira",
            (Language::Tr, Self::Usd, _) => "dolar",
            (Language::Tr, Self::Eur, _) => "avro",
            (Language::Tr, Self::Gbp, _) => "sterlin",
            (Language::En, Self::Try, _) => "Turkish lira",
            (Language::En, Self::Usd, false) => "dollar",
            (Language::En, Self::Usd, true) => "dollars",
            (Language::En, Self::Eur, false) => "euro",
            (Language::En, Self::Eur, true) => "euros",
            (Language::En, Self::Gbp, false) => "pound",
            (Language::En, Self::Gbp, true) => "pounds",
        }
    }

    /// The minor unit (a hundredth); `plural` only matters in English.
    fn minor(self, lang: Language, plural: bool) -> &'static str {
        match (lang, self, plural) {
            (Language::Tr, Self::Try, _) => "kuruş",
            (Language::Tr, Self::Gbp, _) => "peni",
            (Language::Tr, _, _) => "sent",
            (Language::En, Self::Try, _) => "kuruş",
            (Language::En, Self::Gbp, false) => "penny",
            (Language::En, Self::Gbp, true) => "pence",
            (Language::En, _, false) => "cent",
            (Language::En, _, true) => "cents",
        }
    }
}

/// The currency code at `i` (`TL`, `TRY`, `USD`, `EUR`, `GBP`) and its length, when it is a whole
/// word.
fn code_at(cx: &Cx<'_>, i: usize) -> Option<(Currency, usize)> {
    const CODES: [(&str, Currency); 5] = [
        ("TRY", Currency::Try),
        ("USD", Currency::Usd),
        ("EUR", Currency::Eur),
        ("GBP", Currency::Gbp),
        ("TL", Currency::Try),
    ];
    let (len, cur) = CODES
        .iter()
        .find(|(code, _)| cx.starts_with(i, code))
        .map(|(code, cur)| (code.chars().count(), *cur))?;
    // A digit directly before is fine (`150TL`); a letter is not (`ABTL`).
    let before_ok = i
        .checked_sub(1)
        .and_then(|p| cx.ch(p))
        .is_none_or(|p| !p.is_alphabetic());
    let after_ok = cx.ch(i + len).is_none_or(|n| !n.is_alphanumeric());
    (before_ok && after_ok).then_some((cur, len))
}

/// A currency code in front of its amount (`USD 20`, `TL 150`), or alone after a scale word
/// (`5 milyon TL`: the unit alone).
fn currency_code_alone(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    let (cur, len) = code_at(cx, i)?;
    if let Some(start) = amount_start(cx, i + len)
        && let Some(end) = currency_amount(cx, cur, start, out)
    {
        return Some(end);
    }
    Some(cx.emit_dropping_suffix(out, cur.major(cx.lang, true), i + len))
}

/// Where the digits of an amount start at `at`, past at most one space (`USD 20`, `$ 5`).
fn amount_start(cx: &Cx<'_>, at: usize) -> Option<usize> {
    let start = if matches!(cx.ch(at), Some(' ' | '\u{a0}')) {
        at + 1
    } else {
        at
    };
    cx.ch(start)
        .is_some_and(|c| c.is_ascii_digit())
        .then_some(start)
}

/// `US$20`, `US$ 5` (the `U` at `i`): the US dollar with its country prefix.
fn us_dollar(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    if !cx.starts_with(i, "US$") {
        return None;
    }
    let start = amount_start(cx, i + 3)?;
    currency_amount(cx, Currency::Usd, start, out)
}

/// A dollar sign that follows a letter (`R$ 5`, `A$5`, `C$5`) is another dollar-like currency
/// (real, Australian, Canadian ...). It is not spoken as US dollars: the symbol and its amount
/// are copied unchanged.
fn foreign_dollar(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    if cx.ch(i)? != '$' || !i.checked_sub(1).and_then(|p| cx.ch(p))?.is_alphabetic() {
        return None;
    }
    let start = amount_start(cx, i + 1)?;
    out.push_str(&cx.text(i, start));
    Some(copy_word(cx, start, out))
}

/// The amount in words with its units: `150,50 TL` -> `yüz elli lira elli kuruş`.
fn amount_words(num: &Num, cur: Currency, lang: Language) -> String {
    let decimal_reading = || format!("{} {}", num_words(num, lang), cur.major(lang, true));
    let leading_zero = num.int.len() > 1 && num.int.starts_with('0');
    let Some(major) = num
        .int
        .parse::<u64>()
        .ok()
        .filter(|_| num.int.len() <= MAX_CARDINAL_DIGITS && !leading_zero)
    else {
        return decimal_reading();
    };
    let minor = match num.frac.as_deref() {
        None => 0,
        Some(f) if f.len() == 1 => f.parse::<u64>().unwrap_or(0) * 10,
        Some(f) if f.len() == 2 => f.parse::<u64>().unwrap_or(0),
        Some(_) => return decimal_reading(),
    };
    let mut parts = Vec::new();
    if major > 0 || minor == 0 {
        parts.push(spell_cardinal(major, lang));
        parts.push(cur.major(lang, major != 1).to_owned());
    }
    if minor > 0 {
        parts.push(spell_cardinal(minor, lang));
        parts.push(cur.minor(lang, minor != 1).to_owned());
    }
    parts.join(" ")
}

/// A scale word after the number (`1,5 milyon`, `1.5 million`): the word and where it ends.
fn scale_after(cx: &Cx<'_>, end: usize) -> Option<(String, usize)> {
    let mut start = end;
    if cx.ch(start) == Some(' ') {
        start += 1;
    }
    let mut stop = start;
    while cx.ch(stop).is_some_and(char::is_alphabetic) {
        stop += 1;
    }
    let word = cx.text(start, stop);
    let known: &[&str] = if cx.is_tr() {
        &["bin", "milyon", "milyar", "trilyon"]
    } else {
        &["thousand", "million", "billion", "trillion"]
    };
    known.contains(&word.as_str()).then_some((word, stop))
}

/// `₺150`, `$20`, `€1.250,50`, `$1.5 million` (the symbol at `i`).
fn currency_before(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    let cur = Currency::from_symbol(cx.ch(i)?)?;
    if let Some(end) = foreign_dollar(cx, i, out) {
        return Some(end);
    }
    if !cx.ch(i + 1)?.is_ascii_digit() {
        return None;
    }
    currency_amount(cx, cur, i + 1, out)
}

/// The amount whose digits start at `start`, spoken with the unit of `cur` (and an optional
/// scale word between: `1,5 milyon`).
fn currency_amount(cx: &Cx<'_>, cur: Currency, start: usize, out: &mut String) -> Option<usize> {
    let num = scan_number(cx, start)?;
    if let Some((scale, stop)) = scale_after(cx, num.end) {
        let spoken = format!(
            "{} {scale} {}",
            num_words(&num, cx.lang),
            cur.major(cx.lang, true)
        );
        return Some(cx.emit_dropping_suffix(out, &spoken, stop));
    }
    let spoken = amount_words(&num, cur, cx.lang);
    Some(cx.emit_dropping_suffix(out, &spoken, num.end))
}

/// `150 TL`, `150₺`, `20 $`, `20 USD` right after a number token.
fn currency_after(cx: &Cx<'_>, num: &Num, out: &mut String) -> Option<usize> {
    // `5 milyar $`, `5 million USD`: a scale word between the number and the unit.
    let (scale, scale_end) = match scale_after(cx, num.end) {
        Some((scale, stop)) => (Some(scale), stop),
        None => (None, num.end),
    };
    let mut j = scale_end;
    if matches!(cx.ch(j), Some(' ' | '\u{a0}')) {
        j += 1;
    }
    let (cur, len) = match cx.ch(j).and_then(Currency::from_symbol) {
        Some(cur) => (cur, 1),
        None => code_at(cx, j)?,
    };
    let spoken = match scale {
        Some(scale) => format!(
            "{} {scale} {}",
            num_words(num, cx.lang),
            cur.major(cx.lang, true)
        ),
        None => amount_words(num, cur, cx.lang),
    };
    Some(cx.emit_dropping_suffix(out, &spoken, j + len))
}

// ---------------------------------------------------------------------------------------------
// Dates and times
// ---------------------------------------------------------------------------------------------

const TR_MONTHS: [&str; 12] = [
    "Ocak", "Şubat", "Mart", "Nisan", "Mayıs", "Haziran", "Temmuz", "Ağustos", "Eylül", "Ekim",
    "Kasım", "Aralık",
];
const EN_MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn month_name(lang: Language, month: u64) -> &'static str {
    let table: &[&str] = match lang {
        Language::Tr => &TR_MONTHS,
        Language::En => &EN_MONTHS,
    };
    pick(table, month.saturating_sub(1))
}

/// The spoken date: `on iki Mart iki bin yirmi altı` / `March twelfth, twenty twenty-six`.
fn date_words(lang: Language, year: u64, month: u64, day: u64) -> String {
    match lang {
        Language::Tr => format!(
            "{} {} {}",
            spell_cardinal(day, lang),
            month_name(lang, month),
            spell_cardinal(year, lang)
        ),
        Language::En => format!(
            "{} {}, {}",
            month_name(lang, month),
            spell_ordinal(day, lang),
            year_en(year)
        ),
    }
}

/// The number of days of `month` (1-12) in `year` (Gregorian leap years).
fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `12.03.2026`, `12/03/2026`, `12-03-2026`, `2026-03-12` (see the module docs for the order).
fn numeric_date(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    let a_end = cx.digits_end(i);
    let sep = cx.ch(a_end).filter(|c| matches!(c, '.' | '/' | '-'))?;
    let b_start = a_end + 1;
    let b_end = cx.digits_end(b_start);
    if cx.ch(b_end)? != sep {
        return None;
    }
    let c_start = b_end + 1;
    let c_end = cx.digits_end(c_start);
    if cx.token_continues_after(c_end, sep) {
        return None;
    }
    let (la, lb, lc) = (a_end - i, b_end - b_start, c_end - c_start);
    let (a, b, c) = (
        cx.value(i, a_end)?,
        cx.value(b_start, b_end)?,
        cx.value(c_start, c_end)?,
    );
    let short = |l: usize| (1..=2).contains(&l);
    let (year, month, day) = if la == 4 && short(lb) && short(lc) {
        (a, b, c)
    } else if short(la) && short(lb) && lc == 4 {
        let us_order = cx.lang == Language::En && sep != '.' && a <= 12 && b <= 31;
        let european = cx.lang == Language::En && sep != '.' && a > 12;
        if us_order && !european {
            (c, a, b)
        } else {
            (c, b, a)
        }
    } else {
        return None;
    };
    if !(1000..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
    {
        return None;
    }
    Some(cx.emit(out, &date_words(cx.lang, year, month, day), c_end))
}

/// The English month whose name starts at `i` (a whole capitalised word) and its end.
fn english_month_at(cx: &Cx<'_>, i: usize) -> Option<(u64, usize)> {
    EN_MONTHS.iter().zip(1u64..).find_map(|(name, number)| {
        let end = i + name.chars().count();
        (cx.starts_with(i, name) && !cx.ch(end).is_some_and(char::is_alphanumeric))
            .then_some((number, end))
    })
}

/// A day number (1-2 digits, 1..=31) with an optional English ordinal suffix at `i`.
fn english_day_at(cx: &Cx<'_>, i: usize) -> Option<(u64, usize)> {
    let end = cx.digits_end(i);
    if end - i > 2 {
        return None;
    }
    let day = cx.value(i, end)?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let suffix = cx.text(end, end + 2).to_ascii_lowercase();
    let end = if matches!(suffix.as_str(), "st" | "nd" | "rd" | "th") {
        end + 2
    } else {
        end
    };
    Some((day, end))
}

/// An optional English year after a date: `, 2026` or ` 2026`; returns the year and the end.
fn english_year_after(cx: &Cx<'_>, end: usize) -> Option<(u64, usize)> {
    let mut start = end;
    if cx.ch(start) == Some(',') {
        start += 1;
    }
    if cx.ch(start) != Some(' ') {
        return None;
    }
    start += 1;
    let stop = cx.digits_end(start);
    if stop - start != 4 || cx.token_continues(stop) {
        return None;
    }
    let year = cx.value(start, stop)?;
    (1000..=9999).contains(&year).then_some((year, stop))
}

/// `March 12, 2026`, `March 12th` (English; the month name at `i`).
fn month_day(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    if cx.lang != Language::En {
        return None;
    }
    let (month, after_month) = english_month_at(cx, i)?;
    if cx.ch(after_month)? != ' ' {
        return None;
    }
    let (day, day_end) = english_day_at(cx, after_month + 1)?;
    if cx.token_continues(day_end) || cx.ch(day_end).is_some_and(char::is_alphanumeric) {
        return None;
    }
    let mut spoken = format!(
        "{} {}",
        month_name(cx.lang, month),
        spell_ordinal(day, cx.lang)
    );
    let mut end = day_end;
    if let Some((year, year_end)) = english_year_after(cx, day_end) {
        spoken.push_str(", ");
        spoken.push_str(&year_en(year));
        end = year_end;
    }
    out.push_str(&spoken);
    Some(end)
}

/// `12 March 2026`, `12th of March` (English; the day at `i`).
fn day_month(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    let (day, day_end) = english_day_at(cx, i)?;
    let mut j = day_end;
    if cx.ch(j)? != ' ' {
        return None;
    }
    j += 1;
    if cx.starts_with(j, "of ") {
        j += 3;
    }
    let (month, month_end) = english_month_at(cx, j)?;
    // "the 3rd of May" already has its article.
    let article = if i >= 4 && cx.text(i - 4, i).eq_ignore_ascii_case("the ") {
        ""
    } else {
        "the "
    };
    let mut spoken = format!(
        "{article}{} of {}",
        spell_ordinal(day, cx.lang),
        month_name(cx.lang, month)
    );
    let mut end = month_end;
    if let Some((year, year_end)) = english_year_after(cx, month_end) {
        spoken.push_str(", ");
        spoken.push_str(&year_en(year));
        end = year_end;
    }
    out.push_str(&spoken);
    Some(end)
}

/// Two-digit minute or second words: `05` -> `sıfır beş` / `oh five`, `30` -> `otuz`.
fn two_digits(n: u64, lang: Language) -> String {
    match (n, lang) {
        (0, Language::Tr) => "sıfır sıfır".to_owned(),
        (0, Language::En) => "oh oh".to_owned(),
        (1..=9, Language::Tr) => format!("sıfır {}", spell_cardinal(n, lang)),
        (1..=9, Language::En) => format!("oh {}", spell_cardinal(n, lang)),
        _ => spell_cardinal(n, lang),
    }
}

/// The meridiem (`AM`, `pm`) after a time, as spoken letters, and where it ends.
fn meridiem_after(cx: &Cx<'_>, end: usize) -> Option<(&'static str, usize)> {
    let mut start = end;
    if cx.ch(start) == Some(' ') {
        start += 1;
    }
    let word = cx.text(start, start + 2).to_ascii_lowercase();
    let spoken = match word.as_str() {
        "am" => "a m",
        "pm" => "p m",
        _ => return None,
    };
    let stop = start + 2;
    (!cx.ch(stop).is_some_and(char::is_alphanumeric)).then_some((spoken, stop))
}

/// `14:30`, `09:05`, `14:30:15`, `9:30 PM`.
fn time(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    let h_end = cx.digits_end(i);
    if h_end - i > 2 || cx.ch(h_end)? != ':' {
        return None;
    }
    let m_start = h_end + 1;
    let m_end = cx.digits_end(m_start);
    if m_end - m_start != 2 {
        return None;
    }
    let mut end = m_end;
    let mut seconds = 0;
    if cx.ch(m_end) == Some(':') {
        let s_end = cx.digits_end(m_end + 1);
        if s_end - (m_end + 1) == 2 {
            seconds = cx.value(m_end + 1, s_end)?;
            end = s_end;
        }
    }
    if cx.token_continues_after(end, ':') {
        return None;
    }
    let (hour, minute) = (cx.value(i, h_end)?, cx.value(m_start, m_end)?);
    if hour > 23 || minute > 59 || seconds > 59 {
        return None;
    }
    Some(speak_time(cx, out, (hour, minute, seconds), end))
}

/// `saat 14.30`, `10.30'da` (Turkish only): the dotted clock time. A dot between two numbers is
/// also a decimal point (`10.30` TL), so the form is accepted only in a context that makes it a
/// time: after the word `saat`, or followed by an apostrophe suffix of the case endings that
/// times take (`'da`, `'de`, `'ta`, `'te`, `'dan`, `'den`, `'tan`, `'ten`, `'a`, `'e`, `'ya`,
/// `'ye`). Hours 0-23, exactly two minute digits, no seconds.
fn dot_time(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    if !cx.is_tr() {
        return None;
    }
    let h_end = cx.digits_end(i);
    if h_end - i > 2 || cx.ch(h_end)? != '.' {
        return None;
    }
    let m_start = h_end + 1;
    let m_end = cx.digits_end(m_start);
    if m_end - m_start != 2 || cx.token_continues_after(m_end, '.') {
        return None;
    }
    let (hour, minute) = (cx.value(i, h_end)?, cx.value(m_start, m_end)?);
    if hour > 23 || minute > 59 {
        return None;
    }
    let after_saat = word_before_is(cx, i, "saat") || ends_saat_time_range(cx, i);
    let time_suffix = cx.suffix_at(m_end).is_some_and(|(suffix, _)| {
        matches!(
            suffix.as_str(),
            "da" | "de" | "ta" | "te" | "dan" | "den" | "tan" | "ten" | "a" | "e" | "ya" | "ye"
        )
    });
    if !after_saat && !time_suffix {
        return None;
    }
    Some(speak_time(cx, out, (hour, minute, 0), m_end))
}

/// Whether the `HH.MM` at `i` is the second half of a range whose first half follows `saat`
/// (`saat 09.00-17.00`): a `-` directly before `i`, and before it a dotted clock time that is
/// itself right after the word `saat`.
fn ends_saat_time_range(cx: &Cx<'_>, i: usize) -> bool {
    let is_digit = |at: Option<usize>| {
        at.and_then(|p| cx.ch(p))
            .is_some_and(|c| c.is_ascii_digit())
    };
    let Some(dash) = i.checked_sub(1).filter(|d| cx.ch(*d) == Some('-')) else {
        return false;
    };
    // The minutes `MM` and the dot directly before the dash, then one or two hour digits.
    if !(is_digit(dash.checked_sub(1)) && is_digit(dash.checked_sub(2))) {
        return false;
    }
    let Some(dot) = dash.checked_sub(3).filter(|d| cx.ch(*d) == Some('.')) else {
        return false;
    };
    let mut hour_start = dot;
    while dot - hour_start < 2 && is_digit(hour_start.checked_sub(1)) {
        hour_start -= 1;
    }
    hour_start < dot && word_before_is(cx, hour_start, "saat")
}

/// Whether the word right before `i` (one run of spaces between) is `word`, ignoring case.
fn word_before_is(cx: &Cx<'_>, i: usize, word: &str) -> bool {
    let mut stop = i;
    while stop > 0 && cx.ch(stop - 1).is_some_and(char::is_whitespace) {
        stop -= 1;
    }
    if stop == i {
        return false;
    }
    let len = word.chars().count();
    let Some(start) = stop.checked_sub(len) else {
        return false;
    };
    let before_ok = start
        .checked_sub(1)
        .and_then(|p| cx.ch(p))
        .is_none_or(|c| !c.is_alphanumeric());
    before_ok && cx.text(start, stop).to_lowercase() == word
}

/// The words of a validated time `(hour, minute, seconds)` ending at `end`, with the suffix and
/// English meridiem that follow.
fn speak_time(
    cx: &Cx<'_>,
    out: &mut String,
    (hour, minute, seconds): (u64, u64, u64),
    end: usize,
) -> usize {
    let mut end = end;
    let meridiem = if cx.lang == Language::En && (1..=12).contains(&hour) {
        meridiem_after(cx, end)
    } else {
        None
    };
    let mut parts = vec![spell_cardinal(hour, cx.lang)];
    match (minute, seconds, cx.lang) {
        (0, 0, Language::Tr) => {
            if hour == 0 {
                parts.push("sıfır".to_owned());
            }
        }
        (0, 0, Language::En) => {
            if meridiem.is_some() {
                // "nine p m"
            } else if hour == 0 {
                parts = vec!["midnight".to_owned()];
            } else if hour <= 12 {
                parts.push("o'clock".to_owned());
            } else {
                parts.push("hundred".to_owned());
            }
        }
        _ => {
            parts.push(two_digits(minute, cx.lang));
            if seconds > 0 {
                parts.push(two_digits(seconds, cx.lang));
            }
        }
    }
    if let Some((spoken, stop)) = meridiem {
        parts.push(spoken.to_owned());
        end = stop;
    }
    cx.emit(out, &parts.join(" "), end)
}

// ---------------------------------------------------------------------------------------------
// Phone numbers
// ---------------------------------------------------------------------------------------------

/// Group lengths for a contiguous digit string, by the national format.
fn phone_pattern(digits: &str, plus: bool) -> Vec<usize> {
    let n = digits.len();
    if plus && n == 12 && digits.starts_with("90") {
        vec![2, 3, 3, 2, 2]
    } else if !plus && n == 11 && digits.starts_with('0') {
        vec![4, 3, 2, 2]
    } else if !plus && n == 10 && digits.starts_with('5') {
        // Turkish mobile without the leading zero: 532 123 45 67.
        vec![3, 3, 2, 2]
    } else if !plus && n == 7 && digits.starts_with("444") {
        vec![3, 4]
    } else {
        let mut sizes = vec![3; n / 3];
        match n % 3 {
            0 => {}
            1 => {
                if let Some(last) = sizes.last_mut() {
                    *last += 1;
                } else {
                    sizes.push(1);
                }
            }
            r => sizes.push(r),
        }
        sizes
    }
}

/// A phone number at `i` (a digit or `+`): grouped digit groups, read digit by digit with a
/// pause between groups.
fn phone(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    /// Most digit groups looked at (a grouped number has at most five or six).
    const MAX_GROUPS: usize = 8;
    let plus = cx.ch(i)? == '+';
    let mut j = if plus { i + 1 } else { i };
    // The digit groups as `(start, stop)` ranges; nothing is allocated unless the number is
    // accepted (this runs at every digit that starts a token).
    let mut ranges = [(0usize, 0usize); MAX_GROUPS];
    let mut count = 0usize;
    while count < MAX_GROUPS {
        let paren = cx.ch(j) == Some('(');
        let start = if paren { j + 1 } else { j };
        let stop = cx.digits_end(start);
        if stop == start || (paren && cx.ch(stop) != Some(')')) {
            break;
        }
        if let Some(slot) = ranges.get_mut(count) {
            *slot = (start, stop);
        }
        count += 1;
        j = if paren { stop + 1 } else { stop };
        let more = matches!(cx.ch(j), Some(' ' | '-' | '.'))
            && cx.ch(j + 1).is_some_and(|c| c.is_ascii_digit() || c == '(');
        if more {
            j += 1;
        } else {
            break;
        }
    }
    let used = ranges.get(..count)?;
    if used.is_empty() || cx.token_continues(j) {
        return None;
    }
    let mut lens_buf = [0usize; MAX_GROUPS];
    for (len, (start, stop)) in lens_buf.iter_mut().zip(used) {
        *len = stop - start;
    }
    let lens = lens_buf.get(..count)?;
    let total: usize = lens.iter().sum();
    let (first_start, _) = *used.first()?;
    let first_zero = cx.ch(first_start) == Some('0');
    let accepted = if plus {
        (8..=15).contains(&total)
    } else if first_zero {
        (10..=11).contains(&total)
    } else if cx.is_tr() && total == 10 && cx.ch(first_start) == Some('5') {
        // A Turkish mobile number without the leading zero: bare or grouped (532 123 45 67).
        count == 1 || lens == [3, 3, 2, 2] || lens == [3, 3, 4]
    } else if cx.is_tr()
        && total == 7
        && cx.starts_with(first_start, "444")
        && (count == 1 || lens.first() == Some(&3))
    {
        // A call-centre short number: 444 1 234, 444 12 34, 444 1234.
        matches!(lens, [7] | [3, 4] | [3, 1, 3] | [3, 2, 2])
    } else {
        (10..=11).contains(&total) && (lens == [3, 3, 4] || lens == [1, 3, 3, 4])
    };
    if !accepted {
        return None;
    }
    let groups: Vec<String> = if let [(start, stop)] = used {
        let single = cx.text(*start, *stop);
        let mut regrouped = Vec::new();
        let mut at = 0;
        for size in phone_pattern(&single, plus) {
            regrouped.push(single.get(at..at + size)?.to_owned());
            at += size;
        }
        regrouped
    } else {
        used.iter()
            .map(|(start, stop)| cx.text(*start, *stop))
            .collect()
    };
    let spoken: Vec<String> = groups.iter().map(|g| spell_digits(g, cx.lang)).collect();
    let mut text = spoken.join(", ");
    if plus {
        let word = if cx.is_tr() { "artı" } else { "plus" };
        text = format!("{word} {text}");
    }
    out.push_str(&text);
    Some(j)
}

// ---------------------------------------------------------------------------------------------
// Abbreviations
// ---------------------------------------------------------------------------------------------

/// When an expanded abbreviation keeps its period (the abbreviation's dot doubles as a sentence
/// end only in these positions).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dot {
    /// Never: a title or word that always has a name or object after it (`Dr.`).
    Never,
    /// Only when it is the last thing in the chunk (`A.Ş.` is usually followed by more words).
    AtEnd,
    /// When it is last in the chunk or a capital letter follows (`vb. Sonra`).
    AtEndOrCapital,
}

/// What must follow an abbreviation for it to be expanded. Many abbreviations are also words
/// or end a sentence (`Dr.` = Drive in `Elm Dr.`, Turkish `Bul.` = "find!"), so the expansion is
/// done only when the text after it shows that it is an abbreviation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    /// No requirement (a period that ends the chunk is fine: `vb.`, `Inc.`).
    Any,
    /// A word, number or name follows after whitespace in the same chunk (`approx. 5`).
    Word,
    /// For abbreviations that are also words (`Sok.`, `Bul.`, `Tel.`): a number or a capitalised
    /// name stands before it (`Gazi Bul.`, `12. Sok.`), or a digit follows (`Tel. 0212`).
    Context,
    /// A capitalised name follows (`Dr. Ayşe`, `Mr. Smith`, `Prof. Dr.`).
    Capital,
}

/// A written abbreviation, its spoken form, and the period and context rules.
struct Abbr {
    written: &'static str,
    spoken: &'static str,
    dot: Dot,
    next: Next,
}

const fn abbr(written: &'static str, spoken: &'static str, dot: Dot) -> Abbr {
    Abbr {
        written,
        spoken,
        dot,
        next: Next::Any,
    }
}

/// An abbreviation that is also a word and needs an address or number context.
const fn abbr_context(written: &'static str, spoken: &'static str, dot: Dot) -> Abbr {
    Abbr {
        written,
        spoken,
        dot,
        next: Next::Context,
    }
}

/// An abbreviation that needs a following word.
const fn abbr_word(written: &'static str, spoken: &'static str, dot: Dot) -> Abbr {
    Abbr {
        written,
        spoken,
        dot,
        next: Next::Word,
    }
}

/// A title that needs a following capitalised name.
const fn title(written: &'static str, spoken: &'static str) -> Abbr {
    Abbr {
        written,
        spoken,
        dot: Dot::Never,
        next: Next::Capital,
    }
}

/// Turkish abbreviations (case sensitive).
const TR_ABBREVIATIONS: &[Abbr] = &[
    title("Dr.", "Doktor"),
    title("Prof.", "Profesör"),
    title("Doç.", "Doçent"),
    title("Yrd.", "Yardımcı"),
    title("Öğr.", "Öğretim"),
    title("Gör.", "Görevlisi"),
    title("Uzm.", "Uzman"),
    title("Av.", "Avukat"),
    title("Müh.", "Mühendis"),
    title("Op.", "Operatör"),
    title("Sn.", "Sayın"),
    abbr("Mah.", "Mahallesi", Dot::Never),
    abbr("Cad.", "Caddesi", Dot::Never),
    abbr_context("Sok.", "Sokak", Dot::Never),
    abbr_context("Bul.", "Bulvarı", Dot::Never),
    abbr("Apt.", "Apartmanı", Dot::Never),
    abbr_context("Tel.", "Telefon", Dot::Never),
    abbr("Ltd.", "Limited", Dot::Never),
    abbr("Şti.", "Şirketi", Dot::Never),
    abbr("A.Ş.", "Anonim Şirketi", Dot::AtEnd),
    abbr("T.C.", "Türkiye Cumhuriyeti", Dot::AtEnd),
    abbr("M.Ö.", "Milattan önce", Dot::AtEnd),
    abbr("M.S.", "Milattan sonra", Dot::AtEnd),
    abbr("bkz.", "bakınız", Dot::Never),
    abbr("örn.", "örneğin", Dot::Never),
    abbr("dk.", "dakika", Dot::AtEndOrCapital),
    abbr("sn.", "saniye", Dot::AtEndOrCapital),
    abbr("vb.", "ve benzeri", Dot::AtEndOrCapital),
    abbr("vs.", "vesaire", Dot::AtEndOrCapital),
    abbr("vd.", "ve diğerleri", Dot::AtEndOrCapital),
];

/// English abbreviations (case sensitive).
const EN_ABBREVIATIONS: &[Abbr] = &[
    title("Mr.", "Mister"),
    title("Mrs.", "Missus"),
    title("Ms.", "Miz"),
    title("Dr.", "Doctor"),
    title("Prof.", "Professor"),
    abbr("Jr.", "Junior", Dot::AtEnd),
    abbr("Sr.", "Senior", Dot::AtEnd),
    abbr("Inc.", "Incorporated", Dot::AtEnd),
    abbr("Ltd.", "Limited", Dot::AtEnd),
    abbr_word("approx.", "approximately", Dot::Never),
    abbr_word("vs.", "versus", Dot::Never),
    abbr_word("e.g.", "for example", Dot::Never),
    abbr_word("i.e.", "that is", Dot::Never),
    abbr("etc.", "et cetera", Dot::AtEndOrCapital),
];

/// The longest abbreviation of the language written at `i`.
fn find_abbreviation(cx: &Cx<'_>, i: usize) -> Option<&'static Abbr> {
    let table = if cx.is_tr() {
        TR_ABBREVIATIONS
    } else {
        EN_ABBREVIATIONS
    };
    table
        .iter()
        .filter(|a| cx.starts_with(i, a.written) && next_ok(cx, i, a))
        .max_by_key(|a| a.written.chars().count())
}

/// Whether a number (or an ordinal `12.`) directly precedes `i`: `12. Sok.` ends a sentence
/// legitimately, `ateşi yak.` style words do not follow numbers.
fn preceded_by_number(cx: &Cx<'_>, i: usize) -> bool {
    let mut j = i;
    while j > 0 && cx.ch(j - 1).is_some_and(char::is_whitespace) {
        j -= 1;
    }
    if j > 0 && cx.ch(j - 1) == Some('.') {
        j -= 1;
    }
    j > 0 && cx.ch(j - 1).is_some_and(|c| c.is_ascii_digit())
}

/// Whether a capitalised word directly precedes `i` (`Gazi Bul.`, `Atatürk Sok.`).
fn preceded_by_capitalised_word(cx: &Cx<'_>, i: usize) -> bool {
    let mut j = i;
    while j > 0 && cx.ch(j - 1).is_some_and(char::is_whitespace) {
        j -= 1;
    }
    if j == i {
        return false;
    }
    let stop = j;
    while j > 0 && cx.ch(j - 1).is_some_and(char::is_alphanumeric) {
        j -= 1;
    }
    j < stop && cx.ch(j).is_some_and(char::is_uppercase)
}

/// Whether the text after the abbreviation written at `i` meets its [`Next`] requirement.
fn next_ok(cx: &Cx<'_>, i: usize, abbreviation: &Abbr) -> bool {
    let end = i + abbreviation.written.chars().count();
    // A longer word that merely starts with the abbreviation (`Dr.Ayşe`) is a token of its own.
    let mut next = end;
    while cx.ch(next).is_some_and(char::is_whitespace) {
        next += 1;
    }
    match abbreviation.next {
        Next::Any => true,
        Next::Word => next > end && cx.ch(next).is_some_and(char::is_alphanumeric),
        Next::Context => {
            cx.ch(next).is_some_and(|c| c.is_ascii_digit())
                || preceded_by_number(cx, i)
                || preceded_by_capitalised_word(cx, i)
        }
        Next::Capital => cx
            .ch(next)
            .is_some_and(|c| c.is_uppercase() && (next > end || c.is_alphabetic())),
    }
}

/// An abbreviation at `i`.
fn abbreviation(cx: &Cx<'_>, i: usize, out: &mut String) -> Option<usize> {
    let found = find_abbreviation(cx, i)?;
    let end = i + found.written.chars().count();
    out.push_str(found.spoken);
    let mut next = end;
    while cx.ch(next).is_some_and(char::is_whitespace) {
        next += 1;
    }
    let keeps_period = match (found.dot, cx.ch(next)) {
        (Dot::Never, _) => false,
        (_, None) => true,
        (Dot::AtEnd, Some(_)) => false,
        (Dot::AtEndOrCapital, Some(c)) => c.is_uppercase() && next > end,
    };
    if keeps_period {
        out.push('.');
    } else if cx.ch(end).is_some_and(char::is_alphabetic) {
        out.push(' ');
    }
    Some(end)
}
