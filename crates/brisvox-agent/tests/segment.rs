//! Tests of the TTS text segmenter: rule table in both languages, split-invariance,
//! length limits, the one-token lookahead and time-to-first-chunk on token streams.

use std::time::Duration;

use brisvox_agent::segment::{Language, Segmenter, SegmenterConfig, timed_chunks};

/// Runs `input` as one delta, then finishes.
fn run_whole(config: SegmenterConfig, input: &str) -> Vec<String> {
    let mut seg = Segmenter::new(config);
    let mut out = Vec::new();
    seg.push(input, &mut out);
    seg.finish(&mut out);
    out
}

/// Runs `input` as deltas of `n` chars each, then finishes.
fn run_pieces(config: SegmenterConfig, input: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut seg = Segmenter::new(config);
    let mut out = Vec::new();
    for piece in chars.chunks(n) {
        let delta: String = piece.iter().collect();
        seg.push(&delta, &mut out);
    }
    seg.finish(&mut out);
    out
}

const EN: Language = Language::En;
const TR: Language = Language::Tr;

/// (language, input, expected chunks) with the default config (first_min_words 3,
/// max_chunk_chars 250).
const TABLE: &[(Language, &str, &[&str])] = &[
    // --- sentence ends, first chunk rule, later chunks ---
    (
        EN,
        "The weather is nice today. Bring a jacket tonight.",
        &["The weather is nice today.", "Bring a jacket tonight."],
    ),
    (EN, "Yes, I can help.", &["Yes, I can help."]),
    (EN, "Hi. How are you?", &["Hi. How are you?"]),
    (
        EN,
        "Hello there friend. Well, you know, that is fine.",
        &["Hello there friend.", "Well, you know, that is fine."],
    ),
    (EN, "Wow!", &["Wow!"]),
    (EN, "Hello world", &["Hello world"]),
    (
        EN,
        "Is it done?! Really?! Yes indeed.",
        &["Is it done?!", "Really?!", "Yes indeed."],
    ),
    (
        EN,
        "What's up? Nothing much, you?",
        &["What's up? Nothing much,", "you?"],
    ),
    (
        EN,
        "Step 1. Open the app now. Then continue.",
        &["Step 1. Open the app now.", "Then continue."],
    ),
    // --- abbreviations (EN) ---
    (
        EN,
        "Dr. Smith arrived at 10:30. He was late.",
        &["Dr. Smith arrived at 10:30.", "He was late."],
    ),
    (
        EN,
        "Mr. and Mrs. Smith went home. It rained.",
        &["Mr. and Mrs. Smith went home.", "It rained."],
    ),
    (
        EN,
        "Ms. Lee and Prof. Kim met at St. Mary's today. They talked.",
        &[
            "Ms. Lee and Prof. Kim met at St. Mary's today.",
            "They talked.",
        ],
    ),
    (
        EN,
        "Use tools, e.g. the web and docs.",
        &["Use tools, e.g. the web and docs."],
    ),
    (
        EN,
        "The U.S. Army is big. Inc. and Ltd. too.",
        &["The U.S. Army is big.", "Inc. and Ltd. too."],
    ),
    (
        EN,
        "a.m. and p.m. are times. OK.",
        &["a.m. and p.m. are times.", "OK."],
    ),
    (
        EN,
        "Time is 10:30 p.m. tonight. See you.",
        &["Time is 10:30 p.m. tonight.", "See you."],
    ),
    (
        EN,
        "Ph.D. candidates are many. Yes.",
        &["Ph.D. candidates are many.", "Yes."],
    ),
    (EN, "DR. SMITH came. Okay.", &["DR. SMITH came.", "Okay."]),
    (
        EN,
        "Bread, etc. Then we left.",
        &["Bread, etc. Then we left."],
    ),
    (
        EN,
        "No. 5 is the answer. Right.",
        &["No. 5 is the answer.", "Right."],
    ),
    (
        EN,
        "The answer was clearly no. Right now.",
        &["The answer was clearly no.", "Right now."],
    ),
    // --- decimals, thousands, times, numbers (EN) ---
    (
        EN,
        "Pi is 3.14 exactly. Really.",
        &["Pi is 3.14 exactly.", "Really."],
    ),
    (
        EN,
        "It costs 1,000 dollars. Pay now.",
        &["It costs 1,000 dollars.", "Pay now."],
    ),
    (
        EN,
        "Numbers like 1,000,000 are big. Ok.",
        &["Numbers like 1,000,000 are big.", "Ok."],
    ),
    (
        EN,
        "Call 555.1234 now. Thanks a lot.",
        &["Call 555.1234 now.", "Thanks a lot."],
    ),
    (
        EN,
        "Ordinal: 3. the next item here. Done.",
        &["Ordinal: 3. the next item here.", "Done."],
    ),
    // --- URLs, e-mail, ellipsis ---
    (
        EN,
        "Visit https://example.com/docs for more. Then read.",
        &["Visit https://example.com/docs for more.", "Then read."],
    ),
    (
        EN,
        "Please contact jane.doe@example.org. Thanks a lot.",
        &["Please contact jane.doe@example.org.", "Thanks a lot."],
    ),
    (
        EN,
        "See example.com. and more.",
        &["See example.com. and more."],
    ),
    (
        EN,
        "Wait... I think so. Yes.",
        &["Wait... I think so.", "Yes."],
    ),
    (
        EN,
        "Well... what now is the plan. OK.",
        &["Well... what now is the plan.", "OK."],
    ),
    (
        EN,
        "Please hold on... Thanks.",
        &["Please hold on...", "Thanks."],
    ),
    // --- clause boundaries, dashes, colons ---
    (
        EN,
        "Score: 3-2 at halftime. Fine.",
        &["Score: 3-2 at halftime.", "Fine."],
    ),
    (
        EN,
        "Coffee - tea - water are fluids. Right.",
        &["Coffee - tea - water are fluids.", "Right."],
    ),
    (
        EN,
        "Well-known fact: water boils. Next.",
        &["Well-known fact: water boils.", "Next."],
    ),
    (
        EN,
        "Visit the site today; it has news.",
        &["Visit the site today;", "it has news."],
    ),
    // --- closing quotes and brackets belong to the previous chunk ---
    (
        EN,
        "He said \"Stop.\" Then left. Bye now.",
        &["He said \"Stop.\"", "Then left.", "Bye now."],
    ),
    (
        EN,
        "She said (quietly.) Then she left now.",
        &["She said (quietly.)", "Then she left now."],
    ),
    // --- emoji and multi-codepoint graphemes ---
    (
        EN,
        "Great job today 😊. Keep going 👍 friend.",
        &["Great job today 😊.", "Keep going 👍 friend."],
    ),
    (
        EN,
        "Family 👨\u{200d}👩\u{200d}👧 time is nice. Yes.",
        &["Family 👨\u{200d}👩\u{200d}👧 time is nice.", "Yes."],
    ),
    (EN, "Flag 🇹🇷 is here. Bye.", &["Flag 🇹🇷 is here.", "Bye."]),
    // --- newlines (lists, paragraphs) ---
    (
        EN,
        "1. Buy milk\n2. Call mom\n3. Sleep",
        &["1. Buy milk", "2. Call mom", "3. Sleep"],
    ),
    (
        EN,
        "Line one\n\nLine two is here.",
        &["Line one\n\nLine two is here."],
    ),
    // --- unlisted dotted acronyms are not domains ---
    (
        EN,
        "We always follow E.U. Rules strictly for now. Yes.",
        &["We always follow E.U. Rules strictly for now.", "Yes."],
    ),
    (
        EN,
        "Born in the year 100 B.C. Caesar ruled Rome later. Yes.",
        &["Born in the year 100 B.C. Caesar ruled Rome later.", "Yes."],
    ),
    (
        EN,
        "N.Y.C. Office is open today. Yes.",
        &["N.Y.C. Office is open today.", "Yes."],
    ),
    (
        EN,
        "A.I. Systems are here now. Yes.",
        &["A.I. Systems are here now.", "Yes."],
    ),
    // --- more abbreviations (EN) ---
    (
        EN,
        "Pick A vs. B for now. Done.",
        &["Pick A vs. B for now.", "Done."],
    ),
    (
        EN,
        "John Smith Jr. and Sr. Jones came. Okay.",
        &["John Smith Jr. and Sr. Jones came.", "Okay."],
    ),
    (
        EN,
        "That is, i.e. the same thing. Yes.",
        &["That is, i.e. the same thing.", "Yes."],
    ),
    // --- markdown emphasis and code around sentence ends ---
    (
        EN,
        "Hello there friend. **Done.** Next we go home now. Bye.",
        &[
            "Hello there friend.",
            "**Done.**",
            "Next we go home now.",
            "Bye.",
        ],
    ),
    (
        EN,
        "_Italic._ Next one here. Bye.",
        &["_Italic._ Next one here.", "Bye."],
    ),
    // --- a list marker is exempt only at a line start ---
    (
        EN,
        "It is done now. 2. We left. Bye",
        &["It is done now.", "2.", "We left.", "Bye"],
    ),
    // --- newlines end a first chunk and later chunks ---
    (
        EN,
        "Line one here\n\nLine two is here.",
        &["Line one here", "Line two is here."],
    ),
    (
        EN,
        "Hello there friend. Line two\nLine three is here.",
        &["Hello there friend.", "Line two", "Line three is here."],
    ),
    // --- a newline after a clause mark or dash ends a later chunk (list items) ---
    (
        EN,
        "Shopping list:\n- eggs,\n- milk,\n- bread.",
        &["Shopping list:\n- eggs,", "- milk,", "- bread."],
    ),
    (
        EN,
        "Hello there friend. Here is the list:\n1. Buy milk\n2. Call mom.",
        &[
            "Hello there friend.",
            "Here is the list:",
            "1. Buy milk",
            "2. Call mom.",
        ],
    ),
    (
        EN,
        "Hello there friend. -\nMilk is here now.",
        &["Hello there friend.", "-\nMilk is here now."],
    ),
    (
        TR,
        "Bu iki şey lazım:\n- yumurta,\n- süt, un.",
        &["Bu iki şey lazım:", "- yumurta,", "- süt, un."],
    ),
    // --- whitespace only ---
    (EN, "   ", &[]),
    (EN, "\n\n\t", &[]),
    // --- closing curly quotes, guillemets, brackets and backticks ---
    (
        EN,
        "He said “Stop.” Then left. Bye now.",
        &["He said “Stop.”", "Then left.", "Bye now."],
    ),
    (
        EN,
        "Please run ‘ls.’ Then we go home. Bye.",
        &["Please run ‘ls.’", "Then we go home.", "Bye."],
    ),
    (
        EN,
        "This is [Note.] Then left. Bye.",
        &["This is [Note.]", "Then left.", "Bye."],
    ),
    (
        EN,
        "Please run `ls.` Then we go home. Bye.",
        &["Please run `ls.`", "Then we go home.", "Bye."],
    ),
    (
        TR,
        "O dedi «Dur.» Sonra gitti. Hoş.",
        &["O dedi «Dur.»", "Sonra gitti.", "Hoş."],
    ),
    // --- Turkish: first chunk and later chunks ---
    (
        TR,
        "Merhaba, nasılsın? İyiyim, teşekkür ederim.",
        &["Merhaba, nasılsın? İyiyim,", "teşekkür ederim."],
    ),
    (
        TR,
        "Bugün hava çok güzel. Yarın yağmur var.",
        &["Bugün hava çok güzel.", "Yarın yağmur var."],
    ),
    (TR, "Tamam mı?", &["Tamam mı?"]),
    (TR, "Ne? Hiç! İyi.", &["Ne? Hiç! İyi."]),
    (
        TR,
        "Gerçekten mi?! Evet, öyle.",
        &["Gerçekten mi?! Evet,", "öyle."],
    ),
    (
        TR,
        "Kitap şöyle: bir, iki, üç. Bitti.",
        &["Kitap şöyle: bir,", "iki, üç.", "Bitti."],
    ),
    (
        TR,
        "Bir, iki ve üç olsun. Tamam.",
        &["Bir, iki ve üç olsun.", "Tamam."],
    ),
    (
        TR,
        "Bu - çok iyi bir fikir. Evet.",
        &["Bu - çok iyi bir fikir.", "Evet."],
    ),
    // --- Turkish abbreviations (incl. case and dotted/dotless i) ---
    (
        TR,
        "Dr. Ahmet Bey geldi. Oturdu.",
        &["Dr. Ahmet Bey geldi.", "Oturdu."],
    ),
    (
        TR,
        "Prof. Dr. Ayşe Yılmaz konuştu. Dinledik.",
        &["Prof. Dr. Ayşe Yılmaz konuştu.", "Dinledik."],
    ),
    (
        TR,
        "Doc. Dr. Mehmet Sn. Kaya geldi.",
        &["Doc. Dr. Mehmet Sn. Kaya geldi."],
    ),
    (
        TR,
        "Bsk. Ali Bey toplantıda. Tamam.",
        &["Bsk. Ali Bey toplantıda.", "Tamam."],
    ),
    (
        TR,
        "Ankara vb. şehirlerde yağmur yağdı. Sonra durdu.",
        &["Ankara vb. şehirlerde yağmur yağdı.", "Sonra durdu."],
    ),
    (
        TR,
        "Tel. No 123 ile arayın. Teşekkürler.",
        &["Tel. No 123 ile arayın.", "Teşekkürler."],
    ),
    (
        TR,
        "Cad. ve Sok. üzerinde ev var. Güzel.",
        &["Cad. ve Sok. üzerinde ev var.", "Güzel."],
    ),
    (
        TR,
        "Mah. Atatürk Apt. 5 numarada. Gelin.",
        &["Mah. Atatürk Apt. 5 numarada.", "Gelin."],
    ),
    (
        TR,
        "TEL. ile DR. ahmet bey. Tamam.",
        &["TEL. ile DR. ahmet bey.", "Tamam."],
    ),
    (
        TR,
        "Ltd. Şti. ile ŞTİ. ortak. Tamam.",
        &["Ltd. Şti. ile ŞTİ. ortak.", "Tamam."],
    ),
    (
        TR,
        "Tic. Dr. Can geldi. Hoş.",
        &["Tic. Dr. Can geldi.", "Hoş."],
    ),
    // --- Turkish numbers, ordinals, times ---
    (
        TR,
        "Oğlum 3. sinif öğrencisi. Harika.",
        &["Oğlum 3. sinif öğrencisi.", "Harika."],
    ),
    (TR, "Fiyat 3,14 TL. Ucuz.", &["Fiyat 3,14 TL.", "Ucuz."]),
    (
        TR,
        "Toplam 1.000 kişi geldi. Güzel.",
        &["Toplam 1.000 kişi geldi.", "Güzel."],
    ),
    (
        TR,
        "Toplantı 10:30'da başladı. Bitti.",
        &["Toplantı 10:30'da başladı.", "Bitti."],
    ),
    // --- Turkish URLs, e-mail, ellipsis, emoji ---
    (
        TR,
        "Siteyi ziyaret edin https://ornek.com.tr/yardim. Teşekkürler.",
        &[
            "Siteyi ziyaret edin https://ornek.com.tr/yardim.",
            "Teşekkürler.",
        ],
    ),
    (
        TR,
        "Bana ornek@mail.com adresinden yaz. Tamam.",
        &["Bana ornek@mail.com adresinden yaz.", "Tamam."],
    ),
    (
        TR,
        "Bu bir test… Sonra devam.",
        &["Bu bir test…", "Sonra devam."],
    ),
    (
        TR,
        "Bu bir test... sonra devam ederiz. Tamam.",
        &["Bu bir test... sonra devam ederiz.", "Tamam."],
    ),
    (
        TR,
        "Bugün çok güzel 😊. Devam edelim 👍 lütfen.",
        &["Bugün çok güzel 😊.", "Devam edelim 👍 lütfen."],
    ),
    // --- Turkish: Doç., No., dotted and dotless i in abbreviations ---
    (
        TR,
        "Bugün hava güzel. Doç. Dr. Ali Bey geldi. Tamam.",
        &["Bugün hava güzel.", "Doç. Dr. Ali Bey geldi.", "Tamam."],
    ),
    (
        TR,
        "Bkz. No. 5 e bakın. Tamam.",
        &["Bkz. No. 5 e bakın.", "Tamam."],
    ),
    (
        TR,
        "TİC. Dr. Can geldi. Hoş.",
        &["TİC. Dr. Can geldi.", "Hoş."],
    ),
    (
        TR,
        "Tıc. Dr. Can geldi. Hoş.",
        &["Tıc. Dr. Can geldi.", "Hoş."],
    ),
    // --- Turkish closing quotes, brackets, markdown, newlines ---
    (
        TR,
        "O dedi \"Dur.\" Sonra gitti. Hoş.",
        &["O dedi \"Dur.\"", "Sonra gitti.", "Hoş."],
    ),
    (
        TR,
        "Bu (çok önemli.) Sonra gider. Tamam.",
        &["Bu (çok önemli.)", "Sonra gider.", "Tamam."],
    ),
    (
        TR,
        "**Uyarı: bu kötü.** Çalıştırma. Tamam.",
        &["**Uyarı: bu kötü.**", "Çalıştırma.", "Tamam."],
    ),
    (
        TR,
        "Bir şey var\nİki şey daha. Tamam.",
        &["Bir şey var", "İki şey daha.", "Tamam."],
    ),
];

/// (language, max_chunk_chars, first_min_words, input, expected chunks): small limits, where the
/// over-limit rules decide. Every row also runs split into 1-3 char pieces.
const SMALL_TABLE: &[(Language, usize, usize, &str, &[&str])] = &[
    // Pending "a, b …" is undecided until "X" arrives: no fallback before the lookahead.
    (EN, 6, 0, "Hi. a, b … X", &["Hi.", "a, b …", "X"]),
    // Over the limit with no clause: the last whitespace inside the limit, then the rest.
    (EN, 4, 0, "Ok. ab c. Yes.", &["Ok.", "ab", "c.", "Yes."]),
    // The token stays whole when there is no whitespace inside the limit.
    (
        EN,
        8,
        3,
        "Hi there ab👨\u{200d}👩\u{200d}👧cdefgh",
        &["Hi there", "ab👨\u{200d}👩\u{200d}👧cdefgh"],
    ),
    (
        TR,
        5,
        0,
        "Yarın, belki. Çok uzun bir cümle var.",
        &["Yarın,", "belki.", "Çok", "uzun", "bir", "cümle", "var."],
    ),
    // A period that is not a sentence end (ordinal, abbreviation) is never a fallback cut.
    (
        TR,
        10,
        0,
        "Oğlum 3. sinif öğrencisi",
        &["Oğlum", "3. sinif", "öğrencisi"],
    ),
    (EN, 6, 0, "Dr. Smith went", &["Dr. Smith", "went"]),
    // A number and period before a lowercase word is declined (`1. x`, not a sentence end). In
    // 1-char pieces the fallback sees `1. ` (a non-space char already at the limit) with only a
    // space after the period and must wait for the word instead of cutting after the period.
    (EN, 1, 0, "Hi. 1. x yz", &["Hi.", "1. x", "yz"]),
];

/// (language, input, expected chunks) with `first_min_words` 0, so the first-chunk word rule
/// cannot hide a missing abbreviation (a one-word boundary would otherwise be skipped).
const ABBREVIATION_TABLE: &[(Language, &str, &[&str])] = &[
    (
        EN,
        "Dr. Smith arrived at 10:30. He was late.",
        &["Dr. Smith arrived at 10:30.", "He was late."],
    ),
    (EN, "DR. SMITH came. Okay.", &["DR. SMITH came.", "Okay."]),
    (
        EN,
        "N.Y.C. Office is open today. Yes.",
        &["N.Y.C. Office is open today.", "Yes."],
    ),
    (
        EN,
        "A.I. Systems are here now. Yes.",
        &["A.I. Systems are here now.", "Yes."],
    ),
    (
        TR,
        "Dr. Ahmet Bey geldi. Oturdu.",
        &["Dr. Ahmet Bey geldi.", "Oturdu."],
    ),
    (
        TR,
        "Bsk. Ali Bey toplantıda. Tamam.",
        &["Bsk. Ali Bey toplantıda.", "Tamam."],
    ),
    (
        TR,
        "Tic. Dr. Can geldi. Hoş.",
        &["Tic. Dr. Can geldi.", "Hoş."],
    ),
    (
        TR,
        "Prof. Dr. Ayşe Yılmaz konuştu. Dinledik.",
        &["Prof. Dr. Ayşe Yılmaz konuştu.", "Dinledik."],
    ),
    (
        TR,
        "Doc. Dr. Mehmet Sn. Kaya geldi.",
        &["Doc. Dr. Mehmet Sn. Kaya geldi."],
    ),
];

#[test]
fn abbreviation_rows_hold_without_the_first_chunk_word_rule() {
    for (language, input, expected) in ABBREVIATION_TABLE {
        let config = SegmenterConfig {
            language: *language,
            first_min_words: 0,
            ..SegmenterConfig::new(*language)
        };
        let want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(run_whole(config, input), want, "whole push: {input:?}");
        for n in 1..=3 {
            assert_eq!(
                run_pieces(config, input, n),
                want,
                "{n}-char pieces: {input:?}"
            );
        }
    }
}

#[test]
fn combining_mark_run_is_cut_at_the_cap_not_into_lone_marks() {
    // One grapheme (x plus 1200 combining acutes) with no whitespace: no safe boundary exists,
    // so the hard cap (4 × 250) is the cut. The old fallback emitted one mark per chunk.
    let config = SegmenterConfig::new(EN);
    let input = format!("x{}", "\u{301}".repeat(1200));
    let got = run_whole(config, &input);
    assert_eq!(got.len(), 2, "chunk lengths: {:?}", lengths(&got));
    assert_eq!(got.first().map(|c| c.chars().count()), Some(1000));
    assert!(got.iter().all(|c| c.chars().count() > 1));
    assert_eq!(got.concat(), input);
    assert_eq!(run_pieces(config, &input, 1), got);
}

#[test]
fn combining_mark_run_with_a_small_cap_keeps_chunks_above_one_char() {
    let config = SegmenterConfig {
        max_chunk_chars: 2,
        first_min_words: 0,
        ..SegmenterConfig::new(EN)
    };
    let input = format!("x{}", "\u{301}".repeat(10));
    let got = run_whole(config, &input);
    assert_eq!(got.concat(), input);
    assert_eq!(lengths(&got), [8, 3]);
    assert_eq!(run_pieces(config, &input, 2), got);
}

/// Chars per chunk, for failure messages.
fn lengths(chunks: &[String]) -> Vec<usize> {
    chunks.iter().map(|c| c.chars().count()).collect()
}

#[test]
fn small_limit_table_matches_whole_and_split_runs() {
    for (language, max, min_words, input, expected) in SMALL_TABLE {
        let config = SegmenterConfig {
            language: *language,
            first_min_words: *min_words,
            max_chunk_chars: *max,
        };
        let want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(run_whole(config, input), want, "whole push: {input:?}");
        for n in 1..=3 {
            assert_eq!(
                run_pieces(config, input, n),
                want,
                "{n}-char pieces: {input:?} (max {max})"
            );
        }
    }
}

/// Small deterministic generator (no external crates): returns values in `0..bound`.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap_or(0) % bound.max(1)
    }
}

/// Runs `input` split at random positions (pieces of 1 to 4 chars), checking the pending bound
/// after every push.
fn run_random_split(config: SegmenterConfig, input: &str, rng: &mut Lcg) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut seg = Segmenter::new(config);
    let mut out = Vec::new();
    // Documented bound (`Segmenter::pending_chars`): the normal 4 × max + 2, plus the whitespace
    // tail a fallback waits on after an ambiguous period (at most max + 1 chars).
    let bound = config.max_chunk_chars.max(1) * 5 + 1;
    let mut at = 0;
    while at < chars.len() {
        let size = 1 + rng.below(4);
        let piece: String = chars.iter().skip(at).take(size).collect();
        seg.push(&piece, &mut out);
        assert!(
            seg.pending_chars() <= bound,
            "pending {} over bound {bound}",
            seg.pending_chars()
        );
        at += size;
    }
    seg.finish(&mut out);
    out
}

#[test]
fn random_text_gives_the_same_chunks_however_it_is_split() {
    // Tokens that exercise every rule: terminators, closers, clauses, dashes, numbers, URLs,
    // acronyms, Turkish letters, emoji sequences, combining marks, long and multi-line spaces.
    const TOKENS: &[&str] = &[
        "a",
        "Bu",
        "x",
        "Dr",
        "Mr",
        "I",
        "ı",
        "İ",
        "ş",
        "doç",
        " ",
        "  ",
        "\n",
        "\t",
        ".",
        "!",
        "?",
        "…",
        "...",
        ",",
        ";",
        ":",
        " - ",
        "-",
        "\"",
        ")",
        "*",
        "_",
        "3",
        "14",
        "1.000",
        "10:30",
        "e.g",
        "E.U",
        "https://x.co/a",
        "@",
        "é\u{301}",
        "👨\u{200d}👩",
        "🇹🇷",
        "No",
        "2",
        "Mrs.",
        "“",
        "”",
        "’",
        "»",
        "`",
        "]",
        "ب\u{64E}",
        "क\u{93F}",
    ];
    let mut rng = Lcg(0x5EED_0035);
    for case in 0..1500 {
        let mut input = String::new();
        let length = 1 + rng.below(40);
        for _ in 0..length {
            input.push_str(TOKENS.get(rng.below(TOKENS.len())).copied().unwrap_or(" "));
        }
        let language = if case % 2 == 0 { EN } else { TR };
        let max = [1, 2, 3, 5, 8, 20][rng.below(6)];
        let min_words = [0, 1, 3][rng.below(3)];
        let config = SegmenterConfig {
            language,
            first_min_words: min_words,
            max_chunk_chars: max,
        };
        let whole = run_whole(config, &input);
        for n in 1..=3 {
            assert_eq!(
                run_pieces(config, &input, n),
                whole,
                "{n}-char pieces, max {max}: {input:?}"
            );
        }
        assert_eq!(
            run_random_split(config, &input, &mut rng),
            whole,
            "random split, max {max}: {input:?}"
        );
        // No content is lost or duplicated; only whitespace may change.
        let strip = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        let joined: String = whole.concat();
        assert_eq!(strip(&joined), strip(&input), "content changed: {input:?}");
        for chunk in &whole {
            assert!(
                !chunk.is_empty() && chunk.trim() == chunk,
                "chunk {chunk:?}"
            );
        }
    }
}

#[test]
fn long_token_does_not_stall_the_rest_of_the_stream() {
    let config = SegmenterConfig {
        max_chunk_chars: 20,
        ..SegmenterConfig::new(EN)
    };
    let url = format!("https://example.com/{}", "a".repeat(32));
    let input = format!(
        "Here is the link: {url} and then some more words follow. Next sentence here. Last one."
    );
    let mut seg = Segmenter::new(config);
    let mut early = Vec::new();
    for piece in input.as_bytes().chunks(9) {
        seg.push(std::str::from_utf8(piece).unwrap_or(""), &mut early);
    }
    // The stream keeps flowing before finish(): the link, the words and the sentences are out.
    assert_eq!(
        early,
        [
            "Here is the link:",
            url.as_str(),
            "and then some more",
            "words follow.",
            "Next sentence here.",
        ]
    );
    let mut rest = Vec::new();
    seg.finish(&mut rest);
    assert_eq!(rest, ["Last one."]);
    assert_eq!(run_pieces(config, &input, 1), run_whole(config, &input));
}

#[test]
fn whitespace_tail_keeps_pending_text_bounded_and_newlines_count() {
    let config = SegmenterConfig::new(EN);
    let bound = config.max_chunk_chars * 4 + 2;
    let mut seg = Segmenter::new(config);
    let mut out = Vec::new();
    seg.push("Alpha beta gamma", &mut out);
    let spaces = " ".repeat(1000);
    for _ in 0..100 {
        seg.push(&spaces, &mut out);
        assert!(
            seg.pending_chars() <= bound,
            "pending {}",
            seg.pending_chars()
        );
    }
    seg.push("\nDelta epsilon.", &mut out);
    seg.finish(&mut out);
    assert_eq!(out, ["Alpha beta gamma", "Delta epsilon."]);
}

#[test]
fn long_whitespace_free_stream_is_cut_inside_tokens_with_bounded_memory() {
    let config = SegmenterConfig::new(EN);
    let bound = config.max_chunk_chars * 4 + 2;
    let input: String = "x".repeat(20_000);
    let mut seg = Segmenter::new(config);
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    for piece in chars.chunks(4) {
        let delta: String = piece.iter().collect();
        seg.push(&delta, &mut out);
        assert!(
            seg.pending_chars() <= bound,
            "pending {}",
            seg.pending_chars()
        );
    }
    seg.finish(&mut out);
    assert!(
        out.iter()
            .all(|c| c.chars().count() <= config.max_chunk_chars * 4)
    );
    assert_eq!(out.concat(), input);
    assert!(
        out.len() >= 20,
        "expected hard-capped chunks, got {}",
        out.len()
    );
}

#[test]
fn hard_cap_cut_never_splits_a_zwj_emoji_sequence() {
    let config = SegmenterConfig {
        max_chunk_chars: 2,
        first_min_words: 0,
        ..SegmenterConfig::new(EN)
    };
    let family = "👨\u{200d}👩\u{200d}👧";
    let input = format!("aaaaa{family}bbbbbbbbbbbbbbbbbbbbbbb");
    let got = run_whole(config, &input);
    assert!(
        got.iter().any(|c| c.contains(family)),
        "family emoji split: {got:?}"
    );
    assert!(got.iter().all(|c| !c.ends_with('\u{200d}')));
    assert_eq!(got.concat(), input);
    assert_eq!(run_pieces(config, &input, 1), got);
}

#[test]
fn table_whole_and_split_outputs_match_expected() {
    assert!(TABLE.len() >= 40, "table has {} rows", TABLE.len());
    for (language, input, expected) in TABLE {
        let config = SegmenterConfig::new(*language);
        let want: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(run_whole(config, input), want, "whole push: {input:?}");
        for n in 1..=3 {
            assert_eq!(
                run_pieces(config, input, n),
                want,
                "{n}-char pieces: {input:?}"
            );
        }
    }
}

#[test]
fn chunk_edges_are_trimmed() {
    let config = SegmenterConfig::new(EN);
    let got = run_whole(config, "   Hello there friend.   Bye now.   ");
    assert_eq!(got, ["Hello there friend.", "Bye now."]);
}

#[test]
fn default_config_matches_spec() {
    let config = SegmenterConfig::default();
    assert_eq!(config.language, EN);
    assert_eq!(config.first_min_words, 3);
    assert_eq!(config.max_chunk_chars, 250);
}

#[test]
fn lookahead_waits_for_next_non_space_character() {
    let mut seg = Segmenter::new(SegmenterConfig::new(EN));
    let mut out = Vec::new();
    seg.push("Hello there friend.", &mut out);
    assert!(out.is_empty(), "period at the end of a delta must wait");
    seg.push(" ", &mut out);
    assert!(
        out.is_empty(),
        "whitespace alone does not decide the period"
    );
    seg.push("W", &mut out);
    assert_eq!(out, ["Hello there friend."]);
    seg.finish(&mut out);
    assert_eq!(out, ["Hello there friend.", "W"]);
}

#[test]
fn abbreviation_followed_by_digit_is_not_split_across_deltas() {
    let mut seg = Segmenter::new(SegmenterConfig::new(EN));
    let mut out = Vec::new();
    for delta in ["Pay 3", ".", "14", " dollars", "."] {
        seg.push(delta, &mut out);
    }
    assert!(out.is_empty());
    seg.push(" Next", &mut out);
    assert_eq!(out, ["Pay 3.14 dollars."]);
}

#[test]
fn first_min_words_zero_disables_the_rule() {
    let config = SegmenterConfig {
        first_min_words: 0,
        ..SegmenterConfig::new(EN)
    };
    assert_eq!(
        run_whole(config, "Hi. Bye there now."),
        ["Hi.", "Bye there now."]
    );
}

#[test]
fn later_chunks_end_only_at_sentence_ends() {
    let config = SegmenterConfig::new(EN);
    let text = "Alpha beta gamma delta. Epsilon zeta, eta theta, iota kappa, lambda mu.";
    assert_eq!(
        run_whole(config, text),
        [
            "Alpha beta gamma delta.",
            "Epsilon zeta, eta theta, iota kappa, lambda mu."
        ]
    );
}

#[test]
fn over_limit_cuts_at_clause_then_whitespace_within_limit() {
    let config = SegmenterConfig {
        max_chunk_chars: 30,
        ..SegmenterConfig::new(EN)
    };
    let text = "Alpha beta gamma delta. Epsilon zeta, eta theta iota kappa lambda mu nu.";
    let got = run_whole(config, text);
    assert_eq!(
        got,
        [
            "Alpha beta gamma delta.",
            "Epsilon zeta,",
            "eta theta iota kappa lambda mu",
            "nu.",
        ]
    );
    for chunk in &got {
        assert!(chunk.chars().count() <= 30, "chunk too long: {chunk:?}");
    }
    assert_eq!(run_pieces(config, text, 1), got);
    assert_eq!(run_pieces(config, text, 3), got);
}

#[test]
fn over_limit_without_whitespace_keeps_the_token_whole() {
    let config = SegmenterConfig {
        max_chunk_chars: 8,
        ..SegmenterConfig::new(EN)
    };
    let token = "ab👨\u{200d}👩\u{200d}👧cdefgh";
    let got = run_whole(config, &format!("Hi there {token}"));
    assert_eq!(got, ["Hi there", token]);
    assert_eq!(run_pieces(config, &format!("Hi there {token}"), 1), got);
}

#[test]
fn reset_discards_pending_text_and_restarts_the_first_chunk_rule() {
    let mut seg = Segmenter::new(SegmenterConfig::new(EN));
    let mut out = Vec::new();
    seg.push("Hello there friend. Half a sent", &mut out);
    assert_eq!(out, ["Hello there friend."]);
    seg.reset();
    seg.push("Hi there. New start here.", &mut out);
    seg.finish(&mut out);
    assert_eq!(out, ["Hello there friend.", "Hi there. New start here."]);
}

#[test]
fn finish_resets_for_the_next_stream() {
    let mut seg = Segmenter::new(SegmenterConfig::new(EN));
    let mut out = Vec::new();
    seg.push("Hello there friend. Then", &mut out);
    seg.finish(&mut out);
    assert_eq!(out, ["Hello there friend.", "Then"]);
    out.clear();
    seg.push("Hi. How are you?", &mut out);
    seg.finish(&mut out);
    assert_eq!(out, ["Hi. How are you?"]);
}

/// Feeds tokens one every 30 ms.
fn stream(tokens: &[&'static str]) -> Vec<(&'static str, Duration)> {
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, Duration::from_millis(30 * i as u64)))
        .collect()
}

/// A realistic answer and the index of the token that completes its first eligible boundary.
struct LatencyCase {
    language: Language,
    tokens: &'static [&'static str],
    boundary: usize,
    first_chunk: &'static str,
}

const LATENCY_CASES: &[LatencyCase] = &[
    LatencyCase {
        language: EN,
        tokens: &[
            "Sure", ",", " I", " can", " help", " you", " with", " that", ".", " Let", " me",
            " know", " what", " you", " need", ".",
        ],
        boundary: 8,
        first_chunk: "Sure, I can help you with that.",
    },
    LatencyCase {
        language: EN,
        tokens: &[
            "The",
            " forecast",
            " is",
            " rain",
            ",",
            " so",
            " bring",
            " an",
            " umbrella",
            ".",
            " Stay",
            " dry",
            ".",
        ],
        boundary: 4,
        first_chunk: "The forecast is rain,",
    },
    LatencyCase {
        language: EN,
        tokens: &[
            "Hi", "!", " How", " can", " I", " help", " today", "?", " Tell", " me",
        ],
        boundary: 7,
        first_chunk: "Hi! How can I help today?",
    },
    LatencyCase {
        language: TR,
        tokens: &[
            "Merhaba",
            ",",
            " bugün",
            " size",
            " nasıl",
            " yardımcı",
            " olabilirim",
            "?",
            " Lütfen",
            " sorunuzu",
            " yazın",
            ".",
        ],
        boundary: 7,
        first_chunk: "Merhaba, bugün size nasıl yardımcı olabilirim?",
    },
    LatencyCase {
        language: TR,
        tokens: &[
            "Tabii",
            " ki",
            ",",
            " hemen",
            " bakıyorum",
            ".",
            " Bir",
            " saniye",
            " lütfen",
            ".",
        ],
        boundary: 5,
        first_chunk: "Tabii ki, hemen bakıyorum.",
    },
    LatencyCase {
        language: TR,
        tokens: &[
            "Toplam",
            " 1",
            ".",
            "000",
            " kişi",
            " geldi",
            ".",
            " Hoş",
            " geldiniz",
        ],
        boundary: 6,
        first_chunk: "Toplam 1.000 kişi geldi.",
    },
    LatencyCase {
        language: TR,
        tokens: &[
            "Oğlum",
            " 3",
            ".",
            " sinif",
            " öğrencisi",
            ".",
            " Harika",
            " tebrik",
            " ederim",
        ],
        boundary: 5,
        first_chunk: "Oğlum 3. sinif öğrencisi.",
    },
    LatencyCase {
        language: EN,
        tokens: &[
            "**", "Warning", ":", " this", " is", " bad", ".**", " Do", " not", " run", " it",
            " today", ".",
        ],
        boundary: 6,
        first_chunk: "**Warning: this is bad.**",
    },
];

#[test]
fn first_chunk_is_emitted_within_one_token_of_the_first_eligible_boundary() {
    for case in LATENCY_CASES {
        let chunks = timed_chunks(SegmenterConfig::new(case.language), &stream(case.tokens));
        let first = chunks.first().expect("a first chunk");
        assert_eq!(first.text, case.first_chunk, "tokens {:?}", case.tokens);
        assert!(
            !first.from_finish,
            "first chunk must come from a token, not from finish"
        );
        // Emitted no earlier than the token that completes the boundary, and at most one token
        // later (the lookahead needs the next non-space character).
        assert!(
            first.token_index >= case.boundary && first.token_index <= case.boundary + 1,
            "first chunk at token {} for boundary {} ({:?})",
            first.token_index,
            case.boundary,
            case.tokens
        );
        let boundary_at = Duration::from_millis(30 * case.boundary as u64);
        let latency = first.at.saturating_sub(boundary_at);
        assert!(latency <= Duration::from_millis(30), "latency {latency:?}");
    }
}

#[test]
fn later_chunks_follow_the_first_one_without_extra_delay() {
    let tokens = [
        "Sure", ",", " I", " can", " help", ".", " Next", " we", " look", " at", " prices", ",",
        " then", " we", " decide", ".", " Done", ".",
    ];
    let chunks = timed_chunks(SegmenterConfig::new(EN), &stream(&tokens));
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "Sure, I can help.",
            "Next we look at prices, then we decide.",
            "Done."
        ]
    );
    // The second chunk ends at the sentence end "decide." (token 15) and is released by the next
    // token (" Done", index 16): one token of lookahead, no more.
    let second = &chunks[1];
    assert_eq!(second.token_index, 16);
    assert!(!second.from_finish);
    assert!(chunks[2].from_finish);
}
