//! Grants and revokes platform staff from a shell.
//!
//! ```text
//! CONTROL_DATABASE_URL=… cargo run --bin operator -- grant-staff noura@erp.example superadmin
//! CONTROL_DATABASE_URL=… cargo run --bin operator -- revoke-staff noura@erp.example
//! ```
//!
//! # Why a CLI when there are staff routes
//!
//! Every staff route needs a superadmin, so HTTP cannot make the first one.
//! This is how the first is made — and how the last one is taken away when
//! that account is the problem, which `DELETE /v1/platform/staff/{identity}`
//! refuses so that nobody locks the platform out by accident. Holding the
//! control database's credentials is the authority here; after the first
//! superadmin, staff are managed over HTTP, where the audit trail names who.
//!
//! Both are recorded in the audit trail as the system. `grant-staff` goes
//! through `ControlPlane::grant_staff`, the same checks the route makes: the
//! account must exist, have a second factor, and not already be staff — to
//! change somebody's role, revoke and grant again. `revoke-staff` is the one
//! thing HTTP cannot do: it has no last-superadmin guard. **And it ends every
//! session the account holds**, which the route does not: this is the path
//! for an account that is the problem, and its sessions are part of it.
//!
//! **It shares invalidations when the deployment has `REDIS_URL`**, as the API
//! does. A revocation forgets the platform cache of the process that made it,
//! and this process is not the one serving requests; without the broadcast
//! every API node kept the revoked role for up to the cache's five seconds.
//! Without Redis that is still the answer — see `erp_control::shared`.
//!
//! **Anything else is refused with the usage and exit 2**, and nothing runs
//! first. A tool whose unrecognised arguments fall through to a default action
//! is a tool where a typo on an old image does something.

use erp_control::shared::Shared;
use erp_control::{
    Actor, ClusterRegistry, ControlPlane, PlatformRole, PoolConfig, Scope, TenantPools,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "usage:
  operator grant-staff <email> <support|billing|superadmin>
  operator revoke-staff <email>";

/// What was asked for, once the arguments have been read.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Grant { email: String, role: PlatformRole },
    Revoke { email: String },
}

/// The arguments after the binary's name, or `None` for anything that is not
/// exactly one of the two commands.
fn parse(args: &[String]) -> Option<Command> {
    match args {
        [command, email, role] if command == "grant-staff" => Some(Command::Grant {
            email: email.clone(),
            role: role.parse().ok()?,
        }),
        [command, email] if command == "revoke-staff" => Some(Command::Revoke {
            email: email.clone(),
        }),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = parse(&args) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

    let control_url =
        std::env::var("CONTROL_DATABASE_URL").map_err(|_| "CONTROL_DATABASE_URL is not set")?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&control_url)
        .await?;
    let control = control_plane(pool, Shared::from_env().await?);

    println!("{}", run(&control, command).await?);
    Ok(())
}

/// The control plane the commands act through.
///
/// No cluster: staff are control-plane rows, and nothing here opens a tenant.
fn control_plane(pool: sqlx::PgPool, shared: Option<Shared>) -> ControlPlane {
    let control = ControlPlane::new(
        pool,
        TenantPools::new(ClusterRegistry::new(), PoolConfig::default()),
    );
    match shared {
        Some(shared) => control.sharing(shared),
        None => control,
    }
}

/// Does what was asked, and says what it did.
async fn run(control: &ControlPlane, command: Command) -> Result<String, Error> {
    match command {
        Command::Grant { email, role } => {
            let identity = control.grant_staff(&email, role, Actor::system()).await?;
            Ok(format!("{email} ({identity}) is now {role}"))
        }
        Command::Revoke { email } => {
            let identity = control
                .identity_by_login(&email)
                .await?
                .ok_or_else(|| format!("no account signs in as {email}"))?;
            if !control
                .revoke_membership(identity, Scope::Platform, Actor::system())
                .await?
            {
                return Err(format!("{email} is not platform staff").into());
            }
            let ended = control.log_out_everywhere(identity).await?;
            Ok(format!(
                "{email} ({identity}) is no longer staff; {ended} session(s) ended"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn only_the_two_commands_are_read_and_nothing_else_is_guessed() {
        assert_eq!(
            parse(&args("grant-staff noura@erp.example superadmin")),
            Some(Command::Grant {
                email: "noura@erp.example".to_owned(),
                role: PlatformRole::Superadmin,
            })
        );
        assert_eq!(
            parse(&args("revoke-staff noura@erp.example")),
            Some(Command::Revoke {
                email: "noura@erp.example".to_owned(),
            })
        );

        for refused in [
            "",
            "suspend acme",
            "grant-staff noura@erp.example",
            "grant-staff noura@erp.example owner",
            "grant-staff noura@erp.example superadmin extra",
            "revoke-staff",
            "revoke-staff a@b c@d",
            "Grant-Staff noura@erp.example support",
        ] {
            assert_eq!(parse(&args(refused)), None, "{refused:?} was read");
        }
    }

    /// **Break glass reaches the API nodes at once**: the revoked superadmin is
    /// refused on a node that had their role cached, and their session is
    /// gone there too.
    ///
    /// An API node and this CLI over one control database and one Redis, as in
    /// the compose stack. Needs a Redis at `REDIS_URL` (default
    /// `redis://127.0.0.1/`) and fails without one, as `erp-control`'s
    /// `tests/shared.rs` does.
    #[tokio::test]
    async fn revoke_staff_closes_the_door_on_every_node_and_ends_the_sessions() {
        use erp_control::{AccessError, AuthError, PlatformPower, totp};
        use std::time::Duration;

        static CONTROL: erp_testkit::Schema =
            erp_testkit::Schema::migrations("control", &erp_control::MIGRATIONS);
        let db = erp_testkit::Template::get(&CONTROL)
            .await
            .expect("template builds")
            .fresh()
            .await
            .expect("clones");
        let redis = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1/".into());
        let connect = || async {
            Some(
                Shared::connect(&redis)
                    .await
                    .unwrap_or_else(|e| panic!("this test needs a Redis at {redis}: {e}")),
            )
        };

        let api = std::sync::Arc::new(control_plane(db.pool().clone(), connect().await));
        erp_control::shared::apply_invalidations_in_background(&api);
        // A publish before the subscriber listens is received by nobody.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Mallory: a superadmin with a factor and a session, as the API makes one.
        let email = "mallory@erp.test";
        let mallory = api
            .create_identity(Actor::system())
            .await
            .expect("creates")
            .id;
        api.register_login(mallory, email.into(), "hunter2hunter2".into())
            .await
            .expect("registers");
        let (token, _) = api.log_in(email, "hunter2hunter2").await.expect("logs in");
        let sealing = erp_eventlog::SealingKey::new("test", &[5u8; 32]).expect("32 bytes");
        let enrolment = api
            .begin_second_factor(mallory, "ERP", email, &sealing, None)
            .await
            .expect("begins");
        let secret = totp::unbase32(&enrolment.secret).expect("base32");
        let now = chrono::Utc::now();
        let seconds = u64::try_from(now.timestamp()).expect("after 1970");
        let code = totp::code_at(&secret, seconds, totp::DIGITS).expect("a code");
        api.confirm_second_factor(
            mallory,
            &code,
            None,
            now,
            &sealing,
            Some(token.expose()),
            None,
        )
        .await
        .expect("confirms");
        api.grant_staff(email, PlatformRole::Superadmin, Actor::system())
            .await
            .expect("grants");

        // **The API node caches both.** An empty cache would find the change in
        // the database whatever the operator did.
        api.staff_may(mallory, PlatformPower::ManageStaff)
            .await
            .expect("a superadmin may");
        api.session(token.expose()).await.expect("a live session");

        let operator = control_plane(db.pool().clone(), connect().await);
        run(
            &operator,
            Command::Revoke {
                email: email.into(),
            },
        )
        .await
        .expect("revokes");

        // Well inside the five-second cache this is beating.
        let mut refused = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if let Err(e) = api.staff_may(mallory, PlatformPower::ManageStaff).await {
                refused = Some(e);
                break;
            }
        }
        assert!(
            matches!(refused, Some(AccessError::StaffOnly(_))),
            "the API node still let a revoked superadmin manage staff: {refused:?}"
        );
        assert!(
            matches!(api.session(token.expose()).await, Err(AuthError::NoSession)),
            "the revoked superadmin's session still works on the API node"
        );
    }
}
