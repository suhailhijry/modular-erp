//! Ready-made rotas.
//!
//! A tenant that has just enabled `booking` has an empty diary and nothing to
//! put in it, which is technically correct and useless. These are the shortcut:
//! pick the trade, get a working rota, rename and withdraw whatever does not
//! fit. Same shape and same argument as [`ledger::CHARTS`], because it is the
//! same idea.
//!
//! # What these are actually for
//!
//! **They are the phase's own test.** The claim `booking` makes is that a
//! salon, a restaurant, a hotel, a class studio, a gym and a museum are one
//! engine, and the way to find out is to write all six down as data and see
//! whether any of them needs a branch. The six below are configuration; nothing
//! in this module reads a trade's id or asks what kind of business it is.
//! `modules/booking/tests/fixtures.rs` books the characteristic thing for each.
//!
//! # A blueprint is a list of commands, not a list of rows (D8)
//!
//! [`fit_out`] runs `declare_resource` and `schedule_resource`, the same two
//! commands a person clicking through the screens would run. So a trade cannot
//! produce anything the domain would refuse, and a broken one fails where every
//! other refusal does.
//!
//! # Where the six came from
//!
//! Rekaz sells to salons, clinics, gyms, studios, museums, event ticketing and
//! horse stables. Those seven need four shapes between them: capacity one with a
//! named person, capacity N in one slot, a pool of interchangeable units, and
//! pure capacity with nobody assigned. Restaurants add covers-as-capacity, and
//! the gym adds the case where there is no slot at all.

use erp_i18n::Locale;

use crate::availability::Availability;
use crate::resource::Kind;

/// One thing a trade declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateResource {
    /// The id it is declared under, and what a booking names.
    pub id: &'static str,
    pub name_en: &'static str,
    pub name_ar: &'static str,
    pub kind: Kind,
    /// How many can be held at once. **This is where most of the difference
    /// between trades lives** — one stylist, six covers, three rooms of a type,
    /// twelve places in a class, five hundred tickets in a slot.
    pub capacity: u16,
    /// Whether this one keeps the trade's opening hours.
    ///
    /// A stylist does. A hotel room does not: a guest checks in at any hour and
    /// stays through the night, so a room offered only between nine and five
    /// could not be booked for a single night.
    pub keeps_hours: bool,
}

impl TemplateResource {
    #[must_use]
    pub const fn name(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.name_ar,
            Locale::English => self.name_en,
        }
    }
}

/// One window of a trade's week.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateHours {
    /// ISO weekdays, Monday as 1. Empty means every day.
    ///
    /// **Sunday to Thursday is the Saudi working week**, so the salon and the
    /// studio below say `[7, 1, 2, 3, 4]` rather than the Monday-to-Friday a
    /// template written anywhere else would carry.
    pub weekdays: &'static [u8],
    /// Minutes past local midnight. `540` is nine in the morning.
    pub opens_at: u16,
    /// Minutes past local midnight, exclusive. `1440` is midnight.
    pub closes_at: u16,
}

/// A named starting point for a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// Stable identifier. What a client sends to fit one out.
    pub id: &'static str,
    pub name_en: &'static str,
    pub name_ar: &'static str,
    pub description_en: &'static str,
    pub description_ar: &'static str,
    /// The hours everything with `keeps_hours` is offered in. **Empty means
    /// always open**, which is what a hotel and a museum's own storeroom want.
    pub hours: &'static [TemplateHours],
    pub resources: &'static [TemplateResource],
}

impl Trade {
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

    /// The trade's opening hours as rules, or the reason one is not a rule.
    ///
    /// Fallible because [`Availability::from_parts`] is, and a template with a
    /// window that closes before it opens is a build bug this turns into a
    /// refusal — `every_trade_installs_as_written` in `tests/fixtures.rs` is
    /// what stops one shipping.
    pub fn timetable(&self) -> Result<Vec<Availability>, crate::BadRule> {
        self.hours
            .iter()
            .map(|window| {
                Availability::from_parts(
                    &[],
                    window.weekdays,
                    &[],
                    window.opens_at,
                    window.closes_at,
                    None,
                    None,
                )
            })
            .collect()
    }
}

/// Sunday to Thursday, the Saudi working week.
const WORKING_WEEK: &[u8] = &[7, 1, 2, 3, 4];

const SALON: &[TemplateResource] = &[
    person("stylist-1", "Stylist 1", "مصففة ١"),
    person("stylist-2", "Stylist 2", "مصففة ٢"),
    // The chair is a separate resource from the person who works in it, which
    // is what lets a salon with three stylists and two chairs refuse the third
    // booking without anybody writing that rule down.
    place("chair-1", "Chair 1", "كرسي ١", 1, true),
    place("chair-2", "Chair 2", "كرسي ٢", 1, true),
    place("basin-1", "Basin", "مغسلة", 1, true),
];

const RESTAURANT: &[TemplateResource] = &[
    // **Covers are the capacity.** A table for six takes one booking of six or
    // two of three, and the engine that counts places is already the engine
    // that knows the difference.
    place("table-2", "Table for two", "طاولة لشخصين", 2, true),
    place("table-4", "Table for four", "طاولة لأربعة", 4, true),
    place("table-6", "Table for six", "طاولة لستة", 6, true),
    place("terrace", "Terrace", "التراس", 12, true),
];

const HOTEL: &[TemplateResource] = &[
    // The **type**, which is what a guest books. Three of them, and which three
    // is nobody's business until check-in.
    place("double", "Double room", "غرفة مزدوجة", 3, false),
    place("suite", "Suite", "جناح", 1, false),
    // The **units**, which is what a guest is given. Assigning one takes a
    // second claim on a different resource, so nothing is counted twice.
    place("room-101", "Room 101", "غرفة ١٠١", 1, false),
    place("room-102", "Room 102", "غرفة ١٠٢", 1, false),
    place("room-103", "Room 103", "غرفة ١٠٣", 1, false),
    place("room-201", "Room 201", "غرفة ٢٠١", 1, false),
];

const STUDIO: &[TemplateResource] = &[
    person("instructor-1", "Instructor 1", "مدربة ١"),
    // Twelve mats. Twelve separate customers hold one place each in the same
    // hour, which is the shape a salon's chair cannot express and the reason
    // capacity is a number rather than a flag.
    place("studio-hall", "Studio", "الاستوديو", 12, true),
];

const GYM: &[TemplateResource] = &[
    // **Classes only.** The gym floor is deliberately not here: a member does
    // not book the floor, they hold a membership and walk in. Declaring it
    // would put a resource on the rota that nothing ever claims, and would
    // suggest that turning up is something to reserve.
    person("trainer-1", "Trainer 1", "مدرب ١"),
    place("spin-studio", "Spin studio", "قاعة الدراجات", 20, true),
];

const MUSEUM: &[TemplateResource] = &[
    // **Pure capacity, nobody assigned.** Five hundred places at an hour, no
    // named person and no unit to give out. Rekaz sells to museums, event
    // ticketing and horse stables, and this is the shape all three need.
    place("entry-slot", "Timed entry", "دخول بموعد", 500, true),
    place("guided-tour", "Guided tour", "جولة مصحوبة", 25, true),
];

/// Every trade this build ships.
///
/// **Six shapes and no branches.** Nothing in this module reads `Trade::id`, so
/// a seventh trade is an entry in this list and no code at all.
pub static TRADES: &[Trade] = &[
    Trade {
        id: "salon",
        name_en: "Salon or barber",
        name_ar: "صالون أو حلاق",
        description_en: "A named person and the chair they work in. Each takes one booking at a time.",
        description_ar: "شخص محدد والكرسي الذي يعمل فيه. يأخذ كل منهما حجزًا واحدًا في المرة.",
        hours: &[TemplateHours {
            weekdays: WORKING_WEEK,
            opens_at: 9 * 60,
            closes_at: 21 * 60,
        }],
        resources: SALON,
    },
    Trade {
        id: "restaurant",
        name_en: "Restaurant",
        name_ar: "مطعم",
        description_en: "Tables, where the number of covers is the capacity and a sitting is the booking.",
        description_ar: "طاولات، عدد المقاعد هو السعة والجلسة هي الحجز.",
        hours: &[TemplateHours {
            weekdays: &[],
            opens_at: 12 * 60,
            closes_at: 24 * 60,
        }],
        resources: RESTAURANT,
    },
    Trade {
        id: "hotel",
        name_en: "Hotel or guest house",
        name_ar: "فندق أو نزل",
        description_en: "Room types booked by the night. The room itself is given out at check-in.",
        description_ar: "أنواع غرف تُحجز بالليلة. تُخصص الغرفة نفسها عند الوصول.",
        // Empty: a guest checks in at any hour and stays through the night.
        hours: &[],
        resources: HOTEL,
    },
    Trade {
        id: "studio",
        name_en: "Class studio",
        name_ar: "استوديو حصص",
        description_en: "An instructor and a room, with many people in the same hour.",
        description_ar: "مدربة وقاعة، مع عدة أشخاص في الساعة نفسها.",
        hours: &[TemplateHours {
            weekdays: WORKING_WEEK,
            opens_at: 6 * 60,
            closes_at: 22 * 60,
        }],
        resources: STUDIO,
    },
    Trade {
        id: "gym",
        name_en: "Gym",
        name_ar: "نادٍ رياضي",
        description_en: "Classes are booked. Using the gym is not — that is a membership, and members walk in.",
        description_ar: "الحصص تُحجز. استخدام النادي لا يُحجز، فهو اشتراك والأعضاء يدخلون مباشرة.",
        hours: &[TemplateHours {
            weekdays: &[],
            opens_at: 5 * 60,
            closes_at: 24 * 60,
        }],
        resources: GYM,
    },
    Trade {
        id: "museum",
        name_en: "Museum or attraction",
        name_ar: "متحف أو معلم",
        description_en: "Timed entry: a number of places at an hour, with nobody assigned to them.",
        description_ar: "دخول بموعد: عدد من الأماكن في ساعة محددة، دون تخصيص أحد لها.",
        hours: &[TemplateHours {
            weekdays: &[],
            opens_at: 9 * 60,
            closes_at: 18 * 60,
        }],
        resources: MUSEUM,
    },
];

/// Finds a trade by id.
///
/// There is deliberately no "empty" trade: not fitting one out is already that.
#[must_use]
pub fn trade(id: &str) -> Option<&'static Trade> {
    TRADES.iter().find(|t| t.id == id)
}

/// How a fit-out went.
///
/// `skipped` is not a failure. Fitting out the same trade twice leaves the rota
/// as it is, which is what makes the button safe to press again — and what lets
/// a business that renamed a chair keep the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FittedOut {
    pub declared: usize,
    pub skipped: usize,
    /// Resources given the trade's opening hours.
    pub scheduled: usize,
}

const fn person(
    id: &'static str,
    name_en: &'static str,
    name_ar: &'static str,
) -> TemplateResource {
    TemplateResource {
        id,
        name_en,
        name_ar,
        kind: Kind::Person,
        capacity: 1,
        keeps_hours: true,
    }
}

const fn place(
    id: &'static str,
    name_en: &'static str,
    name_ar: &'static str,
    capacity: u16,
    keeps_hours: bool,
) -> TemplateResource {
    TemplateResource {
        id,
        name_en,
        name_ar,
        kind: Kind::Place,
        capacity,
        keeps_hours,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_trade_is_findable_by_its_id() {
        for t in TRADES {
            assert_eq!(trade(t.id).map(|found| found.id), Some(t.id));
        }
        assert!(trade("no-such-trade").is_none());
    }

    /// **Every timetable in every trade is a rule the domain would accept.**
    ///
    /// A window that closes before it opens is a typo in a `const`, and this is
    /// where it is found rather than in front of the first tenant who picks
    /// that trade.
    #[test]
    fn every_trades_hours_are_hours() {
        for t in TRADES {
            let rules = t
                .timetable()
                .unwrap_or_else(|e| panic!("{} has an impossible timetable: {e}", t.id));
            assert_eq!(rules.len(), t.hours.len());
        }
    }

    /// **No two trades name the same resource differently.**
    ///
    /// Two trades may share an id — a `chair-1` is a `chair-1` — but a tenant
    /// who fits out one and then the other must not end up with a resource
    /// whose capacity depends on which order they pressed the buttons.
    #[test]
    fn a_shared_resource_id_means_the_same_thing_everywhere() {
        let mut seen: std::collections::BTreeMap<&str, TemplateResource> =
            std::collections::BTreeMap::new();
        for t in TRADES {
            for r in t.resources {
                if let Some(first) = seen.get(r.id) {
                    assert_eq!(
                        first, r,
                        "{} declares {} differently from another trade",
                        t.id, r.id
                    );
                } else {
                    seen.insert(r.id, *r);
                }
            }
        }
    }

    /// **The gym declares nothing for the floor.** It is the fixture that
    /// proves occupancy is optional, and it only proves it while the floor
    /// stays absent.
    #[test]
    fn the_gym_books_its_classes_and_not_its_door() {
        let gym = trade("gym").unwrap_or_else(|| unreachable!("a trade this file declares"));
        assert!(
            !gym.resources.iter().any(|r| r.id.contains("floor")),
            "a gym member does not book the floor; they hold a membership"
        );
        assert!(
            gym.resources.iter().any(|r| r.capacity > 1),
            "a gym's classes take more than one person"
        );
    }
}
