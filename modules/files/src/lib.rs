//! Documents attached to things.
//!
//! # What is here and what is a layer down
//!
//! [`erp_storage`] knows bytes, a key and a checksum, and nothing else. This
//! module knows what a document is *for*: what it is called, what it is
//! attached to, when it was put there and when it was taken off.
//!
//! The split is what makes a tenant's choice of engine (D15) a deployment fact
//! rather than a change here. A business that keeps its own documents on its own
//! hardware is not a configuration detail — for some of them it is the reason
//! they can buy this at all.
//!
//! # An event stores a key, never a URL
//!
//! A URL is where a file is today; a key is what it is. A tenant who moves from
//! disk to object storage has not changed any of their documents, and an event
//! log full of `https://…/bucket-2019/…` would say otherwise for ever in a
//! record nobody can edit.
//!
//! # The checksum is verified on read (L6)
//!
//! A document that comes back different from what was stored is a **failure**,
//! not a warning. `erp_storage::fetch` refuses, and the handler answers with a
//! `500` and `storage.corrupt` rather than handing somebody bytes that are not
//! the document.
//!
//! # What is deliberately absent
//!
//! **Per-document authorization.** The owner is recorded and listing is by
//! owner, but "may this person see this invoice's attachments" is the same
//! question as "may this person see this invoice", and this system answers
//! neither per record yet — a role is tenant-wide. That is Phase 5's rules
//! engine, and inventing a second, weaker answer here would be a thing to
//! unpick when the real one arrives.
//!
//! **Streaming.** A file is read into memory, which is what caps it at
//! `erp_storage::MAX_BYTES`. A tenant who needs more needs a different shape
//! rather than a bigger number.
//!
//! **Thumbnails, previews, virus scanning, text extraction.** Each is a
//! separate service and none of them is what "can a business keep the signed
//! contract with the invoice" needs.

mod commands;
mod file;
pub mod http;
pub mod messages;
mod projections;

pub use commands::{FileError, attach, detach};
pub use file::{File, FileEvent, Owner, OwnerKind, UnknownOwner};
pub use projections::{Attachment, Attachments, Files, attached_to, attachment, projections};

use erp_i18n::StaticCatalog;
use erp_types::{DomainName, EventName, SchemaVersion, TenantId};

/// This module's messages, in every supported language.
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);

pub(crate) const VERSION_1: SchemaVersion = SchemaVersion::ONE;

/// This module's projection group name, for `?consistent_after=`.
pub const GROUP_NAME: &str = <Files as erp_projection::ProjectionGroup>::NAME;

const GROUPS: &[(&str, &str)] = &[(
    <Files as erp_projection::ProjectionGroup>::NAME,
    <Files as erp_projection::ProjectionGroup>::SCHEMA,
)];

/// Creates this module's read models in a tenant database.
pub async fn install(conn: &mut sqlx::PgConnection) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS proj_files; SET search_path TO proj_files, public;")
        .execute(&mut *conn)
        .await?;

    sqlx::raw_sql(include_str!("../schema/install.sql"))
        .execute(&mut *conn)
        .await?;

    sqlx::raw_sql("SET search_path TO public")
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

/// What a tenant enabling this module needs installed.
///
/// **Nothing.** A document attaches to an invoice, a booking or an employee
/// record, and a tenant with none of those can still keep their trade licence
/// against the business itself. Requiring a module here would be requiring one
/// of the seven owner kinds, and there is no reason to prefer any of them.
#[must_use]
pub fn setup() -> erp_tenant::ModuleSetup {
    erp_tenant::ModuleSetup::new(
        module_id(),
        include_str!("../schema/install.sql"),
        GROUPS,
        upcasters,
    )
}

/// This module's entitlement name.
#[must_use]
pub fn module_id() -> erp_types::ModuleId {
    erp_types::ModuleId::new("files")
        .unwrap_or_else(|_| unreachable!("a literal that satisfies ModuleId"))
}

/// Every event shape this build can read.
#[must_use]
pub fn upcasters() -> &'static erp_eventlog::Upcasters {
    static UPCASTERS: std::sync::OnceLock<erp_eventlog::Upcasters> = std::sync::OnceLock::new();
    UPCASTERS.get_or_init(|| {
        FileEvent::NAMES
            .iter()
            .fold(erp_eventlog::Upcasters::new(), |u, n| {
                u.declare(&name(n), VERSION_1)
            })
    })
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn name(literal: &'static str) -> EventName {
    EventName::new(literal).expect("event names in this crate are valid literals")
}

#[expect(
    clippy::expect_used,
    reason = "a malformed literal is a build bug, not a runtime condition"
)]
pub(crate) fn domain(literal: &'static str) -> DomainName {
    DomainName::new(literal).expect("domain names in this crate are valid literals")
}

/// Where a document's bytes go.
///
/// **Generated, never typed.** The name a person gave the file is on the record
/// and not in the key: a key with a filename in it is a key with a space, a
/// slash or an Arabic character in it, and three engines with three opinions
/// about each.
///
/// # The tenant is the first segment, and it has to be
///
/// One process serves every tenant and holds **one** `Storage`, so one bucket
/// or one directory holds all of their documents. Invoice numbers, booking
/// references and employee ids are unique inside a tenant and nowhere else —
/// two companies both having an `INV-1` is the normal case, not a collision to
/// design against. Without this segment the second one to upload overwrites the
/// first, and the first one to read gets the other company's contract.
///
/// The tenant's **id** rather than its subdomain: a company that renames itself
/// has not moved any of its documents, and a key derived from a name would say
/// otherwise for ever. It is the same argument the crate docs make for storing
/// a key instead of a URL.
#[must_use]
pub fn key_for(tenant: TenantId, owner: &Owner, id: &str) -> String {
    format!(
        "{tenant}/{}/{}/{}",
        owner.kind.as_str(),
        owner.id.as_str(),
        id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp_types::AggregateId;

    #[test]
    fn a_key_is_generated_from_ids_and_never_from_a_name() {
        let tenant = TenantId::new();
        let owner = Owner {
            kind: OwnerKind::Invoice,
            id: AggregateId::new("INV-1").expect("valid"),
        };
        let key = key_for(tenant, &owner, "doc-1");
        assert_eq!(key, format!("{tenant}/invoice/INV-1/doc-1"));
        erp_storage::check_key(&key).expect("a usable key");
    }

    /// **The collision that one bucket for every tenant would otherwise be.**
    /// Two companies both having an `INV-1` is the normal case.
    #[test]
    fn two_tenants_with_the_same_invoice_number_do_not_share_a_key() {
        let owner = Owner {
            kind: OwnerKind::Invoice,
            id: AggregateId::new("INV-1").expect("valid"),
        };
        assert_ne!(
            key_for(TenantId::new(), &owner, "doc-1"),
            key_for(TenantId::new(), &owner, "doc-1")
        );
    }

    /// Every owner kind produces a key the storage layer will accept, which is
    /// what stops the eighth one being a runtime failure on upload.
    #[test]
    fn every_owner_kind_makes_a_usable_key() {
        for kind in OwnerKind::ALL {
            let owner = Owner {
                kind,
                id: AggregateId::new("ABC-123").expect("valid"),
            };
            erp_storage::check_key(&key_for(TenantId::new(), &owner, "doc-1"))
                .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
            assert_eq!(kind.as_str().parse(), Ok(kind));
        }
    }
}
