//! Guards that depend on the line: the noise-adapted policy and the bot-echo text match.

use std::time::Duration;

use brisvox_agent::{InterruptionPolicy, NoiseAdapt, echoes_bot, is_bot_echo};

const MS: Duration = Duration::from_millis(1);

#[test]
fn a_quiet_line_keeps_the_policy_and_a_noisy_one_tightens_it() {
    let base = InterruptionPolicy::recommended();
    let adapt = NoiseAdapt::default();
    assert_eq!(base.adapted_to_noise(-70.0, &adapt), base);
    assert_eq!(base.adapted_to_noise(-46.0, &adapt), base);
    let noisy = base.adapted_to_noise(-40.0, &adapt);
    assert_eq!(noisy.min_speech, base.min_speech + 200 * MS);
    assert_eq!(noisy.min_words, 1);
    // Everything else is untouched.
    assert_eq!(noisy.echo_guard, base.echo_guard);
    assert_eq!(noisy.backchannels, base.backchannels);
}

#[test]
fn a_larger_min_words_stays_and_a_disabled_policy_stays_disabled() {
    let strict = InterruptionPolicy {
        min_words: 3,
        ..InterruptionPolicy::recommended()
    };
    assert_eq!(
        strict
            .adapted_to_noise(-30.0, &NoiseAdapt::default())
            .min_words,
        3
    );
    let off = InterruptionPolicy::disabled();
    assert_eq!(off.adapted_to_noise(-30.0, &NoiseAdapt::default()), off);
}

#[test]
fn the_bots_own_words_coming_back_are_an_echo() {
    let bot = "Sure, your appointment is on Tuesday at three in the afternoon.";
    assert!(echoes_bot("your appointment is on Tuesday", bot, 3));
    // Partial words and a wrong word or two are still an echo.
    assert!(echoes_bot("your appointments on tuesday at", bot, 3));
    // Other speech is not, and neither is one word the bot happened to say.
    assert!(!echoes_bot("turn the volume up please", bot, 3));
    assert!(!echoes_bot("Tuesday", bot, 3));
    assert!(!echoes_bot("", bot, 3));
    assert!(!echoes_bot("your appointment", "", 1));
}

#[test]
fn a_short_command_is_never_taken_for_the_bots_echo_even_when_the_bot_said_it() {
    let bot = "Please wait a moment while I check. Your number is five five five one two three.";
    let young = Duration::from_millis(400);
    let old = Duration::from_millis(2_000);
    // The echo: several of the bot's words, held.
    assert!(is_bot_echo("please wait a moment while", bot, young));
    assert!(is_bot_echo("while I check your", bot, young));
    // The first transcript of an echo is often one word: held while the candidate is young,
    // but a single word of an older candidate is speech.
    assert!(is_bot_echo("check", bot, young));
    assert!(!is_bot_echo("moment", bot, old));
    // Short commands go through even when the bot just said them.
    assert!(!is_bot_echo("wait", bot, young));
    assert!(!is_bot_echo("please wait", bot, young));
    assert!(!is_bot_echo("stop", "Stop. I will stop now.", young));
    assert!(!is_bot_echo("no no", "No, no, that is not it.", young));
    // A longer sentence with the command word in it is still the echo.
    assert!(is_bot_echo("please wait a moment", bot, young));
    // An older candidate needs three words.
    assert!(!is_bot_echo("one two", bot, old));
    assert!(is_bot_echo("five five five one", bot, old));
    // A caller who says something else is not an echo, with or without shared words.
    assert!(!is_bot_echo("yes that is my number", bot, young));
}
