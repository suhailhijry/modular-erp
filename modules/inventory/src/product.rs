//! A thing the business keeps, the unit it keeps it in, and how closely it
//! watches it.
//!
//! # Why the unit is frozen at declaration
//!
//! Every quantity ever recorded against a product is a number in its unit and
//! nothing else — there is no conversion factor anywhere in this module. So a
//! product whose unit changed from grams to kilos would silently restate a
//! thousandfold every movement already in the log, and the count that found the
//! discrepancy would be right about the shelf and wrong about the books. A new
//! unit is a new product, which is the honest record. Same decision, and the
//! same reason, as `booking::ResourceEvent::Declared` freezing branch and kind.
//!
//! # Why the tracking mode is frozen too
//!
//! For exactly that reason. A product that became serial-tracked on a Tuesday
//! would have lots behind it whose units have no names, and every rule that
//! says *"a serial-tracked line names its serials"* would be false about its
//! own history. Changing it is declaring a new product.
//!
//! # Why there is no `retired`
//!
//! A product with nothing on hand and nothing moving is already invisible on
//! every screen here, and a verb that only hides rows is a verb somebody has to
//! remember to un-do before the next delivery.

use erp_eventlog::{Aggregate, DomainEvent};
use erp_types::{DomainName, EventName, SchemaVersion, Timestamp};
use serde::{Deserialize, Serialize};

/// **How closely a product is watched**, frozen at declaration.
///
/// The mode decides what a receipt has to say and what a movement may say. It
/// does *not* decide whether lots exist: every receipt creates a lot whatever
/// the mode, because costing is per lot and a product with no lots would need a
/// second costing method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tracking {
    /// Sacks of flour. Lots exist and are FIFO layers nobody names; a receipt
    /// carries neither a code nor an expiry, and a movement is a quantity.
    #[default]
    None,
    /// Milk, medicine, paint. A receipt carries the tenant's own batch code and,
    /// when the batch has a shelf life, the date it reaches.
    Lot,
    /// Phones, machines, anything with a plate on it. A receipt names one
    /// serial per unit and a movement names the units it takes.
    Serial,
}

impl Tracking {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lot => "lot",
            Self::Serial => "serial",
        }
    }

    /// Parses what a caller sent. `None` for anything else — a mode this build
    /// does not know is refused rather than quietly treated as untracked.
    #[must_use]
    pub fn parse(literal: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == literal)
    }

    pub const ALL: [Self; 3] = [Self::None, Self::Lot, Self::Serial];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProductEvent {
    /// It exists, it is called this, it is counted in these, and it is watched
    /// this closely.
    Declared {
        name: String,
        /// The business's own word for what one of these is. See the module
        /// doc: frozen here, for ever.
        unit: String,
        /// **Defaulted when absent**, so a product declared by an older build
        /// reads back as the untracked thing it was. The only upcast this
        /// module needs so far.
        #[serde(default)]
        tracking: Tracking,
        at: Timestamp,
    },
}

impl ProductEvent {
    pub const NAMES: [&'static str; 1] = ["inventory.product.declared"];
}

impl DomainEvent for ProductEvent {
    fn event_name(&self) -> EventName {
        crate::name(match self {
            Self::Declared { .. } => Self::NAMES[0],
        })
    }

    fn schema_version(&self) -> SchemaVersion {
        crate::VERSION_1
    }
}

/// What a command needs to know about a product before deciding.
///
/// Whether it exists, and how it is tracked — which is what says whether a
/// receipt needs serials and whether a count of a quantity means anything. The
/// name and unit a screen shows come from the read model like every other read
/// (L7).
#[derive(Debug, Default, Clone)]
pub struct Product {
    declared: bool,
    tracking: Tracking,
}

impl Aggregate for Product {
    type Event = ProductEvent;

    fn domain() -> DomainName {
        crate::domain("inventory_product")
    }

    fn apply(&mut self, event: &Self::Event) {
        match event {
            ProductEvent::Declared { tracking, .. } => {
                self.declared = true;
                self.tracking = *tracking;
            }
        }
    }
}

impl Product {
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.declared
    }

    #[must_use]
    pub const fn tracking(&self) -> Tracking {
        self.tracking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A mode this build does not know is not "untracked".** A typo that
    /// silently declared a serial-tracked product as a bag of sand would be
    /// found by the first unit somebody tried to name.
    #[test]
    fn a_tracking_mode_is_one_of_three_or_none_of_them() {
        for mode in Tracking::ALL {
            assert_eq!(Tracking::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(Tracking::parse("batch"), None);
        assert_eq!(Tracking::parse("Lot"), None);
    }

    /// An older event carries no mode, and reads back as the untracked product
    /// it was declared as.
    #[test]
    fn a_product_declared_before_tracking_existed_is_untracked() {
        let older = serde_json::json!({
            "type": "declared",
            "name": "Sand",
            "unit": "kilo",
            "at": "2026-04-01T08:00:00Z",
        });
        let event: ProductEvent = serde_json::from_value(older).expect("decodes");

        let mut product = Product::default();
        product.apply(&event);
        assert!(product.exists());
        assert_eq!(product.tracking(), Tracking::None);
    }
}
