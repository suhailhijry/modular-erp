use std::sync::Arc;

use crate::event_sourcing::*;
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodKind {
    Monthly,
    Quarterly,
    Yearly,
}

impl PeriodKind {
    /// Canonical id + bounds for the period of this kind containing
    /// `date`. Helpers for the common case - the calendar itself only
    /// cares about bounds, so a 4-4-5 retail calendar or a fiscal year
    /// starting in July can be opened with explicit custom bounds and
    /// any label; nothing downstream assumes calendar-aligned periods.
    pub fn canonical_for(self, date: NaiveDate) -> (String, NaiveDate, NaiveDate) {
        let year = date.year();
        match self {
            PeriodKind::Monthly => {
                let month = date.month();
                let start = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
                let end = if month == 12 {
                    NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap()
                }
                .pred_opt()
                .unwrap();
                (format!("{year:04}-M{month:02}"), start, end)
            }
            PeriodKind::Quarterly => {
                let quarter = (date.month() - 1) / 3 + 1; // 1..=4
                let start_month = (quarter - 1) * 3 + 1;
                let start = NaiveDate::from_ymd_opt(year, start_month, 1).unwrap();
                let end = if quarter == 4 {
                    NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(year, start_month + 3, 1).unwrap()
                }
                .pred_opt()
                .unwrap();
                (format!("{year:04}-Q{quarter}"), start, end)
            }
            PeriodKind::Yearly => {
                let start = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
                let end = NaiveDate::from_ymd_opt(year, 12, 31).unwrap();
                (format!("{year:04}-Y"), start, end)
            }
        }
    }
}

pub const FISCAL_CALENDAR_ID: &str = "fiscal-calendar";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredPeriod {
    pub period_id: String,
    pub kind: PeriodKind,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate, // inclusive
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "fiscal_calendar")]
pub enum FiscalCalendarEvent {
    PeriodRegistered { period: RegisteredPeriod },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "fiscal_calendar")]
pub struct FiscalCalendar {
    id: String,
    version: u64,
    periods: Vec<RegisteredPeriod>,
}

impl FiscalCalendar {
    /// The routing primitive: which registered period covers this date?
    /// At most one, by the non-overlap invariant.
    pub fn period_covering(&self, date: NaiveDate) -> Option<&RegisteredPeriod> {
        self.periods
            .iter()
            .find(|p| date >= p.start_date && date <= p.end_date)
    }

    pub fn periods(&self) -> &[RegisteredPeriod] {
        &self.periods
    }
}

#[derive(Debug, Clone)]
pub enum FiscalCalendarCommand {
    RegisterPeriod { period: RegisteredPeriod },
}

#[derive(Debug, thiserror::Error)]
pub enum FiscalCalendarError {
    #[error("period bounds are inverted: {start} > {end}")]
    InvertedBounds { start: NaiveDate, end: NaiveDate },
    #[error("period id '{0}' is already registered")]
    DuplicateId(String),
    #[error(
        "period [{start}, {end}] overlaps existing period '{existing_id}' [{existing_start}, {existing_end}]"
    )]
    Overlap {
        start: NaiveDate,
        end: NaiveDate,
        existing_id: String,
        existing_start: NaiveDate,
        existing_end: NaiveDate,
    },
}

impl Aggregate for FiscalCalendar {
    type Event = FiscalCalendarEvent;
    type Command = FiscalCalendarCommand;
    type Error = FiscalCalendarError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            FiscalCalendarEvent::PeriodRegistered { period } => self.periods.push(period.clone()),
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            FiscalCalendarCommand::RegisterPeriod { period } => {
                if period.start_date > period.end_date {
                    return Err(FiscalCalendarError::InvertedBounds {
                        start: period.start_date,
                        end: period.end_date,
                    });
                }
                if self.periods.iter().any(|p| p.period_id == period.period_id) {
                    return Err(FiscalCalendarError::DuplicateId(period.period_id));
                }
                // Non-overlap: THE invariant that makes date routing
                // unambiguous. Kinds may differ across time (monthly
                // 2026, quarterly 2027) - only the date ranges matter.
                if let Some(existing) = self
                    .periods
                    .iter()
                    .find(|p| period.start_date <= p.end_date && period.end_date >= p.start_date)
                {
                    return Err(FiscalCalendarError::Overlap {
                        start: period.start_date,
                        end: period.end_date,
                        existing_id: existing.period_id.clone(),
                        existing_start: existing.start_date,
                        existing_end: existing.end_date,
                    });
                }
                Ok(vec![FiscalCalendarEvent::PeriodRegistered { period }])
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FiscalPeriodStatus {
    Open,
    Closed,
    Locked,
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "fiscal_period")]
pub enum FiscalPeriodEvent {
    Opened {
        kind: PeriodKind,
        start_date: NaiveDate,
        end_date: NaiveDate,
    },
    Closed,
    Reopened,
    Locked,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "fiscal_period")]
pub struct FiscalPeriod {
    id: String, // e.g. "2026-03" for a monthly period
    version: u64,

    status: Option<FiscalPeriodStatus>,
    kind: Option<PeriodKind>,
    start_date: Option<NaiveDate>,
    end_date: Option<NaiveDate>,
}

impl FiscalPeriod {
    pub fn status(&self) -> Option<FiscalPeriodStatus> {
        self.status.clone()
    }

    pub fn kind(&self) -> Option<PeriodKind> {
        self.kind
    }

    pub fn contains(&self, date: NaiveDate) -> bool {
        matches!((self.start_date, self.end_date), (Some(s), Some(e)) if date >= s && date <= e)
    }
}

#[derive(Debug, Clone)]
pub enum FiscalPeriodCommand {
    Open {
        kind: PeriodKind,
        start_date: NaiveDate,
        end_date: NaiveDate,
    },
    Close,
    Reopen,
    Lock,
}

#[derive(Debug, thiserror::Error)]
pub enum FiscalPeriodError {
    #[error("period already opened")]
    AlreadyOpened,
    #[error("start date is higher than end date")]
    InvalidBounds,
    #[error("period is locked and cannot be reopened")]
    Locked,
    #[error("period is not open")]
    NotOpen,
    #[error("period is not closed")]
    NotClosed,
}

impl Aggregate for FiscalPeriod {
    type Event = FiscalPeriodEvent;
    type Command = FiscalPeriodCommand;
    type Error = FiscalPeriodError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            FiscalPeriodEvent::Opened {
                kind,
                start_date,
                end_date,
            } => {
                self.status = Some(FiscalPeriodStatus::Open);
                self.kind = Some(*kind);
                self.start_date = Some(*start_date);
                self.end_date = Some(*end_date);
            }
            FiscalPeriodEvent::Closed => self.status = Some(FiscalPeriodStatus::Closed),
            FiscalPeriodEvent::Reopened => self.status = Some(FiscalPeriodStatus::Open),
            FiscalPeriodEvent::Locked => self.status = Some(FiscalPeriodStatus::Locked),
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            FiscalPeriodCommand::Open {
                kind,
                start_date,
                end_date,
            } => {
                if self.status.is_some() {
                    return Err(FiscalPeriodError::AlreadyOpened);
                }
                if start_date > end_date {
                    return Err(FiscalPeriodError::InvalidBounds);
                }
                Ok(vec![FiscalPeriodEvent::Opened {
                    kind,
                    start_date,
                    end_date,
                }])
            }
            FiscalPeriodCommand::Close => match self.status {
                Some(FiscalPeriodStatus::Open) => Ok(vec![FiscalPeriodEvent::Closed]),
                _ => Err(FiscalPeriodError::NotOpen),
            },
            FiscalPeriodCommand::Reopen => match self.status {
                Some(FiscalPeriodStatus::Closed) => Ok(vec![FiscalPeriodEvent::Reopened]),
                Some(FiscalPeriodStatus::Locked) => Err(FiscalPeriodError::Locked),
                _ => Err(FiscalPeriodError::NotClosed),
            },
            FiscalPeriodCommand::Lock => match self.status {
                Some(FiscalPeriodStatus::Closed) => Ok(vec![FiscalPeriodEvent::Locked]),
                _ => Err(FiscalPeriodError::NotClosed),
            },
        }
    }
}

pub async fn open_fiscal_period(
    store: &dyn EventStore,
    bus: Option<Arc<dyn EventBus>>,
    period_id: &str,
    kind: PeriodKind,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> anyhow::Result<()> {
    let mut calendar = load_aggregate::<FiscalCalendar>(store, FISCAL_CALENDAR_ID).await?;
    let cal_start_seq = calendar.version() + 1;
    let registered = calendar.handle(FiscalCalendarCommand::RegisterPeriod {
        period: RegisteredPeriod {
            period_id: period_id.to_string(),
            kind,
            start_date,
            end_date,
        },
    })?;
    for e in &registered {
        calendar.apply(e);
    }

    let mut period = FiscalPeriod::default();
    let period_start_seq = period.version() + 1;
    let opened = period.handle(FiscalPeriodCommand::Open {
        kind,
        start_date,
        end_date,
    })?;
    for e in &opened {
        period.apply(e);
    }

    let mut ctx = Context::new();
    ctx.queue_events::<FiscalCalendar>(FISCAL_CALENDAR_ID, cal_start_seq, registered);
    ctx.queue_events::<FiscalPeriod>(period_id, period_start_seq, opened);
    ctx.commit(store, bus).await?;
    Ok(())
}

pub async fn resolve_open_period_for_date(
    store: &dyn EventStore,
    date: NaiveDate,
) -> anyhow::Result<FiscalPeriod> {
    let calendar = load_aggregate::<FiscalCalendar>(store, FISCAL_CALENDAR_ID).await?;
    let Some(registered) = calendar.period_covering(date) else {
        anyhow::bail!("no fiscal period covers {date} - open one before posting to this date");
    };
    let period = load_aggregate::<FiscalPeriod>(store, &registered.period_id).await?;
    if period.status() != Some(FiscalPeriodStatus::Open) {
        anyhow::bail!(
            "fiscal period '{}' covering {date} is not open: {:?}",
            registered.period_id,
            period.status()
        );
    }
    // Belt-and-suspenders: calendar registration and period bounds were
    // written atomically, so a mismatch here means data corruption -
    // fail loudly rather than post into it.
    if !period.contains(date) {
        anyhow::bail!(
            "fiscal period '{}' bounds disagree with calendar registration for {date} - refusing to post",
            registered.period_id
        );
    }
    Ok(period)
}
