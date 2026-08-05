//! CLDR plural categories.
//!
//! Arabic is the reason this module exists. English has two forms and tempts
//! everyone into `if n == 1 { "item" } else { "items" }`. Arabic has **six**,
//! and the rule is not "big numbers are different" — it depends on `n % 100`, so
//! 3 and 103 take one form while 11 and 111 take another. No amount of care with
//! ad-hoc conditionals gets this right; the rules have to be implemented.
//!
//! Source: Unicode CLDR plural rules, cardinal.

use serde::{Deserialize, Serialize};

/// A CLDR plural category.
///
/// Not every language uses every category. English uses `One` and `Other`;
/// Arabic uses all six.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plural {
    Zero,
    One,
    Two,
    Few,
    Many,
    Other,
}

impl Plural {
    /// Every category, for exhaustiveness checks in tests and tooling.
    pub const ALL: [Self; 6] = [
        Self::Zero,
        Self::One,
        Self::Two,
        Self::Few,
        Self::Many,
        Self::Other,
    ];
}

/// English: `one` for exactly 1, `other` for everything else including 0.
pub(crate) const fn english(n: i64) -> Plural {
    if n == 1 { Plural::One } else { Plural::Other }
}

/// Arabic, per CLDR:
///
/// ```text
/// zero  n = 0
/// one   n = 1
/// two   n = 2
/// few   n % 100 = 3..=10
/// many  n % 100 = 11..=99
/// other everything else  (100, 101, 102, 200, …)
/// ```
///
/// Negative counts are classified by magnitude — a count is a quantity, and
/// "-3 items" should read like "3 items" grammatically.
pub(crate) const fn arabic(n: i64) -> Plural {
    let magnitude = n.unsigned_abs();
    match magnitude {
        0 => Plural::Zero,
        1 => Plural::One,
        2 => Plural::Two,
        _ => match magnitude % 100 {
            3..=10 => Plural::Few,
            11..=99 => Plural::Many,
            _ => Plural::Other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_has_two_forms() {
        assert_eq!(english(1), Plural::One);
        for n in [0, 2, 3, 11, 100, -1] {
            assert_eq!(english(n), Plural::Other, "english({n})");
        }
    }

    /// The cases that a hand-rolled `if n == 1` gets wrong. Values taken from
    /// the CLDR specification rather than intuition.
    #[test]
    fn arabic_follows_cldr() {
        assert_eq!(arabic(0), Plural::Zero);
        assert_eq!(arabic(1), Plural::One);
        assert_eq!(arabic(2), Plural::Two);

        // few: n % 100 in 3..=10
        for n in [3, 4, 10, 103, 110, 1003] {
            assert_eq!(arabic(n), Plural::Few, "arabic({n}) should be few");
        }

        // many: n % 100 in 11..=99
        for n in [11, 26, 99, 111, 199, 1099] {
            assert_eq!(arabic(n), Plural::Many, "arabic({n}) should be many");
        }

        // other: n % 100 in 0..=2 but n > 2
        for n in [100, 101, 102, 200, 1000, 1002] {
            assert_eq!(arabic(n), Plural::Other, "arabic({n}) should be other");
        }
    }

    #[test]
    fn negatives_are_classified_by_magnitude() {
        assert_eq!(arabic(-1), Plural::One);
        assert_eq!(arabic(-3), Plural::Few);
        assert_eq!(arabic(-11), Plural::Many);
        // And the extreme value must not panic — `abs()` on i64::MIN does.
        let _ = arabic(i64::MIN);
    }

    #[test]
    fn the_boundaries_are_where_cldr_puts_them() {
        assert_eq!(arabic(2), Plural::Two);
        assert_eq!(arabic(3), Plural::Few); // few starts at 3
        assert_eq!(arabic(10), Plural::Few); // few ends at 10
        assert_eq!(arabic(11), Plural::Many); // many starts at 11
        assert_eq!(arabic(99), Plural::Many); // many ends at 99
        assert_eq!(arabic(100), Plural::Other); // other resumes at 100
    }
}
