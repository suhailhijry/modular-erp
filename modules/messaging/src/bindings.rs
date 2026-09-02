//! Answering what a template asked for.
//!
//! The other half of 11b. [`crate::template`] declares a vocabulary and refuses
//! anything outside it when a template is saved; this resolves that vocabulary
//! against the read models at the moment a message is sent.
//!
//! # Why this is not a cross-group read
//!
//! `proj_booking`, `proj_crm`, `proj_sales` and `proj_hr` are four groups and
//! none may read another (L3). Nothing here does: it calls each module's own
//! read function and assembles the answers in Rust, which is exactly what
//! `tax_sa::report` does and for the same reason.
//!
//! What L3 protects is that a group is the unit of consistency, and the
//! protection is unchanged. A message is not a total — it is a sentence with a
//! name and a time in it — so four groups an event apart produce the same
//! sentence.
//!
//! # Why at send time rather than when the message was decided
//!
//! Because **a reminder for a booking that moved must say the new time.** L5
//! says an *event* carries the outcome a command decided; an effect is a
//! promise to do something later, and what it should say is what is true when
//! it is said. Freezing the wording when the reminder was scheduled is how a
//! customer gets told to come at ten for an appointment somebody moved to two.

use std::collections::BTreeMap;

use erp_types::Timestamp;
use sqlx::PgConnection;

use crate::audience::{Subject, Topic};

/// How an instant reads in a message.
///
/// Minutes, and no seconds or zone: a person reading "your appointment is at
/// 2026-05-04 10:00" knows what that means, and "10:00:00+03:00" is a machine
/// talking. **UTC**, because that is what is stored and this system does not
/// yet know a tenant's zone — see the note in `crate::messages`.
fn when(at: Timestamp) -> String {
    at.format("%Y-%m-%d %H:%M").to_string()
}

/// Everything a template about this subject may say.
///
/// **Absent rather than empty** when something is not there: an unresolved
/// binding keeps its braces (see [`crate::template::render`]), which somebody
/// notices and fixes. An empty string would render as a sentence with a hole in
/// it that reads as if it were finished.
pub async fn of(
    conn: &mut PgConnection,
    subject: &Subject,
) -> Result<BTreeMap<String, String>, sqlx::Error> {
    let mut values = BTreeMap::new();
    let id = subject.id.as_str();

    match subject.topic {
        Topic::Reservation => reservation(conn, id, &mut values).await?,
        Topic::Invoice => invoice(conn, id, &mut values).await?,
        Topic::Customer => customer(conn, id, &mut values).await?,
        Topic::Employee => employee(conn, id, &mut values).await?,
    }

    Ok(values)
}

async fn reservation(
    conn: &mut PgConnection,
    id: &str,
    values: &mut BTreeMap<String, String>,
) -> Result<(), sqlx::Error> {
    let Some(detail) = booking::reservation(conn, id).await? else {
        return Ok(());
    };

    values.insert("reservation.id".to_owned(), detail.summary.id.clone());
    values.insert(
        "reservation.starts_at".to_owned(),
        when(detail.summary.starts_at),
    );
    values.insert(
        "reservation.ends_at".to_owned(),
        when(detail.summary.ends_at),
    );
    values.insert("reservation.stage".to_owned(), detail.summary.stage.clone());

    // **The name on the booking, not the `crm` record's.** A walk-in has no
    // record and still has a name, and a booking taken under "Noura" should not
    // start saying "N. Al-Otaibi" because somebody tidied the address book.
    values.insert(
        "customer.name".to_owned(),
        detail.summary.customer_name.clone(),
    );
    if let Some(phone) = detail.summary.customer_phone.clone() {
        values.insert("customer.phone".to_owned(), phone);
    }

    // The worker and the branch come off whichever resource names them, which
    // is the same walk `crate::audience` does.
    for line in &detail.lines {
        for held in &line.takes {
            let Some(resource) = booking::resource(conn, held.resource.as_str()).await? else {
                continue;
            };
            if let Some(employee) = resource.summary.employee.as_deref()
                && !values.contains_key("worker.name")
                && let Some(person) = hr::employee(conn, employee).await?
            {
                values.insert("worker.name".to_owned(), person.name);
            }
            if let Some(branch) = resource.summary.branch.as_deref()
                && !values.contains_key("branch.name")
                && let Some(place) = branches::branch(conn, branch).await?
            {
                values.insert("branch.name".to_owned(), place.name);
            }
        }
    }
    Ok(())
}

async fn invoice(
    conn: &mut PgConnection,
    id: &str,
    values: &mut BTreeMap<String, String>,
) -> Result<(), sqlx::Error> {
    let Some(detail) = sales::invoice(conn, id).await? else {
        return Ok(());
    };
    let summary = &detail.summary;

    values.insert("invoice.id".to_owned(), summary.id.clone());
    values.insert("invoice.number".to_owned(), summary.number.clone());
    values.insert("invoice.issued_on".to_owned(), when(summary.issued_on));
    if let Some(due) = summary.due_on {
        values.insert("invoice.due_on".to_owned(), when(due));
    }
    values.insert("invoice.total".to_owned(), summary.gross.to_string());
    values.insert(
        "invoice.outstanding".to_owned(),
        summary.outstanding.to_string(),
    );
    // **The name the document printed.** Not the `crm` record's, for the reason
    // `sales::attach_customer` never restates it: a filed document says what it
    // said.
    values.insert("customer.name".to_owned(), summary.customer.clone());
    Ok(())
}

async fn customer(
    conn: &mut PgConnection,
    id: &str,
    values: &mut BTreeMap<String, String>,
) -> Result<(), sqlx::Error> {
    let Some(detail) = crm::customer(conn, id).await? else {
        return Ok(());
    };
    values.insert("customer.id".to_owned(), detail.summary.id.clone());
    values.insert("customer.name".to_owned(), detail.summary.name.clone());
    if let Some(phone) = detail.summary.phone.clone() {
        values.insert("customer.phone".to_owned(), phone);
    }
    if let Some(email) = detail.summary.email.clone() {
        values.insert("customer.email".to_owned(), email);
    }
    Ok(())
}

async fn employee(
    conn: &mut PgConnection,
    id: &str,
    values: &mut BTreeMap<String, String>,
) -> Result<(), sqlx::Error> {
    let Some(person) = hr::employee(conn, id).await? else {
        return Ok(());
    };
    values.insert("employee.id".to_owned(), person.id.clone());
    values.insert("employee.name".to_owned(), person.name.clone());
    if let Some(branch) = person.branch.as_deref()
        && let Some(place) = branches::branch(conn, branch).await?
    {
        values.insert("employee.branch".to_owned(), place.name);
    }
    Ok(())
}
