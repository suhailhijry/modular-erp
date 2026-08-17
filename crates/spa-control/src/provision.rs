//! Turning a signup into a working tenant.
//!
//! # Why this is not a transaction
//!
//! `CREATE DATABASE` cannot run inside one, and the work spans two databases
//! anyway. So partial failure is real: a tenant row can exist with no database
//! behind it, or a database with no schema in it.
//!
//! Two things make that survivable, and neither is a workflow engine:
//!
//! - **Every step is idempotent.** Re-running from the start is always safe, so
//!   "recover" and "retry" are the same operation.
//! - **A failure compensates.** [`ControlPlane::provision`] drops the database
//!   and the row on its way out, which frees the name — and the person who just
//!   failed to sign up is exactly the person about to try that name again.
//!
//! The architecture calls for signup as a durable event-sourced workflow. That
//! is the right shape when a step can block for hours — a payment, a DNS record,
//! a human. Every step here is a second of local SQL, and a durable log of five
//! synchronous statements is machinery around a problem idempotency already
//! solved. ponytail: revisit when a step goes async.
//!
//! # Why modules arrive as data
//!
//! The control plane must not know what a ledger is (D11), and a module must not
//! know what provisioning is. So a module *describes* its installation — some
//! SQL and the projection groups it owns — and this runs it.
//!
//! # Why [`ControlPlane::provision`] looks the way it does
//!
//! An axum handler's future must be `Send`, and rustc cannot prove that for a
//! chain of `async fn`s carrying elided lifetimes. It reports the failure at the
//! *route table*, naming borrows from files that look unrelated
//! (rust-lang/rust#102211), and `#[axum::debug_handler]` finds nothing.
//!
//! Four things break it, each one on its own. All four are avoided here, and
//! none of them is decoration:
//!
//! 1. **A helper `async fn` taking several references.** An
//!    `install_modules(&Tenant, &[ModuleSetup], &mut PgConnection)` is three
//!    elided lifetimes. Inlined instead.
//! 2. **A borrowed iterator held across an await.** `for setup in &modules`
//!    carries a `slice::Iter<'_, _>`; indexing does not. This is why the loops
//!    below are indexed and clippy's `needless_range_loop` is silenced.
//! 3. **A closure capturing by reference across an await.** Any such closure
//!    here must be `move`.
//! 4. **A generic `Acquire<'_>` bound reaching the caller.** `raw_sql` and
//!    `Migrator::run` both have one. [`run_ddl`] and [`migrate`] take and return
//!    the connection **by value**, which keeps the bound inside them —
//!    `Box::pin` does *not* help, because the opaque future still carries it.
//!    `Migrator::run_direct` exists for exactly this and says so in sqlx's own
//!    source.
//!
//! [`provision_is_send`](self) at the bottom of this file is the check. It fails
//! here, next to the cause, rather than three crates away at a route table.

use spa_types::{IdentityId, ModuleId, TenantId};
use sqlx::{Connection, PgConnection, PgPool};

use crate::model::{Actor, Scope, Tenant};
use crate::{AccessError, ControlPlane, PlacementPolicy, TenantStatus};

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
    pub upcasters: fn() -> &'static spa_eventlog::Upcasters,
    /// Modules this one cannot work without, by name.
    ///
    /// Declared here rather than checked at each call site, because three
    /// places need the same answer: signing up, enabling later, and refusing to
    /// disable something another module is standing on.
    pub requires: &'static [&'static str],
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
        upcasters: fn() -> &'static spa_eventlog::Upcasters,
    ) -> Self {
        Self {
            module,
            install_sql,
            seed_sql: "",
            groups,
            upcasters,
            requires: &[],
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

    /// Names the modules this one needs underneath it.
    #[must_use]
    pub const fn requiring(mut self, modules: &'static [&'static str]) -> Self {
        self.requires = modules;
        self
    }
}

/// A finished signup.
#[derive(Debug)]
pub struct SignedUp {
    pub tenant: Tenant,
    pub identity: IdentityId,
    pub token: crate::SessionToken,
    pub session: crate::Session,
}

impl ControlPlane {
    /// Everything a signup does: an account, a tenant, its database, its
    /// modules, and a session to start using it with.
    ///
    /// One method rather than five calls from the API layer. It is one business
    /// operation — a half-done signup is not a state anyone wants to name — and
    /// it keeps the `async fn` chain short enough to stay provably `Send`.
    pub async fn sign_up(
        &self,
        email: String,
        password: String,
        slug: String,
        company: String,
        modules: Vec<ModuleSetup>,
    ) -> Result<SignedUp, AccessError> {
        // The identity first: it is the only step with no cleanup, so a failure
        // later leaves an account with no tenant rather than a tenant with no
        // owner.
        //
        // An address that already has an account has to **prove it**. This used
        // to be an upsert that overwrote the existing password — signing up with
        // somebody else's email took their account over. Now the same person
        // signing up for a second company logs in on the way through, and
        // anybody else gets `InvalidCredentials`.
        let identity = if let Some(existing) = self.identity_for_handle(&email).await? {
            self.authenticate(&email, &password)
                .await
                .map_err(AccessError::Auth)?;
            existing
        } else {
            let created = self.create_identity(Actor::system()).await?;
            self.register_login(created.id, email, password)
                .await
                .map_err(AccessError::Auth)?;
            created.id
        };

        let tenant = self.provision(slug, company, identity, modules).await?;

        let (token, session) = self
            .start_session(identity)
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;

        Ok(SignedUp {
            tenant,
            identity,
            token,
            session,
        })
    }

    /// Registers a tenant, builds its database, installs its modules, and grants
    /// the owner their membership.
    ///
    /// Compensates on failure — the database is dropped and the row deleted, so
    /// the name is free again. Returns the activated tenant.
    #[expect(
        clippy::needless_range_loop,
        reason = "indexed on purpose; a borrowed iterator held across an await \
                  costs this function its `Send` proof, and clippy cannot see \
                  that. See the module docs."
    )]
    pub fn provision(
        &self,
        slug: String,
        company: String,
        owner: IdentityId,
        modules: Vec<ModuleSetup>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Tenant, AccessError>> + Send + '_>> {
        Box::pin(async move {
            // Placement picks the cluster (D13). A taken slug fails here, before
            // anything is built and with nothing to undo.
            let mut tenant = Box::pin(self.register_tenant(
                &slug,
                &company,
                PlacementPolicy::Balanced,
                Actor::system(),
            ))
            .await?;

            let built: Result<(), AccessError> = 'build: {
                // --- the database ------------------------------------------------
                let admin = match self.tenants.cluster_options(&tenant.cluster) {
                    Ok(options) => options.database("postgres"),
                    Err(e) => break 'build Err(e.into()),
                };
                let maintenance = match Box::pin(PgConnection::connect_with(&admin)).await {
                    Ok(conn) => conn,
                    Err(e) => break 'build Err(AccessError::Database(e)),
                };

                // `CREATE DATABASE` cannot be parameterized and cannot run in a
                // transaction. The name is generated by `tenant_database_name` and
                // the column's CHECK refuses anything outside `[a-z][a-z0-9_]*`, so
                // it cannot carry input — but the character set is verified anyway,
                // because "it's internal" is how injection is argued into existence.
                let quoted = match quote_ident(&tenant.database_name) {
                    Ok(name) => name,
                    Err(e) => break 'build Err(e),
                };
                match run_ddl(maintenance, format!("CREATE DATABASE {quoted}")).await {
                    Ok(conn) => {
                        conn.close().await.ok();
                    }
                    // 42P04: already exists. The idempotent case, not a failure.
                    Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P04") => {
                        tracing::debug!(
                            tenant = %tenant.id,
                            database = %tenant.database_name,
                            "database already exists; migrating it"
                        );
                    }
                    Err(e) => break 'build Err(AccessError::Database(e)),
                }

                // --- the tenant schema -------------------------------------------
                let tenant_options = match self.tenants.cluster_options(&tenant.cluster) {
                    Ok(options) => options.database(&tenant.database_name),
                    Err(e) => break 'build Err(e.into()),
                };
                let conn = match Box::pin(PgConnection::connect_with(&tenant_options)).await {
                    Ok(conn) => conn,
                    Err(e) => break 'build Err(AccessError::Database(e)),
                };
                let mut conn = match migrate(conn).await {
                    Ok(conn) => conn,
                    Err(e) => {
                        break 'build Err(AccessError::Corrupt(format!("tenant migrations: {e}")));
                    }
                };

                // --- the modules --------------------------------------------------
                //
                // Indexed rather than iterated: `for setup in &modules` holds a
                // `slice::Iter` across the awaits below, and a borrowed iterator is
                // one of the things that costs this function its `Send`.
                for index in 0..modules.len() {
                    let setup = modules[index].clone();

                    // Entitlement before schema, so a retry can read back what was
                    // wanted even if it died part-way through installing. Safe
                    // here and *not* safe on a live tenant — see `install_module`.
                    if let Err(e) =
                        Box::pin(self.enable_module(tenant.id, &setup.module, Actor::system()))
                            .await
                    {
                        break 'build Err(e);
                    }

                    conn = match install_schema(conn, setup).await {
                        Ok(conn) => conn,
                        Err(e) => break 'build Err(e),
                    };
                }
                conn.close().await.ok();

                // --- the owner ----------------------------------------------------
                let granted = Box::pin(self.grant_membership(
                    owner,
                    Scope::Tenant(tenant.id),
                    "owner",
                    Actor::system(),
                ))
                .await;
                if let Err(e) = granted {
                    break 'build Err(e);
                }

                // Last. Until this, the tenant is invisible to `enter` and to the
                // worker, so nothing can observe it half-built.
                Box::pin(self.activate_tenant(tenant.id, Actor::system())).await
            };

            match built {
                Ok(()) => {
                    // The row was activated above; this copy was read before that.
                    // Returning the stale one would have every caller believe a
                    // working tenant is still being built.
                    tenant.status = TenantStatus::Active;
                    Ok(tenant)
                }
                Err(e) => {
                    if let Err(cleanup) = Box::pin(self.abandon(tenant.clone())).await {
                        // Logged, not returned: the caller's signup failed either
                        // way, and it is the operator who needs to know a database
                        // was left behind.
                        tracing::error!(
                            tenant = %tenant.id,
                            slug = %tenant.slug,
                            error = %cleanup,
                            "could not abandon a half-built tenant; its name is still taken"
                        );
                    }
                    Err(e)
                }
            }
        })
    }

    /// Turns a module on for a tenant that is already running.
    ///
    /// # Schema first, entitlement second — the opposite of `provision`
    ///
    /// During provisioning the tenant is invisible, so entitling early is free
    /// and buys retry visibility. Here the tenant is *live*: entitling before
    /// the tables exist opens a window in which the module's routes are found
    /// and every one of them fails on a missing relation. So the schema goes in
    /// first, and the entitlement — the thing that makes it visible — last.
    ///
    /// Idempotent throughout, and it does not check dependencies: what a module
    /// needs underneath it is [`ModuleSetup::requires`], and refusing belongs at
    /// the boundary that can say so in the caller's language.
    pub async fn install_module(
        &self,
        tenant_id: TenantId,
        setup: ModuleSetup,
        actor: Actor,
    ) -> Result<(), AccessError> {
        let tenant = self
            .tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;

        let options = self
            .tenants
            .cluster_options(&tenant.cluster)?
            .database(&tenant.database_name);

        let module = setup.module.clone();
        let conn = Box::pin(PgConnection::connect_with(&options)).await?;
        let conn = install_schema(conn, setup).await?;
        conn.close().await.ok();

        self.enable_module(tenant_id, &module, actor).await
    }

    /// Rebuilds a module's read models from the log.
    ///
    /// # When a module's schema changes
    ///
    /// `install.sql` is `CREATE TABLE IF NOT EXISTS` throughout, so re-running
    /// it will not add a column to a table that already exists. That is
    /// deliberate: everything a module projects is *derived*, so the answer to
    /// a changed read model is not a migration but a rebuild — drop the schema,
    /// install it again, and replay the log into it.
    ///
    /// # Why it is one transaction, and why it takes the checkpoint lock first
    ///
    /// `SELECT ... FOR UPDATE` on the checkpoint row is the same lock a
    /// projection run takes, so this waits for a run in flight rather than
    /// dropping the tables out from under it. Resetting the checkpoint in the
    /// same transaction as the drop means there is no moment where the tables
    /// are gone and the checkpoint still claims they are current — which a
    /// worker would read as "nothing to do".
    ///
    /// # Why this is not the deploy path any more
    ///
    /// **The tenant reads empty tables until the worker catches up** — seconds
    /// on a small tenant, minutes on a large one, and every screen in the
    /// product wrong for the whole of it. `just migrate-fleet refresh <module>`
    /// uses `spa_projection::rebuild_swap` instead, which builds the new tables
    /// beside the live ones and exchanges them at the end.
    ///
    /// This stays as the fallback for a caller that has no projections to
    /// replay with — the swap needs them, and only a composition root has both
    /// them and the fleet.
    pub async fn refresh_module(
        &self,
        tenant_id: TenantId,
        setup: ModuleSetup,
    ) -> Result<(), AccessError> {
        let tenant = self
            .tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;

        let options = self
            .tenants
            .cluster_options(&tenant.cluster)?
            .database(&tenant.database_name);

        let conn = Box::pin(PgConnection::connect_with(&options)).await?;
        let conn = rebuild_schema(conn, setup).await?;
        conn.close().await.ok();

        tracing::info!(
            tenant = %tenant.id,
            slug = %tenant.slug,
            "rebuilt a module's read models; the worker will replay them"
        );
        Ok(())
    }

    /// A pool straight at one tenant's database, for a deploy step.
    ///
    /// # Why this exists when `TenantDb` deliberately does not expose one
    ///
    /// `TenantDb` is the request path: it carries lanes, per-operation permits,
    /// and proof that somebody was allowed in. None of that applies here, and
    /// pretending it does would be worse — a rebuild is not a request, it has no
    /// member behind it, and it wants a pool rather than one connection because
    /// it runs several transactions.
    ///
    /// The trust level is exactly
    /// [`enter_for_maintenance`](crate::ControlPlane::enter_for_maintenance)'s:
    /// a caller that already has a tenant id from a fleet walk, running as the
    /// deploy rather than as a person. It is **not** reachable from a handler,
    /// because a handler has no way to get here without one.
    pub async fn maintenance_pool(&self, tenant_id: TenantId) -> Result<PgPool, AccessError> {
        let tenant = self
            .tenant(tenant_id)
            .await?
            .ok_or(AccessError::NoSuchTenant)?;

        let options = self
            .tenants
            .cluster_options(&tenant.cluster)?
            .database(&tenant.database_name);

        Ok(sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?)
    }

    /// Every live tenant with a module enabled.
    ///
    /// Public for operator tools that have to walk the fleet themselves — the
    /// swap rebuild in `bin/migrator` needs the tenant *and* this crate's
    /// projections, and only a composition root has both.
    pub async fn tenants_with_module(
        &self,
        module: &ModuleId,
    ) -> Result<Vec<crate::model::Tenant>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT t.id, t.slug, t.display_name, t.status, t.cluster,
                      t.database_name, t.demo_expires_at, t.created_at
                 FROM tenant t
                 JOIN entitlement e ON e.tenant_id = t.id
                WHERE t.status IN ('active', 'suspended')
                  AND e.module_id = $1
                  AND e.disabled_at IS NULL
                ORDER BY t.created_at"#,
            module.as_str(),
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                crate::tenant_from_row(
                    TenantId::from_uuid(row.id),
                    row.slug,
                    row.display_name,
                    &row.status,
                    row.cluster,
                    row.database_name,
                    row.demo_expires_at,
                    row.created_at,
                )
            })
            .collect()
    }

    /// Drops a half-built tenant's database and row.
    ///
    /// Refuses anything that is not still `provisioning`, so it cannot become a
    /// delete-my-customer button.
    async fn abandon(&self, tenant: Tenant) -> Result<(), AccessError> {
        if !matches!(tenant.status, TenantStatus::Provisioning) {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        self.drop_database(&tenant).await?;

        sqlx::query!(
            "DELETE FROM tenant WHERE id = $1 AND status = 'provisioning'",
            tenant.id.as_uuid(),
        )
        .execute(&self.pool)
        .await?;

        tracing::info!(
            tenant = %tenant.id,
            slug = %tenant.slug,
            "abandoned a half-built tenant; its name is free again"
        );
        Ok(())
    }

    /// Destroys a tenant's database. **No guard of its own** — every caller
    /// checks first, and there are exactly two.
    ///
    /// Private for that reason. The moment this is public it is a
    /// delete-my-customer button with no confirmation on it.
    async fn drop_database(&self, tenant: &Tenant) -> Result<(), AccessError> {
        // Pools first: `DROP DATABASE` fails while anything is connected, and
        // installing modules will have opened one.
        self.tenants.forget(tenant.id).await;

        let options = self
            .tenants
            .cluster_options(&tenant.cluster)?
            .database("postgres");
        let maintenance = PgConnection::connect_with(&options)
            .await
            .map_err(AccessError::Database)?;

        let quoted = quote_ident(&tenant.database_name)?;
        // `WITH (FORCE)` terminates sessions rather than failing (Postgres 13+).
        // Anything still connected to a tenant being destroyed is a leak, not a
        // user.
        let maintenance = run_ddl(
            maintenance,
            format!("DROP DATABASE IF EXISTS {quoted} WITH (FORCE)"),
        )
        .await
        .map_err(AccessError::Database)?;
        maintenance.close().await.ok();

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Demo tenants
    // -----------------------------------------------------------------------

    /// Marks a tenant as a demo that expires after `ttl`.
    ///
    /// The instant is computed by Postgres rather than by this process, for the
    /// same reason event times are: two machines' clocks disagree, and the one
    /// that decides when a database is destroyed should be the one everybody
    /// already agrees with.
    ///
    /// A demo that converts to a real tenant becomes real by clearing this
    /// column. ponytail: no `convert` method until somebody converts one —
    /// `UPDATE tenant SET demo_expires_at = NULL` is the whole of it.
    pub async fn set_demo_expiry(
        &self,
        tenant_id: TenantId,
        ttl: std::time::Duration,
        actor: Actor,
    ) -> Result<(), AccessError> {
        let seconds = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        sqlx::query!(
            "UPDATE tenant
                SET demo_expires_at = now() + ($2::BIGINT * INTERVAL '1 second')
              WHERE id = $1",
            tenant_id.as_uuid(),
            seconds,
        )
        .execute(&self.pool)
        .await?;

        self.record(
            actor,
            "tenant.demo_expiry_set",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "ttl_seconds": seconds }),
        )
        .await
    }

    /// Demo tenants whose time is up.
    pub async fn expired_demos(&self, limit: i64) -> Result<Vec<Tenant>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT id, slug, display_name, status, cluster,
                      database_name, demo_expires_at, created_at
                 FROM tenant
                WHERE demo_expires_at IS NOT NULL
                  AND demo_expires_at <= now()
                ORDER BY demo_expires_at
                LIMIT $1"#,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                crate::tenant_from_row(
                    TenantId::from_uuid(row.id),
                    row.slug,
                    row.display_name,
                    &row.status,
                    row.cluster,
                    row.database_name,
                    row.demo_expires_at,
                    row.created_at,
                )
            })
            .collect()
    }

    /// Destroys one expired demo.
    ///
    /// # Three guards, on purpose
    ///
    /// This is the only code in the system that deletes a live tenant, so
    /// "which tenant" is checked more than once: the argument must carry an
    /// expiry, the row is re-read under the same condition before anything is
    /// dropped, and the final `DELETE` repeats it. A tenant converted to a real
    /// one between the sweep and this call is skipped rather than destroyed.
    ///
    /// Returns whether it actually reaped one.
    pub async fn reap_demo(&self, tenant: &Tenant) -> Result<bool, AccessError> {
        if !tenant.is_demo() {
            return Err(AccessError::Corrupt(format!(
                "{} is not a demo tenant; refusing to destroy it",
                tenant.id
            )));
        }

        // Re-read under the condition rather than trusting the value passed in.
        // The sweep and the reap are separate statements, and the gap between
        // them is exactly where a demo becomes a customer.
        let still_expired = sqlx::query_scalar!(
            "SELECT EXISTS (
                 SELECT 1 FROM tenant
                  WHERE id = $1
                    AND demo_expires_at IS NOT NULL
                    AND demo_expires_at <= now()
             )",
            tenant.id.as_uuid(),
        )
        .fetch_one(&self.pool)
        .await?
        .unwrap_or(false);

        if !still_expired {
            return Ok(false);
        }

        // Database first. The other order leaves a database no row points at,
        // which nothing would ever find; this order leaves a row pointing at
        // nothing, which the next sweep retries and `DROP ... IF EXISTS`
        // absorbs.
        self.drop_database(tenant).await?;

        let deleted = sqlx::query!(
            "DELETE FROM tenant
              WHERE id = $1
                AND demo_expires_at IS NOT NULL
                AND demo_expires_at <= now()",
            tenant.id.as_uuid(),
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        self.record(
            Actor::system(),
            "tenant.demo_reaped",
            "tenant",
            &tenant.id.to_string(),
            serde_json::json!({ "slug": tenant.slug, "database": tenant.database_name }),
        )
        .await?;

        tracing::info!(
            tenant = %tenant.id,
            slug = %tenant.slug,
            "reaped an expired demo tenant"
        );
        Ok(deleted > 0)
    }

    /// Destroys every expired demo, up to `limit`. Returns how many went.
    ///
    /// One failure does not stop the sweep: a cluster that is unreachable
    /// should not keep every other expired demo alive. Each failure is logged
    /// and the next run retries it.
    pub async fn reap_expired_demos(&self, limit: i64) -> Result<usize, AccessError> {
        let expired = self.expired_demos(limit).await?;
        let mut reaped = 0;

        for tenant in &expired {
            match self.reap_demo(tenant).await {
                Ok(true) => reaped += 1,
                Ok(false) => {}
                Err(e) => tracing::error!(
                    tenant = %tenant.id,
                    slug = %tenant.slug,
                    error = %e,
                    "could not reap an expired demo; it will be retried"
                ),
            }
        }

        Ok(reaped)
    }
}

/// Runs the tenant-plane migrations, taking and returning the connection.
///
/// Owned in and owned out, which is the point. `Migrator::run` is generic over
/// `Acquire<'_>`, and a helper that *borrowed* the connection would put that
/// bound into the caller's future — where rustc cannot discharge it, and reports
/// so at whatever HTTP route eventually awaits it. Handing the connection over
/// and getting it back keeps the bound local to this function.
pub(crate) fn migrate(
    mut conn: PgConnection,
) -> BoxFuture<Result<PgConnection, sqlx::migrate::MigrateError>> {
    Box::pin(async move {
        // `run_direct` rather than `run`. sqlx marks it `#[doc(hidden)]` with the
        // comment "getting around the annoying `implementation of Acquire is not
        // general enough` error" — which is exactly the error `run` produces
        // here, because it is generic over `Acquire<'_>` and this future has to
        // be provably `Send` for an axum handler to await it.
        spa_eventlog::MIGRATIONS
            .run_direct(None, &mut conn, false)
            .await?;
        Ok(conn)
    })
}

/// Runs DDL, taking and returning the connection.
///
/// Same reason as [`migrate`]: `raw_sql` is generic over `Acquire<'_>` because
/// it may contain several statements, and a helper that borrowed the connection
/// would put that bound in the caller's future. Handing the connection over and
/// getting it back keeps it local.
///
/// `AssertSqlSafe` is defensible because every caller passes either a module's
/// `&'static str` install script or a name that has been through
/// [`quote_ident`].
/// Creates one module's read models and projection checkpoints.
///
/// Takes and returns the connection **by value** for the same reason every
/// other helper here does: a borrowed `&mut PgConnection` held across an await
/// is one of the four things that costs `sign_up` its `Send`. See the module
/// docs.
///
/// Idempotent — every statement it runs is — so a retry is a retry rather than
/// a second install.
#[expect(
    clippy::needless_pass_by_value,
    reason = "by value on purpose: a borrow would have to be `&'static` to live in a `Send + 'static` future, and that is the constraint this whole file is shaped by"
)]
fn install_schema(
    conn: PgConnection,
    setup: ModuleSetup,
) -> BoxFuture<Result<PgConnection, AccessError>> {
    Box::pin(async move {
        let mut conn = conn;

        // The projection group's schema and its checkpoint row, **before** the
        // install SQL, because the schema is what the SQL lands in.
        //
        // Inlined rather than calling `spa_projection::ensure_group`: the
        // control plane has no business knowing what a projection is, and a
        // cross-crate `async fn` taking `&mut PgConnection` puts an `Acquire`
        // bound in this future that rustc will not discharge. Two statements is
        // cheaper than either problem.
        for index in 0..setup.groups.len() {
            let (name, schema) = setup.groups[index];
            let quoted = quote_ident(schema)?;
            conn = run_ddl(conn, format!("CREATE SCHEMA IF NOT EXISTS {quoted}"))
                .await
                .map_err(AccessError::Database)?;
            conn = run_ddl(
                conn,
                format!(
                    "INSERT INTO projection_checkpoint (group_name) VALUES ('{name}')
                     ON CONFLICT (group_name) DO NOTHING"
                ),
            )
            .await
            .map_err(AccessError::Database)?;
        }

        // **The install SQL is schema-relative, and this is what aims it.**
        //
        // It used to name `proj_sales.invoice` outright, which meant the only
        // schema it could ever build was the live one — and a rebuild that
        // cannot build somewhere else has to drop the live tables first, which
        // is the outage `spa_projection::rebuild_swap` exists to avoid. The
        // projections already wrote unqualified through `search_path`; the DDL
        // does now too, and the two agree.
        //
        // ponytail: aimed at the *first* group, so a module with two would put
        // both groups' tables in one schema. Every module has exactly one and
        // `a_module_has_exactly_one_projection_group` keeps it that way; a
        // second one needs the SQL to move onto the group.
        let aimed = match setup.groups.first() {
            Some((_, schema)) => Some(quote_ident(schema)?),
            None => None,
        };
        if let Some(schema) = aimed {
            conn = run_ddl(conn, format!("SET search_path TO {schema}, public"))
                .await
                .map_err(AccessError::Database)?;
        }

        conn = run_ddl(conn, setup.install_sql.to_owned())
            .await
            .map_err(AccessError::Database)?;

        // **The data the module cannot work without**, after the structure that
        // holds it and under the same `search_path`. Separate from the DDL
        // because they are separate things — see `ModuleSetup::seed_sql`.
        if !setup.seed_sql.is_empty() {
            conn = run_ddl(conn, setup.seed_sql.to_owned())
                .await
                .map_err(AccessError::Database)?;
        }

        // Back, so the connection is handed on the way it was found.
        conn = run_ddl(conn, "SET search_path TO public".to_owned())
            .await
            .map_err(AccessError::Database)?;

        Ok(conn)
    })
}

/// Drops a module's schemas, installs them again, and rewinds its checkpoints.
///
/// All in one transaction, holding the same checkpoint lock a projection run
/// takes. See [`ControlPlane::refresh_module`].
fn rebuild_schema(
    conn: PgConnection,
    setup: ModuleSetup,
) -> BoxFuture<Result<PgConnection, AccessError>> {
    Box::pin(async move {
        let mut conn = run_ddl(conn, "BEGIN".to_owned())
            .await
            .map_err(AccessError::Database)?;

        // The lock first, so a projection run in flight finishes rather than
        // finding its tables gone mid-batch.
        for index in 0..setup.groups.len() {
            let (name, _) = setup.groups[index];
            conn = run_ddl(
                conn,
                format!(
                    "SELECT 1 FROM projection_checkpoint WHERE group_name = '{name}' FOR UPDATE"
                ),
            )
            .await
            .map_err(AccessError::Database)?;
        }

        for index in 0..setup.groups.len() {
            let (name, schema) = setup.groups[index];
            let quoted = quote_ident(schema)?;
            conn = run_ddl(conn, format!("DROP SCHEMA IF EXISTS {quoted} CASCADE"))
                .await
                .map_err(AccessError::Database)?;
            conn = run_ddl(
                conn,
                format!(
                    "UPDATE projection_checkpoint SET position = 0 WHERE group_name = '{name}'"
                ),
            )
            .await
            .map_err(AccessError::Database)?;
        }

        conn = install_schema(conn, setup).await?;

        run_ddl(conn, "COMMIT".to_owned())
            .await
            .map_err(AccessError::Database)
    })
}

fn run_ddl(mut conn: PgConnection, sql: String) -> BoxFuture<Result<PgConnection, sqlx::Error>> {
    Box::pin(async move {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut conn)
            .await?;
        Ok(conn)
    })
}

/// A future with its type erased, and therefore its bounds with it.
///
/// `'static` because every helper below takes owned arguments — which is what
/// makes the erasure possible and what keeps the caller's future provably
/// `Send`.
type BoxFuture<T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Quotes a database name, refusing anything not plainly safe.
fn quote_ident(name: &str) -> Result<String, AccessError> {
    let ok = !name.is_empty()
        && name.len() < 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(format!("\"{name}\""))
    } else {
        Err(AccessError::Corrupt(format!(
            "{name:?} is not a safe database identifier"
        )))
    }
}

/// **The check that keeps the HTTP route buildable.**
///
/// axum requires a handler's future to be `Send`, and reports a failure at the
/// route table with types from files that look unrelated. This fails here
/// instead — at the function whose shape is the cause.
const _: fn() = || {
    fn assert_send<T: Send>(_: T) {}
    fn provision_is_send(control: &ControlPlane, modules: Vec<ModuleSetup>) {
        assert_send(control.create_identity(Actor::system()));
        assert_send(control.register_login(IdentityId::new(), String::new(), String::new()));
        assert_send(control.start_session(IdentityId::new()));
        assert_send(control.sign_up(
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            modules,
        ));
    }
    let _ = provision_is_send;
};
