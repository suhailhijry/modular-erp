//! What a tenant has switched on, and what switching it on installs.

use erp_types::ModuleId;

/// The set of modules live for a tenant, resolved once and carried on the
/// [`TenantDb`](crate::TenantDb) handle.
///
/// Sorted, so equality and logging are stable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnabledModules(Vec<ModuleId>);

impl EnabledModules {
    #[must_use]
    pub fn new(mut modules: Vec<ModuleId>) -> Self {
        modules.sort();
        modules.dedup();
        Self(modules)
    }

    #[must_use]
    pub fn contains(&self, module: &ModuleId) -> bool {
        self.0.binary_search(module).is_ok()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModuleId> {
        self.0.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What a module needs installed in a tenant that enables it.
///
/// Data, not a trait: there is exactly one thing to do with it, and a trait
/// would be an interface with one method and one implementation per module.
#[derive(Debug, Clone)]
pub struct ModuleSetup {
    pub module: ModuleId,
    /// Idempotent DDL. Run with `raw_sql`, so it may hold several statements.
    ///
    /// **Structure only.** Data a module needs in order to work goes in
    /// [`Self::seed_sql`], which runs after this.
    pub install_sql: &'static str,
    /// The data a module cannot work without, if it has any.
    ///
    /// # Why this is separate from the DDL
    ///
    /// The Saudi rate used to ride on `tax_sa`'s schema install, because that
    /// was the only hook a module had. It worked — the insert is idempotent, so
    /// re-running it was harmless — and it made two different things look like
    /// one. A tenant's *data* was being written by something named "install
    /// schema", which is the sort of thing that is fine until somebody makes the
    /// reasonable-looking change of running the DDL somewhere the data must not
    /// go.
    ///
    /// `just prepare` is already that somewhere: it installs every module's DDL
    /// into a throwaway type-check database, where a `configuration` row is
    /// noise at best.
    ///
    /// Run **after** the install, under the same `search_path`, so it can write
    /// both the module's own tables and the tenant's `public` ones. Idempotent,
    /// like the DDL, because a rebuild runs both again.
    pub seed_sql: &'static str,
    /// The projection groups this module owns, as `(name, schema)`.
    pub groups: &'static [(&'static str, &'static str)],
    /// Every event shape this module can read, and the version it writes.
    ///
    /// Declared here, and **required** in [`Self::new`] rather than added by a
    /// builder method, because a module that forgot it would be invisible to the
    /// pre-deploy version gate — and invisible is exactly the answer that lets a
    /// build ship that cannot read the fleet's logs.
    ///
    /// A function pointer rather than a reference so the whole thing stays
    /// const-constructible; every module's is a `OnceLock` behind one.
    pub upcasters: fn() -> &'static erp_eventlog::Upcasters,
    /// Modules this one cannot work without, by name. **All of them.**
    ///
    /// Declared here rather than checked at each call site, because three
    /// places need the same answer: signing up, enabling later, and refusing to
    /// disable something another module is standing on.
    pub requires: &'static [&'static str],
    /// Modules this one needs **at least one of**.
    ///
    /// # Why an AND list was not enough
    ///
    /// `tax_sa` computes a VAT return, which nets output tax against input tax.
    /// It needs a source for at least one of those sides and does not care
    /// which: a business that only sells still files a return, and so does one
    /// that has bought but not yet sold. Putting `sales` and `purchases` in
    /// [`Self::requires`] would force a shop with no supplier bills to enable a
    /// module they do not use; leaving both out — which is what it did — let a
    /// tenant turn on a return with nothing on either side, and let them
    /// disable the last module feeding it without a word.
    ///
    /// One group, not a list of groups. "ledger AND (sales OR purchases)" is
    /// what this system needs and a second disjunction has no consumer; the
    /// shape that takes nested alternatives can arrive with the module that
    /// wants one.
    pub requires_any: &'static [&'static str],
    /// Why this module is no longer offered, if it is not.
    ///
    /// # Why modules are deprecated and never removed
    ///
    /// A build that drops a module strands every tenant entitled to it: their
    /// events are in the log with nothing that can read them, their read models
    /// stop being refreshed, and their routes 404 with no explanation. Nothing
    /// about that is recoverable by the tenant.
    ///
    /// So a module on its way out stays in the build and stops being *offered*.
    /// Nobody new can enable it, the catalogue says why, and the tenants who
    /// have it keep working until they are migrated off deliberately. It leaves
    /// the build when the last entitlement does, which is a fact somebody can
    /// check rather than a date somebody guessed.
    pub deprecated: Option<&'static str>,
}

impl ModuleSetup {
    #[must_use]
    pub const fn new(
        module: ModuleId,
        install_sql: &'static str,
        groups: &'static [(&'static str, &'static str)],
        upcasters: fn() -> &'static erp_eventlog::Upcasters,
    ) -> Self {
        Self {
            module,
            install_sql,
            seed_sql: "",
            groups,
            upcasters,
            requires: &[],
            requires_any: &[],
            deprecated: None,
        }
    }

    /// The data this module cannot work without. See [`Self::seed_sql`].
    #[must_use]
    pub const fn seeding(mut self, sql: &'static str) -> Self {
        self.seed_sql = sql;
        self
    }

    /// Marks a module as no longer offered, and says why.
    ///
    /// Existing tenants keep it. See [`Self::deprecated`].
    #[must_use]
    pub const fn deprecated(mut self, why: &'static str) -> Self {
        self.deprecated = Some(why);
        self
    }

    /// Names the modules this one needs underneath it. All of them.
    #[must_use]
    pub const fn requiring(mut self, modules: &'static [&'static str]) -> Self {
        self.requires = modules;
        self
    }

    /// Names the modules this one needs **at least one** of. See
    /// [`Self::requires_any`].
    #[must_use]
    pub const fn requiring_any(mut self, modules: &'static [&'static str]) -> Self {
        self.requires_any = modules;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_modules_are_sorted_and_deduplicated() {
        let ledger = ModuleId::new("ledger").unwrap();
        let invoicing = ModuleId::new("invoicing").unwrap();
        let modules = EnabledModules::new(vec![ledger.clone(), invoicing.clone(), ledger.clone()]);
        assert_eq!(modules.len(), 2);
        assert!(modules.contains(&ledger));
        assert!(modules.contains(&invoicing));
        assert!(!modules.contains(&ModuleId::new("inventory").unwrap()));
        // Sorted, so two equal sets built in different orders compare equal.
        assert_eq!(modules, EnabledModules::new(vec![invoicing, ledger]));
    }
}
