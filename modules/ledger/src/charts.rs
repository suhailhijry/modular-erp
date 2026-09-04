//! Ready-made charts of accounts.
//!
//! A tenant that has just signed up has an empty ledger, which is technically
//! correct and useless. These are the shortcut: pick one, get a working chart,
//! rename and close whatever does not fit.
//!
//! # Why the accounts are bilingual
//!
//! Account names are what a bookkeeper reads all day, and the first market is
//! Saudi Arabia. Installing a chart in English and telling people to rename
//! eighteen accounts is not a starting point, it is a chore. Each account
//! carries both names; installation picks by the caller's locale, and the tenant
//! can rename anything afterwards because these are ordinary accounts from the
//! moment they exist.
//!
//! # Why VAT and Zakat are in every chart
//!
//! Saudi VAT is 15% and ZATCA-reported; Zakat applies to Saudi and GCC-owned
//! businesses. A chart without those accounts is one a Saudi business has to fix
//! before its first invoice — so they are not an "advanced" template, they are
//! the baseline.
//!
//! # What this deliberately is not
//!
//! The architecture describes blueprints: browse, parameterize, materialize,
//! edit, preview, install. This is the first two and the last. ponytail: editing
//! before install is worth building when someone asks to change a chart they
//! cannot already change *after* installing it — which, since every account here
//! is renameable and closeable, is nobody yet.

use erp_i18n::Locale;

use crate::account::AccountKind;

/// One account in a template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateAccount {
    /// The account code, and the aggregate id it will be opened under.
    pub code: &'static str,
    pub name_en: &'static str,
    pub name_ar: &'static str,
    pub kind: AccountKind,
}

impl TemplateAccount {
    #[must_use]
    pub const fn name(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.name_ar,
            Locale::English => self.name_en,
        }
    }
}

/// A named starting point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chart {
    /// Stable identifier. What a client sends to install one.
    pub id: &'static str,
    pub name_en: &'static str,
    pub name_ar: &'static str,
    pub description_en: &'static str,
    pub description_ar: &'static str,
    pub accounts: &'static [TemplateAccount],
}

impl Chart {
    #[must_use]
    pub const fn name(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.name_ar,
            Locale::English => self.name_en,
        }
    }

    #[must_use]
    pub const fn description(&self, locale: Locale) -> &'static str {
        match locale {
            Locale::Arabic => self.description_ar,
            Locale::English => self.description_en,
        }
    }
}

const fn account(
    code: &'static str,
    name_en: &'static str,
    name_ar: &'static str,
    kind: AccountKind,
) -> TemplateAccount {
    TemplateAccount {
        code,
        name_en,
        name_ar,
        kind,
    }
}

/// Codes follow the usual decade convention — 1000s assets, 2000s liabilities,
/// 3000s equity, 4000s revenue, 5000s expenses — because every accountant who
/// opens this already knows it.
static SERVICES: &[TemplateAccount] = &[
    account(
        "1000",
        "Cash on hand",
        "النقد في الصندوق",
        AccountKind::Asset,
    ),
    account("1010", "Bank", "البنك", AccountKind::Asset),
    // **Money a gateway has taken and not yet paid over.** A card settles to
    // the bank days later and net of fees, so posting a card sale straight to
    // `1010` says the bank holds money it does not. In every template because
    // every business that takes a card has this gap.
    account(
        "1150",
        "Payments in transit",
        "مدفوعات قيد التحصيل",
        AccountKind::Asset,
    ),
    // **Buy-now-pay-later is not a card.** The provider pays the merchant and
    // collects from the buyer, so what is owed after a capture is owed by
    // Tabby or Tamara — a different counterparty, a different credit risk, and
    // a different line on the balance sheet. Netting it into `1150` would hide
    // which of the two owes the money.
    account(
        "1160",
        "Instalment provider receivable",
        "ذمم مزودي التقسيط",
        AccountKind::Asset,
    ),
    account(
        "1100",
        "Accounts receivable",
        "الذمم المدينة",
        AccountKind::Asset,
    ),
    // Input VAT: paid on purchases, reclaimed from ZATCA.
    account(
        "1200",
        "VAT receivable",
        "ضريبة القيمة المضافة المستحقة",
        AccountKind::Asset,
    ),
    account(
        "1500",
        "Prepaid expenses",
        "مصروفات مدفوعة مقدمًا",
        AccountKind::Asset,
    ),
    account(
        "2000",
        "Accounts payable",
        "الذمم الدائنة",
        AccountKind::Liability,
    ),
    // Output VAT: charged on sales, owed to ZATCA.
    account(
        "2100",
        "VAT payable",
        "ضريبة القيمة المضافة المستحقة الدفع",
        AccountKind::Liability,
    ),
    account(
        "2200",
        "Salaries payable",
        "رواتب مستحقة",
        AccountKind::Liability,
    ),
    // **What is held back from pay on somebody's behalf** — an advance being
    // repaid, a loan instalment. In every template for the reason `2400` and
    // `5910` are: a business that runs payroll with a single deduction on its
    // first month needs this on its first month, and netting it against the
    // wage cost instead would understate what the business spent on wages.
    account(
        "2210",
        "Payroll deductions",
        "استقطاعات الرواتب",
        AccountKind::Liability,
    ),
    account(
        "2300",
        "Zakat payable",
        "الزكاة المستحقة",
        AccountKind::Liability,
    ),
    // Money taken for something not yet delivered: a ten-session package, a
    // gym year, a deposit. In every template for the reason VAT and Zakat are:
    // a business that sells a package on its first day needs this on its first
    // day, so it is not an advanced option. See `modules/prepaid`.
    account(
        "2400",
        "Deferred revenue",
        "إيرادات مؤجلة",
        AccountKind::Liability,
    ),
    account("3000", "Owner's capital", "رأس المال", AccountKind::Equity),
    account(
        "3100",
        "Retained earnings",
        "الأرباح المبقاة",
        AccountKind::Equity,
    ),
    account(
        "4000",
        "Service revenue",
        "إيرادات الخدمات",
        AccountKind::Revenue,
    ),
    // A contra-revenue account: normally carries a debit balance, which is why
    // nothing in this module refuses a balance on the "wrong" side.
    account(
        "4900",
        "Discounts given",
        "الخصومات الممنوحة",
        AccountKind::Revenue,
    ),
    account(
        "5000",
        "Salaries and wages",
        "الرواتب والأجور",
        AccountKind::Expense,
    ),
    account("5100", "Rent", "الإيجار", AccountKind::Expense),
    account("5200", "Utilities", "المرافق", AccountKind::Expense),
    account("5300", "Marketing", "التسويق", AccountKind::Expense),
    // **A fee is an expense, never a smaller sale.** A tenant that nets the
    // gateway's cut against revenue cannot answer what it actually sold, and
    // the VAT return it files is wrong.
    account(
        "5400",
        "Payment processing fees",
        "رسوم معالجة المدفوعات",
        AccountKind::Expense,
    ),
    account(
        "5900",
        "Other expenses",
        "مصروفات أخرى",
        AccountKind::Expense,
    ),
    // **What a counted drawer disagrees with the books by.** In every template
    // for the reason `2400` is: a till that cannot post its shortage leaves the
    // books saying the drawer holds what it does not, for ever.
    account(
        "5910",
        "Cash over and short",
        "فروقات الصندوق",
        AccountKind::Expense,
    ),
];

/// The services chart plus what a business holding stock needs.
static RETAIL: &[TemplateAccount] = &[
    account(
        "1000",
        "Cash on hand",
        "النقد في الصندوق",
        AccountKind::Asset,
    ),
    account("1010", "Bank", "البنك", AccountKind::Asset),
    // **Money a gateway has taken and not yet paid over.** A card settles to
    // the bank days later and net of fees, so posting a card sale straight to
    // `1010` says the bank holds money it does not. In every template because
    // every business that takes a card has this gap.
    account(
        "1150",
        "Payments in transit",
        "مدفوعات قيد التحصيل",
        AccountKind::Asset,
    ),
    // **Buy-now-pay-later is not a card.** The provider pays the merchant and
    // collects from the buyer, so what is owed after a capture is owed by
    // Tabby or Tamara — a different counterparty, a different credit risk, and
    // a different line on the balance sheet. Netting it into `1150` would hide
    // which of the two owes the money.
    account(
        "1160",
        "Instalment provider receivable",
        "ذمم مزودي التقسيط",
        AccountKind::Asset,
    ),
    account(
        "1100",
        "Accounts receivable",
        "الذمم المدينة",
        AccountKind::Asset,
    ),
    account(
        "1200",
        "VAT receivable",
        "ضريبة القيمة المضافة المستحقة",
        AccountKind::Asset,
    ),
    account("1300", "Inventory", "المخزون", AccountKind::Asset),
    account(
        "1500",
        "Prepaid expenses",
        "مصروفات مدفوعة مقدمًا",
        AccountKind::Asset,
    ),
    account(
        "2000",
        "Accounts payable",
        "الذمم الدائنة",
        AccountKind::Liability,
    ),
    account(
        "2100",
        "VAT payable",
        "ضريبة القيمة المضافة المستحقة الدفع",
        AccountKind::Liability,
    ),
    account(
        "2200",
        "Salaries payable",
        "رواتب مستحقة",
        AccountKind::Liability,
    ),
    // **What is held back from pay on somebody's behalf** — an advance being
    // repaid, a loan instalment. In every template for the reason `2400` and
    // `5910` are: a business that runs payroll with a single deduction on its
    // first month needs this on its first month, and netting it against the
    // wage cost instead would understate what the business spent on wages.
    account(
        "2210",
        "Payroll deductions",
        "استقطاعات الرواتب",
        AccountKind::Liability,
    ),
    account(
        "2300",
        "Zakat payable",
        "الزكاة المستحقة",
        AccountKind::Liability,
    ),
    // Money taken for something not yet delivered: a ten-session package, a
    // gym year, a deposit. In every template for the reason VAT and Zakat are:
    // a business that sells a package on its first day needs this on its first
    // day, so it is not an advanced option. See `modules/prepaid`.
    account(
        "2400",
        "Deferred revenue",
        "إيرادات مؤجلة",
        AccountKind::Liability,
    ),
    account("3000", "Owner's capital", "رأس المال", AccountKind::Equity),
    account(
        "3100",
        "Retained earnings",
        "الأرباح المبقاة",
        AccountKind::Equity,
    ),
    account("4000", "Sales", "المبيعات", AccountKind::Revenue),
    account(
        "4100",
        "Sales returns",
        "مردودات المبيعات",
        AccountKind::Revenue,
    ),
    account(
        "4900",
        "Discounts given",
        "الخصومات الممنوحة",
        AccountKind::Revenue,
    ),
    account(
        "5010",
        "Cost of goods sold",
        "تكلفة البضاعة المباعة",
        AccountKind::Expense,
    ),
    account(
        "5000",
        "Salaries and wages",
        "الرواتب والأجور",
        AccountKind::Expense,
    ),
    account("5100", "Rent", "الإيجار", AccountKind::Expense),
    account("5200", "Utilities", "المرافق", AccountKind::Expense),
    account("5300", "Marketing", "التسويق", AccountKind::Expense),
    // **A fee is an expense, never a smaller sale.** A tenant that nets the
    // gateway's cut against revenue cannot answer what it actually sold, and
    // the VAT return it files is wrong.
    account(
        "5400",
        "Payment processing fees",
        "رسوم معالجة المدفوعات",
        AccountKind::Expense,
    ),
    account(
        "5900",
        "Other expenses",
        "مصروفات أخرى",
        AccountKind::Expense,
    ),
    // **What a counted drawer disagrees with the books by.** In every template
    // for the reason `2400` is: a till that cannot post its shortage leaves the
    // books saying the drawer holds what it does not, for ever.
    account(
        "5910",
        "Cash over and short",
        "فروقات الصندوق",
        AccountKind::Expense,
    ),
];

/// Every chart this build ships.
pub static CHARTS: &[Chart] = &[
    Chart {
        id: "services",
        name_en: "Services business",
        name_ar: "منشأة خدمات",
        description_en: "For consultancies, agencies, clinics and trades. \
                         Includes VAT and Zakat.",
        description_ar: "للاستشارات والوكالات والعيادات والحرف. يشمل ضريبة القيمة المضافة والزكاة.",
        accounts: SERVICES,
    },
    Chart {
        id: "retail",
        name_en: "Retail and trading",
        name_ar: "تجزئة وتجارة",
        description_en: "Adds inventory, cost of goods sold and sales returns.",
        description_ar: "يضيف المخزون وتكلفة البضاعة المباعة ومردودات المبيعات.",
        accounts: RETAIL,
    },
];

/// Finds a chart by id.
///
/// There is deliberately no "empty" chart: not installing one is already that,
/// and a template that creates nothing is a menu item that does nothing.
#[must_use]
pub fn chart(id: &str) -> Option<&'static Chart> {
    CHARTS.iter().find(|c| c.id == id)
}

/// How an installation went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Installed {
    pub opened: usize,
    /// Accounts that were already there. Not a failure — see
    /// [`install_chart`](crate::install_chart).
    pub skipped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chart_is_findable_by_its_id() {
        for c in CHARTS {
            assert_eq!(chart(c.id).map(|found| found.id), Some(c.id));
        }
        assert!(chart("no-such-chart").is_none());
    }

    #[test]
    fn codes_are_unique_within_a_chart() {
        // A duplicate would make installation refuse itself halfway.
        for c in CHARTS {
            let mut codes: Vec<_> = c.accounts.iter().map(|a| a.code).collect();
            let before = codes.len();
            codes.sort_unstable();
            codes.dedup();
            assert_eq!(before, codes.len(), "duplicate code in chart {}", c.id);
        }
    }

    #[test]
    fn every_account_is_named_in_both_languages() {
        for c in CHARTS {
            assert!(!c.name(Locale::Arabic).is_empty(), "{}", c.id);
            assert!(!c.description(Locale::Arabic).is_empty(), "{}", c.id);
            for a in c.accounts {
                assert!(!a.name_en.is_empty(), "{}/{}", c.id, a.code);
                assert!(
                    !a.name_ar.is_empty(),
                    "{}/{} has no Arabic name",
                    c.id,
                    a.code
                );
                // And the Arabic is actually Arabic, not a copied English
                // string — the failure mode a translator's TODO leaves behind.
                assert!(
                    a.name_ar
                        .chars()
                        .any(|ch| ('\u{0600}'..='\u{06FF}').contains(&ch)),
                    "{}/{}: {:?} is not Arabic",
                    c.id,
                    a.code,
                    a.name_ar
                );
            }
        }
    }

    #[test]
    fn every_code_is_a_valid_aggregate_id() {
        // Codes become aggregate ids, so a code that does not parse would fail
        // at install time rather than here.
        for c in CHARTS {
            for a in c.accounts {
                assert!(
                    erp_types::AggregateId::new(a.code).is_ok(),
                    "{}/{}",
                    c.id,
                    a.code
                );
            }
        }
    }

    /// **Every chart can take a card and a Tabby order.**
    ///
    /// A business whose chart has no clearing account posts a card sale
    /// straight to the bank, which says the bank holds money that is still at
    /// the gateway; one with no fee account nets the gateway's cut against
    /// revenue, and then cannot answer what it sold. Both are wrong in a way
    /// that only shows up at a reconciliation.
    #[test]
    fn every_chart_can_settle_a_gateway_payment() {
        for c in CHARTS {
            let has = |code: &str| c.accounts.iter().any(|a| a.code == code);
            assert!(has("1150"), "{} has nowhere for money in transit", c.id);
            assert!(
                has("1160"),
                "{} has nowhere for an instalment provider",
                c.id
            );
            assert!(has("5400"), "{} has nowhere to put a gateway fee", c.id);
        }
    }

    /// Saudi VAT is 15% and ZATCA-reported; Zakat applies to GCC-owned
    /// businesses. A chart missing either is one the first customer has to fix
    /// before their first invoice.
    #[test]
    fn every_chart_can_account_for_saudi_vat_and_zakat() {
        for c in CHARTS {
            let has = |code: &str| c.accounts.iter().any(|a| a.code == code);
            assert!(has("1200"), "{} has no input VAT account", c.id);
            assert!(has("2100"), "{} has no output VAT account", c.id);
            assert!(has("2300"), "{} has no Zakat account", c.id);
        }
    }

    #[test]
    fn every_chart_has_all_five_account_kinds() {
        // A chart missing a kind cannot produce a balance sheet or an income
        // statement, which is the entire point of having one.
        for c in CHARTS {
            for kind in [
                AccountKind::Asset,
                AccountKind::Liability,
                AccountKind::Equity,
                AccountKind::Revenue,
                AccountKind::Expense,
            ] {
                assert!(
                    c.accounts.iter().any(|a| a.kind == kind),
                    "{} has no {} account",
                    c.id,
                    kind.as_str()
                );
            }
        }
    }
}
