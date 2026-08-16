//! Who the business is, as ZATCA knows them.
//!
//! # Why this is an event and not configuration
//!
//! Every other tenant setting in this system is configuration: the VAT rate, the
//! posting accounts, the chart. They are read **inside the command's
//! transaction** and stamped onto the event, so a setting that changes later
//! cannot restate a document that was already issued (architecture L5).
//!
//! That mechanism is unavailable here, and the reason is the dependency
//! direction. The command that issues an invoice lives in `sales`, and `sales`
//! must not know that Saudi Arabia exists — so nothing in the issuing
//! transaction can read a ZATCA registration, and the ZATCA document is built
//! afterwards, by a projection over the log.
//!
//! A projection that read `configuration` would break L2 outright: rebuilding it
//! after the business moved offices would render every historic invoice with the
//! new address, produce a different hash for each, and **break the chain** —
//! silently, because each document on its own would look fine.
//!
//! So the registration is a fact in the log with a position, like everything
//! else the projection reads. An invoice issued in March is rendered with the
//! registration that was current in March, whatever happened in April, and a
//! rebuild reproduces exactly the bytes that were submitted. It also answers
//! "who changed the VAT number, and when" — which for a tax registration is a
//! question somebody eventually asks.
//!
//! # Arabic is the invoice's language, not a translation of it
//!
//! ZATCA requires the invoice in Arabic. UBL carries one registration name, so
//! that name is the Arabic one, and [`Registration::name`] is validated to
//! contain Arabic script rather than trusted to. The Latin name is ours, for our
//! own screens — it never reaches ZATCA.

use serde::{Deserialize, Serialize};
use spa_eventlog::{Aggregate, DomainEvent};
use spa_types::{AggregateId, DomainName, EventName, SchemaVersion, Timestamp};

/// The single stream a tenant's registration lives in.
///
/// One EGS unit per tenant: every invoice this tenant issues flows through one
/// solution, so one certificate, one counter, one chain. A business running two
/// tills that each need their own device certificate is a real ZATCA shape and
/// not this one — it would make the unit part of this id, and the counter and
/// chain per unit with it.
#[must_use]
pub fn taxpayer_id() -> AggregateId {
    AggregateId::new("self")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies AggregateId"))
}

/// What ZATCA needs to know about the business issuing the invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// The VAT registration number: 15 digits, first and last both `3`.
    pub vat_number: String,
    /// The legal name **in Arabic**, as registered. This is what the invoice
    /// says, because the invoice is in Arabic.
    pub name: String,
    /// The same name in Latin script, for our own screens. Never submitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_latin: Option<String>,
    /// Which register the [`identifier`](Self::identifier) is from.
    pub scheme: IdScheme,
    /// The number in that register — the commercial registration, usually.
    pub identifier: String,
    pub address: Address,
}

/// The other register a business is identified in, besides VAT.
///
/// ZATCA's `schemeID` list. A business gives whichever one it has; the
/// commercial registration is the usual answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdScheme {
    /// Commercial registration number.
    Crn,
    /// Momra licence.
    Mom,
    /// MLSD licence.
    Mls,
    /// Sagia licence.
    Sag,
    /// 700 number.
    Number700,
    /// Anything else the taxpayer is registered under.
    Other,
}

impl IdScheme {
    pub const ALL: [Self; 6] = [
        Self::Crn,
        Self::Mom,
        Self::Mls,
        Self::Sag,
        Self::Number700,
        Self::Other,
    ];

    /// The code ZATCA expects in `schemeID`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Crn => "CRN",
            Self::Mom => "MOM",
            Self::Mls => "MLS",
            Self::Sag => "SAG",
            Self::Number700 => "700",
            Self::Other => "OTH",
        }
    }
}

impl std::str::FromStr for IdScheme {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|scheme| scheme.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown identification scheme {s:?}"))
    }
}

/// A Saudi national address, which is a fixed shape rather than free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub street: String,
    /// Four digits. The national address gives every building one.
    pub building: String,
    /// The four-digit secondary number, where the address has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional: Option<String>,
    pub district: String,
    pub city: String,
    /// Five digits.
    pub postal_code: String,
    /// ISO 3166-1 alpha-2. `SA` for a business registered here, and not
    /// hard-coded to it: a foreign branch issuing under a Saudi VAT number is a
    /// shape that exists.
    pub country: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidRegistration {
    /// 15 digits, starting and ending with `3`. ZATCA rejects anything else, and
    /// finding that out at clearance is finding it out after the invoice was
    /// given to a customer.
    #[error("{0:?} is not a Saudi VAT registration number: 15 digits, beginning and ending with 3")]
    VatNumber(String),
    /// The invoice is an Arabic document. A name with no Arabic in it is one
    /// ZATCA will refuse, and one no Saudi customer can read.
    #[error("the registered name must be in Arabic; {0:?} has no Arabic letters in it")]
    NotArabic(String),
    #[error("{field} is required")]
    Missing { field: &'static str },
    #[error("{field} must be {digits} digits; {found:?} is not")]
    Digits {
        field: &'static str,
        digits: usize,
        found: String,
    },
    #[error("{0:?} is not an ISO 3166 country code")]
    Country(String),
}

impl Registration {
    /// Everything ZATCA validates, checked here instead.
    ///
    /// # Why not let ZATCA say no
    ///
    /// Because by then the invoice exists. A standard invoice is cleared
    /// *before* it goes to the buyer, so a bad VAT number stalls the sale; a
    /// simplified one has already been handed over when the reporting call
    /// fails. Both are worse than refusing the registration.
    pub fn check(&self) -> Result<(), InvalidRegistration> {
        if !is_vat_number(&self.vat_number) {
            return Err(InvalidRegistration::VatNumber(self.vat_number.clone()));
        }
        if self.name.trim().is_empty() {
            return Err(InvalidRegistration::Missing { field: "name" });
        }
        if !self.name.chars().any(is_arabic) {
            return Err(InvalidRegistration::NotArabic(self.name.clone()));
        }
        if self.identifier.trim().is_empty() {
            return Err(InvalidRegistration::Missing {
                field: "identifier",
            });
        }
        self.address.check()
    }
}

impl Address {
    pub fn check(&self) -> Result<(), InvalidRegistration> {
        for (field, value) in [
            ("street", &self.street),
            ("district", &self.district),
            ("city", &self.city),
        ] {
            if value.trim().is_empty() {
                return Err(InvalidRegistration::Missing { field });
            }
        }
        digits("building", &self.building, 4)?;
        if let Some(additional) = &self.additional {
            digits("additional", additional, 4)?;
        }
        digits("postal_code", &self.postal_code, 5)?;

        if self.country.len() != 2 || !self.country.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(InvalidRegistration::Country(self.country.clone()));
        }
        Ok(())
    }
}

fn digits(field: &'static str, value: &str, digits: usize) -> Result<(), InvalidRegistration> {
    if value.len() == digits && value.chars().all(|c| c.is_ascii_digit()) {
        return Ok(());
    }
    Err(InvalidRegistration::Digits {
        field,
        digits,
        found: value.to_owned(),
    })
}

/// 15 digits, first and last `3`.
fn is_vat_number(value: &str) -> bool {
    value.len() == 15
        && value.chars().all(|c| c.is_ascii_digit())
        && value.starts_with('3')
        && value.ends_with('3')
}

/// Arabic script, including the presentation forms a paste from Word carries.
fn is_arabic(c: char) -> bool {
    matches!(c, '\u{0600}'..='\u{06FF}' | '\u{0750}'..='\u{077F}' | '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}')
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaxpayerEvent {
    /// The business's ZATCA registration, as it stands from here on.
    ///
    /// Recorded again on every correction rather than amended, so the log says
    /// which registration each document was rendered under.
    Registered {
        registration: Registration,
        /// When the business treats the registration as effective. Not a clock
        /// reading, for the same reason a tax point is not one.
        on: Timestamp,
    },
}

impl TaxpayerEvent {
    pub const NAMES: [&'static str; 1] = ["tax_sa.taxpayer.registered"];
}

impl DomainEvent for TaxpayerEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Registered { .. } => Self::NAMES[0],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know before registering.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Taxpayer {
    pub registration: Option<Registration>,
    pub registered_on: Option<Timestamp>,
}

impl Aggregate for Taxpayer {
    type Event = TaxpayerEvent;

    fn domain() -> DomainName {
        crate::domain("tax_sa_taxpayer")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            TaxpayerEvent::Registered { registration, on } => {
                self.registration = Some(registration.clone());
                self.registered_on = Some(*on);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn registration() -> Registration {
        Registration {
            vat_number: "310122393500003".to_owned(),
            name: "روابي للاستشارات".to_owned(),
            name_latin: Some("Rawabi Consulting".to_owned()),
            scheme: IdScheme::Crn,
            identifier: "1010101010".to_owned(),
            address: Address {
                street: "طريق الملك فهد".to_owned(),
                building: "2322".to_owned(),
                additional: Some("9999".to_owned()),
                district: "العليا".to_owned(),
                city: "الرياض".to_owned(),
                postal_code: "12211".to_owned(),
                country: "SA".to_owned(),
            },
        }
    }

    #[test]
    fn a_well_formed_registration_is_accepted() {
        assert_eq!(registration().check(), Ok(()));
    }

    #[test]
    fn a_vat_number_is_fifteen_digits_from_three_to_three() {
        for bad in [
            "31012239350000",   // 14
            "3101223935000034", // 16
            "410122393500003",  // does not start with 3
            "310122393500004",  // does not end with 3
            "31012239350000a",  // not digits
            "",
        ] {
            let mut registration = registration();
            registration.vat_number = bad.to_owned();
            assert_eq!(
                registration.check(),
                Err(InvalidRegistration::VatNumber(bad.to_owned())),
                "{bad:?} was accepted"
            );
        }
    }

    /// The invoice is an Arabic document, so the name on it is an Arabic name.
    #[test]
    fn the_registered_name_has_to_be_arabic() {
        let mut registration = registration();
        registration.name = "Rawabi Consulting".to_owned();
        assert_eq!(
            registration.check(),
            Err(InvalidRegistration::NotArabic(
                "Rawabi Consulting".to_owned()
            ))
        );

        // Bilingual is fine — plenty of businesses register that way.
        registration.name = "روابي للاستشارات Rawabi Consulting".to_owned();
        assert_eq!(registration.check(), Ok(()));
    }

    #[test]
    fn a_national_address_is_a_shape_and_not_free_text() {
        let mut registration = registration();
        registration.address.building = "23".to_owned();
        assert_eq!(
            registration.check(),
            Err(InvalidRegistration::Digits {
                field: "building",
                digits: 4,
                found: "23".to_owned()
            })
        );

        let mut registration = super::tests::registration();
        registration.address.postal_code = "1221".to_owned();
        assert!(matches!(
            registration.check(),
            Err(InvalidRegistration::Digits {
                field: "postal_code",
                ..
            })
        ));

        let mut registration = super::tests::registration();
        registration.address.city = "  ".to_owned();
        assert_eq!(
            registration.check(),
            Err(InvalidRegistration::Missing { field: "city" })
        );
    }

    #[test]
    fn every_scheme_round_trips_through_its_zatca_code() {
        for scheme in IdScheme::ALL {
            assert_eq!(scheme.as_str().parse::<IdScheme>(), Ok(scheme));
        }
        assert!("nonsense".parse::<IdScheme>().is_err());
    }

    #[test]
    fn registering_again_replaces_what_came_before() {
        let mut taxpayer = Taxpayer::default();
        assert!(taxpayer.registration.is_none());

        taxpayer.apply(&TaxpayerEvent::Registered {
            registration: registration(),
            on: Timestamp::UNIX_EPOCH,
        });
        assert_eq!(taxpayer.registration, Some(registration()));

        let mut moved = registration();
        moved.address.building = "1111".to_owned();
        taxpayer.apply(&TaxpayerEvent::Registered {
            registration: moved.clone(),
            on: Timestamp::UNIX_EPOCH,
        });
        assert_eq!(taxpayer.registration, Some(moved));
    }
}
