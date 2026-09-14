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
//! one function, which is where a fact-based override attaches: `crate::limits`,
//! which a tenant's owner writes at `PUT /v1/tenant/permission-limits`. The
//! first example is one rule there, naming the `accountant` role, and it holds
//! on the ledger's entries and reversals in any currency. An invoice or a till
//! sale reaches this check before it has a total, so an amount rule cannot
//! judge one; how large one sales document may be is a different control,
//! `sales`' document limit, judged inside the command where the total exists.
//! "Their own branch" is [`Access::branches`], recorded per membership since
//! 2026-09-14 and judged by [`Access::branch_for`] before any limit is read.
//!
//! # Why the check is a type, not a call
//!
//! `Allowed<PostEntries>` in a handler's signature *is* the check. The failure
//! mode of `tenant.require(Capability::Post)?` is forgetting to write it, which
//! is silent, security-relevant, and invisible in review — the same argument
//! that gave `TenantDb` no public constructor.

use erp_types::{AggregateId, ModuleId};
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

    /// Every capability — the values a permission limit may name, less the
    /// one it cannot (`limits::narrows`).
    pub const ALL: [Self; 4] = [
        Self::Read,
        Self::PostEntries,
        Self::ManageAccounts,
        Self::ManageTenant,
    ];
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

/// What somebody may do in a tenant, module by module.
///
/// # Why a default and a handful of exceptions
///
/// Most people have one job. A structure that made every module's role explicit
/// would turn "give Sara access" into a form with a row per module, most of them
/// saying the same thing — and would silently give a new module *no* role rather
/// than the obvious one.
///
/// So [`Access::role`] is what this person is here, and [`Access::in_module`]
/// overrides it where the tenant said something different. A module nobody has
/// spoken about falls back, which is what makes adding a module to the product
/// not a permissions migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// What they are in this tenant, and in any module not named below.
    pub role: Role,
    /// Where the tenant said something different. Small — the exception, not
    /// the rule — so a `Vec` scan beats a map.
    pub overrides: Vec<(ModuleId, Role)>,
    /// **An API key, not a person.** The role still answers every capability
    /// — a key is issued one on purpose — but nothing that exempts *the owner*
    /// exempts a key issued the owner's role: see [`Self::is_owner`].
    pub machine: bool,
    /// **The branches this member belongs to**, or `None` for every branch.
    ///
    /// Decided 2026-09-14: `X-Branch` was a header the caller wrote, and this
    /// is what makes it a claim the tenant can refuse. A request from a
    /// confined member is judged by [`Self::branch_for`]; a key is bound like a
    /// person, because it is a membership like a person's.
    pub branches: Option<Vec<AggregateId>>,
}

/// Why a confined member's request was refused the branch it named, or
/// refused for naming none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchRefusal {
    /// A branch the member does not belong to.
    NotTheirs(AggregateId),
    /// No branch named, and the member belongs to several — one has to be
    /// chosen, and choosing for them would pick a place they did not mean.
    NameOne(Vec<AggregateId>),
}

impl Access {
    #[must_use]
    pub const fn new(role: Role) -> Self {
        Self {
            role,
            overrides: Vec::new(),
            machine: false,
            branches: None,
        }
    }

    /// **The branch this request is in**, given the one it named.
    ///
    /// An unconfined member gets what they asked for, named or not — every
    /// branch is theirs, and a request naming none is one at no branch, which
    /// is what every request was before confinement existed. A confined
    /// member may name one of theirs; naming none is answered with their one
    /// branch when they have exactly one, and refused when they have several.
    /// Naming one that is not theirs is refused whatever else they hold.
    ///
    /// # Errors
    /// [`BranchRefusal`], which the request layer renders as a 403.
    pub fn branch_for(
        &self,
        asked: Option<&AggregateId>,
    ) -> Result<Option<AggregateId>, BranchRefusal> {
        let Some(theirs) = &self.branches else {
            return Ok(asked.cloned());
        };
        match asked {
            Some(branch) if theirs.contains(branch) => Ok(Some(branch.clone())),
            Some(branch) => Err(BranchRefusal::NotTheirs(branch.clone())),
            None => match theirs.as_slice() {
                [only] => Ok(Some(only.clone())),
                several => Err(BranchRefusal::NameOne(several.to_vec())),
            },
        }
    }

    /// Whether this member may look across every branch at once — an
    /// unconfined one may; a confined one is answered from their list.
    #[must_use]
    pub const fn spans_branches(&self) -> bool {
        self.branches.is_none()
    }

    /// The same access, held by a machine.
    #[must_use]
    pub const fn as_machine(mut self) -> Self {
        self.machine = true;
        self
    }

    /// **A person holding the owner's role** — the one member every control
    /// in the system exempts: the document limit, the claims, the second-factor
    /// reset. A key issued the owner's role is not that person, and until
    /// 2026-09-14 it walked past all three, because each of them read the
    /// role alone. Decided by the product owner: a key is judged as an
    /// ordinary member wherever "the owner" is exempt, and a machine can hold
    /// no claim, so it is refused wherever a claim is the way past.
    #[must_use]
    pub const fn is_owner(&self) -> bool {
        matches!(self.role, Role::Owner) && !self.machine
    }

    /// The role that applies in a module, or tenant-wide when `module` is
    /// `None`.
    ///
    /// `None` is not "no module" in the sense of no permission — it is the
    /// tenant's own surface: members, invitations, entitlements. Those are not
    /// any module's business and use the tenant-wide role, which is what stops
    /// an accountant-for-sales from managing who else has access.
    #[must_use]
    pub fn role_in(&self, module: Option<&ModuleId>) -> Role {
        module
            .and_then(|m| {
                self.overrides
                    .iter()
                    .find(|(known, _)| known == m)
                    .map(|(_, role)| *role)
            })
            .unwrap_or(self.role)
    }

    /// Whether they may do this, there.
    #[must_use]
    pub fn allows(&self, capability: Capability, module: Option<&ModuleId>) -> bool {
        self.role_in(module).allows(capability)
    }
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

#[cfg(test)]
mod access_tests {
    use super::*;

    fn module(name: &str) -> ModuleId {
        ModuleId::new(name.to_owned()).unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn a_module_nobody_has_spoken_about_falls_back() {
        let access = Access::new(Role::Accountant);
        assert_eq!(access.role_in(Some(&module("sales"))), Role::Accountant);
        assert_eq!(access.role_in(None), Role::Accountant);
    }

    #[test]
    fn an_override_applies_only_where_it_was_set() {
        let mut access = Access::new(Role::Viewer);
        access.overrides.push((module("sales"), Role::Accountant));

        assert_eq!(access.role_in(Some(&module("sales"))), Role::Accountant);
        assert_eq!(access.role_in(Some(&module("ledger"))), Role::Viewer);
        assert_eq!(
            access.role_in(None),
            Role::Viewer,
            "the tenant's own surface is nobody's module"
        );

        assert!(access.allows(Capability::PostEntries, Some(&module("sales"))));
        assert!(!access.allows(Capability::PostEntries, Some(&module("ledger"))));
        assert!(!access.allows(Capability::ManageTenant, Some(&module("sales"))));
    }

    #[test]
    fn an_override_can_hold_somebody_back_as_well_as_forward() {
        // The other direction, and the one easier to get wrong: an accountant
        // everywhere, deliberately not in sales.
        let mut access = Access::new(Role::Accountant);
        access.overrides.push((module("sales"), Role::Viewer));

        assert!(access.allows(Capability::ManageAccounts, Some(&module("ledger"))));
        assert!(!access.allows(Capability::PostEntries, Some(&module("sales"))));
    }

    fn branch(id: &str) -> AggregateId {
        AggregateId::new(id).unwrap_or_else(|_| unreachable!())
    }

    /// **The branch rule, as a table.** Unconfined: what was asked. One
    /// branch: theirs when none is named, refused when another is. Several:
    /// one of theirs, or a refusal to choose for them.
    #[test]
    fn a_confined_member_acts_in_their_branches_and_nowhere_else() {
        let olaya = branch("BR-OLAYA");
        let malaz = branch("BR-MALAZ");

        let free = Access::new(Role::Clerk);
        assert_eq!(free.branch_for(None), Ok(None));
        assert_eq!(free.branch_for(Some(&malaz)), Ok(Some(malaz.clone())));
        assert!(free.spans_branches());

        let mut one = Access::new(Role::Clerk);
        one.branches = Some(vec![olaya.clone()]);
        assert_eq!(
            one.branch_for(None),
            Ok(Some(olaya.clone())),
            "one branch needs no header"
        );
        assert_eq!(one.branch_for(Some(&olaya)), Ok(Some(olaya.clone())));
        assert_eq!(
            one.branch_for(Some(&malaz)),
            Err(BranchRefusal::NotTheirs(malaz.clone()))
        );
        assert!(!one.spans_branches());

        let mut two = Access::new(Role::Clerk);
        two.branches = Some(vec![olaya.clone(), malaz.clone()]);
        assert_eq!(
            two.branch_for(None),
            Err(BranchRefusal::NameOne(vec![olaya.clone(), malaz.clone()])),
            "several branches: the request has to say which"
        );
        assert_eq!(two.branch_for(Some(&malaz)), Ok(Some(malaz)));
        assert_eq!(
            two.branch_for(Some(&branch("BR-JEDDAH"))),
            Err(BranchRefusal::NotTheirs(branch("BR-JEDDAH")))
        );
    }
}
