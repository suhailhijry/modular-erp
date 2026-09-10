//! **Ready-made tariffs.**
//!
//! A form is the translation between "Thursday evenings, a bit more" and
//! `opens_at: 1020, uplift: 1500`. A pack is the next step down: the form,
//! already filled in — and several of them, in the order they should be tried.
//!
//! # Why this may ship numbers while `templates` ships no presets
//!
//! `crate::templates` refuses to ship a preset, and the refusal stands: *"a
//! preset that picked 25% for a salon would be inventing their pricing, and
//! they would have to un-pick it."*
//!
//! The difference is where the number lands. **A preset has no blanks, so its
//! number is unreachable** — a salon offered "25% dearer" cannot make it 20
//! without abandoning the preset. A pack writes an ordinary
//! [`Authored::Form`](erp_rules::Authored::Form), answers and all, so every
//! number it suggests arrives in the box a business already edits numbers in,
//! on the screen they already have. The shape is the gift; the number is a
//! suggestion sitting in an editable field.
//!
//! # A blueprint of the catalogue shape, and not of the command-script shape
//!
//! D8 says a blueprint is "a versioned, parameterized list of **commands** —
//! never rows", and it names rule packs as one. Half of that is literally true
//! here and half of it is not, which is worth writing down rather than arguing
//! away.
//!
//! **True:** a step is a template id and the answers to fill it with — the
//! same pair `PUT /v1/booking/tariff` takes from a person. Nothing in this file
//! builds a [`Band`](crate::Band) itself; [`Authored::written`](erp_rules::Authored::written)
//! does, which is the only road from answers to a band and the one the
//! settings screen takes. A pack cannot write a band the form would have
//! refused. "Fails at build time rather than in front of a customer" is
//! *stronger* here than for a chart: the whole pipeline is pure, so
//! `every_pack_builds_every_band_it_promises` needs no database.
//!
//! **Not true:** a tariff is one configuration value, and writing it is not a
//! command. There is no aggregate, no event, no per-step refusal — where a
//! chart is eighteen independently refusable `open_account_in` calls against
//! the log. So this is a blueprint in the sense of *browse, preview, install*,
//! and not in the sense of a script the log replays.
//!
//! What survives, and matters: [`TariffAsWritten::write`] is the one write, and
//! both this and the settings screen go through it. "A pack writes exactly what
//! the screen writes" is the call graph.
//!
//! # Why a pack goes underneath
//!
//! First match wins, so position is priority, and appending is the only
//! position that cannot change a price the business has already decided: an
//! appended band fires only in hours nothing above it claims. A pack that put
//! itself on top would silently reprice the hours somebody opened the screen to
//! set.
//!
//! It has a failure mode of its own — a tenant whose first band already claims
//! every hour gets a pack that changes nothing — and the answer is that
//! [`Installed::tariff`] hands back the resulting list *in order*, from the
//! preview and from the install alike. That is visible rather than argued
//! about, and it is the same reason `Installed` names the bands rather than
//! counting them.
//!
//! # What this deliberately is not
//!
//! **No Ramadan or Eid pack**, which the market would rank first. Both are
//! Hijri and drift about eleven days a Gregorian year, and nothing in this
//! build can compute one: `erp_types::Calendar` is a time zone and nothing
//! else. A band *can* carry a date window — `Availability` has `from` and
//! `until` — so a pack with the dates typed in would work for one year and be
//! wrong the next, which is worse than not shipping it. It waits on a Hijri
//! calendar, not on a pack.
//!
//! **No taking a pack back out.** ponytail: worth building when somebody has a
//! tariff long enough that removing four bands by hand is a chore — which,
//! since `PUT /v1/booking/tariff` already replaces the whole list and no pack
//! here writes more than four, is nobody yet. What it would cost is worth
//! naming so it is not re-derived: provenance beside each band, because neither
//! of the cheap keys works. Not the name — two bands may share one, which is
//! the bug `Tariff::band_for` already paid for. Not the content — an edited
//! band stops matching, which is exactly when somebody most wants it gone.
//!
//! **No discount pack.** A negative percentage is a band like any other and the
//! form takes one; what is not here is a *pack* of them, because installing one
//! would cut a week of prices in a click that nobody asked to be a discount.

use erp_i18n::Locale;
use erp_rules::{Answers, Authored, Value};

use crate::pricing::TariffAsWritten;
use crate::templates::{FROM_HOUR, NAME, PERCENT, TARIFF_TEMPLATES, WEEKDAY};

/// One band a pack writes, **as answers rather than as a band**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackBand {
    /// Which form, by [`erp_rules::Template::id`].
    pub template: &'static str,
    /// The `name` answer. Printed beside a price on a receipt, so it is here
    /// in both languages and picked at install time — the same decision a
    /// chart makes about an account name.
    pub name_en: &'static str,
    pub name_ar: &'static str,
    /// Every other blank, keyed by the field that asks for it.
    ///
    /// **Numbers only, because every blank that is not a name is a number** — a
    /// weekday, an hour, a percentage. A template that asked a yes-or-no would
    /// need a list beside this one, and until one does, a second empty list is
    /// a shape nobody fills in.
    ///
    /// Keyed off the `Field` constants rather than string literals, so a
    /// renamed field is a compile error here rather than a refusal somebody
    /// meets at install time.
    pub numbers: &'static [(&'static str, i64)],
}

impl PackBand {
    #[must_use]
    pub const fn name(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.name_ar,
            Locale::English => self.name_en,
        }
    }

    /// The answers, exactly as the form would have received them from a person.
    ///
    /// Public so a catalogue can show them before anything is installed: what
    /// a pack would fill in is the most useful thing to read about it, and it
    /// is the same map the install writes.
    #[must_use]
    pub fn answers(&self, locale: Locale) -> Answers {
        let mut answers = Answers::new();
        answers.insert(
            NAME.key.to_owned(),
            Value::Text(self.name(locale).to_owned()),
        );
        for (key, number) in self.numbers {
            answers.insert((*key).to_owned(), Value::Int(*number));
        }
        answers
    }
}

/// A named starting point for a tariff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pack {
    /// Stable identifier. What a client sends to install one.
    ///
    /// **And its version.** Nothing stores which pack a band came from, so a
    /// `version` field would be a number nothing could ever be compared
    /// against. A pack whose bands change is a different pack and takes a
    /// different id; what a *tenant* has is versioned by the configuration
    /// generation the write landed in, which is what the `ETag` already says.
    /// `CHARTS` and `TRADES` carry no version either, for the same reason.
    pub id: &'static str,
    pub name_en: &'static str,
    pub name_ar: &'static str,
    pub description_en: &'static str,
    pub description_ar: &'static str,
    /// **In the order they are tried.** First match wins, so a band for an
    /// evening goes above one for the whole of that day.
    pub bands: &'static [PackBand],
}

impl Pack {
    #[must_use]
    pub const fn name(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.name_ar,
            Locale::English => self.name_en,
        }
    }

    #[must_use]
    pub const fn description(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.description_ar,
            Locale::English => self.description_en,
        }
    }

    /// **What this pack comes to on top of a tariff.** Writes nothing.
    ///
    /// # Where preview and install stop being two things
    ///
    /// This returns the tariff an install stores, so a preview *is* this
    /// function and an install is this function plus one
    /// [`TariffAsWritten::write`]. They cannot disagree about what a pack does,
    /// because there is only one of them — the property
    /// `ledger::preview_chart` buys with a transaction it rolls back.
    ///
    /// It buys it without the transaction, and deliberately. A chart install is
    /// eighteen commands against the log that can each refuse, and the only
    /// honest preview of that is a real run. A tariff is one value: the whole
    /// of an install is computing it and storing it, so the computed value is
    /// already the preview. Wrapping a rolled-back transaction around a pure
    /// function would be ceremony that can still be got wrong — and it would
    /// burn a `configuration_version` on every preview, because a sequence does
    /// not roll back.
    ///
    /// # Errors
    /// The tenant's stored tariff names a template this build no longer ships,
    /// or a band this pack promises does not build — the second being our bug
    /// rather than theirs, and one `every_pack_builds_every_band_it_promises`
    /// is there to catch first.
    pub fn onto(
        &self,
        existing: &TariffAsWritten,
        locale: Locale,
    ) -> Result<Installed, erp_eventlog::ConfigError> {
        let priced = existing.resolve()?.bands;
        let mut installed = Installed {
            tariff: existing.clone(),
            added: Vec::new(),
            skipped: Vec::new(),
        };

        for want in self.bands {
            let written = Authored::written(TARIFF_TEMPLATES, want.template, want.answers(locale))
                .map_err(|why| self.broken(&why))?;
            let band = written
                .rule(TARIFF_TEMPLATES)
                .map_err(|why| self.broken(&why))?;

            // **Already priced hours are left alone**, and by the window rather
            // than by the name: two bands may share a name, and a business that
            // renamed the one covering Thursday evening has still answered the
            // question this pack was about to ask. Installing twice is
            // therefore not an error, and neither is installing over a band
            // somebody wrote themselves.
            if priced.iter().any(|already| already.when == band.when) {
                installed.skipped.push(band.name);
                continue;
            }
            installed.added.push(band.name);
            installed.tariff.bands.push(written);
        }

        Ok(installed)
    }

    /// A shipped pack that does not build is ours to fix, and says so.
    fn broken(&self, why: &erp_rules::Unfillable) -> erp_eventlog::ConfigError {
        erp_eventlog::ConfigError::Invalid {
            key: format!("booking.pack.{}", self.id),
            reason: why.to_string(),
        }
    }
}

/// How installing a pack went — **or would have gone**.
///
/// Carries the tariff itself and not just the names, because a preview that
/// says "four bands would be added" cannot answer the question first-match-wins
/// makes urgent: where they land, and what is now above them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The tariff as it would then read. What an install stores, unchanged.
    pub tariff: TariffAsWritten,
    /// Added, named as this pack names them, in the pack's own order.
    pub added: Vec<String>,
    /// Not added, because something already prices those hours. **Not a
    /// failure**: installing twice is not an error, and neither is installing
    /// over a band somebody wrote themselves.
    pub skipped: Vec<String>,
}

/// The pack with this id, if this build ships one.
#[must_use]
pub fn pack(id: &str) -> Option<&'static Pack> {
    PACKS.iter().find(|p| p.id == id)
}

/// Every tariff pack this build ships.
///
/// # Where the numbers come from, and what they are not
///
/// The *shape* is well attested and the *number* is not, so these are starting
/// points and are said to be: Thursday evening is the Saudi weekend eve and the
/// hardest table in Riyadh to book; barbershops trade to midnight and Friday
/// morning is structurally empty; a city hotel fills Sunday to Wednesday on
/// corporate demand while a resort fills Thursday and Friday. No public source
/// gives an hour-by-hour figure for any of it. Every percentage below arrives
/// in an editable field for that reason.
///
/// **Two hotel packs, because one would be wrong for half the market.** A city
/// hotel and a resort peak on opposite days, and a single "weekend premium"
/// would tell one of them the reverse of the truth.
///
/// Nothing here reads a trade id: a pack is picked, not inferred, because a
/// business that fitted out as a salon may still price like a spa.
pub static PACKS: &[Pack] = &[
    Pack {
        id: "barbershop",
        name_en: "Barbershop evenings",
        name_ar: "أمسيات صالون الحلاقة",
        description_en: "The two evenings before the weekend, when every chair is taken.",
        description_ar: "الأمسيتان قبل نهاية الأسبوع، حين تمتلئ كل الكراسي.",
        bands: &[
            PackBand {
                template: "weekday_evening",
                name_en: "Thursday evening",
                name_ar: "مساء الخميس",
                numbers: &[(WEEKDAY.key, 4), (FROM_HOUR.key, 17), (PERCENT.key, 15)],
            },
            PackBand {
                template: "weekday_evening",
                name_en: "Wednesday evening",
                name_ar: "مساء الأربعاء",
                numbers: &[(WEEKDAY.key, 3), (FROM_HOUR.key, 17), (PERCENT.key, 10)],
            },
        ],
    },
    Pack {
        id: "ladies_salon",
        name_en: "Ladies' salon, Thursday",
        name_ar: "صالون نسائي، الخميس",
        description_en: "Thursday is wedding night. The evening costs most, and the day before it costs more than the rest of the week.",
        description_ar: "الخميس ليلة الأعراس. المساء هو الأغلى، والنهار الذي يسبقه أغلى من بقية الأسبوع.",
        bands: &[
            // **The evening first.** First match wins, and a Thursday-evening
            // appointment is inside both windows — so the specific one has to
            // be tried first or the whole day would swallow it.
            PackBand {
                template: "weekday_evening",
                name_en: "Thursday evening",
                name_ar: "مساء الخميس",
                numbers: &[(WEEKDAY.key, 4), (FROM_HOUR.key, 17), (PERCENT.key, 20)],
            },
            PackBand {
                template: "weekday",
                name_en: "Thursday",
                name_ar: "الخميس",
                numbers: &[(WEEKDAY.key, 4), (PERCENT.key, 10)],
            },
        ],
    },
    Pack {
        id: "restaurant",
        name_en: "Restaurant, weekend dinner",
        name_ar: "مطعم، عشاء نهاية الأسبوع",
        description_en: "Thursday and Friday dinner, the two hardest sittings of the week to get.",
        description_ar: "عشاء الخميس والجمعة، أصعب جلستين في الأسبوع.",
        bands: &[
            PackBand {
                template: "weekday_evening",
                name_en: "Thursday dinner",
                name_ar: "عشاء الخميس",
                numbers: &[(WEEKDAY.key, 4), (FROM_HOUR.key, 19), (PERCENT.key, 15)],
            },
            PackBand {
                template: "weekday_evening",
                name_en: "Friday dinner",
                name_ar: "عشاء الجمعة",
                numbers: &[(WEEKDAY.key, 5), (FROM_HOUR.key, 19), (PERCENT.key, 10)],
            },
        ],
    },
    Pack {
        id: "hotel_leisure",
        name_en: "Hotel or resort, weekend",
        name_ar: "فندق أو منتجع، نهاية الأسبوع",
        description_en: "Friday and Saturday, which is when a resort fills. Whole days, because a room is booked by the night.",
        description_ar: "الجمعة والسبت، حين يمتلئ المنتجع. أيام كاملة، لأن الغرفة تُحجز بالليلة.",
        bands: &[
            PackBand {
                template: "weekday",
                name_en: "Friday",
                name_ar: "الجمعة",
                numbers: &[(WEEKDAY.key, 5), (PERCENT.key, 15)],
            },
            PackBand {
                template: "weekday",
                name_en: "Saturday",
                name_ar: "السبت",
                numbers: &[(WEEKDAY.key, 6), (PERCENT.key, 15)],
            },
        ],
    },
    Pack {
        id: "hotel_city",
        name_en: "City hotel, working week",
        name_ar: "فندق مدينة، أيام العمل",
        description_en: "Sunday to Wednesday, when a city hotel fills on business travel. The opposite week to a resort's.",
        description_ar: "من الأحد إلى الأربعاء، حين يمتلئ فندق المدينة بسفر الأعمال. عكس أسبوع المنتجع.",
        bands: &[
            PackBand {
                template: "weekday",
                name_en: "Sunday",
                name_ar: "الأحد",
                numbers: &[(WEEKDAY.key, 7), (PERCENT.key, 10)],
            },
            PackBand {
                template: "weekday",
                name_en: "Monday",
                name_ar: "الاثنين",
                numbers: &[(WEEKDAY.key, 1), (PERCENT.key, 10)],
            },
            PackBand {
                template: "weekday",
                name_en: "Tuesday",
                name_ar: "الثلاثاء",
                numbers: &[(WEEKDAY.key, 2), (PERCENT.key, 10)],
            },
            PackBand {
                template: "weekday",
                name_en: "Wednesday",
                name_ar: "الأربعاء",
                numbers: &[(WEEKDAY.key, 3), (PERCENT.key, 10)],
            },
        ],
    },
];

/// **What installing a pack would do**, without doing it.
///
/// # Errors
/// The tenant's tariff is unreadable, or the pack does not build.
pub async fn preview(
    conn: &mut sqlx::PgConnection,
    pack: &Pack,
    locale: Locale,
) -> Result<Installed, erp_eventlog::ConfigError> {
    let (existing, _) = TariffAsWritten::read(conn).await?;
    pack.onto(&existing, locale)
}

/// Installs a pack, and says what it did.
///
/// # Why `expected` cannot be left empty
///
/// This is a read-modify-write, which a chart install is not: it computes the
/// new tariff *from* the stored one. An unconditional write would therefore
/// lose an edit made between the read and the write — a second admin's whole
/// tariff, gone, with both requests answering success.
///
/// So the version this read is what it writes against when the caller names
/// none. A caller that *does* name one — a settings screen holding an `ETag` —
/// is asking a stricter question and gets it asked: their version is used, and
/// a tariff that moved under them refuses rather than being overwritten.
///
/// # Why nothing is written when nothing is added
///
/// Installing twice is not an error, and it should not be a *write* either: a
/// second install that bumped the generation would break every `ETag` a screen
/// was holding, to record that nothing happened.
///
/// **But the caller's precondition is answered before that shortcut is taken.**
/// Somebody who asked "only if the tariff is still at `N`" and got `200` back
/// would have been handed a tariff at `N + 1` that they have never seen — and
/// a pack with nothing to add is still an answer about the whole tariff. So a
/// stated version that is no longer current refuses whether or not there was
/// work to do, which is what makes "you get what the preview showed you, or
/// you get told" true.
///
/// # Errors
/// [`ConfigError::Conflict`](erp_eventlog::ConfigError::Conflict) if the
/// tariff moved since it was read, or since the caller's `If-Match`.
pub async fn install(
    conn: &mut sqlx::PgConnection,
    pack: &Pack,
    locale: Locale,
    set_by: Option<&str>,
    expected: Option<i64>,
) -> Result<Installed, erp_eventlog::ConfigError> {
    let (existing, version) = TariffAsWritten::read(conn).await?;
    if let Some(want) = expected.filter(|want| *want != version) {
        return Err(erp_eventlog::ConfigError::Conflict {
            key: TariffAsWritten::KEY.to_owned(),
            expected: want,
            found: version,
        });
    }

    let installed = pack.onto(&existing, locale)?;
    if installed.added.is_empty() {
        return Ok(installed);
    }
    installed
        .tariff
        .write(conn, set_by, expected.or(Some(version)))
        .await?;
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Band;
    use erp_occupancy::Span;
    use erp_recurrence::{Availability, Calendar};

    /// One hour starting at `hour` **on the tenant's clock**, which is Riyadh.
    /// A window is written in local time, so a span built in UTC would test a
    /// different hour than the one it names.
    fn at(day: &str, hour: u32) -> Span {
        let from: erp_types::Timestamp = format!("{day}T{hour:02}:00:00+03:00")
            .parse()
            .expect("an instant");
        Span::new(from, from + chrono::Duration::hours(1)).expect("a span")
    }

    fn empty() -> TariffAsWritten {
        TariffAsWritten::default()
    }

    fn installed(id: &str, onto: &TariffAsWritten) -> Installed {
        pack(id)
            .unwrap_or_else(|| panic!("no pack {id}"))
            .onto(onto, Locale::Arabic)
            .unwrap_or_else(|e| panic!("{id} does not install: {e}"))
    }

    /// **The blueprint-validity check** ARCHITECTURE §1138 asks for: *"every
    /// shipped blueprint previewed against a fresh tenant"*.
    ///
    /// Needs no database, which is the part that makes it stronger than the
    /// same check for a chart or a trade: every step from a pack's answers to
    /// a band is pure, so a pack promising a weekday that is not one, an hour
    /// that is not one, or a percentage that would have the business paying
    /// the customer fails here rather than in front of a salon.
    #[test]
    fn every_pack_builds_every_band_it_promises() {
        for shipped in PACKS {
            for locale in [Locale::English, Locale::Arabic] {
                let onto = shipped.onto(&empty(), locale).unwrap_or_else(|e| {
                    panic!("{} does not install in {locale:?}: {e}", shipped.id)
                });

                assert_eq!(
                    onto.added.len(),
                    shipped.bands.len(),
                    "{} did not add every band it promises",
                    shipped.id
                );
                assert!(
                    onto.skipped.is_empty(),
                    "{} skipped on a fresh tariff",
                    shipped.id
                );
                // And what it wrote resolves, which is what a booking will do.
                let priced = onto.tariff.resolve().unwrap_or_else(|e| {
                    panic!("{} wrote a tariff that will not read: {e}", shipped.id)
                });
                assert_eq!(priced.bands.len(), shipped.bands.len());
                for band in &priced.bands {
                    assert!(
                        !band.name.trim().is_empty(),
                        "{} wrote a nameless band",
                        shipped.id
                    );
                }
            }
        }
    }

    /// The names come out in the caller's language, the way a chart's accounts
    /// do — a band's name is printed beside a price on a receipt.
    #[test]
    fn a_pack_writes_its_names_in_the_callers_language() {
        let arabic = installed("barbershop", &empty());
        let english = pack("barbershop")
            .expect("shipped")
            .onto(&empty(), Locale::English)
            .expect("installs");

        assert_eq!(arabic.added[0], "مساء الخميس");
        assert_eq!(english.added[0], "Thursday evening");
    }

    /// **A pack goes underneath.** First match wins, so appending is the only
    /// position that cannot change a price the business already decided.
    #[test]
    fn a_pack_never_outranks_a_band_the_tenant_wrote() {
        let mine = Band {
            name: "عرض الخميس".to_owned(),
            when: Availability::from_parts(&[], &[4], &[], 17 * 60, 24 * 60, None, None)
                .expect("a window"),
            uplift: -2_000,
        };
        let onto = TariffAsWritten {
            bands: vec![Authored::Raw { rule: mine.clone() }],
        };

        let after = installed("restaurant", &onto);

        assert_eq!(
            after.tariff.bands[0],
            Authored::Raw { rule: mine },
            "the tenant's own band did not stay first"
        );
        // And it still wins on the hour they set it for.
        let priced = after.tariff.resolve().expect("reads");
        assert_eq!(
            priced
                .band_for(at("2026-05-07", 20), Calendar::default())
                .map(|b| b.uplift),
            Some(-2_000),
            "the pack repriced an hour the business had already decided"
        );
    }

    /// **Installing twice adds nothing the second time**, and neither does
    /// installing over a band somebody wrote for the same hours themselves.
    #[test]
    fn hours_that_are_already_priced_are_skipped_and_named() {
        let once = installed("barbershop", &empty());
        let twice = installed("barbershop", &once.tariff);

        assert!(twice.added.is_empty(), "a second install added something");
        assert_eq!(
            twice.skipped, once.added,
            "and did not say what it left alone"
        );
        assert_eq!(
            twice.tariff, once.tariff,
            "a second install changed the tariff"
        );
    }

    /// **Skipping is by the hours, not by the name.**
    ///
    /// Two bands may share a name — the bug `Tariff::band_for` already paid for
    /// — and a business that renamed the band covering Thursday evening has
    /// still answered the question the pack was about to ask.
    #[test]
    fn a_renamed_band_still_counts_as_those_hours_being_priced() {
        let once = installed("barbershop", &empty());
        let mut renamed = once.tariff.clone();
        renamed.bands[0] = Authored::Raw {
            rule: Band {
                name: "ذروتنا".to_owned(),
                when: Availability::from_parts(&[], &[4], &[], 17 * 60, 24 * 60, None, None)
                    .expect("a window"),
                uplift: 4_000,
            },
        };

        let again = installed("barbershop", &renamed);

        assert!(
            again.added.is_empty(),
            "a rename made the pack write a second band over the same hours: {:?}",
            again.added
        );
    }

    /// **A specific band is tried before the day that contains it.**
    ///
    /// The order a pack lists its bands in is the order they are tried, and
    /// `ladies_salon` depends on it: a Thursday-evening appointment is inside
    /// both windows, so the whole day would swallow the evening if it came
    /// first.
    #[test]
    fn a_more_specific_band_is_tried_before_the_day_that_contains_it() {
        let priced = installed("ladies_salon", &empty())
            .tariff
            .resolve()
            .expect("reads");
        let calendar = Calendar::default();

        assert_eq!(
            priced
                .band_for(at("2026-05-07", 20), calendar)
                .map(|b| b.uplift),
            Some(2_000),
            "Thursday evening did not get the evening rate"
        );
        assert_eq!(
            priced
                .band_for(at("2026-05-07", 10), calendar)
                .map(|b| b.uplift),
            Some(1_000),
            "Thursday morning did not fall through to the whole-day rate"
        );
        assert_eq!(
            priced
                .band_for(at("2026-05-06", 20), calendar)
                .map(|b| b.uplift),
            None,
            "Wednesday evening was priced by a pack that is only about Thursday"
        );
    }

    #[test]
    fn every_pack_is_findable_and_named_in_both_languages() {
        let mut ids: Vec<_> = PACKS.iter().map(|p| p.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two packs share an id");

        for shipped in PACKS {
            assert_eq!(pack(shipped.id).map(|p| p.id), Some(shipped.id));
            for locale in [Locale::English, Locale::Arabic] {
                assert!(!shipped.name(locale).trim().is_empty(), "{}", shipped.id);
                assert!(
                    !shipped.description(locale).trim().is_empty(),
                    "{}",
                    shipped.id
                );
                for band in shipped.bands {
                    assert!(!band.name(locale).trim().is_empty(), "{}", shipped.id);
                }
            }
        }
        assert!(pack("seasonal").is_none());
    }

    /// **No pack writes the same hours twice**, which would have its own second
    /// band skipped and leave the pack quietly one band short of what it says.
    #[test]
    fn no_pack_prices_the_same_hours_twice() {
        for shipped in PACKS {
            let windows: Vec<_> = shipped
                .onto(&empty(), Locale::English)
                .expect("installs")
                .tariff
                .resolve()
                .expect("reads")
                .bands
                .iter()
                .map(|b| b.when)
                .collect();

            for (i, window) in windows.iter().enumerate() {
                assert!(
                    !windows[..i].contains(window),
                    "{} lists the same hours twice",
                    shipped.id
                );
            }
        }
    }
}
