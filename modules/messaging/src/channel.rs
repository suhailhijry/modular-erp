//! What a message goes out on, and what it costs to send.
//!
//! # Why segments are counted here rather than by the gateway
//!
//! Because a bill arrives a month later and a refusal arrives now. SMS is
//! **billed per segment**: 160 characters of the GSM 03.38 alphabet, or 70 of
//! anything else — which for this market means every Arabic message. A
//! 200-character Arabic reminder is three segments and costs three times what
//! somebody writing it expected, and nothing in the system would have said so.
//!
//! So the count is part of sending, the meter records it, and a budget refuses
//! against it (L6). A tenant finds out when they write the template, not when
//! the invoice comes.

use erp_types::EffectKind;
use serde::{Deserialize, Serialize};

/// How a message reaches somebody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Email,
    Sms,
    Push,
    WhatsApp,
}

impl Channel {
    pub const ALL: [Self; 4] = [Self::Email, Self::Sms, Self::Push, Self::WhatsApp];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Sms => "sms",
            Self::Push => "push",
            Self::WhatsApp => "whatsapp",
        }
    }

    /// The effect this channel's messages are enqueued under.
    ///
    /// One kind per channel, one handler per kind — so a worker deployed
    /// without an SMS gateway leaves SMS in the outbox for one that has it,
    /// rather than dead-lettering a tenant's reminders during a rollout. That
    /// is the dispatcher's existing behaviour and the reason a channel is a
    /// kind rather than a field.
    #[must_use]
    pub fn kind(self) -> EffectKind {
        EffectKind::new(match self {
            Self::Email => "email.send",
            Self::Sms => "sms.send",
            Self::Push => "push.send",
            Self::WhatsApp => "whatsapp.send",
        })
        .unwrap_or_else(|_| unreachable!("a literal that satisfies EffectKind"))
    }

    /// Whether this channel has a subject line.
    ///
    /// Email does; the other three are a body. A template that writes a subject
    /// for SMS is one whose author expected it to appear somewhere, and it
    /// would not.
    #[must_use]
    pub const fn has_a_subject(self) -> bool {
        matches!(self, Self::Email)
    }

    /// What one message on this channel costs, in billable units.
    ///
    /// Segments for SMS, one for everything else. Not a rate and not a price —
    /// what a segment costs is between the tenant and their gateway, and this
    /// system has no business holding an opinion about it.
    #[must_use]
    pub fn units(self, body: &str) -> i32 {
        match self {
            Self::Sms => segments(body),
            _ => 1,
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a channel")]
pub struct UnknownChannel(pub String);

impl std::str::FromStr for Channel {
    type Err = UnknownChannel;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| UnknownChannel(s.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// GSM 03.38, which is what decides the bill
// ---------------------------------------------------------------------------

/// The GSM 7-bit default alphabet. One septet each.
///
/// Written out rather than computed: it is a fixed table from a 1990s
/// specification and it is never going to change, and a reader checking whether
/// a character is in it can see the answer.
const GSM_BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?\
                         ¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";

/// The extension table. **Two septets each**, because they are sent as an
/// escape followed by the character — which is why a message full of square
/// brackets is shorter than it looks and a message full of them is not.
const GSM_EXTENDED: &str = "^{}\\[~]|€";

/// Characters that fit in one SMS when nothing is concatenated.
const GSM_SINGLE: i32 = 160;
/// …and per part once it is, because a concatenated message spends six septets
/// per part on the header that says which part it is.
const GSM_CONCATENATED: i32 = 153;
/// The same two numbers for UCS-2, which is every message with an Arabic
/// character in it.
const UCS2_SINGLE: i32 = 70;
const UCS2_CONCATENATED: i32 = 67;

/// How many segments a body is billed as.
///
/// **Empty is one**, not zero: a gateway charges for a message that says
/// nothing, and metering it as free would make an empty template look like a
/// way to send for nothing.
#[must_use]
pub fn segments(body: &str) -> i32 {
    let Some(septets) = gsm_septets(body) else {
        // UCS-2, and the unit is the **UTF-16 code unit** rather than the
        // character: an emoji is a surrogate pair and takes two, which is the
        // difference between 70 characters and 70 units on a message somebody
        // ended with a wave.
        let units = i32::try_from(body.encode_utf16().count()).unwrap_or(i32::MAX);
        return parts(units, UCS2_SINGLE, UCS2_CONCATENATED);
    };
    parts(septets, GSM_SINGLE, GSM_CONCATENATED)
}

/// The septet count if every character is in the GSM alphabet, else `None`.
fn gsm_septets(body: &str) -> Option<i32> {
    let mut septets: i32 = 0;
    for character in body.chars() {
        if GSM_BASIC.contains(character) {
            septets = septets.saturating_add(1);
        } else if GSM_EXTENDED.contains(character) {
            septets = septets.saturating_add(2);
        } else {
            return None;
        }
    }
    Some(septets)
}

/// Integer ceiling division, which is what a segment count is.
const fn parts(units: i32, single: i32, concatenated: i32) -> i32 {
    if units <= single {
        // One segment even at zero: a gateway charges for an empty message.
        return 1;
    }
    // `+ concatenated - 1` is the ceiling. Integer arithmetic, because
    // `float_arithmetic` is denied workspace-wide and a segment count has no
    // business being a rounded float.
    (units + concatenated - 1) / concatenated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_latin_message_is_one_segment() {
        assert_eq!(segments(""), 1, "a gateway charges for an empty message");
        assert_eq!(segments("Your appointment is at 10:00."), 1);
        assert_eq!(segments(&"a".repeat(160)), 1);
    }

    /// **The bug this file exists to prevent**: one character over, and the
    /// message costs twice.
    #[test]
    fn one_character_over_the_boundary_costs_twice() {
        assert_eq!(segments(&"a".repeat(160)), 1);
        assert_eq!(segments(&"a".repeat(161)), 2);
        assert_eq!(segments(&"a".repeat(306)), 2);
        assert_eq!(segments(&"a".repeat(307)), 3);
    }

    /// Arabic is UCS-2, and the boundary is 70 rather than 160.
    ///
    /// This is not an edge case in this market — it is every message.
    #[test]
    fn an_arabic_message_is_billed_at_seventy_characters() {
        let short = "موعدك الساعة ١٠:٠٠.";
        assert_eq!(segments(short), 1);

        assert_eq!(segments(&"م".repeat(70)), 1);
        assert_eq!(segments(&"م".repeat(71)), 2);
        assert_eq!(segments(&"م".repeat(134)), 2);
        assert_eq!(segments(&"م".repeat(135)), 3);
    }

    /// One Arabic character in an otherwise Latin message re-bills the whole
    /// thing, which is the surprise a business would otherwise meet on an
    /// invoice.
    #[test]
    fn a_single_non_gsm_character_re_bills_the_whole_message() {
        let latin = "a".repeat(100);
        assert_eq!(segments(&latin), 1);
        assert_eq!(
            segments(&format!("{latin}م")),
            2,
            "101 UCS-2 units is two segments, not one"
        );
    }

    /// The extension characters take two septets each.
    #[test]
    fn a_brace_costs_two_septets() {
        assert_eq!(segments(&"a".repeat(158)), 1);
        assert_eq!(
            segments(&format!("{}{{", "a".repeat(158))),
            1,
            "160 exactly"
        );
        assert_eq!(segments(&format!("{}{{", "a".repeat(159))), 2, "161");
    }

    /// An emoji is a surrogate pair and takes two UCS-2 units.
    #[test]
    fn an_emoji_takes_two_units() {
        assert_eq!(segments(&"م".repeat(69)), 1);
        assert_eq!(segments(&format!("{}👋", "م".repeat(69))), 2, "71 units");
    }

    #[test]
    fn only_sms_is_billed_by_length() {
        let long = "a".repeat(1_000);
        assert_eq!(Channel::Sms.units(&long), 7);
        assert_eq!(Channel::Email.units(&long), 1);
        assert_eq!(Channel::Push.units(&long), 1);
        assert_eq!(Channel::WhatsApp.units(&long), 1);
    }

    #[test]
    fn every_channel_has_its_own_effect_kind() {
        let mut kinds: Vec<_> = Channel::ALL.iter().map(|c| c.kind()).collect();
        kinds.sort();
        kinds.dedup();
        assert_eq!(kinds.len(), Channel::ALL.len(), "two channels share a kind");

        // The one that already exists keeps its name, or every email promised
        // by the control plane stops being delivered.
        assert_eq!(Channel::Email.kind().as_str(), "email.send");
    }
}
