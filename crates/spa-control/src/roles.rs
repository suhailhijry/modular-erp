//! What a member of a tenant is allowed to do.
//!
//! # Why roles and not a permission matrix
//!
//! A small business has an owner, a bookkeeper, and some staff. Handing that
//! owner a grid of forty checkboxes on their first day is how a product gets
//! configured wrong and then blamed. So the surface is four roles, and the
//! grid is the thing they can graduate to.
//!
//! The architecture's answer for "graduate to" is the rule engine (Phase 5) —
//! permissions derived from *facts*, so a bookkeeper can be allowed to post
//! entries under ten thousand riyals, or only to their own branch. That refines
//! [`Role::allows`]; it does not replace it. Every capability check goes through
//! one function, which is where a fact-based override attaches.
//!
//! # Why the check is a type, not a call
//!
//! `Allowed<PostEntries>` in a handler's signature *is* the check. The failure
//! mode of `tenant.require(Capability::Post)?` is forgetting to write it, which
//! is silent, security-relevant, and invisible in review — the same argument
//! that gave `TenantDb` no public constructor.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// What a member of a tenant is.
///
/// Ordered by what they can do, most to least, which is only a documentation
/// aid — [`Role::allows`] is the authority and nothing compares roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The person who signed up. Everything, including inviting others and
    /// changing what the tenant pays for.
    Owner,
    /// Keeps the books: posts entries and maintains the chart of accounts.
    /// Everything except the tenant itself — billing, modules, and who else has
    /// access.
    ///
    /// There is deliberately no separate `admin`. With the capabilities that
    /// exist it would permit exactly this, and a role that is a synonym for
    /// another is a support question ("what is the difference?") with no answer.
    /// When something distinguishes them, it comes back.
    Accountant,
    /// Records what happens: posts entries, but does not restructure the chart
    /// they are posting into.
    Clerk,
    /// Reads. For an external accountant at year end, or a manager who should
    /// not be able to touch anything.
    Viewer,
}

/// Something a caller might be allowed to do.
///
/// Deliberately coarse. These are the distinctions the current endpoints
/// actually make; a capability nobody checks is a capability nobody has thought
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// See the tenant and everything in it.
    Read,
    /// Record what happened — journal entries, and later documents.
    PostEntries,
    /// Change the shape of the books: open, rename and close accounts, install
    /// a chart.
    ManageAccounts,
    /// Change the tenant: who has access, which modules, what it pays for.
    ManageTenant,
}

impl Capability {
    /// For error messages and audit lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::PostEntries => "post_entries",
            Self::ManageAccounts => "manage_accounts",
            Self::ManageTenant => "manage_tenant",
        }
    }
}

impl Role {
    /// **The one place authorization is decided.**
    ///
    /// Every check in the system reaches this function, which is what makes a
    /// fact-based refinement (Phase 5) a change here rather than an audit of
    /// every handler.
    #[must_use]
    pub const fn allows(self, capability: Capability) -> bool {
        match self {
            Self::Owner => true,
            Self::Accountant => !matches!(capability, Capability::ManageTenant),
            Self::Clerk => matches!(capability, Capability::Read | Capability::PostEntries),
            Self::Viewer => matches!(capability, Capability::Read),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Accountant => "accountant",
            Self::Clerk => "clerk",
            Self::Viewer => "viewer",
        }
    }

    /// Every role, for tests and for an "invite a colleague" form.
    pub const ALL: [Self; 4] = [Self::Owner, Self::Accountant, Self::Clerk, Self::Viewer];
}

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "owner" => Ok(Self::Owner),
            "accountant" => Ok(Self::Accountant),
            "clerk" => Ok(Self::Clerk),
            "viewer" => Ok(Self::Viewer),
            other => Err(UnknownRole(other.to_owned())),
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stored role this build does not recognise.
///
/// Refused rather than defaulted. Defaulting down to `Viewer` would silently
/// lock someone out; defaulting up would silently let them in. Both are worse
/// than an error naming the row.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown role {0:?}")]
pub struct UnknownRole(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_through_their_stored_form() {
        for role in Role::ALL {
            assert_eq!(role.as_str().parse::<Role>(), Ok(role));
        }
    }

    #[test]
    fn an_unknown_stored_role_is_reported_not_guessed() {
        assert!("superuser".parse::<Role>().is_err());
        assert!("".parse::<Role>().is_err());
    }

    /// **The authorization matrix, as a test.**
    ///
    /// Written out rather than derived, so a change to `allows` has to be
    /// restated here — which is the point. A permission that widens silently is
    /// the one nobody notices.
    #[test]
    fn every_role_allows_exactly_what_it_should() {
        use Capability::{ManageAccounts, ManageTenant, PostEntries, Read};

        let expected = [
            (
                Role::Owner,
                vec![Read, PostEntries, ManageAccounts, ManageTenant],
            ),
            (Role::Accountant, vec![Read, PostEntries, ManageAccounts]),
            (Role::Clerk, vec![Read, PostEntries]),
            (Role::Viewer, vec![Read]),
        ];

        for (role, allowed) in expected {
            for capability in [Read, PostEntries, ManageAccounts, ManageTenant] {
                assert_eq!(
                    role.allows(capability),
                    allowed.contains(&capability),
                    "{role} / {}",
                    capability.as_str()
                );
            }
        }
    }

    /// Deny by default: a role that gains a capability must do so deliberately.
    #[test]
    fn only_the_owner_can_change_the_tenant() {
        for role in Role::ALL {
            assert_eq!(
                role.allows(Capability::ManageTenant),
                role == Role::Owner,
                "{role} should not be able to change the tenant"
            );
        }
    }

    #[test]
    fn every_role_can_at_least_read() {
        // A member who can see nothing is a membership that should not exist.
        for role in Role::ALL {
            assert!(role.allows(Capability::Read), "{role}");
        }
    }
}
