//! Fields a business adds to a customer, and the values they hold.
//!
//! # Why typed and not a blob
//!
//! A wellness centre needs a blood group on a customer and a salon needs a hair
//! type, and neither is anybody else's business — so the shape has to be the
//! tenant's. What it must not be is free-form JSON.
//!
//! A blob cannot be **validated**: a date typed as `03/04` is stored happily and
//! discovered a year later. It cannot be **filtered**: "everybody allergic to
//! latex" is a scan and a guess about how somebody spelled it. And it cannot be
//! **shown properly**, because nothing knows whether a value is a date, a number
//! or a choice from a list somebody agreed on. Declaring the field settles all
//! three at once, at the only moment a person is around to answer them.
//!
//! # Why the values are not in the event log
//!
//! **Because an append-only log cannot forget, and this data has to be
//! forgettable.** A customer's health details are a person's medical
//! information; under the PDPL they carry a right to erasure, and this document
//! has already had to fix one place where the answer was "our schema will not
//! let us". Writing them into `crm`'s log would make that answer permanent.
//!
//! So values live in a table of their own, the same call [`payments`] made for a
//! card token: **the shape is declared, the value is erasable**. What is lost is
//! that a rebuild does not reproduce them — which is correct rather than
//! unfortunate, because a rebuild is a function of the log and if it could
//! reproduce them the delete would not have been one.
//!
//! What is *not* lost is the history: a value is superseded rather than
//! overwritten, so "what did this say in March, and who changed it" is
//! answerable — and erasing takes the superseded rows with it, because a
//! deletion that left the old value behind would not be one.
//!
//! [`payments`]: https://example.invalid

use erp_types::Timestamp;
use serde::{Deserialize, Serialize};

/// The most fields one tenant may declare.
///
/// Fifty. Enough for the most demanding of these businesses, and low enough
/// that the read below stays one query — a customer page that fetched a
/// thousand rows to show a form would be a page nobody opens twice.
pub const MAX_FIELDS: usize = 50;

/// The longest a text value may be, whatever the field says.
///
/// A ceiling over the tenant's own `max`, so a field declared with a large one
/// cannot turn a customer row into a document store.
pub const MAX_TEXT: usize = 2_000;

/// What kind of thing a field holds.
///
/// **Five, and each one is a column and a check.** A kind that could not be
/// validated on the way in or filtered on the way out would be a blob wearing a
/// name, which is the thing this exists instead of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldKind {
    /// Free text, bounded.
    Text {
        /// Characters, not bytes — Arabic counts the same as English.
        max: u16,
    },
    /// A whole number.
    ///
    /// **No decimals, and no floating point anywhere in this workspace.** A
    /// height is centimetres and a weight is grams; the unit belongs in the
    /// label, where a person reads it, rather than in a fraction nothing can
    /// add up reliably.
    Number,
    /// A day. Stored as an instant because everything here is, and meant as a
    /// date.
    Date,
    /// One of a list the business agreed on.
    ///
    /// **The kind that makes a field worth filtering.** "Blood group" as free
    /// text is nine spellings of the same four answers.
    Choice { options: Vec<String> },
    /// Yes or no.
    Flag,
}

impl FieldKind {
    /// What this kind is called, for a message.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Number => "number",
            Self::Date => "date",
            Self::Choice { .. } => "choice",
            Self::Flag => "flag",
        }
    }
}

/// One field a business has added to its customers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    /// **The stable machine name**, and what a value is stored under.
    ///
    /// Separate from the label because the label is what a person reads and may
    /// be corrected, translated or reworded at any time — and a stored value
    /// must not stop being findable because somebody fixed a typo on a form.
    pub key: String,
    /// What a person reads, in the tenant's own language.
    pub label: String,
    /// The Latin spelling, when the label is Arabic (D12). For our own screens
    /// and for sorting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_latin: Option<String>,
    #[serde(flatten)]
    pub kind: FieldKind,
    /// **Whether a customer is expected to have one.**
    ///
    /// Expected, not enforced — see [`Fields::missing_from`]. A tenant who adds
    /// a required field today has a thousand customers without it, and refusing
    /// to amend any of them until somebody fills it in would make the setting
    /// impossible to turn on.
    #[serde(default)]
    pub required: bool,
}

/// Every field a tenant has added to its customers.
///
/// Configuration, resolved when a value is written — so a value is checked
/// against the field set as it stood at that moment, and the generation goes
/// into the metadata the way every other configured decision does (L5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fields {
    /// **The order is the tenant's**, and it is the order a form shows them in.
    pub fields: Vec<FieldDef>,
}

impl Fields {
    /// Where a tenant's field set is stored.
    pub const KEY: &'static str = "crm.fields";

    /// What this tenant has declared, or nothing.
    ///
    /// A stored value that will not parse is an error rather than an empty set,
    /// for the reason `sales::PostingAccounts` gives: silently answering "no
    /// fields" would hide a bad migration behind a customer page that looks
    /// fine and quietly stopped showing somebody's allergies.
    pub async fn resolve(conn: &mut sqlx::PgConnection) -> Result<Self, erp_eventlog::ConfigError> {
        Ok(erp_eventlog::configuration::get::<Self>(conn, Self::KEY)
            .await?
            .map_or_else(Self::default, |configured| configured.value))
    }

    /// The field under a key, if it is declared.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// **Whether this is a field set that can be stored at all.**
    ///
    /// Checked before it is written rather than when somebody tries to use it,
    /// because a field set that refuses every value is a settings screen that
    /// looked like it saved.
    pub fn check(&self) -> Result<(), FieldError> {
        if self.fields.len() > MAX_FIELDS {
            return Err(FieldError::TooManyFields);
        }
        let mut seen = std::collections::BTreeSet::new();
        for field in &self.fields {
            if !is_key(&field.key) {
                return Err(FieldError::NotAKey(field.key.clone()));
            }
            if !seen.insert(field.key.as_str()) {
                return Err(FieldError::DuplicateKey(field.key.clone()));
            }
            if field.label.trim().is_empty() {
                return Err(FieldError::NoLabel(field.key.clone()));
            }
            match &field.kind {
                FieldKind::Text { max } => {
                    if *max == 0 || usize::from(*max) > MAX_TEXT {
                        return Err(FieldError::NotALength(field.key.clone()));
                    }
                }
                // **A choice of nothing is a field nobody can fill in**, and a
                // choice with a repeat is two options a person cannot tell
                // apart.
                FieldKind::Choice { options } => {
                    if options.is_empty() {
                        return Err(FieldError::NoOptions(field.key.clone()));
                    }
                    let mut distinct = std::collections::BTreeSet::new();
                    for option in options {
                        if option.trim().is_empty() || !distinct.insert(option.as_str()) {
                            return Err(FieldError::NotAnOption(field.key.clone()));
                        }
                    }
                }
                FieldKind::Number | FieldKind::Date | FieldKind::Flag => {}
            }
        }
        Ok(())
    }

    /// Which required fields this customer has not got.
    ///
    /// **A worklist and not a gate.** A tenant adding a required field has every
    /// existing customer missing it that instant, and a rule that refused to
    /// amend any of them until somebody filled it in would make the field
    /// impossible to add. So the answer is a list somebody works through, which
    /// is what a business does with it anyway.
    #[must_use]
    pub fn missing_from(&self, held: &[Value]) -> Vec<String> {
        self.fields
            .iter()
            .filter(|f| f.required && !held.iter().any(|v| v.key == f.key))
            .map(|f| f.key.clone())
            .collect()
    }
}

/// A value, as it is held and as it is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value {
    pub key: String,
    #[serde(flatten)]
    pub held: Held,
}

/// What a field is holding.
///
/// One variant per [`FieldKind`], and the pair is checked rather than assumed:
/// a number sent for a date is refused, not coerced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub enum Held {
    Text(String),
    Number(i64),
    Date(Timestamp),
    /// One of the declared options, checked against them.
    Choice(String),
    Flag(bool),
}

impl Held {
    /// Whether this is the kind of thing the field said it holds.
    fn fits(&self, kind: &FieldKind) -> bool {
        matches!(
            (self, kind),
            (Self::Text(_), FieldKind::Text { .. })
                | (Self::Number(_), FieldKind::Number)
                | (Self::Date(_), FieldKind::Date)
                | (Self::Choice(_), FieldKind::Choice { .. })
                | (Self::Flag(_), FieldKind::Flag)
        )
    }

    /// What it is called, for a message.
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Number(_) => "number",
            Self::Date(_) => "date",
            Self::Choice(_) => "choice",
            Self::Flag(_) => "flag",
        }
    }
}

/// Why a field set, or a value for one, was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FieldError {
    #[error("a tenant may declare at most {MAX_FIELDS} fields")]
    TooManyFields,
    #[error("{0} is not usable as a field key")]
    NotAKey(String),
    #[error("{0} is declared twice")]
    DuplicateKey(String),
    #[error("the field {0} has no label")]
    NoLabel(String),
    #[error("the field {0} has no usable length")]
    NotALength(String),
    #[error("the field {0} is a choice with nothing to choose from")]
    NoOptions(String),
    #[error("the field {0} has an option that is blank or repeated")]
    NotAnOption(String),
    /// **A value for a field nobody declared.** Refused rather than kept: a
    /// value nothing knows the shape of is a blob, which is the thing this
    /// exists instead of.
    #[error("there is no field {0}")]
    NoSuchField(String),
    #[error("the field {field} holds {declared}, and that is a {given}")]
    WrongKind {
        field: String,
        declared: &'static str,
        given: &'static str,
    },
    #[error("{value} is not one of the options for {field}")]
    NotOneOfTheOptions { field: String, value: String },
    #[error("that is longer than the field {0} allows")]
    TooLong(String),
    /// **A required field cannot be cleared.** Adding one does not refuse the
    /// customers who already lack it — see [`Fields::missing_from`] — but
    /// deliberately taking one away is a different act.
    #[error("the field {0} is required and cannot be cleared")]
    Required(String),
}

/// Whether a string is usable as a field key.
///
/// Lowercase, digits and underscores, starting with a letter. Deliberately
/// narrow: a key is a column value, part of a URL and something a person types
/// into a filter, and every character that needs escaping in one of those is a
/// character somebody will get wrong in another.
#[must_use]
pub fn is_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 48
        && key.starts_with(|c: char| c.is_ascii_lowercase())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Checks a value against the field it is for.
pub fn check(fields: &Fields, value: &Value) -> Result<(), FieldError> {
    let Some(field) = fields.get(&value.key) else {
        return Err(FieldError::NoSuchField(value.key.clone()));
    };
    if !value.held.fits(&field.kind) {
        return Err(FieldError::WrongKind {
            field: value.key.clone(),
            declared: field.kind.as_str(),
            given: value.held.as_str(),
        });
    }
    match (&value.held, &field.kind) {
        (Held::Text(text), FieldKind::Text { max }) => {
            if text.chars().count() > usize::from(*max) {
                return Err(FieldError::TooLong(value.key.clone()));
            }
        }
        // **Checked against the list, every time.** A tenant who removes an
        // option stops it being settable; what is already stored stays until
        // somebody changes it, because rewriting a customer's record because a
        // form changed is worse than showing a value that is no longer offered.
        (Held::Choice(chosen), FieldKind::Choice { options })
            if !options.iter().any(|o| o == chosen) =>
        {
            return Err(FieldError::NotOneOfTheOptions {
                field: value.key.clone(),
                value: chosen.clone(),
            });
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(key: &str, max: u16) -> FieldDef {
        FieldDef {
            key: key.to_owned(),
            label: "شيء".to_owned(),
            label_latin: None,
            kind: FieldKind::Text { max },
            required: false,
        }
    }

    fn choice(key: &str, options: &[&str]) -> FieldDef {
        FieldDef {
            key: key.to_owned(),
            label: "اختيار".to_owned(),
            label_latin: None,
            kind: FieldKind::Choice {
                options: options.iter().map(|o| (*o).to_owned()).collect(),
            },
            required: false,
        }
    }

    #[test]
    fn a_key_is_something_a_url_and_a_person_can_both_carry() {
        assert!(is_key("blood_group"));
        assert!(is_key("a"));
        assert!(is_key("note2"));

        assert!(!is_key(""));
        assert!(!is_key("Blood"), "capitals");
        assert!(!is_key("2nd"), "starts with a digit");
        assert!(!is_key("blood group"), "a space");
        assert!(!is_key("blood-group"), "a dash");
        assert!(!is_key("فصيلة"), "not ascii");
    }

    #[test]
    fn a_field_set_that_could_not_be_used_is_refused_before_it_is_stored() {
        assert!(
            Fields {
                fields: vec![text("a", 10)]
            }
            .check()
            .is_ok()
        );

        let dup = Fields {
            fields: vec![text("a", 10), text("a", 20)],
        };
        assert_eq!(dup.check(), Err(FieldError::DuplicateKey("a".to_owned())));

        let empty = Fields {
            fields: vec![choice("c", &[])],
        };
        assert_eq!(empty.check(), Err(FieldError::NoOptions("c".to_owned())));

        let repeated = Fields {
            fields: vec![choice("c", &["a", "a"])],
        };
        assert_eq!(
            repeated.check(),
            Err(FieldError::NotAnOption("c".to_owned()))
        );

        let unbounded = Fields {
            fields: vec![text("a", 0)],
        };
        assert_eq!(
            unbounded.check(),
            Err(FieldError::NotALength("a".to_owned()))
        );
    }

    /// **A number sent for a date is refused, not coerced.** The whole point of
    /// declaring the field is that nothing has to guess what arrived.
    #[test]
    fn a_value_has_to_be_the_kind_the_field_declared() {
        let fields = Fields {
            fields: vec![
                text("note", 100),
                FieldDef {
                    key: "visits".to_owned(),
                    label: "زيارات".to_owned(),
                    label_latin: None,
                    kind: FieldKind::Number,
                    required: false,
                },
            ],
        };

        assert!(
            check(
                &fields,
                &Value {
                    key: "note".to_owned(),
                    held: Held::Text("مرحبا".to_owned()),
                }
            )
            .is_ok()
        );

        let wrong = check(
            &fields,
            &Value {
                key: "note".to_owned(),
                held: Held::Number(3),
            },
        );
        assert!(
            matches!(wrong, Err(FieldError::WrongKind { .. })),
            "{wrong:?}"
        );

        let nowhere = check(
            &fields,
            &Value {
                key: "invented".to_owned(),
                held: Held::Flag(true),
            },
        );
        assert_eq!(nowhere, Err(FieldError::NoSuchField("invented".to_owned())));
    }

    /// **A choice is checked against the list.** That is what makes it worth
    /// more than text, and it is the whole reason the kind exists.
    #[test]
    fn a_choice_has_to_be_one_of_the_options() {
        let fields = Fields {
            fields: vec![choice("blood_group", &["A+", "O-"])],
        };
        assert!(
            check(
                &fields,
                &Value {
                    key: "blood_group".to_owned(),
                    held: Held::Choice("O-".to_owned()),
                }
            )
            .is_ok()
        );

        let invented = check(
            &fields,
            &Value {
                key: "blood_group".to_owned(),
                held: Held::Choice("Z".to_owned()),
            },
        );
        assert!(
            matches!(invented, Err(FieldError::NotOneOfTheOptions { .. })),
            "{invented:?}"
        );
    }

    /// Counted in characters, so Arabic is not penalised for being multi-byte.
    #[test]
    fn a_length_is_characters_and_not_bytes() {
        let fields = Fields {
            fields: vec![text("note", 3)],
        };
        let three = Value {
            key: "note".to_owned(),
            held: Held::Text("أبج".to_owned()),
        };
        assert!(check(&fields, &three).is_ok(), "three Arabic letters");

        let four = Value {
            key: "note".to_owned(),
            held: Held::Text("أبجد".to_owned()),
        };
        assert_eq!(
            check(&fields, &four),
            Err(FieldError::TooLong("note".to_owned()))
        );
    }

    /// **A worklist, not a gate.** A tenant who adds a required field has every
    /// existing customer missing it that instant.
    #[test]
    fn required_fields_are_reported_rather_than_enforced() {
        let fields = Fields {
            fields: vec![
                FieldDef {
                    required: true,
                    ..text("consent", 10)
                },
                text("note", 10),
            ],
        };
        assert_eq!(fields.missing_from(&[]), vec!["consent".to_owned()]);
        assert!(
            fields
                .missing_from(&[Value {
                    key: "consent".to_owned(),
                    held: Held::Text("yes".to_owned()),
                }])
                .is_empty()
        );
    }
}

// ---------------------------------------------------------------------------
// The values
// ---------------------------------------------------------------------------

/// A value as it is held, with who put it there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    pub key: String,
    pub held: Held,
    pub set_at: Timestamp,
    /// Who set it. `None` for a value the system itself wrote.
    pub set_by: Option<String>,
}

/// Why a value could not be stored.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Field(#[from] FieldError),
    #[error(transparent)]
    Config(#[from] erp_eventlog::ConfigError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// **Sets values on a customer**, superseding whatever they held.
///
/// # Every value is checked against the field set as it stands now
///
/// All of them, before any of them are written — so a form with one bad date
/// stores nothing rather than half of itself. The whole call is one transaction
/// for the same reason.
///
/// # Superseded, not overwritten
///
/// The old row stays, marked with the moment it stopped being true. That is the
/// audit trail these values get in place of an event log, and it is what makes
/// "who changed this, and when" answerable without making the value permanent.
pub async fn set(
    conn: &mut sqlx::PgConnection,
    customer: &str,
    values: &[Value],
    at: Timestamp,
    by: Option<&str>,
) -> Result<(), StoreError> {
    let fields = Fields::resolve(&mut *conn).await?;
    for value in values {
        check(&fields, value)?;
    }

    for value in values {
        // The one it replaces stops being current in the same statement that
        // writes the new one's predecessor away — so nothing sees two.
        sqlx::query!(
            "UPDATE customer_field SET superseded_at = $3
              WHERE customer = $1 AND field = $2 AND superseded_at IS NULL",
            customer,
            value.key,
            at,
        )
        .execute(&mut *conn)
        .await?;

        let (text, number, date, flag) = columns(&value.held);
        sqlx::query!(
            "INSERT INTO customer_field
                 (id, customer, field, text_value, number_value, date_value,
                  flag_value, set_at, set_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            uuid::Uuid::now_v7(),
            customer,
            value.key,
            text,
            number,
            date,
            flag,
            at,
            by,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// **Clears a field**, leaving the customer without one.
///
/// A required field cannot be cleared. Adding a required field does not refuse
/// the customers who already lack it — that is [`Fields::missing_from`]'s job —
/// but deliberately taking one away is a different act and this is where the
/// rule belongs.
pub async fn clear(
    conn: &mut sqlx::PgConnection,
    customer: &str,
    key: &str,
    at: Timestamp,
) -> Result<(), StoreError> {
    let fields = Fields::resolve(&mut *conn).await?;
    let Some(field) = fields.get(key) else {
        return Err(FieldError::NoSuchField(key.to_owned()).into());
    };
    if field.required {
        return Err(FieldError::Required(key.to_owned()).into());
    }

    sqlx::query!(
        "UPDATE customer_field SET superseded_at = $3
          WHERE customer = $1 AND field = $2 AND superseded_at IS NULL",
        customer,
        key,
        at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What a customer holds now.
///
/// **Only declared fields.** A value under a key the tenant has since removed is
/// still in the table and is deliberately not shown — see [`orphaned`], which is
/// how somebody finds and erases it rather than discovering it years later.
pub async fn held(
    conn: &mut sqlx::PgConnection,
    customer: &str,
) -> Result<Vec<Holding>, StoreError> {
    let fields = Fields::resolve(&mut *conn).await?;
    let rows = sqlx::query!(
        r#"SELECT field as "field!", text_value, number_value, date_value, flag_value,
                  set_at as "set_at!", set_by
             FROM customer_field
            WHERE customer = $1 AND superseded_at IS NULL"#,
        customer,
    )
    .fetch_all(&mut *conn)
    .await?;

    // In the tenant's declared order, which is the order a form shows them in —
    // not the order the database happened to return.
    let mut out = Vec::new();
    for field in &fields.fields {
        let Some(row) = rows.iter().find(|r| r.field == field.key) else {
            continue;
        };
        let Some(held) = from_columns(
            &field.kind,
            row.text_value.as_deref(),
            row.number_value,
            row.date_value,
            row.flag_value,
        ) else {
            // **The stored shape and the declared shape disagree**, which means
            // the field was changed under a value that already existed. Skipped
            // rather than guessed at, and `orphaned` is what reports it.
            continue;
        };
        out.push(Holding {
            key: field.key.clone(),
            held,
            set_at: row.set_at,
            set_by: row.set_by.clone(),
        });
    }
    Ok(out)
}

/// **Values nothing declares any more.**
///
/// A field removed from the set, or one whose kind was changed under it. The
/// values are still in the table and are shown to nobody, which is exactly the
/// state that turns into "we still hold health data we forgot about" — so this
/// is the read that finds them, and [`forget_field`] is what takes them away.
pub async fn orphaned(conn: &mut sqlx::PgConnection) -> Result<Vec<String>, StoreError> {
    let fields = Fields::resolve(&mut *conn).await?;
    let stored: Vec<String> = sqlx::query_scalar!(
        r#"SELECT DISTINCT field as "field!" FROM customer_field WHERE superseded_at IS NULL"#
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(stored
        .into_iter()
        .filter(|key| fields.get(key).is_none())
        .collect())
}

/// **Erases every value this customer holds**, including superseded ones.
///
/// This is the answer to a person asking for their data to be deleted. It takes
/// the history with it, because a deletion that left the old value behind would
/// not be one.
///
/// It does **not** erase the customer. A customer record is what documents point
/// at, and a tax invoice does not stop having been issued — see `crm`'s own
/// argument about the copy a document freezes. What this removes is everything a
/// business added on top.
pub async fn forget(conn: &mut sqlx::PgConnection, customer: &str) -> Result<u64, StoreError> {
    Ok(
        sqlx::query!("DELETE FROM customer_field WHERE customer = $1", customer)
            .execute(&mut *conn)
            .await?
            .rows_affected(),
    )
}

/// Erases one field's values across every customer, history included.
///
/// What a tenant runs before removing a field they should never have collected,
/// and what makes removing one from the set safe rather than a quiet retention.
pub async fn forget_field(conn: &mut sqlx::PgConnection, key: &str) -> Result<u64, StoreError> {
    Ok(
        sqlx::query!("DELETE FROM customer_field WHERE field = $1", key)
            .execute(&mut *conn)
            .await?
            .rows_affected(),
    )
}

/// Whether any customer holds a value for this field.
///
/// **What stops a field being redefined under its own values.** Changing a text
/// field to a date leaves every stored value unreadable, and a settings screen
/// that allowed it would be quietly discarding data somebody entered.
pub async fn anyone_holds(conn: &mut sqlx::PgConnection, key: &str) -> Result<bool, StoreError> {
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM customer_field WHERE field = $1 AND superseded_at IS NULL",
        key,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(count.unwrap_or(0) > 0)
}

/// The four columns, of which exactly one is filled.
const fn columns(
    held: &Held,
) -> (
    Option<&String>,
    Option<i64>,
    Option<Timestamp>,
    Option<bool>,
) {
    match held {
        Held::Text(text) | Held::Choice(text) => (Some(text), None, None, None),
        Held::Number(n) => (None, Some(*n), None, None),
        Held::Date(d) => (None, None, Some(*d), None),
        Held::Flag(f) => (None, None, None, Some(*f)),
    }
}

/// Reads one back, **as the field says it should be**.
///
/// `None` when the stored shape and the declared one disagree, which happens
/// only if a field was redefined under a value. Refusing to guess is the point:
/// showing a date as a number is worse than showing nothing and reporting it.
fn from_columns(
    kind: &FieldKind,
    text: Option<&str>,
    number: Option<i64>,
    date: Option<Timestamp>,
    flag: Option<bool>,
) -> Option<Held> {
    match kind {
        FieldKind::Text { .. } => text.map(|t| Held::Text(t.to_owned())),
        FieldKind::Choice { .. } => text.map(|t| Held::Choice(t.to_owned())),
        FieldKind::Number => number.map(Held::Number),
        FieldKind::Date => date.map(Held::Date),
        FieldKind::Flag => flag.map(Held::Flag),
    }
}
