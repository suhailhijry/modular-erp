use crate::{accounting::resolve_open_period_for_date, event_sourcing::*};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "accounting_side", rename_all = "lowercase")]
pub enum Side {
    Debit,
    Credit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PayableDocument {
    Invoice { id: String },
    DebitNote { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEntryReference {
    /// Another journal entry - reversals, corrections.
    JournalEntry {
        id: String,
    },
    Invoice {
        id: String,
    },
    CreditNote {
        id: String,
        original_invoice_id: Option<String>,
    },
    DebitNote {
        id: String,
        original_invoice_id: Option<String>,
    },
    /// A payment recorded against an invoice.
    Payment {
        id: String,
        document: PayableDocument,
    },
    /// A payment recorded against an invoice.
    Refund {
        id: String,
        credit_note_id: String,
    },
    /// Anything outside the system: bank statement line, contract
    /// number, government filing.
    External {
        description: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalLine {
    pub account_code: String,
    pub side: Side,
    pub amount: i64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalEntryStatus {
    Draft,
    Posted,
    Reversed,
}

#[derive(Debug, Clone, Serialize, Deserialize, DomainEvent)]
#[event(prefix = "journal_entry")]
pub enum JournalEntryEvent {
    Drafted {
        date: NaiveDate,
        memo: String,
        lines: Vec<JournalLine>,
        reference: Option<JournalEntryReference>,
    },
    Posted {
        date: NaiveDate,
        lines: Vec<JournalLine>,
    },
    Reversed {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, AggregateMeta)]
#[aggregate(type = "journal_entry")]
pub struct JournalEntry {
    id: String,
    version: u64,
    date: Option<NaiveDate>,
    status: Option<JournalEntryStatus>,
    lines: Vec<JournalLine>,
    reference: Option<JournalEntryReference>,
    memo: String,
}

impl JournalEntry {
    pub fn status(&self) -> Option<JournalEntryStatus> {
        self.status.clone()
    }

    pub fn date(&self) -> Option<NaiveDate> {
        self.date
    }

    pub fn lines(&self) -> &[JournalLine] {
        &self.lines
    }

    pub fn memo(&self) -> &String {
        &self.memo
    }

    pub fn reference(&self) -> Option<&JournalEntryReference> {
        self.reference.as_ref()
    }

    fn check_balanced(lines: &[JournalLine]) -> Result<(), JournalEntryError> {
        if lines.is_empty() {
            return Err(JournalEntryError::Empty);
        }
        // Balance is checked PER CURRENCY - a multi-currency entry must
        // balance within each currency independently, not by converting and
        // summing (that would hide real imbalances behind exchange-rate math
        // that doesn't belong in the ledger's own invariant).
        let mut totals: HashMap<&str, (i64, i64)> = HashMap::new();
        for line in lines {
            let entry = totals.entry(&line.currency).or_insert((0, 0));
            match line.side {
                Side::Debit => entry.0 += line.amount,
                Side::Credit => entry.1 += line.amount,
            }
        }
        for (currency, (debit, credit)) in totals {
            if debit != credit {
                return Err(JournalEntryError::Unbalanced {
                    currency: currency.to_string(),
                    debit_minor: debit,
                    credit_minor: credit,
                });
            }
        }
        Ok(())
    }

    fn reversed_lines(lines: &[JournalLine]) -> Vec<JournalLine> {
        lines
            .iter()
            .map(|l| JournalLine {
                account_code: l.account_code.clone(),
                side: match l.side {
                    Side::Debit => Side::Credit,
                    Side::Credit => Side::Debit,
                },
                amount: l.amount,
                currency: l.currency.clone(),
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub enum JournalEntryCommand {
    Draft {
        date: NaiveDate,
        memo: String,
        reference: Option<JournalEntryReference>,
        lines: Vec<JournalLine>,
    },
    Post,
    Reverse {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum JournalEntryError {
    #[error("entry has no lines")]
    Empty,
    #[error("unbalanced entry in {currency}: debits {debit_minor} != credits {credit_minor}")]
    Unbalanced {
        currency: String,
        debit_minor: i64,
        credit_minor: i64,
    },
    #[error("entry already exists")]
    AlreadyDrafted,
    #[error("entry is not in draft status")]
    NotDraft,
    #[error("entry is not posted")]
    NotPosted,
}

impl Aggregate for JournalEntry {
    type Event = JournalEntryEvent;
    type Command = JournalEntryCommand;
    type Error = JournalEntryError;

    fn apply(&mut self, event: &Self::Event) {
        match event {
            JournalEntryEvent::Drafted {
                date,
                memo,
                reference,
                lines,
            } => {
                self.date = Some(*date);
                self.memo = memo.clone();
                self.reference = reference.clone();
                self.lines = lines.clone();
                self.status = Some(JournalEntryStatus::Draft);
            }
            JournalEntryEvent::Posted { .. } => self.status = Some(JournalEntryStatus::Posted),
            JournalEntryEvent::Reversed { .. } => self.status = Some(JournalEntryStatus::Reversed),
        }
        self.version += 1;
    }

    fn handle(&self, command: Self::Command) -> Result<Vec<Self::Event>, Self::Error> {
        match command {
            JournalEntryCommand::Draft {
                date,
                memo,
                reference,
                lines,
            } => {
                if self.status.is_some() {
                    return Err(JournalEntryError::AlreadyDrafted);
                }
                JournalEntry::check_balanced(lines.as_ref())?;
                Ok(vec![JournalEntryEvent::Drafted {
                    date,
                    memo,
                    reference,
                    lines,
                }])
            }
            JournalEntryCommand::Post => {
                if self.status != Some(JournalEntryStatus::Draft) {
                    return Err(JournalEntryError::NotDraft);
                }
                Ok(vec![JournalEntryEvent::Posted {
                    date: self.date.expect("set by drafted"),
                    lines: self.lines.clone(),
                }])
            }
            JournalEntryCommand::Reverse { reason } => {
                if self.status != Some(JournalEntryStatus::Posted) {
                    return Err(JournalEntryError::NotPosted);
                }
                Ok(vec![JournalEntryEvent::Reversed { reason }])
            }
        }
    }
}

pub async fn post_journal_entry(
    store: &dyn EventStore,
    bus: Option<Arc<dyn EventBus>>,
    entry_id: &str,
    date: NaiveDate,
    memo: String,
    reference: Option<JournalEntryReference>,
    lines: Vec<JournalLine>,
) -> anyhow::Result<JournalEntry> {
    use crate::accounting::LedgerAccount;

    for line in &lines {
        let account = load_aggregate::<LedgerAccount>(store, &line.account_code).await?;
        if !account.is_active() {
            anyhow::bail!("account {} is inactive", line.account_code);
        }
    }

    let _period = resolve_open_period_for_date(store, date).await?;

    let mut entry = JournalEntry::default();
    let start_seq = entry.version() + 1;
    let drafted = entry.handle(JournalEntryCommand::Draft {
        date,
        memo,
        reference,
        lines,
    })?;
    for e in &drafted {
        entry.apply(e);
    }
    let posted = entry.handle(JournalEntryCommand::Post)?;
    for e in &posted {
        entry.apply(e);
    }

    let mut ctx = Context::new();
    ctx.queue_events::<JournalEntry>(
        entry_id,
        start_seq,
        drafted.into_iter().chain(posted).collect(),
    );
    ctx.commit(store, bus).await?;

    Ok(entry)
}

pub async fn reverse_journal_entry(
    store: &dyn EventStore,
    bus: Option<Arc<dyn EventBus>>,
    original_id: &str,
    reversal_id: &str,
    date: NaiveDate,
    reason: String,
) -> anyhow::Result<()> {
    let mut original = load_aggregate::<JournalEntry>(store, original_id).await?;
    if let Some(original_date) = original.date() {
        if date < original_date {
            anyhow::bail!(
                "reversal date {date} precedes the original entry's date {original_date} - date the reversal on or after the original"
            );
        }
    }
    crate::accounting::fiscal_period::resolve_open_period_for_date(store, date).await?;

    let original_start_seq = original.version() + 1;
    let reversal_events = original.handle(JournalEntryCommand::Reverse {
        reason: reason.clone(),
    })?;
    for e in &reversal_events {
        original.apply(e);
    }

    let mut reversal = JournalEntry::default();
    let reversal_start_seq = reversal.version() + 1;
    let lines = JournalEntry::reversed_lines(original.lines());
    let drafted = reversal.handle(JournalEntryCommand::Draft {
        date,
        memo: format!("Reversal of {original_id}: {reason}"),
        reference: Some(JournalEntryReference::JournalEntry {
            id: original_id.to_string(),
        }),
        lines,
    })?;

    for e in &drafted {
        reversal.apply(e);
    }
    let posted = reversal.handle(JournalEntryCommand::Post)?;
    for e in &posted {
        reversal.apply(e);
    }

    let mut ctx = Context::new();
    ctx.queue_events::<JournalEntry>(original_id, original_start_seq, reversal_events);
    ctx.queue_events::<JournalEntry>(
        reversal_id,
        reversal_start_seq,
        drafted.into_iter().chain(posted).collect(),
    );
    ctx.commit(store, bus).await?;
    Ok(())
}
