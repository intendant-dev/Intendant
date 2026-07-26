//! Export-time content redaction for the Track NS prose lane (ruled R8).
//!
//! Nothing else in the tree scrubs secret VALUES out of transcript text —
//! `intendant_core::env_scrub` classifies env-var NAMES at spawn
//! boundaries only. This module is the value-level pass the prose export
//! applies before any byte leaves the raw stores: best-effort pattern
//! redaction, replacement `[REDACTED:<class>]`, per-class counts surfaced
//! so the export header and the digest journal can report honestly.
//! Best-effort is the ruled posture: novel secret shapes can survive; the
//! artifacts stay in the same `~/.intendant` trust domain as the raw
//! transcripts themselves.

use regex::Regex;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// One redaction pass over a message text: the scrubbed text plus how
/// many spans each class replaced (empty map = untouched input).
pub(crate) struct Redaction {
    pub text: String,
    pub counts: BTreeMap<&'static str, u32>,
}

fn marker(class: &'static str) -> String {
    format!("[REDACTED:{class}]")
}

/// Whole-match value rules, applied in declaration order (PEM first so a
/// key block is one redaction, not a spray of inner-pattern hits).
static WHOLE_MATCH_RULES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        (
            "pem-key",
            // (?s): private-key blocks span lines. Lazy body: a stray
            // BEGIN without an END matches nothing (linear scan — this
            // engine has no backtracking blowup, and a counted bound here
            // would exceed the compiled-size limit).
            Regex::new(
                r"(?s)-----BEGIN [A-Z0-9 ]{0,48}PRIVATE KEY-----.*?-----END [A-Z0-9 ]{0,48}PRIVATE KEY-----",
            )
            .expect("static regex"),
        ),
        (
            "aws-key",
            Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").expect("static regex"),
        ),
        (
            "provider-key",
            // sk- covers OpenAI-style and sk-ant- provider keys alike.
            Regex::new(r"\bsk-[A-Za-z0-9_-]{16,}\b").expect("static regex"),
        ),
        (
            "github-token",
            Regex::new(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}\b|\bgithub_pat_[A-Za-z0-9_]{20,}\b")
                .expect("static regex"),
        ),
        (
            "slack-token",
            Regex::new(r"\bxox[abposr]-[A-Za-z0-9-]{8,}\b").expect("static regex"),
        ),
        (
            "jwt",
            Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
                .expect("static regex"),
        ),
        (
            "bearer",
            Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{16,}").expect("static regex"),
        ),
        (
            "claim-code",
            // Connect claim codes render as twelve hyphen-joined words.
            // Only the hyphen-joined form is matched: twelve consecutive
            // space-separated words is ordinary prose.
            Regex::new(r"\b(?:[a-z]{3,12}-){11}[a-z]{3,12}\b").expect("static regex"),
        ),
    ]
});

/// `NAME=value` / `key: value` rules where only the VALUE span is
/// replaced. The value class excludes `[` so a marker from an earlier
/// pass is never re-redacted.
static ENV_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b([A-Z][A-Z0-9_]{2,63})=([^\s"'\[]{8,})"#).expect("static regex")
});
static KV_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(password|passwd|secret|api[_-]?key|access[_-]?key|auth[_-]?token)(["']?\s*[:=]\s*["']?)([^\s"'\[]{8,})"#,
    )
    .expect("static regex")
});

/// Scrub one message text. Returns the (possibly unchanged) text plus
/// per-class replacement counts.
pub(crate) fn redact_text(input: &str) -> Redaction {
    let mut counts: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut text: Cow<'_, str> = Cow::Borrowed(input);

    for (class, regex) in WHOLE_MATCH_RULES.iter() {
        let hits = regex.find_iter(&text).count() as u32;
        if hits > 0 {
            *counts.entry(class).or_insert(0) += hits;
            text = Cow::Owned(regex.replace_all(&text, marker(class)).into_owned());
        }
    }

    // Ambient-credential env assignments (`AWS_SECRET_ACCESS_KEY=…`):
    // the shipped NAME classifier decides, the value span is replaced.
    // Runs before the generic kv rule so the more specific class wins.
    let env_hits = ENV_ASSIGN
        .captures_iter(&text)
        .filter(|caps| intendant_core::env_scrub::is_ambient_credential_env(&caps[1]))
        .count() as u32;
    if env_hits > 0 {
        counts.insert("env-credential", env_hits);
        let replaced = ENV_ASSIGN.replace_all(&text, |caps: &regex::Captures<'_>| {
            if intendant_core::env_scrub::is_ambient_credential_env(&caps[1]) {
                format!("{}={}", &caps[1], marker("env-credential"))
            } else {
                caps[0].to_string()
            }
        });
        text = Cow::Owned(replaced.into_owned());
    }

    let kv_hits = KV_SECRET.find_iter(&text).count() as u32;
    if kv_hits > 0 {
        counts.insert("kv-secret", kv_hits);
        let replaced = KV_SECRET.replace_all(&text, |caps: &regex::Captures<'_>| {
            format!("{}{}{}", &caps[1], &caps[2], marker("kv-secret"))
        });
        text = Cow::Owned(replaced.into_owned());
    }

    Redaction {
        text: text.into_owned(),
        counts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(redaction: &Redaction) -> Vec<&'static str> {
        redaction.counts.keys().copied().collect()
    }

    #[test]
    fn value_patterns_redact_with_class_markers() {
        let cases: &[(&str, &str)] = &[
            ("key AKIAABCDEFGHIJKLMNOP in prose", "aws-key"),
            ("token sk-abc123def456ghi789jkl here", "provider-key"),
            (
                "pat ghp_abcdefghijklmnopqrstuvwxyz0123456789 ok",
                "github-token",
            ),
            ("hook xoxb-1234567890-abcdef done", "slack-token"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKKF2QT4fwpM",
                "jwt",
            ),
            ("Authorization: Bearer abcdef1234567890abcdef", "bearer"),
        ];
        for (input, class) in cases {
            let out = redact_text(input);
            assert_eq!(&classes(&out), &[*class], "input: {input}");
            assert!(
                out.text.contains(&format!("[REDACTED:{class}]")),
                "marker missing for {input}: {}",
                out.text
            );
        }
    }

    #[test]
    fn pem_block_is_one_redaction() {
        let input = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let out = redact_text(input);
        assert_eq!(out.counts.get("pem-key"), Some(&1));
        assert!(out.text.starts_with("before\n[REDACTED:pem-key]"));
        assert!(out.text.ends_with("after"));
    }

    #[test]
    fn ambient_env_assignment_redacts_value_only() {
        let out = redact_text("run with AWS_SECRET_ACCESS_KEY=abc123def456 please");
        assert_eq!(out.counts.get("env-credential"), Some(&1));
        assert_eq!(
            out.text,
            "run with AWS_SECRET_ACCESS_KEY=[REDACTED:env-credential] please"
        );
    }

    #[test]
    fn kv_secret_keeps_key_and_separator() {
        let out = redact_text("set password=hunter2hunter2 now");
        assert_eq!(out.counts.get("kv-secret"), Some(&1));
        assert_eq!(out.text, "set password=[REDACTED:kv-secret] now");
    }

    #[test]
    fn twelve_hyphen_joined_words_redact_but_prose_does_not() {
        let code =
            "code apple-brave-cider-delta-eagle-frost-grape-house-igloo-jolly-kite-lemon end";
        let out = redact_text(code);
        assert_eq!(out.counts.get("claim-code"), Some(&1));

        let prose = "the twelve words were lovely and nobody hyphenated anything at all today";
        let untouched = redact_text(prose);
        assert!(untouched.counts.is_empty());
        assert_eq!(untouched.text, prose);
    }

    #[test]
    fn benign_text_passes_untouched() {
        for benign in [
            "my password is strong and I will not share it",
            "INTENDANT_SESSION_TOKEN=abcdef123456 is intendant-internal, never ambient",
            "the sk- prefix alone: sk-short is too short",
            "plain prose about tokens and secrets in general",
        ] {
            let out = redact_text(benign);
            assert!(out.counts.is_empty(), "false positive on: {benign}");
            assert_eq!(out.text, benign);
        }
    }

    #[test]
    fn markers_are_never_re_redacted() {
        let once = redact_text("password=[REDACTED:kv-secret] and AKIAABCDEFGHIJKLMNOP");
        assert_eq!(once.counts.get("kv-secret"), None, "marker not re-matched");
        assert_eq!(once.counts.get("aws-key"), Some(&1));
    }
}
