//! A phone number, the one way this system writes one.
//!
//! # Why this is a kernel type
//!
//! Three places used to normalise a number — the control plane's one-time
//! codes, the booking module's verification and the SMS transport — and each
//! did it slightly differently. The transport refused the dashes the other two
//! stripped, so a number a customer typed as `+966-50-000-0000` was accepted at
//! the door and dead-lettered at the gateway. One rule, here, and every caller
//! is the same rule.
//!
//! # The rule
//!
//! E.164: a `+`, a country code that does not start with zero, and eight to
//! fifteen digits in all. Punctuation people type — spaces, dashes, brackets,
//! dots, a non-breaking space pasted from a web page — is stripped, and a
//! leading `00` becomes `+`, because that is how most of the world writes an
//! international prefix and refusing it is refusing a correct number.
//!
//! **A national number is refused, not repaired.** `0500000000` is a Saudi
//! number to a Saudi reader and nothing at all to a message gateway, and
//! inventing a country code sends somebody else's phone a code.

/// A number, as E.164 (`+966500000000`), or nothing.
#[must_use]
pub fn normalise(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.' | '\u{a0}'))
        .collect();

    let digits = cleaned
        .strip_prefix('+')
        .or_else(|| cleaned.strip_prefix("00"))?;

    if digits.len() < 8 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if digits.starts_with('0') {
        // A country code never starts with zero, and `+0…` is somebody who
        // pasted a national number after a plus.
        return None;
    }
    Some(format!("+{digits}"))
}

/// The digits alone, as a number — what a gateway that wants
/// "international format without `00` or `+`" is given.
///
/// Fifteen digits is E.164's ceiling, which is also what keeps this inside a
/// `u64`.
#[must_use]
pub fn msisdn(raw: &str) -> Option<u64> {
    normalise(raw)?.trim_start_matches('+').parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_is_normalised_the_way_people_write_one() {
        for written in [
            "+966 50 000 0000",
            "+966-50-000-0000",
            "00966500000000",
            " +966500000000 ",
            "+966 (50) 000.0000",
            "+966\u{a0}50\u{a0}000\u{a0}0000",
        ] {
            assert_eq!(
                normalise(written).as_deref(),
                Some("+966500000000"),
                "{written:?}"
            );
        }
        assert_eq!(msisdn("+966-50-000-0000"), Some(966_500_000_000));
        assert_eq!(msisdn("00966500000000"), Some(966_500_000_000));
    }

    /// **A national number is refused, not guessed at.** The commonest
    /// mistake, and accepting it would sign somebody into the wrong country's
    /// account or send a code to the wrong phone.
    #[test]
    fn what_is_not_a_number_is_refused_rather_than_repaired() {
        for wrong in [
            "0500000000",
            "+0500000000",
            "500000000",
            "966500000000",
            "+0966500000000",
            "+966",
            "+1234",
            "+9665000000000000000",
            "+966abc0000000",
            "+96650000000a",
            "not a number",
            "",
        ] {
            assert_eq!(normalise(wrong), None, "{wrong:?}");
            assert_eq!(msisdn(wrong), None, "{wrong:?}");
        }
    }
}
