//! The Turkish backchannel defaults of the interruption policy: the extended phrase list,
//! the Turkish casing and spelling cases that matter (dotted and dotless i), repetitions and mixes,
//! and the words that must keep interrupting the bot.

use brisvox_agent::{BackchannelSet, InterruptionPolicy, Language};

/// Texts that are only listener sounds, acknowledgements, fillers or reactions: the bot keeps
/// talking.
const BACKCHANNELS: &[&str] = &[
    // The original defaults.
    "evet",
    "Evet.",
    "hı-hı",
    "Hı hı",
    "tamam",
    "Tamam!",
    "tamamdır",
    "aynen",
    "anladım",
    "anlıyorum",
    "peki",
    "tabii",
    "tabi",
    "öyle mi",
    "Öyle mi?",
    "he",
    "mhm",
    // Listener sounds as STT spells them.
    "ha",
    "Haa",
    "hıı",
    "hım",
    "Hımm...",
    "ımm",
    "Mmm.",
    "aha",
    "aa",
    "Ah",
    "oh",
    "Ohh!",
    "Ay",
    "of",
    // Fillers while thinking.
    "ee",
    "eee",
    "ıı",
    "ııı",
    "şey",
    "Şey...",
    "yani",
    // Acknowledgements.
    "olur",
    "Olur.",
    "doğru",
    "tabii ki",
    "Tabii ki.",
    "tabi ki",
    "elbette",
    "anlaşıldı",
    "anlaşılıyor",
    "eyvallah",
    "peki tamam",
    // Reactions.
    "gerçekten mi",
    "Gerçekten mi?",
    "sahi mi",
    "Ciddi mi?",
    "demek öyle",
    "vay",
    "yok artık",
    "hadi ya",
    "vay be",
    "harika",
    "Güzel.",
    "süper",
    "iyi",
    "İyi.",
    // Repetitions and mixes of the above.
    "evet evet",
    "hı hı hı",
    "tamam tamam tamam",
    "evet, tamam, anladım",
    "ya öyle mi",
    "aynen aynen",
    "harika, süper",
    "tamam anladım",
    "peki olur",
    // Mixed languages: every piece is a backchannel of one of the configured sets.
    "yeah tamam",
    "okay evet",
];

/// Texts that stop, correct, answer or ask: the bot must stop for them.
const SUBSTANTIVE: &[&str] = &[
    "hayır",
    "Hayır.",
    "olmaz",
    "yok",
    "dur",
    "bekle",
    "bir dakika",
    "dinle",
    "tamam dur",
    "evet ama",
    "iyi değil",
    "güzel ama pahalı",
    "harika fikir ama",
    "yani şunu söyleyeyim",
    "şey var",
    "doğru değil",
    "olur mu",
    "gerçekten mi dedin",
    "teşekkürler",
    "sağ ol",
    "tekrar eder misin",
    "ne dedin",
    "tamam şimdi ben konuşayım",
    // The dotless i is its own letter: "hi hi" is English and not Turkish "hı hı".
    "hi hi",
    "",
    "...",
];

#[test]
fn turkish_backchannels_do_not_interrupt() {
    let policy = InterruptionPolicy::recommended();
    for text in BACKCHANNELS {
        assert!(
            policy.is_only_backchannel(text),
            "should be a backchannel: {text:?}"
        );
    }
}

#[test]
fn substantive_turkish_still_interrupts() {
    let policy = InterruptionPolicy::recommended();
    for text in SUBSTANTIVE {
        assert!(
            !policy.is_only_backchannel(text),
            "must interrupt: {text:?}"
        );
    }
}

/// Every phrase of the Turkish defaults, as listed in `docs/TURKISH.md` section 2 (the original
/// 19, then the four groups added later). The set must hold exactly these.
const TURKISH_DEFAULTS: [&str; 60] = [
    // The original 19.
    "evet",
    "hı-hı",
    "hıhı",
    "hı",
    "he",
    "hm",
    "hmm",
    "mhm",
    "mm",
    "tamam",
    "tamamdır",
    "aynen",
    "anladım",
    "anlıyorum",
    "peki",
    "tabii",
    "tabi",
    "öyle mi",
    "ya",
    // Listener sounds as STT spells them.
    "ha",
    "haa",
    "hıı",
    "hım",
    "hımm",
    "ımm",
    "mmm",
    "aha",
    "aa",
    "ah",
    "oh",
    "ohh",
    "ay",
    "of",
    // Fillers while thinking.
    "ee",
    "eee",
    "ıı",
    "ııı",
    "şey",
    "yani",
    // Acknowledgements.
    "olur",
    "doğru",
    "tabii ki",
    "tabi ki",
    "elbette",
    "anlaşıldı",
    "anlaşılıyor",
    "eyvallah",
    "peki tamam",
    // Reactions.
    "gerçekten mi",
    "sahi mi",
    "ciddi mi",
    "yok artık",
    "hadi ya",
    "demek öyle",
    "vay",
    "vay be",
    "harika",
    "güzel",
    "süper",
    "iyi",
];

#[test]
fn the_turkish_set_alone_agrees_on_turkish_text() {
    let tr = BackchannelSet::turkish();
    for phrase in [
        "hayır",
        "dur",
        "olmaz",
        "bekle",
        "teşekkürler",
        "sağ ol",
        "yok",
    ] {
        assert!(!tr.contains(phrase), "{phrase}");
    }
    assert_eq!(tr.language(), Language::Tr);
}

#[test]
fn the_turkish_defaults_are_exactly_the_documented_sixty_phrases() {
    let tr = BackchannelSet::turkish();
    // Every documented phrase is in the set: a typo (`sahi mı`, `demek oyle`) or a phrase
    // swapped for another one fails here.
    for phrase in TURKISH_DEFAULTS {
        assert!(tr.contains(phrase), "missing from the defaults: {phrase:?}");
    }
    // The documented list itself has no duplicates (so 60 distinct phrases are contained) ...
    let distinct: std::collections::HashSet<&str> = TURKISH_DEFAULTS.into_iter().collect();
    assert_eq!(distinct.len(), TURKISH_DEFAULTS.len());
    // ... and the set holds nothing else and no phrase twice: a silent removal, an addition
    // and a duplicate all change the count.
    assert_eq!(tr.len(), TURKISH_DEFAULTS.len(), "{}", tr.len());
    assert_eq!(tr.len(), 60);
}

#[test]
fn turkish_capital_i_folds_to_dotted_i_and_dotless_stays_distinct() {
    let policy = InterruptionPolicy::recommended();
    // Capital dotted İ lower-cases to "i": "İyi" is "iyi".
    assert!(policy.is_only_backchannel("İYİ"));
    // STT output in capitals: "HIHI" is not "hıhı" because capital I lower-cases to "i".
    assert!(!policy.is_only_backchannel("HIHI"));
    assert!(policy.is_only_backchannel("hıhı"));
}
