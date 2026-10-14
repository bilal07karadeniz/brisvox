//! Sink tests: plain bytes unchanged, the offload timeout on the injected clock, and (with the
//! `recording-encryption` feature) age round trips decrypted with the age crate.
#![allow(clippy::disallowed_methods)]

use std::sync::mpsc;

use brisvox_core::SimClock;

use super::*;

fn temp(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("brisvox-sink-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[tokio::test]
async fn a_plain_sink_writes_exactly_the_bytes_it_is_given() {
    let path = temp("plain");
    let clock = brisvox_core::SystemClock::shared();
    let timeout = Duration::from_secs(5);
    let mut sink = RecordingSink::create(&*clock, &path, timeout, None)
        .await
        .unwrap();
    assert!(!sink.is_encrypted());
    assert!(sink.plain_file().is_some());
    sink.write_all(&*clock, timeout, b"abc").await.unwrap();
    sink.write_all(&*clock, timeout, b"def\n").await.unwrap();
    sink.finish(&*clock, timeout).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef\n");
    std::fs::remove_file(&path).unwrap();
}

/// A blocked blocking-pool job (a stalled disk under the encryption) ends in a timeout counted
/// on the injected clock. The runtime is not paused: tokio's auto-advance does not wait for the
/// blocking pool, so the clock (a [`SimClock`] outside a paused runtime) follows real time here.
#[tokio::test]
async fn a_stalled_offloaded_job_times_out_on_the_injected_clock() {
    let clock = SimClock::shared();
    let (release, gate) = mpsc::channel::<()>();
    let err = offload(&*clock, Duration::from_millis(50), move || {
        let _ = gate.recv();
        Ok(())
    })
    .await
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    drop(release);
}

#[tokio::test]
async fn a_panicking_offloaded_job_is_an_error_not_a_crash() {
    let clock = brisvox_core::SystemClock::shared();
    let err = offload::<()>(&*clock, Duration::from_secs(5), || {
        #[allow(clippy::panic)]
        {
            panic!("boom");
        }
    })
    .await
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
}

#[cfg(not(feature = "recording-encryption"))]
#[test]
fn without_the_feature_recipients_are_refused_naming_the_feature() {
    let err = AgeRecipients::parse("age1anything").unwrap_err();
    assert!(err.contains("recording-encryption"), "{err}");
}

#[cfg(feature = "recording-encryption")]
mod age_tests {
    use std::io::Read as _;

    use age::secrecy::ExposeSecret as _;

    use super::*;

    fn identity() -> age::x25519::Identity {
        age::x25519::Identity::generate()
    }

    fn decrypt(bytes: &[u8], id: &age::x25519::Identity) -> Vec<u8> {
        let decryptor = age::Decryptor::new(bytes).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(id as &dyn age::Identity))
            .unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn recipients_parse_dedupe_and_reject_garbage_without_echoing_it() {
        let a = identity().to_public().to_string();
        let b = identity().to_public().to_string();
        let both = AgeRecipients::parse(&format!(" {a} , {b},{a},")).unwrap();
        assert_eq!(both.len(), 2);
        assert!(!both.is_empty());

        for bad in ["", " , ", "age1notakey", "ssh-ed25519 AAAA", "hello"] {
            assert!(AgeRecipients::parse(bad).is_err(), "{bad:?}");
        }
        let secret = identity().to_string();
        let err = AgeRecipients::parse(&format!("{a},{}", secret.expose_secret())).unwrap_err();
        assert!(err.contains("PRIVATE"), "{err}");
        assert!(
            !err.contains(secret.expose_secret()),
            "the key must not be echoed"
        );
        let lower = secret.expose_secret().to_ascii_lowercase();
        let err = AgeRecipients::parse(&lower).unwrap_err();
        assert!(err.contains("PRIVATE"), "lower case is detected too: {err}");
        assert!(!err.contains(&lower));
        let err = AgeRecipients::parse(&format!("{a},age1zzzz")).unwrap_err();
        assert!(err.contains("entry 2"), "{err}");
        assert!(!err.contains("zzzz"));

        let many: Vec<String> = (0..=MAX_AGE_RECIPIENTS)
            .map(|_| identity().to_public().to_string())
            .collect();
        assert!(AgeRecipients::parse(&many.join(",")).is_err());
    }

    #[tokio::test]
    async fn an_age_sink_round_trips_through_the_age_crate() {
        let id = identity();
        let recipients = AgeRecipients::parse(&id.to_public().to_string()).unwrap();
        let path = temp("roundtrip.age");
        let clock = brisvox_core::SystemClock::shared();
        let timeout = Duration::from_secs(10);
        let mut sink = RecordingSink::create(&*clock, &path, timeout, Some(&recipients))
            .await
            .unwrap();
        assert!(sink.is_encrypted());
        assert!(sink.plain_file().is_none());
        // More than three age chunks (64 KiB each), in uneven writes.
        let mut expected = Vec::new();
        for i in 0..40u32 {
            let piece: Vec<u8> = (0..5_000u32).map(|j| ((i + j) % 251) as u8).collect();
            sink.write_all(&*clock, timeout, &piece).await.unwrap();
            expected.extend_from_slice(&piece);
        }
        sink.finish(&*clock, timeout).await.unwrap();
        assert!(
            sink.write_all(&*clock, timeout, b"late").await.is_err(),
            "no writes after the end of the stream"
        );
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"age-encryption.org/v1\n"));
        assert_eq!(decrypt(&bytes, &id), expected);
        // Someone else's key does not open it.
        let other = identity();
        let decryptor = age::Decryptor::new(&bytes[..]).unwrap();
        assert!(
            decryptor
                .decrypt(std::iter::once(&other as &dyn age::Identity))
                .is_err()
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn a_failed_file_write_poisons_the_sink_for_writes_and_finish() {
        let id = identity();
        let recipients = AgeRecipients::parse(&id.to_public().to_string()).unwrap();
        let path = temp("poison.age");
        let clock = brisvox_core::SystemClock::shared();
        let timeout = Duration::from_secs(10);
        let mut sink = RecordingSink::create(&*clock, &path, timeout, Some(&recipients))
            .await
            .unwrap();
        // Swap in a read-only handle: the next file write fails after the encryption worked.
        let RecordingSink::Age(age_sink) = &mut sink else {
            panic!("an age sink");
        };
        age_sink.file = File::open(&path).await.unwrap();
        // More than one 64 KiB age chunk, so ciphertext is released and written.
        // tokio's file buffers a write and reports its error on a later one, so keep writing
        // until the error shows.
        let big = vec![7u8; 100_000];
        let mut failed = false;
        for _ in 0..8 {
            if sink.write_all(&*clock, timeout, &big).await.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "the read-only file must fail a write");
        assert!(
            sink.write_all(&*clock, timeout, b"more").await.is_err(),
            "no write may continue past a failed one"
        );
        assert!(
            sink.finish(&*clock, timeout).await.is_err(),
            "finish must not report Ok for a file with a hole"
        );
        assert!(
            sink.finish(&*clock, timeout).await.is_err(),
            "and not on a repeat either"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn every_recipient_can_decrypt() {
        let (a, b) = (identity(), identity());
        let recipients =
            AgeRecipients::parse(&format!("{},{}", a.to_public(), b.to_public())).unwrap();
        let path = temp("two.age");
        let clock = brisvox_core::SystemClock::shared();
        let timeout = Duration::from_secs(10);
        let mut sink = RecordingSink::create(&*clock, &path, timeout, Some(&recipients))
            .await
            .unwrap();
        sink.write_all(&*clock, timeout, b"{\"type\":\"call\"}\n")
            .await
            .unwrap();
        sink.finish(&*clock, timeout).await.unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(decrypt(&bytes, &a), b"{\"type\":\"call\"}\n");
        assert_eq!(decrypt(&bytes, &b), b"{\"type\":\"call\"}\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn an_unfinished_age_file_does_not_decrypt_to_the_end() {
        let id = identity();
        let recipients = AgeRecipients::parse(&id.to_public().to_string()).unwrap();
        let path = temp("crash.age");
        let clock = brisvox_core::SystemClock::shared();
        let timeout = Duration::from_secs(10);
        let mut sink = RecordingSink::create(&*clock, &path, timeout, Some(&recipients))
            .await
            .unwrap();
        sink.write_all(&*clock, timeout, &[1u8; 1000])
            .await
            .unwrap();
        sink.flush(&*clock, timeout).await.unwrap();
        // The process "dies" here: the stream was never finished.
        let bytes = std::fs::read(&path).unwrap();
        let decryptor = age::Decryptor::new(&bytes[..]).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap();
        let mut out = Vec::new();
        assert!(reader.read_to_end(&mut out).is_err(), "truncated stream");
        std::fs::remove_file(&path).unwrap();
    }
}
