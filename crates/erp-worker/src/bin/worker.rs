//! The worker process.
//!
//! # The composition root
//!
//! The only file that knows both the kernel and the modules. `erp-worker`
//! depends on no module and `modules/ledger` depends on no worker; they meet
//! here, which is what keeps the dependency arrow pointing one way and lets a
//! module be dropped from a build by deleting three lines.

use std::sync::Arc;
use std::time::Duration;

use erp_control::{ClusterRegistry, ControlPlane, PoolConfig, TenantPools};
use erp_eventlog::{Dispatcher, RetryPolicy};
use erp_types::ModuleId;
use erp_worker::{
    Activity, Finding, HealthJob, Invariant, OutboxJob, PlatformOutboxJob, ProjectionJob, Worker,
    WorkerConfig, shutdown_signal,
};

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "the composition root: every job this deployment runs is named here \
              once, and splitting the list would hide what a worker does"
)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let control_url = std::env::var("CONTROL_DATABASE_URL")
        .map_err(|_| "CONTROL_DATABASE_URL is not set; the worker has nothing to connect to")?;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&control_url)
        .await?;

    // Primary and, if this deployment has one, its read replica.
    let clusters = ClusterRegistry::from_env()?;
    // **Shared state, when this deployment runs more than one of these.**
    //
    // Sessions read from Redis instead of the control database on every
    // request, and a cache invalidation reaches every node rather than only
    // this one. Without `REDIS_URL` both fall back to what this system did
    // before — see `erp_control::shared`.
    let mut control = ControlPlane::new(pool, TenantPools::new(clusters, PoolConfig::from_env()));
    if let Some(shared) = erp_control::shared::Shared::from_env().await? {
        control = control.sharing(shared);
    }
    let control = Arc::new(control);
    let _invalidations = erp_control::shared::apply_invalidations_in_background(&control);

    // **Every projection advance is announced**, so a screen learns of it
    // without polling. Without Redis there is nobody to tell.
    let signals: Option<Arc<dyn erp_worker::Signals>> = control
        .shared()
        .map(|shared| Arc::new(shared.clone()) as Arc<dyn erp_worker::Signals>);

    // States what this process could demand against what the server allows.
    // Nothing wrote either number down before, which is how four processes each
    // holding a 400-permit budget against a 200-connection server went unnoticed.
    control.pools().report_budget("primary").await;

    // Effect handlers come from modules. An empty dispatcher claims nothing —
    // the same behaviour as a worker rolled out before a module's handler
    // exists, and deliberately not an error.
    //
    // **`messaging` is the first module to register any.** Until Phase 11 the
    // tenant dispatcher had none at all, which is why `hr`'s expiring-document
    // reminder had to be a health finding: an effect enqueued from a module
    // would have sat in the outbox for ever.
    //
    // A longer lease than the default, for the reason the platform's is longer:
    // a slow gateway is the normal failure, and a lease that lapses while a
    // message is still in flight sends it twice.
    let mut dispatcher = Dispatcher::new(RetryPolicy {
        lease: Duration::from_mins(2),
        ..RetryPolicy::default()
    });
    for handler in messaging::handlers(message_transports()) {
        dispatcher = dispatcher.register(handler);
    }
    // **The tenant plane only.** A payment callback arrives on a tenant's
    // subdomain and is recorded in that tenant's log; there is no control-plane
    // equivalent. See `payments::Doorbell` for why acknowledging is all there
    // is to do here — the settling is `payments.settle`'s, because it needs a
    // database and a handler is given none.
    for handler in payments::doorbells() {
        dispatcher = dispatcher.register(handler);
    }
    let dispatcher = Arc::new(dispatcher);

    // **The control plane's dispatcher**, which is a different queue in a
    // different database. Email lives here because the things that send it —
    // invitations today, password resets next — are control-plane rows.
    //
    // A longer lease than the default: a slow relay is the normal failure, and
    // a lease that lapses while a message is still in flight sends it twice.
    let mut platform = Dispatcher::new(RetryPolicy {
        lease: Duration::from_mins(2),
        ..RetryPolicy::default()
    });
    for handler in mailer()? {
        platform = platform.register(handler);
    }
    // **The same handlers the tenant plane gets.** A one-time code is
    // control-plane — identities are — and a booking reminder is a tenant's,
    // but they are the same act: `erp_control::mail::Text` is
    // `messaging::Outbound`'s shape on the wire, so one handler answers for
    // both rather than two that could drift.
    for handler in messaging::handlers(message_transports()) {
        platform = platform.register(handler);
    }
    let platform = Arc::new(platform);

    let config = WorkerConfig {
        name: std::env::var("WORKER_NAME")
            .unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_else(|_| "worker".to_owned())),
        drain_timeout: Duration::from_secs(20),
        ..WorkerConfig::default()
    };

    // The composition root, and the only place that knows both the kernel and
    // the modules. `erp-worker` itself depends on no module, which is what keeps
    // the dependency arrow pointing one way.
    //
    // One health job for both planes: every tenant's invariants, and the
    // control plane's outbox on its own turn of the same interval.
    let health = Arc::new(
        HealthJob::every(HEALTH_INTERVAL)
            .with(Arc::new(TrialBalance))
            .with(Arc::new(ReportsReconcile))
            .with(Arc::new(StockValueAgrees))
            .with(Arc::new(StockBellRings {
                grace: bell_grace(&config.schedule),
            }))
            .with(Arc::new(NoOverpaidInvoice))
            .with(Arc::new(NoOverpaidBill))
            .with(Arc::new(CertificateExpiry))
            .with(Arc::new(WorkDocumentExpiry)),
    );
    let stock_bell = Arc::clone(&control);
    let mut worker = Worker::new(control, config)
        .with_platform_job(Arc::new(PlatformOutboxJob::new(platform, EMAIL_BATCH)))
        .with_platform_job(health.clone())
        .with_job(Arc::new(OutboxJob::new(dispatcher, 64)))
        .with_job(health);
    for job in module_jobs(signals.as_ref()) {
        worker = worker.with_job(job);
    }
    worker = worker
        .with_job(Arc::new(LandInboundMessages))
        .with_job(Arc::new(AnnounceNewBookings))
        .with_job(Arc::new(AnnounceExpiringDocuments))
        .with_job(Arc::new(AnnounceExpiringStock {
            control: stock_bell,
        }))
        .with_job(Arc::new(BookingReminders))
        .with_job(Arc::new(ExpireUnpaidHolds))
        .with_job(Arc::new(BillCompletedBookings))
        .with_job(Arc::new(RetirePushTokens))
        .with_platform_job(Arc::new(SweepOneTimeCodes))
        // **What this system forgets.** See `erp_worker::Retention`.
        .with_job(Arc::new(erp_worker::Retention))
        .with_platform_job(Arc::new(erp_worker::SweepSessions));

    // **The ZATCA sweeps, and only with a sealing key.** They read a tenant's
    // private key to sign with, so without one there is nothing to read and the
    // jobs are not registered at all — which is louder than a job that runs and
    // finds it can do nothing.
    if let Ok(configured) = std::env::var("SEALING_KEY") {
        let sealing = erp_eventlog::SealingKey::parse(&configured)?;
        tracing::info!(
            key = ?sealing,
            "sealing key loaded; ZATCA sweeps enabled"
        );
        for job in zatca_jobs(&sealing) {
            worker = worker.with_job(job);
        }
        // **Same call, for the same reason.** A gateway's API key is sealed, so
        // without a sealing key there is nothing to unseal and the sweep is not
        // registered at all — which is louder than a job that runs every tick
        // and finds it can do nothing.
        worker = worker.with_job(Arc::new(SettleGatewayPayments {
            sealing: sealing.clone(),
        }));
    } else {
        tracing::warn!(
            "SEALING_KEY is not set; invoices will be built and chained but never \
             signed or sent to ZATCA, and gateway payments will never be settled"
        );
    }

    let shutdown = worker.run(shutdown_signal()).await;

    if !shutdown.drained {
        // Non-zero, because an orchestrator watching exit codes should be able
        // to tell a clean stop from one that ran out of time.
        tracing::error!("shut down without completing the drain");
        std::process::exit(1);
    }

    Ok(())
}

/// The ledger's trial balance, as an invariant the platform checks.
///
/// It lives here rather than in `erp-worker` because the *kernel* must not know
/// what a trial balance is (D11), and rather than in `modules/ledger` because a
/// module must not depend on the worker. The composition root is where the two
/// meet, and it is three lines.
struct TrialBalance;

#[async_trait::async_trait]
impl Invariant for TrialBalance {
    fn name(&self) -> &'static str {
        "trial_balance"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(ledger::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        let mut conn = db.acquire().await?;
        Ok(ledger::imbalances(&mut conn)
            .await?
            .into_iter()
            .map(|t| {
                Finding::new(
                    "trial_balance",
                    format!(
                        "{} is out by {} ({} debits against {} credits)",
                        t.currency, t.difference, t.debits, t.credits
                    ),
                )
            })
            .collect())
    }
}

/// **What the shelves are worth, against the account that says so.**
///
/// Two readings of one log: what `proj_inventory` carries every lot at, and
/// what `proj_ledger` says `1300` holds. `inventory` writes the account at
/// both ends now — a receipt debits it, and every write-off, count variance and
/// consumption credits it, each in the transaction that writes the movement —
/// so a difference is not paperwork in flight. It is a posting that did not
/// happen, an entry somebody made against the stock account by hand, or a
/// projection that has not caught up.
///
/// It lives here for the same reason [`TrialBalance`] and [`ReportsReconcile`]
/// do, and here specifically rather than in `inventory`: the comparison needs
/// `proj_ledger` beside `proj_inventory`, and L3 forbids a module from reading
/// across projection groups. `inventory::value_on_hand` is the half that
/// belongs to the module; this is the other.
///
/// # It used to fire for every delivery, and that was the alarm nobody could act on
///
/// Until 2026-09-13 receiving posted nothing and the *supplier's bill* debited
/// the asset, so the days between goods arriving and their invoice being typed
/// in were days this check called a violation — the ordinary state of a
/// business, logged at error level every five minutes. The receipt posts now,
/// against `2010 Goods received, not invoiced`, and the bill line that names
/// the product relieves that account instead of `1300`. The window this
/// complained about is a balance on `2010`, where it belongs, and a finding
/// here means something is actually wrong.
///
/// # Both sides, including the one with no rows
///
/// The comparison is driven from [`stock_disagreements`], which pairs each
/// currency the shelves hold with the account's balance **and** adds the
/// account's own currency when no shelf holds it. `value_on_hand` groups
/// `stock_item` rows, so a tenant whose stock account holds money and whose
/// shelves are empty produces no row at all — and that is precisely the case
/// this check's own wording names, a debit that never reached a shelf. A
/// currency the account cannot be compared in is **reported rather than
/// dropped** for the same reason: stock in a second currency is stock somebody
/// has to be told about.
struct StockValueAgrees;

/// **What the two sides disagree by**, as `(held, booked)` pairs, one per
/// currency that either side knows about.
///
/// Pure, and separately tested, because the interesting case has no row on one
/// side and a comparison written as a filter over one of the two lists silently
/// skips it. `booked` is the account's balance, which exists in exactly one
/// currency — the one the account was opened in.
fn stock_disagreements(
    held: &[erp_types::Money],
    booked: Option<erp_types::Money>,
) -> Vec<(erp_types::Money, Option<erp_types::Money>)> {
    let mut sides: Vec<(erp_types::Money, Option<erp_types::Money>)> = held
        .iter()
        .map(|held| {
            (
                *held,
                booked.filter(|booked| booked.currency() == held.currency()),
            )
        })
        .collect();

    // **The side with no shelf.** A balance on the account in a currency no lot
    // is carried in is stock the books claim and the shelves have never heard
    // of, which is the one shape a row-driven comparison cannot see.
    if let Some(booked) = booked.filter(|booked| !booked.is_zero())
        && !held.iter().any(|held| held.currency() == booked.currency())
    {
        sides.push((erp_types::Money::zero(booked.currency()), Some(booked)));
    }

    sides
        .into_iter()
        .filter(|(held, booked)| booked.map(erp_types::Money::minor) != Some(held.minor()))
        .collect()
}

#[async_trait::async_trait]
impl Invariant for StockValueAgrees {
    fn name(&self) -> &'static str {
        "stock_value"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(inventory::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        let mut conn = db.acquire().await?;
        // The tenant's own account, not the conventional code: a business that
        // pointed stock at `1310` would otherwise be reported as broken for
        // ever.
        let account = inventory::PostingAccounts::resolve(&mut conn)
            .await?
            .inventory;
        let booked = ledger::account_balances(&mut conn)
            .await?
            .into_iter()
            .find(|a| a.code == account.as_str())
            .map(|a| a.balance);

        Ok(
            stock_disagreements(&inventory::value_on_hand(&mut conn).await?, booked)
                .into_iter()
                .map(|(held, booked)| {
                    Finding::new(
                        "stock_value",
                        format!(
                            "the shelves are worth {held} and {account} holds {} — a movement \
                             that did not post, an entry made against the stock account by \
                             hand, or a read model behind the log",
                            booked.map_or_else(|| "nothing".to_owned(), |b| b.to_string()),
                        ),
                    )
                })
                .collect(),
        )
    }
}

/// **Stock going off, on the bell of whoever may write it off** — decision 9's
/// warning, raised as a notification (D-B) rather than logged for operators.
///
/// Reads the tenant's own window ([`inventory::ExpiryWindow`]) and calendar and
/// the open lots out of `proj_inventory`, and puts each through [`going_off`]:
/// a lot that reaches its date inside the window is `stock_expiring`, one past
/// its date and still on the shelf `stock_expired`. **A lot with no date, or
/// with nothing left on it, is neither** — whether a sale, a count or a
/// write-off emptied it — and one that empties after it was told is never told
/// again. It posts nothing and moves nothing: what leaves the shelf leaves
/// through a write-off a person enters.
///
/// **A job and not a hook in `inventory`**, because no module may ring the bell
/// (§47): `messaging` reads `inventory` to say what a lot is, so `inventory`
/// announcing would close a cycle cargo refuses. A scan like every producer
/// here — no cursor, every visit.
///
/// # Once per lot, and once more when its date has passed
///
/// A notification's id is derived from its kind and its subject
/// (`notifications::announce::derived_id`); here the subject is the lot and the
/// kind is its state. So a second run writes nothing, and neither does a window
/// widened to reach a lot already told — the window is not part of the id.
/// **Passing its date earns a second, distinct notification**, because it asks
/// for a different act: going off soon is an order to rotate or mark down while
/// the batch can still be sold, and gone but still on the shelf is stock that
/// has to come off it today — a write-off for `expired`, or a return.
/// Collapsing the two is how the second reads as a repeat of the first. It too
/// is said once.
///
/// # Who is told
///
/// **Exactly the members the write-off route would let write that lot off**
/// ([`who_may_write_off`]). That is membership and roles, control-plane, which
/// is why this job holds the control plane and names them to the bell itself —
/// and asks it only when there is somebody to tell.
struct AnnounceExpiringStock {
    control: Arc<ControlPlane>,
}

/// How many lots one read covers, for the announcer and the check alike.
const STOCK_PAGE: i64 = 200;

#[async_trait::async_trait]
impl erp_worker::Job for AnnounceExpiringStock {
    fn name(&self) -> &'static str {
        "notifications.stock"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(inventory::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        if !announces(db) {
            return Ok(Activity::Idle);
        }
        let now = chrono::Utc::now();
        let (window, today, limits) = {
            let mut conn = db.read().await?;
            let window = inventory::ExpiryWindow::resolve(&mut conn).await?;
            // The tenant's day: a batch dated the 11th is still good at one in
            // the morning on the 11th in Riyadh.
            let today = erp_eventlog::configuration::calendar(&mut conn)
                .await?
                .day(now);
            // **Unreadable limits stop the job** rather than reading as none,
            // exactly as `TenantDb::permits` refuses: telling somebody a limit
            // keeps away from the stock is not a smaller fault than telling
            // nobody, and the bell check says so either way.
            let limits = erp_eventlog::configuration::get::<erp_tenant::Limits>(
                &mut conn,
                erp_tenant::Limits::KEY,
            )
            .await?
            .map(|configured| configured.value)
            .unwrap_or_default();
            (window, today, limits)
        };

        // ponytail: every open lot inside the window is read on every visit, a
        // page at a time, so lots past the first page are not starved behind
        // the ones already told. A watermark if a shop with thousands of dated
        // lots ever makes the read show.
        let mut members = None;
        let mut after = None;
        loop {
            let page = {
                let mut conn = db.read().await?;
                inventory::lots(
                    &mut conn,
                    None,
                    None,
                    read_before(today, window),
                    STOCK_PAGE,
                    after.as_ref(),
                )
                .await?
            };
            let (gone, soon) = going_off(&page.items, today, window);
            let mut worked = false;
            for (kind, lots) in [
                (notifications::Kind::StockExpired, gone),
                (notifications::Kind::StockExpiring, soon),
            ] {
                let ids: Vec<String> = lots.iter().map(|lot| lot.id.clone()).collect();
                let told = {
                    let mut conn = db.read().await?;
                    notifications::announced_subjects(&mut conn, kind, &ids).await?
                };
                // **By branch**, because who may write stock off can differ by
                // branch.
                let mut untold = std::collections::BTreeMap::<_, Vec<_>>::new();
                for lot in lots.into_iter().filter(|lot| !told.contains(&lot.id)) {
                    match erp_types::AggregateId::new(lot.id.clone()) {
                        Ok(id) => untold.entry(lot.branch.as_deref()).or_default().push(id),
                        // Not skipped quietly: the bell check reports it too.
                        Err(error) => tracing::warn!(
                            tenant = %db.tenant(),
                            lot = %lot.id,
                            %error,
                            "a lot whose id cannot name a notification"
                        ),
                    }
                }
                if untold.is_empty() {
                    continue;
                }
                if members.is_none() {
                    members = Some(self.control.members(db.tenant()).await?);
                }
                let everybody = members.as_deref().unwrap_or_default();
                for (branch, subjects) in untold {
                    let to = who_may_write_off(everybody, &limits, branch);
                    let swept = notifications::announce_all(db, kind, &subjects, &to, now).await?;
                    worked |= swept.announced > 0;
                }
            }
            // **One page of announcements a tick**, which keeps a visit bounded;
            // the next tick reads past what this one told.
            if worked {
                return Ok(Activity::Worked);
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(Activity::Idle),
            }
        }
    }
}

/// **The logins the write-off route would let write a lot at `branch` off.**
///
/// The route's own decision, asked per member: its capability
/// ([`inventory::http::WRITE_OFF`], named from the type the handler takes)
/// under `inventory`, through the role that applies there — a module role over
/// the tenant-wide one — narrowed by the tenant's limits with the lot's branch
/// as the request's (`Limits::permit` over `limits::facts_at`, which is what
/// `Allowed` runs). **A suspended login is not told**: it cannot sign in to
/// write anything off.
fn who_may_write_off(
    members: &[erp_control::Member],
    limits: &erp_tenant::Limits,
    branch: Option<&str>,
) -> Vec<String> {
    let module = inventory::module_id();
    let facts = erp_tenant::limits::facts_at(inventory::http::WRITE_OFF, branch);
    members
        .iter()
        .filter(|member| !member.suspended)
        .filter(|member| {
            limits.permit(
                &erp_control::Access {
                    role: member.role,
                    overrides: member.module_roles.clone(),
                    machine: false,
                    branches: None,
                },
                inventory::http::WRITE_OFF,
                Some(&module),
                &facts,
            )
        })
        .map(|member| member.identity.to_string())
        .collect()
}

/// **What the listing is asked for**: the day after
/// [`inventory::ExpiryWindow::warns_until`], because
/// `expiring_before` is strictly before and the window's last day warns. Only
/// a narrowing of what is read — [`going_off`] decides — but a narrowing that
/// reads too little warns too little, and no classifier can notice a lot it
/// was never handed, so this half of the boundary is tested too.
fn read_before(
    today: chrono::NaiveDate,
    window: inventory::ExpiryWindow,
) -> Option<chrono::NaiveDate> {
    window.warns_until(today).and_then(|last| last.succ_opt())
}

/// **Which lots have passed their date, and which reach it inside the window**
/// — `(gone, soon)`, in the order given.
///
/// The rule, in one function tested without a database. **A lot with no date
/// never warns**: nothing on it spoils. **An emptied lot never warns**: there
/// is nothing left on it to throw away. A lot is gone once its date is behind
/// `today` — a batch dated today is still good today, which is the reading
/// `GET /v1/inventory/lots?expiring_before=` takes — and soon when its date is
/// no later than [`inventory::ExpiryWindow::warns_until`].
fn going_off(
    lots: &[inventory::LotRow],
    today: chrono::NaiveDate,
    window: inventory::ExpiryWindow,
) -> (Vec<&inventory::LotRow>, Vec<&inventory::LotRow>) {
    let last = window.warns_until(today);
    let mut gone = Vec::new();
    let mut soon = Vec::new();
    for lot in lots.iter().filter(|lot| lot.remaining > 0) {
        let Some(expires_on) = lot.expires_on else {
            continue;
        };
        if expires_on < today {
            gone.push(lot);
        } else if last.is_none_or(|last| expires_on <= last) {
            soon.push(lot);
        }
    }
    (gone, soon)
}

/// **Whether the bell rang for stock going off** — what operators watch in
/// place of every tenant's expiring lots (D-C).
///
/// A tenant is told about its own lots, on its own bell
/// ([`AnnounceExpiringStock`]), and an operator can do nothing about a batch of
/// milk. What an operator can act on is the telling not happening: **a lot the
/// announcer should have told somebody about, with no notification of the kind
/// its state calls for, for longer than [`bell_grace`]**. That is the job not
/// running, the bell's read model behind, nobody who may write stock off where
/// the lot is — a limit refusing everybody, a suspended owner — or a
/// notification that cannot be recorded. Silent while the bell works.
///
/// Two readings of one log, like [`StockValueAgrees`]: the lots out of
/// `proj_inventory` and what was announced out of `proj_notifications`,
/// compared here because L3 keeps either module from reading the other's. The
/// rule is [`unannounced`], pure and tested.
///
/// **Silent without a bell.** A tenant that has not enabled `notifications` has
/// no path to be broken, and nobody is told either — see §77.
struct StockBellRings {
    grace: chrono::TimeDelta,
}

/// How often the health job looks at a tenant.
const HEALTH_INTERVAL: Duration = Duration::from_mins(5);

/// **How long a lot may be due a notification with none raised before that is
/// a finding.**
///
/// [`AnnounceExpiringStock`] runs on every visit to a tenant, so the ordinary
/// wait between a lot falling due and being told is the wait for the next visit
/// — at its longest, for a tenant with nothing else going on,
/// [`erp_control::WorkSchedule::longest_idle_delay`]: the six-hour ceiling and
/// two hours of jitter with the shipped schedule. **Plus one health interval**,
/// because the notification reaches the bell's read model a round after it is
/// written and the check only looks that often: a finding means a whole visit
/// came and went without it. Derived from the schedule the worker runs, so a
/// longer ceiling moves the grace with it.
fn bell_grace(schedule: &erp_control::WorkSchedule) -> chrono::TimeDelta {
    chrono::TimeDelta::from_std(schedule.longest_idle_delay() + HEALTH_INTERVAL)
        .unwrap_or(chrono::TimeDelta::MAX)
}

/// **The lots the bell should have rung for by now and has not** — those past
/// their date first, then those going off.
///
/// A lot is due `stock_expired` from the start of the tenant's day after its
/// date, and `stock_expiring` from the start of the day its window first
/// reaches its date — **or from when the lot was recorded, or (going off) when
/// the tenant last set the window, if either is later**. A delivery that lands
/// already inside its window, or a window widened this morning, is due from
/// then, because nothing could have told anybody before it. It is a finding
/// once it has been due for longer than `grace` and `told` says no notification
/// of that kind about it exists.
fn unannounced(
    lots: &[inventory::LotRow],
    told: impl Fn(notifications::Kind, &str) -> bool,
    calendar: erp_types::Calendar,
    now: erp_types::Timestamp,
    window: inventory::ExpiryWindow,
    window_set_at: Option<erp_types::Timestamp>,
    grace: chrono::TimeDelta,
) -> Vec<&inventory::LotRow> {
    let (gone, soon) = going_off(lots, calendar.day(now), window);
    let days = chrono::Days::new(u64::try_from(window.days).unwrap_or_default());
    let expired = gone.into_iter().map(|lot| {
        let from = lot.expires_on.and_then(|day| day.succ_opt());
        (notifications::Kind::StockExpired, lot, from, None)
    });
    let expiring = soon.into_iter().map(|lot| {
        let from = lot.expires_on.and_then(|day| day.checked_sub_days(days));
        (notifications::Kind::StockExpiring, lot, from, window_set_at)
    });
    expired
        .chain(expiring)
        .filter(|(kind, lot, _, _)| !told(*kind, &lot.id))
        .filter(|(_, lot, from, set)| {
            let due = from
                .map(|day| calendar.start_of(day))
                .into_iter()
                .chain([lot.recorded_at])
                .chain(*set)
                .max()
                .unwrap_or(lot.recorded_at);
            now - due > grace
        })
        .map(|(_, lot, _, _)| lot)
        .collect()
}
/// The first few lots, **by id and nothing else**. An operator acts on the
/// bell, not on the stock: what is on a tenant's shelf, how much and until
/// when, is the tenant's (D-C), and the id is enough to find the lot and its
/// notification.
fn describe_lots(lots: &[&inventory::LotRow]) -> String {
    const NAMED: usize = 5;
    let named: Vec<&str> = lots.iter().take(NAMED).map(|lot| lot.id.as_str()).collect();
    if lots.len() > NAMED {
        format!("{}, and {} more", named.join("; "), lots.len() - NAMED)
    } else {
        named.join("; ")
    }
}

#[async_trait::async_trait]
impl Invariant for StockBellRings {
    fn name(&self) -> &'static str {
        "stock_bell"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(inventory::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        if !announces(db) {
            return Ok(Vec::new());
        }
        let now = chrono::Utc::now();
        let mut conn = db.read().await?;
        // The window with when it was set — see `unannounced`.
        let (window, window_set_at) = erp_eventlog::configuration::get::<inventory::ExpiryWindow>(
            &mut conn,
            inventory::ExpiryWindow::KEY,
        )
        .await?
        .map_or((inventory::ExpiryWindow::DEFAULT, None), |configured| {
            (configured.value, Some(configured.set_at))
        });
        let calendar = erp_eventlog::configuration::calendar(&mut conn).await?;

        let mut late = Vec::new();
        let mut after = None;
        loop {
            let page = inventory::lots(
                &mut conn,
                None,
                None,
                read_before(calendar.day(now), window),
                STOCK_PAGE,
                after.as_ref(),
            )
            .await?;
            let ids: Vec<String> = page.items.iter().map(|lot| lot.id.clone()).collect();
            let expiring = notifications::announced_subjects(
                &mut conn,
                notifications::Kind::StockExpiring,
                &ids,
            )
            .await?;
            let expired = notifications::announced_subjects(
                &mut conn,
                notifications::Kind::StockExpired,
                &ids,
            )
            .await?;
            let told = |kind: notifications::Kind, id: &str| {
                if kind == notifications::Kind::StockExpired {
                    expired.contains(id)
                } else {
                    expiring.contains(id)
                }
            };
            late.extend(
                unannounced(
                    &page.items,
                    told,
                    calendar,
                    now,
                    window,
                    window_set_at,
                    self.grace,
                )
                .into_iter()
                .cloned(),
            );
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        drop(conn);

        if late.is_empty() {
            return Ok(Vec::new());
        }
        let named: Vec<&inventory::LotRow> = late.iter().collect();
        Ok(vec![Finding::new(
            "stock_bell",
            format!(
                "{} {} been due a notification for more than {} minutes with none \
                 announced — the stock announcer is not running, nobody may write stock \
                 off where {} kept, or the bell cannot record one: {}",
                late.len(),
                if late.len() == 1 {
                    "lot has"
                } else {
                    "lots have"
                },
                self.grace.num_minutes(),
                if late.len() == 1 { "it is" } else { "they are" },
                describe_lots(&named),
            ),
        )])
    }
}

/// **The report group against the books** (§10b).
///
/// The invariant that makes a figure on a dashboard worth reading. It lives
/// here for the same reason [`TrialBalance`] does: the kernel must not know what
/// a report is, and a module must not depend on the worker.
///
/// A discrepancy is a **failure**, not a coloured cell (L6) — so it is a health
/// finding, which is what makes the tenant unhealthy, rather than a field on
/// the response that a front end would render in amber.
///
/// The warning this answers, from the system this phase was read against: its
/// customer statement is built from invoices rather than from the ledger,
/// because the ledger was unfinished. Two financial truths that disagree is
/// what this exists to catch on the day it starts rather than at an audit.
struct ReportsReconcile;

#[async_trait::async_trait]
impl Invariant for ReportsReconcile {
    fn name(&self) -> &'static str {
        "reports_reconcile"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(reports::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        let mut conn = db.acquire().await?;
        Ok(reports::reconciles(&mut conn)
            .await?
            .iter()
            .map(|found| Finding::new("reports_reconcile", found.describe()))
            .collect())
    }
}

/// How long before a certificate expires the platform starts saying so.
///
/// Sixty days, because renewing needs a **human**: the taxpayer logs in to the
/// Fatoora portal and reads an OTP off a screen. Nothing here can do that, so
/// the only thing that stops a lapse is telling somebody early enough to act.
const EXPIRY_WARNING: chrono::TimeDelta = chrono::TimeDelta::days(60);

/// **A ZATCA certificate that is running out.**
///
/// When it lapses, every invoice stops being clearable — and the first anyone
/// would know is a customer waiting for one. A five-year certificate is exactly
/// the kind of deadline nobody has a reminder for.
/// **Work documents that have lapsed, or are about to.**
///
/// The producer §9e asks for, in the shape that actually reaches somebody
/// today. The plan asks for an outbox effect on a date; the tenant dispatcher
/// has no handlers registered at all — email is control-plane, because the
/// things that send it are control-plane rows — so an effect enqueued from `hr`
/// would sit in the outbox for ever. A health finding is read.
///
/// **A lapsed document is a separate finding from an expiring one**, and not a
/// louder version of it: one is somebody to remind, the other is somebody who
/// must come off the rota today. Collapsing them into a single severity is how
/// the second gets treated like the first.
struct WorkDocumentExpiry;

#[async_trait::async_trait]
impl Invariant for WorkDocumentExpiry {
    fn name(&self) -> &'static str {
        "work_document_expiry"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(hr::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        // The read model, not the aggregates (L7) — and it costs one indexed
        // scan rather than loading every employee the business has ever had.
        let mut conn = db.read().await?;
        let expiring = hr::expiring(&mut conn, DOCUMENT_WARNING_DAYS, 200).await?;
        drop(conn);

        let (lapsed, soon): (Vec<_>, Vec<_>) = expiring.into_iter().partition(|d| d.days_left < 0);

        let mut findings = Vec::new();
        if !lapsed.is_empty() {
            findings.push(Finding::new(
                "work_document_lapsed",
                format!(
                    "{} work {} lapsed and the people holding them cannot be \
                     rostered: {}",
                    lapsed.len(),
                    if lapsed.len() == 1 {
                        "document has"
                    } else {
                        "documents have"
                    },
                    describe(&lapsed),
                ),
            ));
        }
        if !soon.is_empty() {
            findings.push(Finding::new(
                "work_document_expiring",
                format!(
                    "{} work {} within {DOCUMENT_WARNING_DAYS} days: {}",
                    soon.len(),
                    if soon.len() == 1 {
                        "document expires"
                    } else {
                        "documents expire"
                    },
                    describe(&soon),
                ),
            ));
        }
        Ok(findings)
    }
}

/// The first few, named. **Not all of them**: a finding that lists two hundred
/// people is one nobody reads, and the count above already says how many there
/// are.
fn describe(documents: &[hr::Expiring]) -> String {
    const NAMED: usize = 5;
    let named: Vec<String> = documents
        .iter()
        .take(NAMED)
        .map(|d| format!("{} ({}, {})", d.name, d.kind, d.expires_on))
        .collect();
    if documents.len() > NAMED {
        format!("{}, and {} more", named.join("; "), documents.len() - NAMED)
    } else {
        named.join("; ")
    }
}

/// How far ahead a warning is worth having.
///
/// Sixty days is roughly what an iqama renewal needs — long enough to act on,
/// short enough that it is not permanently on the list.
const DOCUMENT_WARNING_DAYS: i32 = 60;

struct CertificateExpiry;

#[async_trait::async_trait]
impl Invariant for CertificateExpiry {
    fn name(&self) -> &'static str {
        "zatca_certificate_expiry"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        // The read model, not the aggregate (L7). It also costs one row rather
        // than every certificate ever issued, and a renewal appends.
        let mut conn = db.read().await?;
        let onboarded = tax_sa::onboarding(&mut conn).await?;
        drop(conn);

        // Never onboarded: nothing to expire, and not a finding.
        let Some(onboarded) = onboarded else {
            return Ok(Vec::new());
        };
        let not_after = onboarded.not_after.as_str();
        let Some(expires) = certificate_time(not_after) else {
            return Ok(vec![Finding::new(
                "zatca_certificate_expiry",
                format!("the certificate's expiry date cannot be read: {not_after:?}"),
            )]);
        };

        let left = expires - chrono::Utc::now();
        if left > EXPIRY_WARNING {
            return Ok(Vec::new());
        }

        Ok(vec![Finding::new(
            "zatca_certificate_expiry",
            if left.num_seconds() <= 0 {
                format!(
                    "the ZATCA certificate expired on {not_after}; no invoice can be cleared \
                     or reported until it is renewed"
                )
            } else {
                format!(
                    "the ZATCA certificate expires in {} days ({not_after}); renewing needs \
                     an OTP from the taxpayer's Fatoora portal",
                    left.num_days()
                )
            },
        )])
    }
}

/// OpenSSL's `Aug 16 20:28:41 2031 GMT`, as an instant.
///
/// Parsed from what the certificate says rather than from a field this system
/// chose, because the certificate is the authority on when it stops working.
fn certificate_time(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // `%e` is the day with a leading space for single digits, which is what
    // OpenSSL prints and what a `%d` parse would reject on the ninth of a month.
    chrono::NaiveDateTime::parse_from_str(text.trim(), "%b %e %H:%M:%S %Y GMT")
        .ok()
        .map(|naive| naive.and_utc())
}

/// How many documents one sweep handles for one tenant.
///
/// Small on purpose: a visit is meant to be short so the worker gets round
/// every tenant, and a clearance call is a network round trip in front of a
/// person. What is not swept this visit is swept the next.
const ZATCA_BATCH: i64 = 20;

/// How many emails one pass sends.
///
/// Small on purpose. The platform pass runs **inline** in the claim loop, so a
/// batch is time the worker is not visiting tenants — and a relay that answers
/// in 200 ms turns 32 into six seconds. Anything left over goes on the next
/// cycle, which is at most `empty_claim_pause` away.
const EMAIL_BATCH: i64 = 32;

/// The mailer, from the deployment's environment.
///
/// **Registered only when `SMTP_URL` is set**, and the handler is absent
/// otherwise — which is not the same as broken. An effect whose kind has no
/// registered handler is *not claimed* (see `erp_eventlog::outbox`), so on a
/// deployment with no relay an invitation email waits in the outbox as an
/// undelivered promise rather than being attempted and dead-lettered. Configure
/// SMTP later and everything already promised goes out.
///
/// That is the same call `SEALING_KEY` makes below, for the same reason: a job
/// that runs and finds it can do nothing is quieter than one that was never
/// registered.
fn mailer()
-> Result<Vec<Arc<dyn erp_eventlog::EffectHandler>>, Box<dyn std::error::Error + Send + Sync>> {
    let Ok(url) = std::env::var("SMTP_URL") else {
        tracing::warn!(
            "SMTP_URL is not set; invitations are still created and their emails \
             still promised, but nothing sends them — the invitation link in the \
             API response is the only way in until a relay is configured"
        );
        return Ok(Vec::new());
    };
    let from = std::env::var("SMTP_FROM")?;
    let smtp = erp_worker::mail::Smtp::new(&url, &from)?;
    tracing::info!(from = %from, "SMTP configured; email effects will be delivered");
    Ok(vec![Arc::new(erp_worker::mail::EmailHandler::new(
        Arc::new(smtp),
    ))])
}

/// Every message transport this deployment is configured for.
///
/// # Why each one is optional, and separately
///
/// The same call `mailer()` makes: a worker with no SMS gateway configured
/// registers no SMS handler, so SMS effects **wait in the outbox** for a worker
/// that has one rather than being dead-lettered during a staggered rollout.
/// That is the dispatcher's documented behaviour and the reason a channel is an
/// effect kind rather than a field on one.
///
/// # A named gateway, or the generic relay
///
/// Each channel takes at most one transport, and a named provider **wins over**
/// the relay for that channel: a deployment that configures both meant the
/// provider, and registering two handlers for one effect kind would deliver
/// every message twice.
///
/// Email is the exception in shape only: it goes over SMTP like the control
/// plane's, so the transport wraps the mailer that already exists.
fn message_transports() -> Vec<Arc<dyn messaging::Transport>> {
    let mut transports: Vec<Arc<dyn messaging::Transport>> = Vec::new();

    if let (Ok(url), Ok(from)) = (std::env::var("SMTP_URL"), std::env::var("SMTP_FROM")) {
        match erp_worker::mail::Smtp::new(&url, &from) {
            Ok(smtp) => {
                tracing::info!("tenant email will be delivered over SMTP");
                transports.push(Arc::new(Post::new(Arc::new(smtp))));
            }
            Err(error) => tracing::warn!(%error, "SMTP is configured and not usable"),
        }
    }

    for (channel, prefix) in [
        (messaging::Channel::Sms, "SMS"),
        (messaging::Channel::Push, "PUSH"),
        (messaging::Channel::WhatsApp, "WHATSAPP"),
    ] {
        // A named gateway wins over the relay for its channel. **WhatsApp has
        // none**, deliberately: outside a customer service window Meta accepts
        // only pre-approved templates, and this system hands a transport a
        // finished string. See review §21 in the plan.
        let gateway = match channel {
            messaging::Channel::Sms => taqnyat(),
            messaging::Channel::Push => fcm(),
            // **In-system has no gateway and never will**: a bell is a record
            // in the tenant's own log, written by `notifications`, and it does
            // not reach this loop at all.
            messaging::Channel::Email
            | messaging::Channel::WhatsApp
            | messaging::Channel::InSystem => None,
        };
        if let Some(transport) = gateway {
            transports.push(transport);
            continue;
        }

        let (Ok(url), Ok(token)) = (
            std::env::var(format!("{prefix}_RELAY_URL")),
            std::env::var(format!("{prefix}_RELAY_TOKEN")),
        ) else {
            tracing::warn!(
                channel = channel.as_str(),
                "no gateway and no {prefix}_RELAY_URL; messages on this channel are \
                 still promised and wait in the outbox until one is configured"
            );
            continue;
        };
        match messaging::Relay::new(channel, &url, &token) {
            Ok(relay) => {
                tracing::info!(channel = channel.as_str(), url = %url, "relay configured");
                transports.push(Arc::new(relay));
            }
            Err(error) => {
                tracing::warn!(channel = channel.as_str(), %error, "relay is not usable");
            }
        }
    }

    transports
}

/// SMS through Taqnyat, if this deployment has an account.
///
/// The sender name is required and case sensitive — an unregistered one is a
/// permanent refusal on **every** message, so it is refused here where somebody
/// is reading a log rather than silently on the first reminder.
fn taqnyat() -> Option<Arc<dyn messaging::Transport>> {
    let token = std::env::var("TAQNYAT_TOKEN").ok()?;
    let Ok(sender) = std::env::var("TAQNYAT_SENDER") else {
        tracing::warn!(
            "TAQNYAT_TOKEN is set and TAQNYAT_SENDER is not; Taqnyat needs the sender \
             name registered on the account and will refuse every message without it"
        );
        return None;
    };
    match messaging::Taqnyat::new(&token, &sender) {
        Ok(sms) => {
            tracing::info!(sender = %sender, "SMS will be delivered through Taqnyat");
            Some(Arc::new(sms))
        }
        Err(error) => {
            tracing::warn!(%error, "Taqnyat is configured and not usable");
            None
        }
    }
}

/// Push through Firebase, if this deployment has a service account.
///
/// `FCM_SERVICE_ACCOUNT_FILE` is the path a container secret is mounted at, and
/// is preferred: the key is a multi-line PEM and an environment variable
/// holding one is a variable somebody will eventually paste into a chat window.
/// `FCM_SERVICE_ACCOUNT` takes the JSON itself, for deployments with no files.
fn fcm() -> Option<Arc<dyn messaging::Transport>> {
    let json = match std::env::var("FCM_SERVICE_ACCOUNT_FILE") {
        Ok(path) => match std::fs::read_to_string(&path) {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(%path, %error, "the FCM service account file cannot be read");
                return None;
            }
        },
        Err(_) => std::env::var("FCM_SERVICE_ACCOUNT").ok()?,
    };

    let account = match messaging::fcm::ServiceAccount::parse(&json) {
        Ok(account) => account,
        Err(error) => {
            tracing::warn!(%error, "the FCM service account is not usable");
            return None;
        }
    };
    let project = account.project_id.clone();

    match messaging::Fcm::new(account) {
        Ok(push) => {
            tracing::info!(%project, "push will be delivered through FCM");
            Some(Arc::new(push))
        }
        Err(error) => {
            tracing::warn!(%error, "FCM is configured and not usable");
            None
        }
    }
}

/// The SMTP mailer, as a message transport.
///
/// Two traits for one act, and they are in two crates that cannot see each
/// other: `Mailer` is `erp-worker`'s and predates modules having handlers at
/// all, and `messaging::Transport` is a module's. This is the composition root,
/// which is the one place allowed to know both.
struct Post {
    mailer: Arc<dyn erp_worker::mail::Mailer>,
}

impl Post {
    fn new(mailer: Arc<dyn erp_worker::mail::Mailer>) -> Self {
        Self { mailer }
    }
}

#[async_trait::async_trait]
impl messaging::Transport for Post {
    fn channel(&self) -> messaging::Channel {
        messaging::Channel::Email
    }

    async fn send(
        &self,
        message: &messaging::Outbound,
        key: &str,
    ) -> Result<(), messaging::TransportError> {
        let email = erp_control::mail::Email {
            to: message.to.clone(),
            subject: message.subject.clone(),
            body: message.body.clone(),
            locale: message.locale,
        };
        self.mailer.send(&email, key).await.map_err(|e| match e {
            erp_worker::mail::MailError::Unreachable(why) => {
                messaging::TransportError::Unreachable(why)
            }
            erp_worker::mail::MailError::Refused(why) => messaging::TransportError::Refused(why),
        })
    }
}

/// How long before a booking a reminder goes.
///
/// Twenty-four hours, which is the interval every one of these businesses uses
/// and the one a customer can still act on: far enough ahead to rearrange, near
/// enough to be about today.
const REMINDER_NOTICE: chrono::TimeDelta = chrono::TimeDelta::hours(24);

/// How wide a slice of the diary one tick looks at.
///
/// The job runs on every visit, so the window only has to be wider than the gap
/// between visits. Two hours is generous, and every send is keyed on the
/// booking — so a booking seen on four consecutive ticks is promised once.
const REMINDER_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(2);

/// **Phase 11's exit criterion.**
///
/// A booking reminder that reaches a customer in their language, on whichever
/// channel the template names, with a short link, having asked the read model
/// for everything it says.
///
/// # Why the sending is here and not in the dispatcher
///
/// The dispatcher holds **no connection** while it delivers — a documented
/// property, and the reason a slow relay cannot exhaust a tenant's pool — so a
/// handler can read nothing. "At send time" therefore means *as late as
/// possible while a connection is legitimately held*, which is here: this runs
/// minutes before the message goes, so a booking somebody moved this morning is
/// described as it stands this morning.
///
/// # Why nothing happens without a template
///
/// A tenant that has not written `booking.reminder` sends no reminders, and
/// that is the correct default: this system does not get to decide what a
/// business says to its customers, or that it says anything at all.
/// How many lapsed holds one pass releases, per tenant.
const HOLD_BATCH: i64 = 100;

/// **Releases slots nobody paid for.**
///
/// A business that asks for a deposit is asking because a held slot is a slot
/// nobody else can take. Somebody who books and never pays has taken one for
/// free, and the whole point of the deposit is that they cannot — so the hold
/// has to lapse on its own, without anybody watching for it.
///
/// # It asks one question, of one projection group
///
/// "Is this booking still `reserved`, was a deposit asked for, is it past its
/// deadline, and has nothing paid it." All four are columns on
/// `proj_booking.reservation`, because whether the money arrived is a fact this
/// module was **told** — `Secured`, written by the settle job above — rather
/// than one it reads out of `proj_payments`. A job that joined the two would be
/// reading two checkpoints that can disagree, and the disagreement it would hit
/// is the one that matters: a deposit that settled a moment ago and whose
/// booking has not heard yet.
///
/// That ordering is deliberate and it only fails safe. `Secured` is written
/// before this looks, so the worst case is a booking released a tick after its
/// deadline rather than one released after it was paid for.
/// **Bills completed bookings**, for a business that asked for that.
///
/// The composition is `erp_api::billing::bill_completions` — `booking` says
/// what was done, `payments` which prepayment invoice the deposit raised,
/// `sales` raises the final invoice with it deducted — and it runs here for
/// the same reason the deposit join does: neither module may name the other,
/// and the worker depends on all of them. Off until `PUT /v1/booking/billing`
/// turns it on; the desk can always bill on demand.
struct BillCompletedBookings;

#[async_trait::async_trait]
impl erp_worker::Job for BillCompletedBookings {
    fn name(&self) -> &'static str {
        "booking.bill_completed"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(booking::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        // A tenant with booking and no sales cannot raise an invoice, and
        // that is not a failure of this job.
        if !db.has_module(&sales::module_id()) {
            return Ok(Activity::Idle);
        }
        let billed =
            erp_api::billing::bill_completions(db, chrono::Utc::now(), &by_the_platform()).await?;
        Ok(if billed > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

struct ExpireUnpaidHolds;

#[async_trait::async_trait]
impl erp_worker::Job for ExpireUnpaidHolds {
    fn name(&self) -> &'static str {
        "booking.expire_unpaid_holds"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(booking::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        let now = chrono::Utc::now();
        let lapsed = {
            let mut conn = db.read().await?;
            booking::lapsed_holds(&mut conn, now, HOLD_BATCH).await?
        };
        if lapsed.is_empty() {
            return Ok(Activity::Idle);
        }

        let mut released = 0;
        for hold in &lapsed {
            // Each on its own, because one booking refusing to move must not
            // roll back the ten before it that were fine.
            //
            // **`lapse`, not `move_to(Cancelled)`.** The list above came from
            // the projection, and a deposit that settled since it was written
            // is exactly the case that must not be cancelled. `lapse` re-asks
            // the log and refuses a paid booking; this job never overrides that
            // answer, it only reports it.
            match booking::lapse(db, &hold.id, now, &by_the_platform()).await {
                Ok(committed) if committed.at.is_some() => released += 1,
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    tenant = %db.tenant(),
                    reservation = %hold.id,
                    error = %e,
                    "an unpaid hold was not released"
                ),
            }
        }

        Ok(if released > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

struct BookingReminders;

#[async_trait::async_trait]
impl erp_worker::Job for BookingReminders {
    fn name(&self) -> &'static str {
        "messaging.booking_reminders"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(messaging::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        let now = chrono::Utc::now();
        let from = now + REMINDER_NOTICE;
        let until = from + REMINDER_WINDOW;

        let due = {
            let mut conn = db.read().await?;
            // Confirmed only. A booking still at `reserved` has not been agreed
            // by anybody, and one already cancelled or completed needs no
            // reminding.
            booking::reservations(
                &mut conn,
                Some(from),
                Some(until),
                Some("confirmed"),
                100,
                None,
            )
            .await?
            .items
        };
        if due.is_empty() {
            return Ok(Activity::Idle);
        }

        let mut sent = 0;
        for reservation in due {
            let mut tx = db.begin().await?;

            // The link is made in the same transaction as the promise, so a
            // rollback takes both. `shorten` is keyed on the booking, which is
            // what makes a re-run give the customer the same URL.
            let token = erp_links::shorten(
                &mut tx,
                &erp_links::New {
                    key: format!("booking.reminder.{}", reservation.id),
                    target: format!("/v1/booking/public/reservations/{}", reservation.id),
                    external: false,
                    // It stops working when the booking has been and gone. A
                    // link into somebody's diary is not a permanent grant.
                    expires_at: Some(reservation.ends_at),
                    single_use: false,
                    at: now,
                },
            )
            .await?;

            let sending = messaging::Sending {
                template: "booking.reminder".to_owned(),
                subject: messaging::Subject::new(
                    messaging::Topic::Reservation,
                    erp_types::AggregateId::new(&reservation.id)?,
                ),
                key: format!("booking.reminder.{}", reservation.id),
                operator: None,
                extra: std::collections::BTreeMap::from([(
                    "link".to_owned(),
                    format!("/l/{token}"),
                )]),
                locale: None,
                at: now,
            };

            match messaging::send(&mut tx, &sending).await {
                Ok(promised) => {
                    tx.commit().await?;
                    sent += promised.promised;
                }
                // **Every refusal rolls back, including the meter**, and none
                // of them stops the loop. A customer with no mobile number, a
                // tenant with no template, a month that is out of budget — all
                // three are facts about one booking or one tenant, and none is
                // a reason to leave the rest of the diary unreminded.
                Err(error) => {
                    tx.rollback().await?;
                    tracing::debug!(
                        booking = %reservation.id,
                        %error,
                        "no reminder for this booking"
                    );
                }
            }
        }

        Ok(if sent > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// How long a retired push token is kept before it is removed.
///
/// Long enough that somebody investigating "why did this person stop getting
/// notifications" can still see the answer, and short enough that the table
/// does not grow for the life of the tenant.
const RETIRED_TOKEN_GRACE: chrono::TimeDelta = chrono::TimeDelta::days(30);

/// **Push tokens expire, and cleaning them up is scheduled work.**
///
/// Not an afterthought and not a guess about age: a token nobody has sent to in
/// six months may be perfectly good, and one the platform rejected this morning
/// is not. This removes what has already been retired.
struct RetirePushTokens;

#[async_trait::async_trait]
impl erp_worker::Job for RetirePushTokens {
    fn name(&self) -> &'static str {
        "messaging.retire_push_tokens"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(messaging::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        let before = chrono::Utc::now() - RETIRED_TOKEN_GRACE;
        let mut conn = db.acquire().await?;
        let gone = messaging::push::sweep(&mut conn, before).await?;

        Ok(if gone > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **Removes one-time codes that have expired.**
///
/// A row costs an index entry and nothing else, so this is unhurried; the reason
/// it exists is that a table nobody deletes from grows for the life of the
/// deployment, and this one grows with every sign-in attempt anybody makes.
///
/// A `PlatformJob` rather than a `Job`: codes are control-plane, because
/// identities are, and there is no tenant database they could sensibly live in.
struct SweepOneTimeCodes;

#[async_trait::async_trait]
impl erp_worker::PlatformJob for SweepOneTimeCodes {
    fn name(&self) -> &'static str {
        "control.sweep_one_time_codes"
    }

    async fn tick(
        &self,
        control: &erp_control::ControlPlane,
    ) -> Result<Activity, erp_worker::BoxError> {
        let gone = control.sweep_codes().await?;
        Ok(if gone > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// Anything a worker writes is the platform's doing, not a person's.
fn by_the_platform() -> erp_eventlog::Metadata {
    erp_eventlog::Metadata::default()
}

/// **Signs the ZATCA documents that have been built and not yet signed.**
///
/// Separate from submitting because they fail for different reasons, and a
/// document needs this one even when ZATCA is unreachable: a simplified
/// invoice's QR carries the cryptographic stamp, and that receipt goes to the
/// customer at the till.
struct SignZatcaDocuments {
    sealing: erp_eventlog::SealingKey,
}

#[async_trait::async_trait]
impl erp_worker::Job for SignZatcaDocuments {
    fn name(&self) -> &'static str {
        "tax_sa.sign"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    // A suspension drains this before it stops (decided 2026-09-14): a
    // document issued while the tenant was active still gets its stamp.
    fn drains_a_suspension(&self) -> bool {
        true
    }

    async fn drained(&self, db: &erp_control::TenantDb) -> Result<bool, erp_worker::BoxError> {
        let mut conn = db.read().await?;
        let waiting = tax_sa::awaiting_signature(&mut conn).await?;
        drop(conn);
        if waiting == 0 {
            return Ok(true);
        }
        // Nothing to sign with: a tenant that never finished onboarding has
        // documents nobody can sign, and holding its suspension open would
        // hold it for ever. `tick` reports the same rows as waiting for a
        // certificate.
        let signable = tax_sa::zatca::onboarding::production(db, &self.sealing)
            .await?
            .is_some()
            && tax_sa::zatca::onboarding::private_key(db, &self.sealing)
                .await?
                .is_some();
        Ok(!signable)
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        let signed = tax_sa::sign_pending(
            db,
            &self.sealing,
            chrono::Utc::now(),
            ZATCA_BATCH,
            &by_the_platform(),
        )
        .await?;

        // Not an error: a tenant that has not finished onboarding is in a
        // normal state, and the standing report is where that shows.
        if signed.waiting_for_a_certificate > 0 {
            tracing::debug!(
                tenant = %db.tenant(),
                waiting = signed.waiting_for_a_certificate,
                "documents are built and there is no certificate to sign them with"
            );
        }
        if signed.signed > 0 {
            tracing::info!(tenant = %db.tenant(), signed = signed.signed, "signed");
        }

        Ok(if signed.signed > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **Sends signed documents to ZATCA and records what it said.**
///
/// Standard invoices are cleared, simplified ones reported, and the sweep stops
/// on the first call that is not answered — every document after it would fail
/// the same way, and none of them is what is wrong.
struct SubmitToZatca {
    sealing: erp_eventlog::SealingKey,
}

#[async_trait::async_trait]
impl erp_worker::Job for SubmitToZatca {
    fn name(&self) -> &'static str {
        "tax_sa.submit"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    // The other half of the drain: what was signed is reported before the
    // tenant stops, so a suspension is never what makes an invoice late.
    fn drains_a_suspension(&self) -> bool {
        true
    }

    async fn drained(&self, db: &erp_control::TenantDb) -> Result<bool, erp_worker::BoxError> {
        let mut conn = db.read().await?;
        let waiting = tax_sa::awaiting_submission(&mut conn).await?;
        drop(conn);
        if waiting == 0 {
            return Ok(true);
        }
        // Nowhere to send them: `tick` answers `Idle` for the same tenant.
        let sendable = tax_sa::zatca::onboarding::production(db, &self.sealing)
            .await?
            .is_some()
            && zatca_environment(db).await?.is_some();
        Ok(!sendable)
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        // Nothing to authenticate with, or nowhere to send it: a tenant part
        // way through onboarding, which is not a failure.
        let Some(credentials) = tax_sa::zatca::onboarding::production(db, &self.sealing).await?
        else {
            return Ok(Activity::Idle);
        };
        let Some(environment) = zatca_environment(db).await? else {
            return Ok(Activity::Idle);
        };

        let client = tax_sa::zatca::http::Fatoora::new(environment)?.with_credentials(credentials);
        let swept = tax_sa::submit_pending(
            db,
            &client,
            chrono::Utc::now(),
            ZATCA_BATCH,
            &by_the_platform(),
        )
        .await?;

        // **Loudly.** A tenant whose documents are not reaching ZATCA has 24
        // hours on every simplified invoice, and nothing else in the system
        // will say so.
        if let Some(stopped) = &swept.stopped {
            tracing::warn!(
                tenant = %db.tenant(),
                error = %stopped,
                accepted = swept.accepted,
                "the ZATCA sweep stopped early; the rest stay pending"
            );
        }
        if swept.did_something() {
            tracing::info!(
                tenant = %db.tenant(),
                accepted = swept.accepted,
                refused = swept.refused,
                "submitted to ZATCA"
            );
        }

        // **And tell somebody about a refusal.** A refused document is the
        // tenant's problem to fix and nothing else in the system would say so
        // — until now it sat in a read model waiting for somebody to open the
        // right screen. Announced from here rather than from `tax_sa` because a
        // module cannot announce: see the `notifications` crate docs.
        if swept.refused > 0 {
            announce_refusals(db).await;
        }

        Ok(if swept.did_something() {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **Finishes an onboarding the route started.** A tenant holding a compliance
/// certificate is due six sample documents and a production certificate, and
/// nothing about either needs the taxpayer. `finish` records what ZATCA refused
/// and leaves what it did not answer for the next pass.
struct FinishOnboarding {
    sealing: erp_eventlog::SealingKey,
}

#[async_trait::async_trait]
impl erp_worker::Job for FinishOnboarding {
    fn name(&self) -> &'static str {
        "tax_sa.onboard"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(tax_sa::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        // The read model says whether anything is due (L7), and reading it is
        // all an idle tenant costs: never onboarded, live, or waiting on a
        // refusal this build cannot change.
        let mut conn = db.read().await?;
        let onboarded = tax_sa::onboarding(&mut conn).await?;
        drop(conn);
        let Some(onboarded) = onboarded else {
            return Ok(Activity::Idle);
        };
        if tax_sa::zatca::finish::due(&onboarded).is_none() {
            return Ok(Activity::Idle);
        }
        let environment: tax_sa::zatca::csr::Environment = onboarded
            .environment
            .parse()
            .map_err(erp_worker::BoxError::from)?;

        let zatca = tax_sa::zatca::http::Fatoora::new(environment)?;
        let finished = tax_sa::zatca::finish::finish(
            db,
            &self.sealing,
            &zatca,
            chrono::Utc::now(),
            &by_the_platform(),
        )
        .await?;

        if let Some(step) = finished.refused {
            tracing::warn!(
                tenant = %db.tenant(),
                step = step.as_str(),
                "ZATCA refused an onboarding step; waiting for a new build or a new OTP"
            );
        }
        if finished.production.is_some() {
            tracing::info!(tenant = %db.tenant(), "ZATCA onboarding finished; the tenant is live");
        }
        Ok(if finished.did_something() {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **The sweeps that talk to ZATCA**, in one list.
///
/// A function rather than two `with_job` calls in `main` for the same reason
/// [`module_jobs`] is one: a test can look at what a deployment would run, and
/// a job that runs for tenants who never bought the module is caught here
/// rather than on a bill.
fn zatca_jobs(sealing: &erp_eventlog::SealingKey) -> Vec<Arc<dyn erp_worker::Job>> {
    vec![
        Arc::new(SignZatcaDocuments {
            sealing: sealing.clone(),
        }),
        Arc::new(SubmitToZatca {
            sealing: sealing.clone(),
        }),
        Arc::new(FinishOnboarding {
            sealing: sealing.clone(),
        }),
    ]
}

/// How many pending payments one sweep asks about, per provider, per tenant.
///
/// Each is a round trip to somebody else's API, so this is deliberately small:
/// a tenant with a hundred abandoned checkouts should not spend a minute of the
/// worker's tick on them, and the next tick takes the next batch.
const PAYMENT_BATCH: i64 = 25;

/// **Asks the gateway what happened, for everything still waiting.**
///
/// The other half of the inbound story. A callback is authenticated, recorded
/// and acknowledged by the API; this is what acts on it — and, more to the
/// point, what acts when no callback ever arrives. Moyasar drops a webhook
/// after six failed attempts and Tamara documents no retry policy at all, so a
/// system that only settled on callback would lose payments quietly.
///
/// It runs here rather than in an `EffectHandler` because settling writes to
/// the database and the dispatcher deliberately holds no connection — the same
/// reason `messaging` retires a push token from a sweep rather than from its
/// handler.
///
/// # It also sends the saved-card charges somebody asked for
///
/// Two passes, charge then settle, in that order — so a card charged on this
/// tick is settled on this tick rather than the next. The charge pass is here
/// for a different reason from the settle pass: charging is an outbound call to
/// a third party, and a request handler that waited on one would hold a
/// database connection for as long as somebody else's server took. Same
/// argument as the ZATCA submission job.
struct SettleGatewayPayments {
    sealing: erp_eventlog::SealingKey,
}

#[async_trait::async_trait]
impl erp_worker::Job for SettleGatewayPayments {
    fn name(&self) -> &'static str {
        "payments.settle"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(payments::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        // A tenant who has enabled the module and configured no provider is the
        // ordinary case on day one, and is not a failure.
        let gateways = payments::configured(db, &self.sealing).await?;
        if gateways.is_empty() {
            return Ok(Activity::Idle);
        }

        let mut resolved = 0;
        for gateway in &gateways {
            resolved += sweep_gateway(db, gateway.as_ref(), &self.sealing).await?;
        }

        // **And the repair, from what is durable.** The first version told
        // the diary only about what *this pass* settled, so a `secure_in`
        // that failed — pool overloaded, a contended stream — was never
        // retried: the payment was no longer pending, no later pass saw it,
        // and the hold-expiry job cancelled a booking somebody had paid for.
        // This asks `payments` for every deposit that ever arrived and asks
        // `booking` which of them it has not heard about, and tells it. Both
        // are projection reads, so this lags a tick behind the fast path
        // above — and `secure_in` is idempotent, so telling a booking twice
        // is nothing.
        if db.has_module(&booking::module_id()) {
            let arrived = {
                let mut conn = db.read().await?;
                payments::settled_advances(&mut conn, REPAIR_BATCH).await?
            };
            if !arrived.is_empty() {
                let ids: Vec<String> = arrived.iter().map(|(r, _)| r.to_string()).collect();
                let untold = {
                    let mut conn = db.read().await?;
                    booking::unsecured_among(&mut conn, &ids).await?
                };
                for (reservation, payment) in &arrived {
                    if untold.contains(reservation) {
                        secure(db, reservation, payment).await?;
                        resolved += 1;
                    }
                }
            }
        }

        // **And tell somebody money moved.** Announced from here rather than
        // from `payments` because a module cannot announce — see the
        // `notifications` crate docs — and here is where a settlement is
        // already known to have happened.
        announce_payments(db).await;

        Ok(if resolved > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// How many settled deposits the repair pass looks back over per tick.
const REPAIR_BATCH: i64 = 200;

// ---------------------------------------------------------------------------
// Announcing: the producers, one shape
// ---------------------------------------------------------------------------

/// How far back an announcer looks.
///
/// Wide enough to survive a worker restart or a rollout, narrow enough that a
/// tenant switching notifications on does not get a week of history in their
/// bell at once. Overlap costs nothing: the notification id is derived from the
/// kind and the subject, so announcing the same thing again writes nothing.
const ANNOUNCE_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(6);

/// How many rows one announcer reads per tick.
const ANNOUNCE_BATCH: i64 = 100;

/// Whether this tenant has a bell to ring at all.
fn announces(db: &erp_control::TenantDb) -> bool {
    db.has_module(&notifications::module_id())
}

fn announce_window(now: erp_types::Timestamp) -> erp_types::Timestamp {
    now - ANNOUNCE_WINDOW
}

/// Turns a batch of finished payments into what to announce about each.
///
/// **Invoices only.** A deposit against a booking has no invoice and its
/// subject would have to be the reservation — which is a different kind, and
/// the diary already shows a paid deposit live (13a). Split out from the job so
/// the mapping that decides *settled* from *failed* is a function a test can
/// call.
fn payments_to_announce(
    finished: Vec<payments::Finished>,
) -> Vec<(notifications::Kind, erp_types::AggregateId)> {
    finished
        .into_iter()
        .filter_map(|payment| {
            let invoice = erp_types::AggregateId::new(payment.invoice?).ok()?;
            let kind = match payment.stage.as_str() {
                "failed" => notifications::Kind::PaymentsFailed,
                _ => notifications::Kind::PaymentsSettled,
            };
            Some((kind, invoice))
        })
        .collect()
}

/// Tells whoever runs the business what money did.
async fn announce_payments(db: &erp_control::TenantDb) {
    if !announces(db) {
        return;
    }
    let now = chrono::Utc::now();
    let finished = {
        let Ok(mut conn) = db.read().await else {
            return;
        };
        match payments::finished_since(&mut conn, announce_window(now), ANNOUNCE_BATCH).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read what payments finished");
                return;
            }
        }
    };

    for kind in [
        notifications::Kind::PaymentsSettled,
        notifications::Kind::PaymentsFailed,
    ] {
        let subjects: Vec<erp_types::AggregateId> = payments_to_announce(finished.clone())
            .into_iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, invoice)| invoice)
            .collect();
        if let Err(error) = notifications::announce_all(db, kind, &subjects, &[], now).await {
            tracing::warn!(kind = kind.as_str(), %error, "could not announce");
        }
    }
}

/// Tells whoever runs the business that ZATCA refused a document.
async fn announce_refusals(db: &erp_control::TenantDb) {
    if !announces(db) {
        return;
    }
    let now = chrono::Utc::now();
    let refused = {
        let Ok(mut conn) = db.read().await else {
            return;
        };
        match tax_sa::refused_since(&mut conn, announce_window(now), ANNOUNCE_BATCH).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read what ZATCA refused");
                return;
            }
        }
    };

    // **The invoice, not the ZATCA document number.** What somebody has to open
    // and correct is the source document.
    let subjects: Vec<erp_types::AggregateId> = refused
        .into_iter()
        .filter_map(|source| erp_types::AggregateId::new(source).ok())
        .collect();
    if let Err(error) =
        notifications::announce_all(db, notifications::Kind::TaxRefused, &subjects, &[], now).await
    {
        tracing::warn!(%error, "could not announce a refusal");
    }
}

/// **Lands what customers said back.**
///
/// A relay posts an inbound message to `POST /v1/hooks/messages`, which
/// verifies it, deduplicates it on the gateway's own id and records it. This is
/// what turns that row into a line in a conversation — in a job rather than in
/// a handler, because a handler is given no database connection and landing a
/// reply is three read models deep: what was last said to that number, whose
/// number it is, and the thread either of those names.
///
/// **No cursor.** Correlation is against the reply's own instant, so the same
/// webhook lands on the same thread however often this runs, and a thread
/// refuses a message id it has already heard. The window may therefore overlap
/// the last one freely.
#[derive(Debug)]
struct LandInboundMessages;

#[async_trait::async_trait]
impl erp_worker::Job for LandInboundMessages {
    fn name(&self) -> &'static str {
        "conversations.inbound"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(conversations::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        let now = chrono::Utc::now();
        let landing = conversations::land(
            db,
            now - ANNOUNCE_WINDOW,
            CORRELATION_WINDOW,
            ANNOUNCE_BATCH,
        )
        .await?;

        if landing.unreadable > 0 {
            // A relay sending something that is not an inbound message is a
            // misconfiguration somebody has to fix, and nothing else would say
            // so.
            tracing::warn!(
                tenant = %db.tenant(),
                count = landing.unreadable,
                "payloads arrived under the messages provider that are not messages"
            );
        }
        if landing.unmatched > 0 {
            tracing::info!(
                tenant = %db.tenant(),
                count = landing.unmatched,
                "replies from numbers nobody on the books has are waiting to be placed"
            );
        }

        Ok(if landing.landed > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// How far back a reply may have been answered.
///
/// A week: long enough that somebody who reads a reminder on Monday and answers
/// on Friday is still answering it, short enough that a message about last
/// month's booking is not taken for one about this one.
const CORRELATION_WINDOW: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// **Tells the counter a booking arrived.**
///
/// A scan rather than a hook in `booking::reserve`, because a module cannot
/// announce: announcing resolves an audience, which is `messaging`'s job, which
/// reads `booking` — so `booking → notifications` would be a cycle. See the
/// `notifications` crate docs.
///
/// No cursor: the window overlaps by design and the derived notification id
/// makes a repeat free.
#[derive(Debug)]
struct AnnounceNewBookings;

#[async_trait::async_trait]
impl erp_worker::Job for AnnounceNewBookings {
    fn name(&self) -> &'static str {
        "notifications.bookings"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(booking::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        if !announces(db) {
            return Ok(Activity::Idle);
        }
        let now = chrono::Utc::now();

        let made = {
            let mut conn = db.read().await?;
            booking::reserved_since(&mut conn, announce_window(now), ANNOUNCE_BATCH).await?
        };
        let subjects: Vec<erp_types::AggregateId> = made
            .into_iter()
            .filter_map(|id| erp_types::AggregateId::new(id).ok())
            .collect();

        let swept = notifications::announce_all(
            db,
            notifications::Kind::BookingReserved,
            &subjects,
            &[],
            now,
        )
        .await?;

        Ok(if swept.announced > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}

/// **Tells somebody a work document is running out.**
///
/// The same facts `WorkDocumentExpiry` reports as a health finding, told to the
/// tenant instead of to an operator's log. **Both stay**, and that is not
/// duplication: the finding is the operator's channel, and a tenant without
/// this module — or without anybody linked to a login — would otherwise be told
/// by nobody at all.
#[derive(Debug)]
struct AnnounceExpiringDocuments;

#[async_trait::async_trait]
impl erp_worker::Job for AnnounceExpiringDocuments {
    fn name(&self) -> &'static str {
        "notifications.documents"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(hr::module_id())
    }

    async fn tick(&self, db: &erp_control::TenantDb) -> Result<Activity, erp_worker::BoxError> {
        if !announces(db) {
            return Ok(Activity::Idle);
        }
        let now = chrono::Utc::now();

        let expiring = {
            let mut conn = db.read().await?;
            hr::expiring(&mut conn, DOCUMENT_WARNING_DAYS, ANNOUNCE_BATCH).await?
        };
        // **One notification per person, not per document.** Somebody whose
        // iqama and licence both lapse this month has one thing to do about it.
        let mut subjects: Vec<erp_types::AggregateId> = expiring
            .into_iter()
            .filter_map(|document| erp_types::AggregateId::new(document.employee).ok())
            .collect();
        subjects.sort();
        subjects.dedup();

        let swept = notifications::announce_all(
            db,
            notifications::Kind::DocumentExpiring,
            &subjects,
            &[],
            now,
        )
        .await?;

        Ok(if swept.announced > 0 {
            Activity::Worked
        } else {
            Activity::Idle
        })
    }
}
/// One provider's passes, in the order they have to run: charge what was asked
/// of a saved card, refund what was asked back, open the checkouts a lender
/// hosts, find the deposits customers paid themselves, and settle everything
/// pending — then tell the diary what settled. Answers how many payments
/// reached an ending.
async fn sweep_gateway(
    db: &erp_control::TenantDb,
    gateway: &dyn payments::Gateway,
    sealing: &erp_eventlog::SealingKey,
) -> Result<usize, erp_worker::BoxError> {
    let mut resolved = 0;
    // **Charge first.** What this starts, the sweep below settles on
    // the same tick.
    let attempted = payments::charge_requested(
        db,
        gateway,
        sealing,
        chrono::Utc::now(),
        PAYMENT_BATCH,
        &by_the_platform(),
    )
    .await?;
    resolved += attempted.started + attempted.refused;

    if let Some(stopped) = &attempted.stopped {
        tracing::warn!(
            tenant = %db.tenant(),
            provider = gateway.provider(),
            error = %stopped,
            started = attempted.started,
            "the saved-card charge pass stopped early; the rest stay requested"
        );
    }

    // **Refunds somebody asked for.** The same shape as a charge: the
    // request was recorded by the route, the outbound call is made
    // here, and the books follow what the gateway confirmed.
    let refunding = payments::refund_requested(
        db,
        gateway,
        chrono::Utc::now(),
        PAYMENT_BATCH,
        &by_the_platform(),
    )
    .await?;
    resolved += refunding.refunded + refunding.refused;
    if let Some(stopped) = &refunding.stopped {
        tracing::warn!(
            tenant = %db.tenant(),
            provider = gateway.provider(),
            error = %stopped,
            refunded = refunding.refunded,
            "the refund pass stopped early; the rest stay awaited"
        );
    }

    // **The checkouts a lender hosts.** A buy-now-pay-later provider
    // has to be told about the order before there is anywhere to send
    // the customer; that is an outbound call, so it is made here and
    // the page it answers with is recorded for the public read to hand
    // on.
    let opened = payments::open_checkouts(
        db,
        gateway,
        chrono::Utc::now(),
        PAYMENT_BATCH,
        &by_the_platform(),
    )
    .await?;
    resolved += opened.started + opened.refused;
    if let Some(stopped) = &opened.stopped {
        tracing::warn!(
            tenant = %db.tenant(),
            provider = gateway.provider(),
            error = %stopped,
            opened = opened.started,
            "the checkout pass stopped early; the rest stay requested"
        );
    }

    // **And the ones the customer pays themselves.** A deposit's
    // payment is created in their browser, against the id this system
    // already chose; nothing here charges it, and the only question is
    // whether they have.
    let awaited = payments::collect_awaited(
        db,
        gateway,
        chrono::Utc::now(),
        PAYMENT_BATCH,
        &by_the_platform(),
    )
    .await?;
    resolved += awaited.started;

    let swept = payments::settle_pending(
        db,
        gateway,
        chrono::Utc::now(),
        PAYMENT_BATCH,
        &by_the_platform(),
    )
    .await?;
    resolved += swept.resolved;

    // **Loudly.** A tenant whose payments are not resolving has
    // customers who have been charged and invoices that say otherwise,
    // and nothing else in the system will say so.
    if let Some(stopped) = &swept.stopped {
        tracing::warn!(
            tenant = %db.tenant(),
            provider = gateway.provider(),
            error = %stopped,
            resolved = swept.resolved,
            "the payment sweep stopped early; the rest stay pending"
        );
    }

    // **The join, and it lives here because neither module may make
    // it.** `payments` cannot name `booking` and `booking` cannot name
    // `payments`: `requires` is a hard AND, so one direction forces a
    // diary on every shop that takes a card and the other forces a
    // gateway on every salon. The worker depends on both, so it is
    // where "this deposit settled, so that slot is paid for" belongs.
    //
    // **Not in the settling transaction**, and it cannot be — they are
    // different modules' aggregates and the money must commit whatever
    // the diary says. What settled on this pass is told at once, so a
    // paid slot is paid in the diary on the same tick.
    for (reservation, payment) in &swept.secured {
        secure(db, reservation, payment).await?;
    }
    Ok(resolved)
}

/// Tells the diary one deposit arrived. Its own transaction, its own failure.
///
/// A failure is logged and swallowed rather than returned: the repair pass
/// will find this pair again on the next tick, and one booking refusing must
/// not stop the rest being told. **Loudly**, because a customer has paid for a
/// slot the diary does not yet know is paid for.
async fn secure(
    db: &erp_control::TenantDb,
    reservation: &erp_types::AggregateId,
    payment: &erp_types::AggregateId,
) -> Result<(), erp_worker::BoxError> {
    let mut tx = db.begin().await?;
    match booking::secure_in(
        &mut tx,
        reservation,
        payment,
        chrono::Utc::now(),
        &by_the_platform(),
    )
    .await
    {
        Ok(_) => tx.commit().await?,
        Err(e) => {
            tx.rollback().await?;
            tracing::error!(
                tenant = %db.tenant(),
                %reservation,
                %payment,
                error = %e,
                "a settled deposit could not be recorded against its booking; will retry"
            );
        }
    }
    Ok(())
}

/// Which ZATCA a tenant onboarded into.
///
/// `None` when they have not onboarded at all, which is most tenants most of
/// the time and is why this is not an error.
///
/// Read from the projection rather than the log (L7). The stored value is
/// re-parsed rather than trusted: it was written by this system, but a value
/// that no longer names an environment is a bad migration, and law L6 says that
/// stops rather than degrades.
async fn zatca_environment(
    db: &erp_control::TenantDb,
) -> Result<Option<tax_sa::zatca::csr::Environment>, erp_worker::BoxError> {
    let mut conn = db.read().await?;
    let onboarded = tax_sa::onboarding(&mut conn).await?;
    drop(conn);

    onboarded
        .map(|o| o.environment.parse().map_err(erp_worker::BoxError::from))
        .transpose()
}

/// **Every module's projections, in one list.**
///
/// A function rather than a chain of `with_job` calls so that a test can look at
/// it. A module missing from here is the worst omission this system has: the
/// events still commit, the ledger still balances, and the module's read models
/// stay **permanently empty** — no bill list, no input tax, and a VAT return
/// quietly under-reporting what can be reclaimed. Nothing else in the suite
/// notices, which was checked by removing one and watching everything pass.
#[expect(
    clippy::too_many_lines,
    reason = "one entry per module, in a list; splitting it would hide that this \
              is the complete set `every_module_has_a_projection_job` checks"
)]
fn module_jobs(signals: Option<&Arc<dyn erp_worker::Signals>>) -> Vec<Arc<dyn erp_worker::Job>> {
    vec![
        Arc::new(
            ProjectionJob::<booking::Booking>::new(
                booking::projections(),
                Arc::new(booking::upcasters().clone()),
                200,
            )
            .for_module(booking::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<conversations::Conversations>::new(
                conversations::projections(),
                Arc::new(conversations::upcasters().clone()),
                200,
            )
            .for_module(conversations::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<notifications::Notifications>::new(
                notifications::projections(),
                Arc::new(notifications::upcasters().clone()),
                200,
            )
            .for_module(notifications::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<inventory::Inventory>::new(
                inventory::projections(),
                Arc::new(inventory::upcasters().clone()),
                200,
            )
            .for_module(inventory::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<crm::Crm>::new(
                crm::projections(),
                Arc::new(crm::upcasters().clone()),
                200,
            )
            .for_module(crm::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<payments::Payments>::new(
                payments::projections(),
                Arc::new(payments::upcasters().clone()),
                200,
            )
            .for_module(payments::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<ledger::Ledger>::new(
                ledger::projections(),
                Arc::new(ledger::upcasters().clone()),
                200,
            )
            .for_module(ledger::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<sales::Sales>::new(
                sales::projections(),
                Arc::new(sales::upcasters().clone()),
                200,
            )
            .for_module(sales::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<prepaid::Prepaid>::new(
                prepaid::projections(),
                Arc::new(prepaid::upcasters().clone()),
                200,
            )
            .for_module(prepaid::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<payroll::Payroll>::new(
                payroll::projections(),
                Arc::new(payroll::upcasters().clone()),
                200,
            )
            .for_module(payroll::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<hr::Hr>::new(hr::projections(), Arc::new(hr::upcasters().clone()), 200)
                .for_module(hr::module_id())
                .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<branches::Branches>::new(
                branches::projections(),
                Arc::new(branches::upcasters().clone()),
                200,
            )
            .for_module(branches::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<pos::Pos>::new(
                pos::projections(),
                Arc::new(pos::upcasters().clone()),
                200,
            )
            .for_module(pos::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<purchases::Purchases>::new(
                purchases::projections(),
                Arc::new(purchases::upcasters().clone()),
                200,
            )
            .for_module(purchases::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<tax_sa::TaxSa>::new(
                tax_sa::projections(),
                Arc::new(tax_sa::upcasters().clone()),
                200,
            )
            .for_module(tax_sa::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<files::Files>::new(
                files::projections(),
                Arc::new(files::upcasters().clone()),
                200,
            )
            .for_module(files::module_id())
            .signalling(signals.cloned()),
        ),
        Arc::new(
            ProjectionJob::<reports::Reports>::new(
                reports::projections(),
                Arc::new(reports::upcasters().clone()),
                200,
            )
            .for_module(reports::module_id())
            .signalling(signals.cloned()),
        ),
    ]
}

/// No bill may have been paid more than it was for.
///
/// The mirror of [`NoOverpaidInvoice`], and unreachable the same way. Two
/// invariants rather than one because a tenant may have either module without
/// the other, and `module()` is what stops a tenant being checked for something
/// they declined.
struct NoOverpaidBill;

#[async_trait::async_trait]
impl Invariant for NoOverpaidBill {
    fn name(&self) -> &'static str {
        "no_overpaid_bill"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(purchases::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        let mut conn = db.acquire().await?;
        Ok(purchases::overpaid(&mut conn)
            .await?
            .into_iter()
            .map(|b| {
                Finding::new(
                    "no_overpaid_bill",
                    format!(
                        "bill {} is for {} and has been paid {}",
                        b.bill, b.gross, b.paid
                    ),
                )
            })
            .collect())
    }
}

/// No invoice may have taken more money than it asked for.
///
/// Unreachable through `sales::record_payment`, which refuses an overpayment
/// against the invoice's own state — so a finding here means the pipeline is
/// broken, in the same way a non-zero trial balance does.
struct NoOverpaidInvoice;

#[async_trait::async_trait]
impl Invariant for NoOverpaidInvoice {
    fn name(&self) -> &'static str {
        "no_overpaid_invoice"
    }

    fn module(&self) -> Option<ModuleId> {
        Some(sales::module_id())
    }

    async fn check(
        &self,
        db: &erp_control::TenantDb,
    ) -> Result<Vec<Finding>, erp_worker::BoxError> {
        let mut conn = db.acquire().await?;
        Ok(sales::overpaid(&mut conn)
            .await?
            .into_iter()
            .map(|i| {
                Finding::new(
                    "no_overpaid_invoice",
                    format!(
                        "invoice {} is for {} and has taken {}",
                        i.invoice, i.gross, i.paid
                    ),
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnnounceExpiringStock, StockBellRings, bell_grace, certificate_time, describe, going_off,
        module_jobs, payments_to_announce, read_before, stock_disagreements, unannounced,
        who_may_write_off, zatca_jobs,
    };
    use std::collections::BTreeSet;

    fn sar(minor: i64) -> erp_types::Money {
        erp_types::Money::from_minor(
            minor,
            erp_types::CurrencyCode::new("SAR").expect("a real code"),
        )
    }

    /// **The blind spot, and the reason this comparison is not a filter.**
    ///
    /// `value_on_hand` groups rows in `proj_inventory.stock_item`, so a tenant
    /// whose stock account holds money and whose shelves are empty has nothing
    /// to iterate — and the finding's own wording names that case. Driving the
    /// comparison from both sides is what reports it.
    #[test]
    fn a_stock_account_with_no_shelf_behind_it_is_a_finding() {
        let found = stock_disagreements(&[], Some(sar(70_000)));
        assert_eq!(
            found,
            vec![(sar(0), Some(sar(70_000)))],
            "a debit that never reached a shelf has to be reported, and there is no row to hang it on"
        );

        assert!(
            stock_disagreements(&[], Some(sar(0))).is_empty(),
            "a tenant with no stock and no balance is not unhealthy"
        );
        assert!(
            stock_disagreements(&[], None).is_empty(),
            "nor is one who never opened the account"
        );
    }

    /// A shelf in a currency the account is not kept in cannot be compared, so
    /// it is **reported** rather than filtered away: it is stock somebody has
    /// to be told about.
    #[test]
    fn a_shelf_the_account_cannot_be_compared_with_is_still_reported() {
        let usd = erp_types::Money::from_minor(
            5_000,
            erp_types::CurrencyCode::new("USD").expect("a real code"),
        );
        let found = stock_disagreements(&[sar(1_200), usd], Some(sar(1_200)));
        assert_eq!(
            found,
            vec![(usd, None)],
            "the SAR side agrees; the USD side has nothing to agree with"
        );
    }

    fn day(literal: &str) -> chrono::NaiveDate {
        literal.parse().expect("a date")
    }

    fn lot(id: &str, expires_on: Option<&str>, remaining: i64) -> inventory::LotRow {
        inventory::LotRow {
            id: id.to_owned(),
            product: "MILK".to_owned(),
            name: None,
            branch: None,
            code: None,
            expires_on: expires_on.map(day),
            quantity: 12,
            remaining,
            value: remaining * 500,
            currency: "SAR".to_owned(),
            received_at: chrono::DateTime::UNIX_EPOCH,
            recorded_at: chrono::DateTime::UNIX_EPOCH,
            position: 1,
            serials: Vec::new(),
        }
    }

    fn ids(lots: &[&inventory::LotRow]) -> Vec<String> {
        lots.iter().map(|lot| lot.id.clone()).collect()
    }

    /// **The expiry warning's window, at both ends.** Thirty days from the
    /// 10th of April: yesterday's batch is past its date, today's is still good
    /// today and warns, the 10th of May is the last day inside, and the 11th is
    /// outside it.
    #[test]
    fn a_lot_warns_inside_the_window_and_not_outside_it() {
        let today = day("2026-04-10");
        let window = inventory::ExpiryWindow::new(30).expect("a window");
        assert_eq!(window.warns_until(today), Some(day("2026-05-10")));
        assert_eq!(
            read_before(today, window),
            Some(day("2026-05-11")),
            "the listing is strictly before, so it is asked for the day after the last that warns"
        );

        let lots = [
            lot("yesterday", Some("2026-04-09"), 4),
            lot("today", Some("2026-04-10"), 4),
            lot("last-day", Some("2026-05-10"), 4),
            lot("outside", Some("2026-05-11"), 4),
        ];
        let (gone, soon) = going_off(&lots, today, window);
        assert_eq!(ids(&gone), ["yesterday"], "past its date and on the shelf");
        assert_eq!(ids(&soon), ["today", "last-day"], "inside the window");

        let (gone, soon) = going_off(&lots, today, inventory::ExpiryWindow::new(0).expect("none"));
        assert_eq!(
            ids(&gone),
            ["yesterday"],
            "a window of none still reports what has gone"
        );
        assert_eq!(ids(&soon), ["today"], "and what goes today");
    }

    /// **Nothing on an undated lot spoils, and nothing is left on an emptied
    /// one** — neither warns however wide the window, and an emptied lot does
    /// not warn even with its date long gone.
    #[test]
    fn an_undated_or_emptied_lot_never_warns() {
        let today = day("2026-04-10");
        let widest = inventory::ExpiryWindow::new(inventory::expiry::MAX_DAYS).expect("a window");
        let lots = [
            lot("undated", None, 4),
            lot("emptied-and-gone", Some("2026-01-01"), 0),
            lot("emptied-and-soon", Some("2026-04-20"), 0),
        ];
        let (gone, soon) = going_off(&lots, today, widest);
        assert!(gone.is_empty(), "{:?}", ids(&gone));
        assert!(soon.is_empty(), "{:?}", ids(&soon));
    }

    fn member(
        role: erp_control::Role,
        module_roles: &[(&str, erp_control::Role)],
        suspended: bool,
    ) -> erp_control::Member {
        erp_control::Member {
            identity: erp_types::IdentityId::new(),
            module_roles: module_roles
                .iter()
                .map(|(module, role)| (erp_types::ModuleId::new(*module).expect("a module"), *role))
                .collect(),
            handle: None,
            role,
            branches: Vec::new(),
            since: chrono::DateTime::UNIX_EPOCH,
            suspended,
        }
    }

    /// **Who is told a lot is going off is who may write it off** — the
    /// write-off route's own decision (`PERMISSIONS`' `write_off_stock` row:
    /// owner, accountant and clerk), asked per member. The role that applies in
    /// `inventory` counts over the tenant-wide one and a role elsewhere does
    /// not; a suspended login is nobody; and a limit the owner wrote narrows it
    /// at the branch the lot is at, judged on the role in `inventory`.
    #[test]
    fn whoever_may_write_stock_off_is_told_and_nobody_else() {
        use erp_control::Role::{Accountant, Clerk, Owner, Viewer};
        let names = [
            "owner",
            "accountant",
            "clerk",
            "viewer",
            "a clerk who only views stock",
            "a viewer who is a clerk for stock",
            "a clerk who only views sales",
            "a suspended owner",
        ];
        let members = vec![
            member(Owner, &[], false),
            member(Accountant, &[], false),
            member(Clerk, &[], false),
            member(Viewer, &[], false),
            member(Clerk, &[("inventory", Viewer)], false),
            member(Viewer, &[("inventory", Clerk)], false),
            member(Clerk, &[("sales", Viewer)], false),
            member(Owner, &[], true),
        ];
        let told = |limits: &erp_tenant::Limits, branch: Option<&str>| {
            let logins = who_may_write_off(&members, limits, branch);
            members
                .iter()
                .zip(names)
                .filter(|(m, _)| logins.contains(&m.identity.to_string()))
                .map(|(_, name)| name)
                .collect::<Vec<_>>()
        };

        let may = [
            "owner",
            "accountant",
            "clerk",
            "a viewer who is a clerk for stock",
            "a clerk who only views sales",
        ];
        let unlimited = erp_tenant::Limits::default();
        assert_eq!(told(&unlimited, Some("BR-MALAZ")), may);
        assert_eq!(told(&unlimited, None), may);

        let no_clerks_at_malaz: erp_tenant::Limits = serde_json::from_value(serde_json::json!([{
            "name": "no clerks at Malaz",
            "when": { "when": "all", "of": [
                { "when": "is", "fact": "role", "op": "eq", "value": { "type": "text", "of": "clerk" } },
                { "when": "is", "fact": "branch", "op": "eq", "value": { "type": "text", "of": "BR-MALAZ" } },
            ] },
            "then": "refuse",
        }]))
        .expect("a limit");
        assert_eq!(
            told(&no_clerks_at_malaz, Some("BR-MALAZ")),
            ["owner", "accountant"],
            "the limit refuses whoever is a clerk in inventory, at Malaz"
        );
        assert_eq!(
            told(&no_clerks_at_malaz, Some("BR-OLAYA")),
            may,
            "and Olaya is not Malaz"
        );
    }

    /// **Silent while the bell works, and a finding when it did not ring.**
    ///
    /// Thirty days from the 10th of April in Riyadh: a lot going off on the
    /// 20th has been due its notification since the 21st of March and one gone
    /// on the 9th its second since the 10th, both long past the grace. Each is a
    /// finding until the notification its own state calls for exists — the one
    /// the other state calls for does not count — and an undated, an emptied or
    /// a far-off lot is never one.
    #[test]
    fn a_lot_due_a_notification_is_a_finding_until_its_own_one_exists() {
        let calendar = erp_types::Calendar::default();
        let now: erp_types::Timestamp = "2026-04-10T09:00:00Z".parse().expect("an instant");
        let window = inventory::ExpiryWindow::new(30).expect("a window");
        let grace = bell_grace(&erp_control::WorkSchedule::default());
        let lots = [
            lot("soon", Some("2026-04-20"), 4),
            lot("gone", Some("2026-04-09"), 4),
            lot("undated", None, 4),
            lot("emptied", Some("2026-04-15"), 0),
            lot("outside", Some("2026-06-01"), 4),
        ];

        let nobody = |_: notifications::Kind, _: &str| false;
        assert_eq!(
            ids(&unannounced(
                &lots, nobody, calendar, now, window, None, grace
            )),
            ["gone", "soon"]
        );

        let rang = |kind: notifications::Kind, id: &str| {
            matches!(
                (kind, id),
                (notifications::Kind::StockExpiring, "soon")
                    | (notifications::Kind::StockExpired, "gone")
            )
        };
        assert!(unannounced(&lots, rang, calendar, now, window, None, grace).is_empty());

        let crossed = |kind: notifications::Kind, id: &str| {
            matches!(
                (kind, id),
                (notifications::Kind::StockExpired, "soon")
                    | (notifications::Kind::StockExpiring, "gone")
            )
        };
        assert_eq!(
            ids(&unannounced(
                &lots, crossed, calendar, now, window, None, grace
            )),
            ["gone", "soon"],
            "being told a lot was going off is not being told it has gone"
        );
    }

    /// **The ordinary gap is silent.** A lot is due from the start of the day
    /// its window reaches it, from when it was recorded, or from when the window
    /// was last set, whichever is latest, and only one due for longer than the
    /// grace is a finding. The grace is the longest a quiet tenant waits for the
    /// visit that announces, and one health interval.
    #[test]
    fn a_lot_is_silent_inside_the_grace_from_whenever_it_fell_due() {
        let calendar = erp_types::Calendar::default();
        let window = inventory::ExpiryWindow::new(30).expect("a window");
        let grace = bell_grace(&erp_control::WorkSchedule::default());
        assert_eq!(
            grace,
            chrono::TimeDelta::hours(8) + chrono::TimeDelta::minutes(5)
                - chrono::TimeDelta::milliseconds(1),
            "six hours of ceiling, two of jitter, five minutes of health interval"
        );
        let nobody = |_: notifications::Kind, _: &str| false;

        // The 10th of May enters a thirty-day window on the 10th of April.
        let lots = [lot("may-10", Some("2026-05-10"), 4)];
        let entered = calendar.start_of(day("2026-04-10"));
        let inside = entered + grace;
        let past = inside + chrono::TimeDelta::minutes(1);
        assert!(
            unannounced(&lots, nobody, calendar, inside, window, None, grace).is_empty(),
            "the ordinary wait for a visit"
        );
        assert_eq!(
            ids(&unannounced(
                &lots, nobody, calendar, past, window, None, grace
            )),
            ["may-10"]
        );

        // Delivered an hour ago already inside its window: due from then.
        let mut delivered = lot("delivered", Some("2026-04-20"), 4);
        delivered.recorded_at = past - chrono::TimeDelta::hours(1);
        assert!(
            unannounced(
                std::slice::from_ref(&delivered),
                nobody,
                calendar,
                past,
                window,
                None,
                grace
            )
            .is_empty()
        );

        // A window set an hour ago reaches the lot only from then…
        let widened = Some(past - chrono::TimeDelta::hours(1));
        assert!(unannounced(&lots, nobody, calendar, past, window, widened, grace).is_empty());
        // …and does not move when a lot passed its date.
        let gone = [lot("gone", Some("2026-04-08"), 4)];
        assert_eq!(
            ids(&unannounced(
                &gone, nobody, calendar, past, window, widened, grace
            )),
            ["gone"]
        );
    }

    static CONTROL: erp_testkit::Schema =
        erp_testkit::Schema::migrations("control", &erp_control::MIGRATIONS);
    static TENANT: erp_testkit::Schema =
        erp_testkit::Schema::migrations("tenant", &erp_eventlog::MIGRATIONS);

    const MILK: &str = "9f2a6d0c-11d0-7dec-a765-00a0c91e6bf7";
    const BEANS: &str = "f81d4fae-7dec-11d0-a765-00a0c91e6bf6";
    const OLAYA: &str = "BR-OLAYA";

    /// **A café with a shelf and a bell**, for what a unit test cannot reach:
    /// what the announcer reads and whom it tells, and what the check reads
    /// back. Built through the modules' own commands.
    struct Shop {
        control: std::sync::Arc<erp_control::ControlPlane>,
        tenant: erp_types::TenantId,
        db: erp_control::TenantDb,
        pool: sqlx::PgPool,
        database: String,
        _control_db: erp_testkit::TestDb,
    }

    impl Shop {
        #[expect(
            clippy::too_many_lines,
            reason = "a whole café — a control plane, a tenant, four read models, a chart, a \
                      branch and two products — built once for both tests"
        )]
        async fn open(slug: &str) -> Self {
            use erp_control::Actor;
            let control_db = erp_testkit::Template::get(&CONTROL)
                .await
                .expect("control template builds")
                .fresh()
                .await
                .expect("control database clones");
            let clusters = erp_control::ClusterRegistry::new()
                .with_url("primary", &erp_testkit::database_url())
                .expect("the test database URL parses");
            let control = std::sync::Arc::new(erp_control::ControlPlane::new(
                control_db.pool().clone(),
                erp_control::TenantPools::new(clusters, erp_control::PoolConfig::default()),
            ));
            control
                .register_cluster(
                    "primary",
                    "ERP_CLUSTER_PRIMARY_URL",
                    None,
                    10_000,
                    10_000,
                    Actor::system(),
                )
                .await
                .expect("cluster registers");
            let tenant = control
                .register_tenant_on(slug, "Café", "primary", Actor::system())
                .await
                .expect("tenant registers");
            erp_testkit::create_named_database(&tenant.database_name, &TENANT)
                .await
                .expect("tenant database is created");
            control
                .activate_tenant(tenant.id, Actor::system())
                .await
                .expect("tenant activates");
            for module in [inventory::module_id(), notifications::module_id()] {
                control
                    .enable_module(tenant.id, &module, Actor::system())
                    .await
                    .expect("enables");
            }
            let db = control
                .enter_for_maintenance(tenant.id)
                .await
                .expect("maintenance entry");
            {
                let mut conn = db.acquire().await.expect("connection");
                inventory::install(&mut conn).await.expect("inventory");
                erp_projection::ensure_group_schema::<inventory::Inventory>(&mut conn)
                    .await
                    .expect("i");
                ledger::install(&mut conn).await.expect("ledger");
                erp_projection::ensure_group_schema::<ledger::Ledger>(&mut conn)
                    .await
                    .expect("l");
                branches::install(&mut conn).await.expect("branches");
                erp_projection::ensure_group_schema::<branches::Branches>(&mut conn)
                    .await
                    .expect("b");
                notifications::install(&mut conn).await.expect("bell");
                erp_projection::ensure_group_schema::<notifications::Notifications>(&mut conn)
                    .await
                    .expect("n");
            }
            ledger::install_chart(
                &db,
                ledger::chart("services").expect("the services chart ships"),
                erp_types::CurrencyCode::new("SAR").expect("a currency"),
                erp_i18n::Locale::English,
                &erp_eventlog::Metadata::default(),
            )
            .await
            .expect("the chart installs");
            branches::open_branch(
                &db,
                &code(OLAYA),
                &branches::Details {
                    name: "العليا".to_owned(),
                    name_latin: None,
                    address: branches::Address {
                        street: "طريق الملك فهد".to_owned(),
                        building: None,
                        district: None,
                        city: "الرياض".to_owned(),
                        postal_code: None,
                        country: "SA".to_owned(),
                    },
                },
                chrono::Utc::now() - chrono::TimeDelta::days(30),
                &erp_eventlog::Metadata::default(),
            )
            .await
            .expect("the branch opens");
            for (product, name, tracking) in [
                (MILK, "حليب طازج", inventory::Tracking::Lot),
                (BEANS, "بن", inventory::Tracking::None),
            ] {
                inventory::declare(
                    &db,
                    &code(product),
                    name,
                    "piece",
                    tracking,
                    chrono::Utc::now() - chrono::TimeDelta::days(30),
                    &erp_eventlog::Metadata::default(),
                )
                .await
                .expect("declares");
            }

            let url = erp_testkit::database_url();
            let base = url.rsplit_once('/').map_or(url.as_str(), |(h, _)| h);
            let pool = sqlx::PgPool::connect(&format!("{base}/{}", tenant.database_name))
                .await
                .expect("connects");
            Self {
                control,
                tenant: tenant.id,
                db,
                pool,
                database: tenant.database_name,
                _control_db: control_db,
            }
        }

        async fn member(&self, handle: &str, role: erp_control::Role) -> String {
            self.control
                .add_member(
                    self.tenant,
                    format!("{handle}@cafe.test"),
                    "hunter2hunter2".to_owned(),
                    role,
                    erp_control::Actor::system(),
                )
                .await
                .expect("a member")
                .to_string()
        }

        /// A delivery of four at Olaya, carrying a batch and its date when given,
        /// and saying it arrived `at`.
        async fn receive(
            &self,
            product: &str,
            reference: &str,
            batch: Option<(&str, chrono::NaiveDate)>,
            at: erp_types::Timestamp,
        ) -> String {
            self.receive_at(Some(OLAYA), product, reference, batch, at)
                .await
        }

        /// …at `branch`, or at no branch at all.
        async fn receive_at(
            &self,
            branch: Option<&str>,
            product: &str,
            reference: &str,
            batch: Option<(&str, chrono::NaiveDate)>,
            at: erp_types::Timestamp,
        ) -> String {
            let metadata = branch.map_or_else(erp_eventlog::Metadata::default, |branch| {
                erp_eventlog::Metadata::default().at_branch(branch)
            });
            inventory::receive(
                &self.db,
                &code(product),
                &inventory::Receipt {
                    quantity: 4,
                    value: sar(2_000),
                    code: batch.map(|(code, _)| code.to_owned()),
                    expires_on: batch.map(|(_, day)| day),
                    serials: Vec::new(),
                    reference: reference.to_owned(),
                    at,
                },
                &metadata,
            )
            .await
            .expect("receives");
            inventory::lot_of(
                &inventory::stock_id(&code(product), branch).expect("a shelf"),
                reference,
            )
        }

        async fn project(&self) {
            self.projecting(true).await;
        }

        /// …or everything but `branches`, which is a read model behind.
        async fn projecting(&self, with_branches: bool) {
            macro_rules! run {
                ($module:ident, $group:ty) => {{
                    let owned = $module::projections();
                    let refs: Vec<&dyn erp_projection::Projection<Group = $group>> =
                        owned.iter().map(AsRef::as_ref).collect();
                    erp_projection::run_to_head::<$group>(
                        &self.pool,
                        &refs,
                        $module::upcasters(),
                        200,
                    )
                    .await
                    .expect("projects");
                }};
            }
            run!(inventory, inventory::Inventory);
            // What a notification about a lot says of where it is.
            if with_branches {
                run!(branches, branches::Branches);
            }
            run!(notifications, notifications::Notifications);
        }

        async fn bell(&self, login: &str) -> Vec<notifications::InboxRow> {
            let mut conn = self.db.acquire().await.expect("connection");
            notifications::inbox(&mut conn, login, false, 50, None)
                .await
                .expect("reads")
                .items
        }

        async fn cleanup(self) {
            drop(self.db);
            self.pool.close().await;
            let _ = erp_testkit::drop_named_database(&self.database).await;
        }
    }

    /// **A lot going off is told once, to whoever may write it off**, in both
    /// languages, naming the product, the batch, the branch and the date — and
    /// nothing is told about a lot outside the window, an undated one or one
    /// written off to nothing. A second run raises nothing; a wider window
    /// reaches the next lot without telling the first again; and the bell check
    /// is silent, because the bell rang.
    #[tokio::test]
    async fn a_lot_going_off_is_told_once_to_whoever_may_write_it_off() {
        use erp_worker::{Activity, Invariant, Job};
        let shop = Shop::open("stock-bell").await;
        let owner = shop.member("owner", erp_control::Role::Owner).await;
        let clerk = shop.member("clerk", erp_control::Role::Clerk).await;
        let viewer = shop.member("viewer", erp_control::Role::Viewer).await;

        let today = erp_types::Calendar::default().day(chrono::Utc::now());
        let in_days = |n: i64| today + chrono::TimeDelta::days(n);
        let now = chrono::Utc::now();
        let soon = shop
            .receive(MILK, "dn-soon", Some(("B-SOON", in_days(10))), now)
            .await;
        let later = shop
            .receive(MILK, "dn-later", Some(("B-LATER", in_days(60))), now)
            .await;
        let emptied = shop
            .receive(MILK, "dn-emptied", Some(("B-EMPTIED", in_days(5))), now)
            .await;
        shop.receive(BEANS, "dn-beans", None, now).await;
        inventory::write_off(
            &shop.db,
            &code(MILK),
            &inventory::WriteOff {
                reason: inventory::Reason::Damaged,
                quantity: Some(4),
                lot: Some(emptied),
                serials: Vec::new(),
                reference: "wo-emptied".to_owned(),
                at: chrono::Utc::now(),
            },
            &erp_eventlog::Metadata::default().at_branch(OLAYA),
        )
        .await
        .expect("writes the lot off to nothing");
        shop.project().await;

        let job = AnnounceExpiringStock {
            control: std::sync::Arc::clone(&shop.control),
        };
        assert_eq!(job.tick(&shop.db).await.expect("ticks"), Activity::Worked);
        shop.project().await;
        assert_eq!(
            job.tick(&shop.db).await.expect("ticks again"),
            Activity::Idle,
            "a second run raised something"
        );
        shop.project().await;

        for login in [&owner, &clerk] {
            let bell = shop.bell(login).await;
            assert_eq!(bell.len(), 1, "{bell:?}");
            assert_eq!(bell[0].kind, "stock_expiring");
            assert_eq!(bell[0].subject_id, soon);
            for (locale, says) in [("en", "good until"), ("ar", "صالحة حتى")] {
                let body = &bell[0].wording[locale].body;
                for part in [
                    "حليب طازج",
                    "B-SOON",
                    "العليا",
                    &in_days(10).to_string(),
                    says,
                ] {
                    assert!(body.contains(part), "{locale} does not say {part}: {body}");
                }
            }
        }
        assert!(
            shop.bell(&viewer).await.is_empty(),
            "a viewer cannot write stock off, and was told"
        );

        // Ninety days reaches the later batch, and says nothing about the first
        // again.
        {
            let mut conn = shop.db.acquire().await.expect("connection");
            erp_eventlog::configuration::set(
                &mut conn,
                inventory::ExpiryWindow::KEY,
                &inventory::ExpiryWindow::new(90).expect("a window"),
                None,
                None,
            )
            .await
            .expect("widens the window");
        }
        assert_eq!(job.tick(&shop.db).await.expect("ticks"), Activity::Worked);
        shop.project().await;
        let bell = shop.bell(&owner).await;
        let about: Vec<&str> = bell.iter().map(|row| row.subject_id.as_str()).collect();
        assert_eq!(about.len(), 2, "{about:?}");
        assert!(about.contains(&soon.as_str()) && about.contains(&later.as_str()));

        let strict = StockBellRings {
            grace: chrono::TimeDelta::zero(),
        };
        assert!(
            strict.check(&shop.db).await.expect("checks").is_empty(),
            "the bell rang for every lot due, and the check says otherwise"
        );

        shop.cleanup().await;
    }

    /// **A whole sentence, wherever the lot is.** A lot at no branch — a
    /// business with one shelf and no business name set — is told without a
    /// branch, and one at a branch the `branches` read model has not caught up
    /// with is told under the key it was received at. Neither says
    /// `{{ branch.name }}`, which a notification, frozen when announced, would
    /// say for ever.
    #[tokio::test]
    async fn a_lot_at_no_branch_or_an_unknown_one_is_told_in_a_whole_sentence() {
        use erp_worker::{Activity, Job};
        let shop = Shop::open("stock-bell-places").await;
        let owner = shop.member("owner", erp_control::Role::Owner).await;
        let now = chrono::Utc::now();
        let good_until = erp_types::Calendar::default().day(now) + chrono::TimeDelta::days(10);
        let home = shop
            .receive_at(None, MILK, "dn-home", Some(("B-HOME", good_until)), now)
            .await;
        let olaya = shop
            .receive(MILK, "dn-olaya", Some(("B-OLAYA", good_until)), now)
            .await;
        shop.projecting(false).await;

        let job = AnnounceExpiringStock {
            control: std::sync::Arc::clone(&shop.control),
        };
        assert_eq!(job.tick(&shop.db).await.expect("ticks"), Activity::Worked);
        shop.projecting(false).await;

        let bell = shop.bell(&owner).await;
        assert_eq!(bell.len(), 2, "{bell:?}");
        for (lot, batch, place) in [(&home, "B-HOME", None), (&olaya, "B-OLAYA", Some(OLAYA))] {
            let row = bell
                .iter()
                .find(|row| &row.subject_id == lot)
                .unwrap_or_else(|| panic!("nobody was told about {lot}: {bell:?}"));
            for locale in ["en", "ar"] {
                let said = &row.wording[locale];
                for text in [&said.title, &said.body] {
                    assert!(!text.contains("{{"), "{locale} leaves a hole: {text}");
                }
                let date = good_until.to_string();
                for part in ["حليب طازج", batch, date.as_str()].into_iter().chain(place) {
                    assert!(
                        said.body.contains(part),
                        "{locale} does not say {part}: {}",
                        said.body
                    );
                }
            }
        }

        shop.cleanup().await;
    }

    /// **A bell that did not ring is a finding**, read out of the two read
    /// models the check compares: a lot past its date with nothing announced,
    /// once the grace is behind it — and silent inside the grace, and once its
    /// own notification exists.
    #[tokio::test]
    async fn a_lot_past_its_date_nobody_was_told_about_is_a_finding() {
        use erp_worker::{Activity, Invariant, Job};
        let shop = Shop::open("stock-bell-silent").await;
        let today = erp_types::Calendar::default().day(chrono::Utc::now());
        // Entered a moment ago, two days past its date, and saying it arrived
        // three days ago: due from when it was recorded, not from the day it
        // went off, nor from the day it claims to have come in.
        let gone = shop
            .receive(
                MILK,
                "dn-gone",
                Some(("B-GONE", today - chrono::TimeDelta::days(2))),
                chrono::Utc::now() - chrono::TimeDelta::days(3),
            )
            .await;
        shop.project().await;

        let shipped = StockBellRings {
            grace: bell_grace(&erp_control::WorkSchedule::default()),
        };
        assert!(
            shipped.check(&shop.db).await.expect("checks").is_empty(),
            "recorded a moment ago, however long ago it says it arrived, so still inside the grace"
        );
        let strict = StockBellRings {
            grace: chrono::TimeDelta::zero(),
        };
        let found = strict.check(&shop.db).await.expect("checks");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].check, "stock_bell");
        assert!(found[0].detail.contains(&gone), "{}", found[0].detail);
        assert!(
            !found[0].detail.contains("B-GONE"),
            "an operator is told which lot, not what is on it: {}",
            found[0].detail
        );

        let owner = shop.member("owner", erp_control::Role::Owner).await;
        let job = AnnounceExpiringStock {
            control: std::sync::Arc::clone(&shop.control),
        };
        assert_eq!(job.tick(&shop.db).await.expect("ticks"), Activity::Worked);
        shop.project().await;
        let bell = shop.bell(&owner).await;
        assert_eq!(bell.len(), 1, "{bell:?}");
        assert_eq!(
            bell[0].kind, "stock_expired",
            "a lot past its date is told it has gone"
        );
        assert!(
            strict.check(&shop.db).await.expect("checks").is_empty(),
            "told, and still a finding"
        );

        shop.cleanup().await;
    }

    fn finished(id: &str, stage: &str, invoice: Option<&str>) -> payments::Finished {
        payments::Finished {
            id: id.to_owned(),
            stage: stage.to_owned(),
            invoice: invoice.map(str::to_owned),
        }
    }

    /// **"Your money arrived" and "your money did not" are different sentences.**
    ///
    /// One `stage` column decides which, and getting it backwards would tell a
    /// business a failed payment settled — the one mistake in this producer
    /// that nobody would notice until they reconciled.
    #[test]
    fn a_failed_payment_is_not_announced_as_a_settled_one() {
        let announced = payments_to_announce(vec![
            finished("PAY-1", "settled", Some("INV-1")),
            finished("PAY-2", "retained", Some("INV-2")),
            finished("PAY-3", "failed", Some("INV-3")),
        ]);

        assert_eq!(
            announced,
            vec![
                (notifications::Kind::PaymentsSettled, code("INV-1")),
                (notifications::Kind::PaymentsSettled, code("INV-2")),
                (notifications::Kind::PaymentsFailed, code("INV-3")),
            ]
        );
    }

    /// **A deposit has no invoice, and is nobody's bell.**
    ///
    /// Its subject would have to be the reservation, which is a different kind
    /// — and the diary already shows a paid deposit live. Announcing it under
    /// an invoice's topic would resolve its bindings against an invoice that
    /// does not exist.
    #[test]
    fn a_deposit_against_a_booking_is_not_announced_as_an_invoice() {
        assert!(payments_to_announce(vec![finished("PAY-1", "settled", None)]).is_empty());
    }

    fn code(id: &str) -> erp_types::AggregateId {
        erp_types::AggregateId::new(id).expect("a valid id")
    }

    fn document(name: &str, days: i32) -> hr::Expiring {
        hr::Expiring {
            employee: name.to_owned(),
            name: name.to_owned(),
            branch: None,
            kind: "identity".to_owned(),
            number: "X".to_owned(),
            expires_on: chrono::NaiveDate::from_ymd_opt(2026, 5, 31).expect("a real date"),
            days_left: days,
        }
    }

    /// **A finding that lists two hundred people is one nobody reads.**
    ///
    /// The count is already in the sentence, so the list is the first few and
    /// then how many more.
    #[test]
    fn a_long_list_of_documents_is_summarised_rather_than_recited() {
        let few: Vec<_> = (0..3).map(|n| document(&format!("p{n}"), 5)).collect();
        let described = describe(&few);
        assert!(described.contains("p0"));
        assert!(described.contains("p2"));
        assert!(
            !described.contains("more"),
            "a short list should be named in full: {described}"
        );

        let many: Vec<_> = (0..20).map(|n| document(&format!("p{n}"), 5)).collect();
        let described = describe(&many);
        assert!(
            described.contains("and 15 more"),
            "a long list was recited in full: {described}"
        );
        assert!(
            !described.contains("p19"),
            "a long list was recited in full: {described}"
        );
    }

    /// **Every module this build offers has a projection job here.**
    ///
    /// The list of modules comes from `erp_api::modules()`, which is the one
    /// place they are enumerated — so a fourth module cannot be added to the
    /// product and left out of the worker.
    ///
    /// This is the omission nothing else catches. A module registered
    /// everywhere *except* here still signs up, still installs its tables, still
    /// accepts writes, and still posts to the ledger correctly — and its read
    /// models never fill. Verified by deleting one and watching the whole
    /// workspace stay green.
    #[test]
    fn every_module_has_a_projection_job() {
        // **A module with no projection groups needs no job**, and `hr_sa` is
        // the first: it is arithmetic and one configuration key, so there is
        // nothing to project. Keyed off the setup rather than a list of names,
        // because a name here is one somebody has to remember to remove.
        let offered: BTreeSet<String> = erp_api::modules()
            .into_iter()
            .filter(|(_, setup)| !setup.groups.is_empty())
            .map(|(_, setup)| setup.module.as_str().to_owned())
            .collect();

        let worked: BTreeSet<String> = module_jobs(None)
            .iter()
            .filter_map(|job| job.module())
            .map(|module| module.as_str().to_owned())
            .collect();

        assert_eq!(
            offered,
            worked,
            "a module is offered and never projected — its read models would \
             stay empty forever. Missing: {:?}",
            offered.difference(&worked).collect::<Vec<_>>()
        );
    }

    /// And every one of them is scoped to its module.
    ///
    /// A projection job with no `module()` runs for every tenant, including the
    /// ones that declined it — which is the other half of what "modular" has to
    /// mean, and a `for_module` somebody forgot looks identical until the bill
    /// arrives.
    ///
    /// **The ZATCA sweeps are in here too**, because they are the ones that
    /// would cost real money: a submit job with no `module()` opens a
    /// connection to a tax authority for every tenant on the platform.
    #[test]
    fn no_module_job_runs_for_tenants_that_declined_it() {
        let sealing = erp_eventlog::SealingKey::new("test", &[0u8; 32]).expect("32 bytes");
        for job in module_jobs(None).into_iter().chain(zatca_jobs(&sealing)) {
            assert!(
                job.module().is_some(),
                "{} runs for every tenant, including the ones that did not buy it",
                job.name()
            );
        }
    }

    /// **A document is signed before it is sent**, and both halves have to be
    /// registered for either to matter.
    ///
    /// Everything ZATCA-related was written before there was anything to run
    /// it: for several increments the whole path worked in tests and was
    /// unreachable in production. This is the check that says it is wired in.
    #[test]
    fn a_deployment_with_a_sealing_key_both_signs_and_submits() {
        let sealing = erp_eventlog::SealingKey::new("test", &[0u8; 32]).expect("32 bytes");
        let names: Vec<&str> = zatca_jobs(&sealing).iter().map(|job| job.name()).collect();

        assert!(names.contains(&"tax_sa.sign"), "{names:?}");
        assert!(names.contains(&"tax_sa.submit"), "{names:?}");
        assert!(names.contains(&"tax_sa.onboard"), "{names:?}");
        assert!(
            zatca_jobs(&sealing)
                .iter()
                .all(|job| job.module() == Some(tax_sa::module_id()))
        );
    }

    /// OpenSSL prints a single-digit day with a **leading space**, which a
    /// `%d` parse rejects — so this would work for three weeks in four and
    /// report an unreadable certificate on the ninth of the month.
    #[test]
    fn a_certificates_expiry_is_read_the_way_openssl_prints_it() {
        let parsed = certificate_time("Aug 16 20:28:41 2031 GMT").expect("a date");
        assert_eq!(parsed.to_rfc3339(), "2031-08-16T20:28:41+00:00");

        let single_digit = certificate_time("Sep  9 01:02:03 2031 GMT").expect("a date");
        assert_eq!(single_digit.to_rfc3339(), "2031-09-09T01:02:03+00:00");

        assert!(certificate_time("whenever").is_none());
    }
}
