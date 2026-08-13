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

use spa_types::{IdentityId, ModuleId};
use sqlx::{Connection, PgConnection};

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
    pub install_sql: &'static str,
    /// The projection groups this module owns, as `(name, schema)`.
    pub groups: &'static [(&'static str, &'static str)],
}

impl ModuleSetup {
    #[must_use]
    pub const fn new(
        module: ModuleId,
        install_sql: &'static str,
        groups: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self {
            module,
            install_sql,
            groups,
        }
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
        // owner. The same person can sign up again with the same email.
        let identity = self.create_identity(Actor::system()).await?;
        self.set_password(identity.id, email, password)
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;

        let tenant = self.provision(slug, company, identity.id, modules).await?;

        let (token, session) = self
            .start_session(identity.id)
            .await
            .map_err(|e| AccessError::Corrupt(e.to_string()))?;

        Ok(SignedUp {
            tenant,
            identity: identity.id,
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
        clippy::too_many_lines,
        clippy::needless_range_loop,
        reason = "flat and indexed on purpose; a borrowed iterator held across \
                  an await costs this function its `Send` proof, and clippy \
                  cannot see that. See the module docs."
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
                    let module = modules[index].module.clone();
                    let install_sql: &'static str = modules[index].install_sql;
                    let groups: &'static [(&'static str, &'static str)] = modules[index].groups;

                    // Entitlement before schema, so a retry can read back what was
                    // wanted even if it died part-way through installing.
                    if let Err(e) =
                        Box::pin(self.enable_module(tenant.id, &module, Actor::system())).await
                    {
                        break 'build Err(e);
                    }

                    conn = match run_ddl(conn, install_sql.to_owned()).await {
                        Ok(conn) => conn,
                        Err(e) => break 'build Err(AccessError::Database(e)),
                    };

                    // The projection group's schema and its checkpoint row.
                    //
                    // Inlined rather than calling `spa_projection::ensure_group`:
                    // the control plane has no business knowing what a projection
                    // is, and a cross-crate `async fn` taking `&mut PgConnection`
                    // puts an `Acquire` bound in this future that rustc will not
                    // discharge. Two statements is cheaper than either problem.
                    for group in 0..groups.len() {
                        let (name, schema) = groups[group];
                        let quoted_schema = match quote_ident(schema) {
                            Ok(quoted) => quoted,
                            Err(e) => break 'build Err(e),
                        };
                        conn = match run_ddl(
                            conn,
                            format!("CREATE SCHEMA IF NOT EXISTS {quoted_schema}"),
                        )
                        .await
                        {
                            Ok(conn) => conn,
                            Err(e) => break 'build Err(AccessError::Database(e)),
                        };
                        conn = match run_ddl(
                            conn,
                            format!(
                                "INSERT INTO projection_checkpoint (group_name) VALUES ('{name}')
                             ON CONFLICT (group_name) DO NOTHING"
                            ),
                        )
                        .await
                        {
                            Ok(conn) => conn,
                            Err(e) => break 'build Err(AccessError::Database(e)),
                        };
                    }
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
        // Anything still connected to a tenant being abandoned is a leak, not a
        // user.
        let maintenance = run_ddl(
            maintenance,
            format!("DROP DATABASE IF EXISTS {quoted} WITH (FORCE)"),
        )
        .await
        .map_err(AccessError::Database)?;
        maintenance.close().await.ok();

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
}

/// Runs the tenant-plane migrations, taking and returning the connection.
///
/// Owned in and owned out, which is the point. `Migrator::run` is generic over
/// `Acquire<'_>`, and a helper that *borrowed* the connection would put that
/// bound into the caller's future — where rustc cannot discharge it, and reports
/// so at whatever HTTP route eventually awaits it. Handing the connection over
/// and getting it back keeps the bound local to this function.
fn migrate(mut conn: PgConnection) -> BoxFuture<Result<PgConnection, sqlx::migrate::MigrateError>> {
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
        assert_send(control.set_password(IdentityId::new(), String::new(), String::new()));
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
