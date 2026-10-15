//! The script of a call: the utterances of a fixture (its JSON sidecar is the ground truth) and
//! the answer to each. The same matching rule as the in-process mocks (`CallScript::responder`):
//! an exact match of the normalised text, else a prefix match.

use std::path::Path;

use serde::Deserialize;

/// One utterance of the fixture.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// What the caller says.
    pub text: String,
    /// Language of the utterance (`en`, `tr`); picks the language of the answer.
    pub lang: String,
    /// Duration of the speech in seconds (used to time the words of a transcript).
    pub seconds: f64,
}

#[derive(Deserialize)]
struct Sidecar {
    #[serde(default = "default_rate")]
    sample_rate: u32,
    segments: Vec<Segment>,
}

#[derive(Deserialize)]
struct Segment {
    #[serde(default)]
    start: u64,
    #[serde(default)]
    end: u64,
    text: String,
    #[serde(default)]
    lang: Option<String>,
}

fn default_rate() -> u32 {
    16_000
}

/// Reads `fixtures_dir/<name>.json`.
///
/// # Errors
/// A message when the file is missing or malformed, or has no segment.
pub fn load(fixtures_dir: &Path, name: &str) -> Result<Vec<Utterance>, String> {
    let path = fixtures_dir.join(format!("{name}.json"));
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

/// Parses a fixture sidecar.
///
/// # Errors
/// A message for malformed JSON or a fixture without segments.
pub fn parse(raw: &str) -> Result<Vec<Utterance>, String> {
    let sidecar: Sidecar = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if sidecar.segments.is_empty() {
        return Err("the fixture has no segments".to_owned());
    }
    let rate = f64::from(sidecar.sample_rate.max(1));
    Ok(sidecar
        .segments
        .into_iter()
        .map(|s| {
            // Segment sample counts are far below 2^53: the conversion is exact.
            #[allow(clippy::cast_precision_loss)]
            let seconds = s.end.saturating_sub(s.start) as f64 / rate;
            Utterance {
                text: s.text,
                lang: s.lang.unwrap_or_else(|| "en".to_owned()),
                seconds: seconds.max(0.2),
            }
        })
        .collect())
}

/// Lower-case words separated by single spaces, punctuation dropped.
pub fn normalize(text: &str) -> String {
    let lowered: String = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .to_lowercase();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The utterance whose text is `user_text` (or starts with it).
pub fn find<'a>(script: &'a [Utterance], user_text: &str) -> Option<&'a Utterance> {
    let user = normalize(user_text);
    if user.is_empty() {
        return None;
    }
    script
        .iter()
        .find(|u| normalize(&u.text) == user)
        .or_else(|| {
            script
                .iter()
                .find(|u| normalize(&u.text).starts_with(&user))
        })
}

/// Growing word prefixes of `text`, all but the full text.
pub fn interims_of(text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    (1..words.len())
        .map(|n| words.iter().take(n).copied().collect::<Vec<_>>().join(" "))
        .collect()
}

/// Cuts `text` into tokens the way a tokenizer shows it in a stream: a word keeps its leading
/// space, punctuation is a token of its own.
pub fn word_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            current.push(c);
            continue;
        }
        if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        if c.is_whitespace() {
            current.push(' ');
        } else {
            tokens.push(c.to_string());
        }
    }
    if !current.trim().is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDECAR: &str = r#"{"name":"x","sample_rate":16000,"samples":1,"segments":[
        {"start":8000,"end":106720,"lang":"en","text":"The U.N. also hopes to finalize a fund."},
        {"start":130720,"end":174880,"lang":"tr","text":"Ikinci cumle burada."}]}"#;

    #[test]
    fn the_sidecar_is_read() {
        let script = parse(SIDECAR).unwrap();
        assert_eq!(script.len(), 2);
        assert!((script[0].seconds - 6.17).abs() < 0.01);
        assert_eq!(script[1].lang, "tr");
        assert!(parse("{\"segments\":[]}").is_err());
        assert!(parse("nope").is_err());
    }

    #[test]
    fn matching_is_exact_then_prefix() {
        let script = parse(SIDECAR).unwrap();
        assert_eq!(
            find(&script, "the u.n. also hopes to finalize a fund")
                .unwrap()
                .lang,
            "en"
        );
        assert_eq!(find(&script, "Ikinci cumle").unwrap().lang, "tr");
        assert!(find(&script, "").is_none());
        assert!(find(&script, "something else").is_none());
    }

    #[test]
    fn interims_grow_word_by_word() {
        assert_eq!(interims_of("a b c"), ["a", "a b"]);
        assert!(interims_of("single").is_empty());
    }

    #[test]
    fn tokens_keep_their_spaces_and_rejoin() {
        let tokens = word_tokens("Okay, got it.");
        assert_eq!(tokens, ["Okay", ",", " got", " it", "."]);
        assert_eq!(tokens.concat(), "Okay, got it.");
        assert_eq!(
            word_tokens("Peki, anlad\u{131}m.").concat(),
            "Peki, anlad\u{131}m."
        );
    }
}
