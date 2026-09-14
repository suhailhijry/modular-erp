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
//! - **A failure compensates.** [`ControlPlane::provision`] drops the database
//!   and the row on its way out, which frees the name — and the person who just
//!   failed to sign up is exactly the person about to try that name again.
//! - **A provisioning that never finished is abandoned later.** A process that
//!   dies mid-build runs no compensation, so the reaper's
//!   [`ControlPlane::reap_stuck_provisioning`] runs it instead, through the
//!   same [`ControlPlane::abandon`]. Nothing resumes a half-built tenant; the
//!   customer asks again. A request that is merely cut off is not this case:
//!   `confirm_signup` builds on a task the request only waits for.
//!
//! The architecture calls for signup as a durable event-sourced workflow. That
//! is the right shape when a step can block for hours — a payment, a DNS record,
//! a human. Every step here is a second of local SQL, and a durable log of five
//! synchronous statements is machinery around a problem the compensation and
//! the sweep already solve. ponytail: revisit when a step goes async.
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

use erp_tenant::ModuleSetup;
use erp_types::{IdentityId, ModuleId, TenantId};
use sqlx::{Connection, PgConnection, PgPool};

use crate::model::{Actor, Scope, Tenant};
use crate::{AccessError, ControlPlane, PlacementPolicy, TenantStatus};

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
            // **Refused before anything is built, not after.** `start_session`
            // below would refuse an enrolled identity anyway — but by then a
            // tenant row, a database and a whole migration chain exist, and
            // the refusal arrives as a `500` through `Corrupt` with an orphan
            // left behind. A password alone must not walk past a second factor
            // *and* it must not cost a database to be told so.
            if self
                .has_second_factor(existing)
                .await
                .map_err(AccessError::Auth)?
            {
                return Err(AccessError::Auth(crate::AuthError::SecondFactorRequired));
            }
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
    ///
    /// The compensation runs only if this future runs to the end. One dropped
    /// part-way leaves the tenant `provisioning` until
    /// [`Self::reap_stuck_provisioning`] abandons it.
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
                let admin = match self.tenants.maintenance_options(&tenant.cluster) {
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
                    // 42P04: already exists. Not expected — the name is new with
                    // its `TenantId` — and not a failure: the migrations below
                    // run against whatever is there.
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
                let tenant_options = match self.tenants.maintenance_options(&tenant.cluster) {
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

                    // Entitlement before schema. Harmless only because nothing
                    // can see this tenant yet; on a live one it is the wrong
                    // way round — see `install_module`.
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
                            "could not abandon a half-built tenant; the reaper's \
                             stuck-provisioning sweep will retry it"
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
    /// During provisioning the tenant is invisible, so the order does not
    /// matter there. Here the tenant is *live*: entitling before
    /// the tables exist opens a window in which the module's routes are found
    /// and every one of them fails on a missing relation. So the schema goes in
    /// first, and the entitlement — the thing that makes it visible — last.
    ///
    /// Idempotent throughout, and it does not check dependencies: what a module
    /// needs underneath it is [`ModuleSetup::requires`], and refusing belongs at
    /// the boundary that can say so in the caller's language.
    ///
    /// **It does not reshape.** A module enabled again over the tables it had
    /// before keeps them, and keeps the read-model version they were built
    /// under; the migrator rebuilds a disabled module's groups with everyone
    /// else's, so those are this build's unless a rebuild failed — and then its
    /// routes answer 503 rather than serve them.
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
            .maintenance_options(&tenant.cluster)?
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
    /// uses `erp_projection::rebuild_swap` instead, which builds the new tables
    /// beside the live ones and exchanges them at the end — and the bare
    /// `migrator` does that on its own for every group whose recorded
    /// read-model version is not this build's.
    ///
    /// The checkpoint's read-model version is set to the one `setup` declares,
    /// in the same transaction: these are that shape's tables now.
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
            .maintenance_options(&tenant.cluster)?
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
            .maintenance_options(&tenant.cluster)?
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
                      t.database_name, t.demo_expires_at,
                      t.requires_second_factor, t.created_at
                 FROM tenant t
                 JOIN entitlement e ON e.tenant_id = t.id
                WHERE t.status IN ('active', 'suspending', 'suspended')
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
                    row.requires_second_factor,
                    row.created_at,
                )
            })
            .collect()
    }

    /// Drops a half-built tenant's database and row. Returns whether it did:
    /// `false` means the tenant is not half-built any more — it activated, or
    /// it is already gone — and nothing was touched.
    ///
    /// # Why it is safe to call with a stale value
    ///
    /// Provisioning's own failure path calls this, and so does the reaper's
    /// [`Self::reap_stuck_provisioning`], which read the tenant a moment ago.
    /// So the value passed in is not trusted:
    ///
    /// - **The row is the lock.** It is re-read `FOR UPDATE` while still
    ///   `provisioning`, and held until the row is deleted. `activate_tenant`'s
    ///   `UPDATE` waits on it, and so do the foreign-key checks behind
    ///   `enable_module` and `grant_membership`, so nothing can make the tenant
    ///   real between this look and the drop. A provisioner that loses the race
    ///   is refused at activation (`moved()` answers `NoSuchTenant`).
    /// - **The database is asked what is in it**, the question
    ///   [`Self::drop_empty_orphans`] asks. Events, or a setting a person chose,
    ///   and this refuses: a provisioning row over a database with data in it is
    ///   a control plane restored to behind its database, not a dead signup.
    ///   A database it cannot look inside is refused too (L6).
    ///
    /// # Errors
    /// [`AccessError::TenantNotActive`] for a value that is not `provisioning`;
    /// [`AccessError::Corrupt`] for an occupied or unreadable database.
    pub async fn abandon(&self, tenant: Tenant) -> Result<bool, AccessError> {
        if !matches!(tenant.status, TenantStatus::Provisioning) {
            return Err(AccessError::TenantNotActive {
                status: tenant.status,
            });
        }

        let mut tx = self.pool.begin().await?;
        let held = sqlx::query_scalar!(
            "SELECT 1 FROM tenant WHERE id = $1 AND status = 'provisioning' FOR UPDATE",
            tenant.id.as_uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        if held.is_none() {
            return Ok(false);
        }

        match self
            .occupancy_of(&tenant.cluster, &tenant.database_name)
            .await
        {
            Ok(None) => {}
            Ok(Some(why)) => {
                return Err(AccessError::Corrupt(format!(
                    "{} is provisioning but {why}; the control plane is behind its database \
                     — refusing to drop it",
                    tenant.id
                )));
            }
            Err(why) => {
                return Err(AccessError::Corrupt(format!(
                    "cannot look inside {}'s database, so it is not dropped: {why}",
                    tenant.id
                )));
            }
        }

        // Database first, for `reap_demo`'s reason: if the commit below fails,
        // the row points at nothing, and the next sweep finds no database and
        // deletes it.
        self.drop_database(&tenant).await?;

        sqlx::query!("DELETE FROM tenant WHERE id = $1", tenant.id.as_uuid())
            .execute(&mut *tx)
            .await?;
        self.record(
            &mut tx,
            Actor::system(),
            Some(tenant.id),
            "tenant.abandoned",
            "tenant",
            &tenant.id.to_string(),
            serde_json::json!({ "slug": tenant.slug, "database": tenant.database_name }),
        )
        .await?;
        tx.commit().await?;

        tracing::info!(
            tenant = %tenant.id,
            slug = %tenant.slug,
            "abandoned a half-built tenant; its name is free again"
        );
        Ok(true)
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
            .maintenance_options(&tenant.cluster)?
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
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE tenant
                SET demo_expires_at = now() + ($2::BIGINT * INTERVAL '1 second')
              WHERE id = $1",
            tenant_id.as_uuid(),
            seconds,
        )
        .execute(&mut *tx)
        .await?;

        self.record(
            &mut tx,
            actor,
            Some(tenant_id),
            "tenant.demo_expiry_set",
            "tenant",
            &tenant_id.to_string(),
            serde_json::json!({ "ttl_seconds": seconds }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Demo tenants whose time is up.
    pub async fn expired_demos(&self, limit: i64) -> Result<Vec<Tenant>, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT id, slug, display_name, status, cluster,
                      database_name, demo_expires_at,
                      requires_second_factor, created_at
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
                    row.requires_second_factor,
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

        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query!(
            "DELETE FROM tenant
              WHERE id = $1
                AND demo_expires_at IS NOT NULL
                AND demo_expires_at <= now()",
            tenant.id.as_uuid(),
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();

        self.record(
            &mut tx,
            Actor::system(),
            Some(tenant.id),
            "tenant.demo_reaped",
            "tenant",
            &tenant.id.to_string(),
            serde_json::json!({ "slug": tenant.slug, "database": tenant.database_name }),
        )
        .await?;
        tx.commit().await?;

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

    /// Abandons every tenant that has sat in `provisioning` for longer than
    /// `grace_seconds`, up to `limit`. Returns how many went.
    ///
    /// The compensation a provisioning that never finished did not get to run:
    /// its process died, or was killed by a deploy, between registering the
    /// row and activating it. Each goes through [`Self::abandon`], which is
    /// what makes a stale list safe, and one failure does not stop the sweep —
    /// it is logged and retried next run, as [`Self::reap_expired_demos`] does.
    pub async fn reap_stuck_provisioning(
        &self,
        grace_seconds: i64,
        limit: i64,
    ) -> Result<usize, AccessError> {
        let rows = sqlx::query!(
            r#"SELECT id, slug, display_name, status, cluster,
                      database_name, demo_expires_at,
                      requires_second_factor, created_at
                 FROM tenant
                WHERE status = 'provisioning'
                  AND created_at <= now() - ($1::BIGINT * INTERVAL '1 second')
                ORDER BY created_at
                LIMIT $2"#,
            grace_seconds,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut abandoned = 0;
        for row in rows {
            let tenant = crate::tenant_from_row(
                TenantId::from_uuid(row.id),
                row.slug,
                row.display_name,
                &row.status,
                row.cluster,
                row.database_name,
                row.demo_expires_at,
                row.requires_second_factor,
                row.created_at,
            )?;
            let (id, slug) = (tenant.id, tenant.slug.clone());
            match self.abandon(tenant).await {
                Ok(true) => abandoned += 1,
                Ok(false) => {}
                Err(e) => tracing::error!(
                    tenant = %id,
                    slug = %slug,
                    error = %e,
                    "a tenant stuck in provisioning was not abandoned; it will be retried"
                ),
            }
        }

        Ok(abandoned)
    }
}

/// How long a tenant may sit in `provisioning` before the reaper abandons it.
///
/// **Not what makes the sweep safe** — [`ControlPlane::abandon`]'s row lock and
/// its look inside the database are. This is what keeps it from failing a
/// signup that is still running: a confirmation builds on a task that outlives
/// its request, and a statement can go on running on the server after the
/// process that sent it died. A real signup builds in seconds, so a quarter of
/// an hour is a wide margin. The name is held for up to this plus the
/// reaper's schedule.
pub const PROVISIONING_GRACE_SECONDS: i64 = 15 * 60;

/// How settled a tenant database must be before its absence from the control
/// plane is worth reporting.
///
/// A day. **Not a safety margin** — what makes
/// [`ControlPlane::drop_empty_orphans`] safe is its look inside, not the age.
/// It is a noise filter: something created minutes ago and not yet visible is
/// a race with whoever is looking, not a finding.
pub const ORPHAN_GRACE_SECONDS: i64 = 24 * 60 * 60;

impl ControlPlane {
    /// **Reports tenant databases that no `tenant` row claims. Deletes
    /// nothing.**
    ///
    /// # Why this reports rather than reaps, which is a reversal
    ///
    /// It was written as a sweep that dropped them, on the reasoning that a run
    /// dying between `CREATE DATABASE` and the row naming it leaves rubbish
    /// nothing else will ever find. **That reasoning was backwards and the
    /// window does not exist**: `provision` writes the row first (`:167`) and
    /// creates the database second (`:195`), so a provisioning that dies leaves
    /// a `provisioning` row, with or without its database — which
    /// [`Self::reap_stuck_provisioning`] abandons — and never a database with
    /// no row.
    ///
    /// So an unclaimed tenant database has essentially one cause, and it is the
    /// opposite of rubbish: **a control plane that has lost rows.** A restore to
    /// a point before a tenant existed, a failover to a stale replica, a
    /// mis-pointed `CONTROL_DATABASE_URL`. This repo already names that state
    /// and calls it dangerous —
    /// `restore.rs::a_tenant_database_without_its_control_row_is_unreachable`
    /// asserts the events are all still there, *"which is what makes this
    /// dangerous"*. Deleting on that signal converts a recoverable incident into
    /// permanent loss of the one thing this system says is irreplaceable, on a
    /// schedule, while an operator is already mid-restore.
    ///
    /// Nothing here can tell those two apart, so it does not guess. It tells
    /// somebody. That is the same answer `charts.rs` gives about editing before
    /// install and the same one L6 gives everywhere else: refuse rather than
    /// degrade.
    ///
    /// **What a caller does with the answer** is drop them by hand after
    /// looking — which is what the 1139 on a development cluster got, and the
    /// looking is the part that mattered.
    ///
    /// # Errors
    /// If the control plane or the cluster is unreachable.
    pub async fn find_orphaned_databases(
        &self,
        cluster: &str,
        grace_seconds: i64,
    ) -> Result<Vec<Unclaimed>, AccessError> {
        let options = self
            .tenants
            .maintenance_options(cluster)?
            .database("postgres");
        let mut maintenance = PgConnection::connect_with(&options)
            .await
            .map_err(AccessError::Database)?;

        let present: Vec<String> = sqlx::query_scalar!(
            r#"SELECT datname as "name!" FROM pg_database
                WHERE datname LIKE 'erp\_tenant\_%'
                ORDER BY datname"#,
        )
        .fetch_all(&mut maintenance)
        .await
        .map_err(AccessError::Database)?;
        maintenance.close().await.ok();

        let mut unclaimed = Vec::new();
        for name in present {
            let Some(age) = orphan_age_seconds(&name) else {
                continue;
            };
            if age < grace_seconds {
                continue;
            }
            if self.database_is_claimed(&name).await? {
                continue;
            }
            unclaimed.push(self.look_inside(cluster, name).await);
        }

        Ok(unclaimed)
    }

    /// **Opens the database and asks what is in it.**
    ///
    /// The one question that separates a provisioning that died from a tenant
    /// whose control-plane row was lost, and it is asked of the database rather
    /// than of the control plane — which is the thing that may be wrong.
    ///
    /// Two tables answer it:
    ///
    /// - **`event`.** `RUNNING.md` calls the log the thing "nothing else can
    ///   reconstruct". One row in it and this is somebody's business.
    /// - **`configuration`, excluding what a module seeded.** An install writes
    ///   `set_by = 'module:tax_sa'` and `refresh_module` writes it again, so
    ///   those come back by themselves. A row set by anybody else is a decision
    ///   a person made, and nothing recreates that.
    ///
    /// Anything that cannot be opened or read is [`Unclaimed::Unreadable`] and
    /// is never dropped — except one that no longer exists, which is empty.
    /// **Not knowing is not the same as knowing it is empty**, and this is the
    /// one place that distinction is worth a whole variant.
    async fn look_inside(&self, cluster: &str, database: String) -> Unclaimed {
        match self.occupancy_of(cluster, &database).await {
            Ok(None) => Unclaimed::Empty(database),
            Ok(Some(why)) => Unclaimed::Occupied { database, why },
            Err(why) => Unclaimed::Unreadable { database, why },
        }
    }

    /// **Every way looking can fail is one error**, so there is one place that
    /// decides what not-knowing means and it cannot be got right for the
    /// connection and wrong for the query.
    async fn occupancy_of(
        &self,
        cluster: &str,
        database: &str,
    ) -> Result<Option<&'static str>, String> {
        let options = self
            .tenants
            .maintenance_options(cluster)
            .map_err(|e| e.to_string())?
            .database(database);
        let mut conn = match PgConnection::connect_with(&options).await {
            Ok(conn) => conn,
            // 3D000: it does not exist, so there is nothing in it — and `DROP …
            // IF EXISTS`, which both callers do next, agrees. A provisioning
            // that died before `CREATE DATABASE` is exactly this.
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("3D000") => {
                return Ok(None);
            }
            Err(e) => return Err(e.to_string()),
        };
        let verdict = occupancy(&mut conn).await.map_err(|e| e.to_string());
        conn.close().await.ok();
        verdict
    }

    /// **Drops the unclaimed databases that hold nothing a person made.**
    ///
    /// # The refusal that matters
    ///
    /// If *any* unclaimed database on this cluster turns out to be occupied, or
    /// cannot be looked inside, **nothing is dropped at all**. One database full
    /// of events that no row claims does not mean one row was lost; it means the
    /// control plane is not a description of this cluster, and the next database
    /// in the list is not evidence of anything either. The answer to that is a
    /// person, not a batch.
    ///
    /// `limit` is a batch size for the ordinary case, so a first run on a
    /// cluster with a thousand leftovers takes several passes rather than
    /// holding a connection open through all of them.
    ///
    /// # Errors
    /// [`AccessError::Corrupt`] when the cluster holds an unclaimed database
    /// that is occupied or unreadable — which is a refusal, not a fault.
    pub async fn drop_empty_orphans(
        &self,
        cluster: &str,
        grace_seconds: i64,
        limit: usize,
    ) -> Result<Vec<String>, AccessError> {
        let unclaimed = self.find_orphaned_databases(cluster, grace_seconds).await?;

        let doubtful: Vec<&Unclaimed> = unclaimed
            .iter()
            .filter(|u| !matches!(u, Unclaimed::Empty(_)))
            .collect();
        if !doubtful.is_empty() {
            return Err(AccessError::Corrupt(format!(
                "{cluster}: {} unclaimed tenant database(s) that this cannot call rubbish \
                 ({doubtful:?}); dropping nothing. A tenant database with data in it and no \
                 control-plane row means the control plane is behind reality — check that \
                 before restoring anything else",
                doubtful.len(),
            )));
        }

        let options = self
            .tenants
            .maintenance_options(cluster)?
            .database("postgres");
        let mut maintenance = PgConnection::connect_with(&options)
            .await
            .map_err(AccessError::Database)?;

        let mut dropped = Vec::new();
        for name in unclaimed
            .into_iter()
            .filter_map(Unclaimed::empty)
            .take(limit)
        {
            let quoted = quote_ident(&name)?;
            let sql = format!("DROP DATABASE IF EXISTS {quoted}");
            match sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&mut maintenance)
                .await
            {
                Ok(_) => dropped.push(name),
                // Without `WITH (FORCE)`, unlike `drop_database`: there a tenant
                // is being deliberately destroyed and a session is a leak; here
                // the database is only believed to be rubbish, and a connection
                // is evidence the belief is wrong.
                Err(e) => tracing::warn!(
                    database = %name,
                    error = %e,
                    "an empty unclaimed database would not drop; leaving it"
                ),
            }
        }

        maintenance.close().await.ok();
        Ok(dropped)
    }

    /// Whether any tenant row names this database, **whatever state it is in**.
    ///
    /// Not filtered by cluster, though `UNIQUE (cluster, database_name)` would
    /// allow one name on two of them. Names come from `TenantId`, so a
    /// collision needs somebody to have built one by hand — and answering
    /// "claimed" for a name claimed anywhere is the direction that reports
    /// less, which is the right way to be wrong.
    async fn database_is_claimed(&self, name: &str) -> Result<bool, AccessError> {
        Ok(sqlx::query_scalar!(
            "SELECT EXISTS (SELECT 1 FROM tenant WHERE database_name = $1)",
            name,
        )
        .fetch_one(&self.pool)
        .await?
        .unwrap_or(true))
    }

    /// Every cluster tenants may live on, so a check can visit each.
    ///
    /// # Errors
    /// If the control plane is unreachable.
    pub async fn cluster_names(&self) -> Result<Vec<String>, AccessError> {
        Ok(
            sqlx::query_scalar!(r#"SELECT name as "name!" FROM cluster ORDER BY name"#)
                .fetch_all(&self.pool)
                .await?,
        )
    }
}

/// A tenant database no `tenant` row claims, and what is inside it.
///
/// The distinction the whole feature turns on: an unclaimed database is either
/// a provisioning that died before it wrote anything, or a tenant whose
/// control-plane row was lost. They look identical from the control plane, and
/// completely different from inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unclaimed {
    /// No events and no setting anybody chose. Nothing here cannot be made
    /// again.
    Empty(String),
    /// **Somebody's business.** A lost control-plane row, and the database is
    /// the only place that tenant still exists.
    Occupied { database: String, why: &'static str },
    /// Could not be opened or read, so nothing is known about it — which is not
    /// the same as knowing it is empty.
    Unreadable { database: String, why: String },
}

impl Unclaimed {
    /// The name, if this one holds nothing.
    #[must_use]
    pub fn empty(self) -> Option<String> {
        match self {
            Self::Empty(database) => Some(database),
            Self::Occupied { .. } | Self::Unreadable { .. } => None,
        }
    }

    /// The database this is about, whatever it turned out to be.
    #[must_use]
    pub fn database(&self) -> &str {
        match self {
            Self::Empty(database)
            | Self::Occupied { database, .. }
            | Self::Unreadable { database, .. } => database,
        }
    }
}

/// Why a database is somebody's, or `None` if it is nobody's.
///
/// `to_regclass` first, because a database created and never migrated has
/// neither table and asking directly would be a parse error rather than an
/// answer.
async fn occupancy(conn: &mut PgConnection) -> Result<Option<&'static str>, sqlx::Error> {
    let has_log: Option<String> = sqlx::query_scalar("SELECT to_regclass('public.event')::text")
        .fetch_one(&mut *conn)
        .await?;
    if has_log.is_some() {
        let events: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM public.event)")
            .fetch_one(&mut *conn)
            .await?;
        if events {
            return Ok(Some("it has events in it"));
        }
    }

    let has_config: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.configuration')::text")
            .fetch_one(&mut *conn)
            .await?;
    if has_config.is_some() {
        // `module:` is what an install and a refresh write, and both write it
        // again by themselves. Anything else is a person.
        let chosen: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM public.configuration
                  WHERE set_by IS NULL OR set_by NOT LIKE 'module:%'
             )",
        )
        .fetch_one(&mut *conn)
        .await?;
        if chosen {
            return Ok(Some("it has a setting somebody chose"));
        }
    }

    Ok(None)
}

/// [`orphan_age_seconds`], for the test that proves what it refuses to age.
#[must_use]
pub fn orphan_age_seconds_for_tests(datname: &str) -> Option<i64> {
    orphan_age_seconds(datname)
}

/// How long ago a tenant database was named, from its own name.
///
/// **The rule itself lives with the id**, in `TenantId::named_in_database`: the
/// name is derived from the id, so what a name means is the id's business. It
/// was here, and `erp-testkit` needed the same answer for a different reason —
/// two copies of "is this one of ours and how old is it" is exactly the second
/// declaration that eventually disagrees with the first.
fn orphan_age_seconds(datname: &str) -> Option<i64> {
    let named = erp_types::TenantId::named_in_database(datname)?;
    Some(chrono::Utc::now().timestamp() - named.timestamp())
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
        erp_eventlog::MIGRATIONS
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
        // Inlined rather than calling `erp_projection::ensure_group`: the
        // control plane has no business knowing what a projection is, and a
        // cross-crate `async fn` taking `&mut PgConnection` puts an `Acquire`
        // bound in this future that rustc will not discharge. Two statements is
        // cheaper than either problem.
        //
        // **Stamped with the version it is about to build, and only if the row
        // is new.** An existing row means existing tables, which the
        // `IF NOT EXISTS` DDL below will not reshape — a module disabled and
        // enabled again keeps the stamp its tables were built under, so a
        // shape older than this build is still seen as one.
        for index in 0..setup.groups.len() {
            let (name, schema, version) = setup.groups[index];
            let quoted = quote_ident(schema)?;
            conn = run_ddl(conn, format!("CREATE SCHEMA IF NOT EXISTS {quoted}"))
                .await
                .map_err(AccessError::Database)?;
            conn = run_ddl(
                conn,
                format!(
                    "INSERT INTO projection_checkpoint (group_name, read_model_version)
                     VALUES ('{name}', {version})
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
        // is the outage `erp_projection::rebuild_swap` exists to avoid. The
        // projections already wrote unqualified through `search_path`; the DDL
        // does now too, and the two agree.
        //
        // ponytail: aimed at the *first* group, so a module with two would put
        // both groups' tables in one schema. No module has more than one, and
        // `a_module_has_at_most_one_projection_group` in `erp-api` keeps it that
        // way; a second one needs the SQL to move onto the group.
        let aimed = match setup.groups.first() {
            Some((_, schema, _)) => Some(quote_ident(schema)?),
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
            let (name, _, _) = setup.groups[index];
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
            let (name, schema, version) = setup.groups[index];
            let quoted = quote_ident(schema)?;
            conn = run_ddl(conn, format!("DROP SCHEMA IF EXISTS {quoted} CASCADE"))
                .await
                .map_err(AccessError::Database)?;
            // The version too: this really does rebuild, so the tables below
            // are the shape `setup` declares, whatever the row said before.
            conn = run_ddl(
                conn,
                format!(
                    "UPDATE projection_checkpoint SET position = 0, read_model_version = {version}
                      WHERE group_name = '{name}'"
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
