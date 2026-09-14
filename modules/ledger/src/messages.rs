//! The ledger's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const ACCOUNT_EXISTS: MessageCode = MessageCode::new("ledger.account_exists");
pub const ENTRY_TOO_LARGE: MessageCode = MessageCode::new("ledger.entry_too_large");
pub const NO_SUCH_ACCOUNT: MessageCode = MessageCode::new("ledger.no_such_account");
pub const ACCOUNT_CLOSED: MessageCode = MessageCode::new("ledger.account_closed");
pub const ALREADY_POSTED: MessageCode = MessageCode::new("ledger.already_posted");
pub const NO_SUCH_ENTRY: MessageCode = MessageCode::new("ledger.no_such_entry");
pub const NO_SUCH_BRANCH: MessageCode = MessageCode::new("ledger.no_such_branch");
pub const ALREADY_REVERSED: MessageCode = MessageCode::new("ledger.already_reversed");
pub const TOO_FEW_LINES: MessageCode = MessageCode::new("ledger.too_few_lines");
pub const MIXED_CURRENCIES: MessageCode = MessageCode::new("ledger.mixed_currencies");
pub const DOES_NOT_BALANCE: MessageCode = MessageCode::new("ledger.does_not_balance");
pub const ZERO_LINE: MessageCode = MessageCode::new("ledger.zero_line");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("ledger.amount_out_of_range");
/// An entry dated into a period the books were closed for.
pub const PERIOD_CLOSED: MessageCode = MessageCode::new("ledger.period_closed");
/// A period pattern this build does not know. 400.
pub const NOT_A_PATTERN: MessageCode = MessageCode::new("ledger.not_a_pattern");
/// The calendar cannot change while the books are closed. 409.
pub const CALENDAR_LOCKED: MessageCode = MessageCode::new("ledger.calendar_locked");
/// A statement asked for no range, or a backwards one. 400.
pub const NOT_A_RANGE: MessageCode = MessageCode::new("ledger.not_a_range");
/// A period id this calendar does not have. 400.
pub const NO_SUCH_PERIOD: MessageCode = MessageCode::new("ledger.no_such_period");
/// The postings up to the instant asked do not sum to zero, so no balance
/// sheet is shown. 503 — the pipeline, not the caller.
pub const SHEET_DOES_NOT_BALANCE: MessageCode = MessageCode::new("ledger.sheet_does_not_balance");
/// A closing entry reversed by hand rather than by reopening its year. 409.
pub const CLOSING_ENTRY: MessageCode = MessageCode::new("ledger.closing_entry");
/// A period closed out of order. 409.
pub const PERIOD_OUT_OF_ORDER: MessageCode = MessageCode::new("ledger.period_out_of_order");
/// A period reopened that is not the latest closed. 409.
pub const PERIOD_NOT_LATEST: MessageCode = MessageCode::new("ledger.period_not_latest");
/// A booked year's period reopened before the year. 409.
pub const YEAR_BOOKED: MessageCode = MessageCode::new("ledger.year_booked");
/// A year booked while one of its periods is open. 409.
pub const YEAR_OPEN: MessageCode = MessageCode::new("ledger.year_open");
/// A year reopened while a later one is booked. 409.
pub const LATER_YEAR_BOOKED: MessageCode = MessageCode::new("ledger.later_year_booked");
/// A trading balance in a currency with no retained-earnings account. 409.
pub const CLOSING_NEEDS_ACCOUNT: MessageCode = MessageCode::new("ledger.closing_needs_account");
/// A new calendar segment starting off a fiscal-year boundary. 400.
pub const NOT_A_YEAR_START: MessageCode = MessageCode::new("ledger.not_a_year_start");
/// A path segment that is not a fiscal year. 400.
pub const NOT_A_YEAR: MessageCode = MessageCode::new("ledger.not_a_year");
/// The memo on a year's closing entries.
pub const CLOSING_MEMO: MessageCode = MessageCode::new("ledger.closing_memo");
/// The memo on the reversal of a year's closing entries.
pub const REOPENING_MEMO: MessageCode = MessageCode::new("ledger.reopening_memo");
/// A year close asked while the read model the figures come from is behind
/// the log. 503; try again.
pub const LEDGER_BEHIND: MessageCode = MessageCode::new("ledger.read_model_behind");

pub static CODES: &[MessageCode] = &[
    ENTRY_TOO_LARGE,
    ACCOUNT_EXISTS,
    NO_SUCH_ACCOUNT,
    ACCOUNT_CLOSED,
    ALREADY_POSTED,
    NO_SUCH_ENTRY,
    NO_SUCH_BRANCH,
    ALREADY_REVERSED,
    TOO_FEW_LINES,
    MIXED_CURRENCIES,
    DOES_NOT_BALANCE,
    ZERO_LINE,
    AMOUNT_OUT_OF_RANGE,
    PERIOD_CLOSED,
    NOT_A_PATTERN,
    CALENDAR_LOCKED,
    NOT_A_RANGE,
    NO_SUCH_PERIOD,
    SHEET_DOES_NOT_BALANCE,
    CLOSING_ENTRY,
    PERIOD_OUT_OF_ORDER,
    PERIOD_NOT_LATEST,
    YEAR_BOOKED,
    YEAR_OPEN,
    LATER_YEAR_BOOKED,
    CLOSING_NEEDS_ACCOUNT,
    NOT_A_YEAR_START,
    NOT_A_YEAR,
    CLOSING_MEMO,
    REOPENING_MEMO,
    LEDGER_BEHIND,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOT_A_PATTERN,
        Locale::English,
        Template::Simple(
            "{pattern} is not a period pattern. One of: monthly, quarterly, 4-4-5, 4-5-4, 5-4-4, yearly.",
        ),
    ),
    (
        NOT_A_PATTERN,
        Locale::Arabic,
        Template::Simple(
            "{pattern} ليس نمط فترات. الأنماط: monthly، quarterly، 4-4-5، 4-5-4، 5-4-4، yearly.",
        ),
    ),
    (
        CALENDAR_LOCKED,
        Locale::English,
        Template::Simple(
            "The books are closed before {closed_before}, so the fiscal calendar cannot change. \
             Reopen them first, or wait for the next open year.",
        ),
    ),
    (
        CALENDAR_LOCKED,
        Locale::Arabic,
        Template::Simple(
            "الدفاتر مقفلة قبل {closed_before}، فلا يمكن تغيير التقويم المالي. \
             أعد فتحها أولًا، أو انتظر السنة المفتوحة التالية.",
        ),
    ),
    (
        NOT_A_RANGE,
        Locale::English,
        Template::Simple("Give a period, or `from` and `until` with `from` before `until`."),
    ),
    (
        NOT_A_RANGE,
        Locale::Arabic,
        Template::Simple("حدِّد فترة، أو `from` و`until` على أن يسبق `from` قيمة `until`."),
    ),
    (
        NO_SUCH_PERIOD,
        Locale::English,
        Template::Simple("{period} is not a period of this calendar. Periods read like 2026-P03."),
    ),
    (
        NO_SUCH_PERIOD,
        Locale::Arabic,
        Template::Simple("{period} ليست فترة في هذا التقويم. تُكتب الفترات هكذا: 2026-P03."),
    ),
    (
        SHEET_DOES_NOT_BALANCE,
        Locale::English,
        Template::Simple(
            "The postings in {currency} up to {as_at} do not balance, by {difference}. No balance \
             sheet is shown until they do; the trial balance says which currency is out.",
        ),
    ),
    (
        SHEET_DOES_NOT_BALANCE,
        Locale::Arabic,
        Template::Simple(
            "قيود {currency} حتى {as_at} غير متوازنة بفارق {difference}. لن تُعرض الميزانية \
             حتى تتوازن؛ ميزان المراجعة يبيّن العملة المختلّة.",
        ),
    ),
    (
        PERIOD_CLOSED,
        Locale::English,
        Template::Simple(
            "The books are closed before {closed_before}, and this is dated {on}.              Post the correction in the period that is open.",
        ),
    ),
    (
        PERIOD_CLOSED,
        Locale::Arabic,
        Template::Simple(
            "أُقفلت الدفاتر قبل {closed_before}، وتاريخ هذا القيد {on}.              سجّل التصحيح في الفترة المفتوحة.",
        ),
    ),
    (
        ENTRY_TOO_LARGE,
        Locale::English,
        Template::Simple(
            "The amounts on this entry are too large to add up. Split it, or check for a misplaced decimal.",
        ),
    ),
    (
        ENTRY_TOO_LARGE,
        Locale::Arabic,
        Template::Simple(
            "المبالغ في هذا القيد أكبر من أن تُجمع. قسّم القيد أو تحقق من موضع الفاصلة العشرية.",
        ),
    ),
    (
        ACCOUNT_EXISTS,
        Locale::English,
        Template::Simple("Account {code} already exists."),
    ),
    (
        ACCOUNT_EXISTS,
        Locale::Arabic,
        Template::Simple("الحساب {code} موجود بالفعل."),
    ),
    (
        NO_SUCH_ACCOUNT,
        Locale::English,
        Template::Simple("There is no account {code}."),
    ),
    (
        NO_SUCH_ACCOUNT,
        Locale::Arabic,
        Template::Simple("لا يوجد حساب {code}."),
    ),
    (
        ACCOUNT_CLOSED,
        Locale::English,
        Template::Simple("Account {code} is closed and cannot take new entries."),
    ),
    (
        ACCOUNT_CLOSED,
        Locale::Arabic,
        Template::Simple("الحساب {code} مغلق ولا يقبل قيودًا جديدة."),
    ),
    (
        ALREADY_POSTED,
        Locale::English,
        Template::Simple("This entry has already been posted."),
    ),
    (
        ALREADY_POSTED,
        Locale::Arabic,
        Template::Simple("تم ترحيل هذا القيد بالفعل."),
    ),
    // The plural is the reason this is a template and not a sentence: Arabic
    // selects six forms and English two, and "1 lines" is how a product looks
    // unfinished.
    (
        TOO_FEW_LINES,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("An entry needs at least two lines; this has {n}."),
            two: None,
            few: None,
            many: None,
            other: "An entry needs at least two lines; this has {n}.",
        },
    ),
    (
        TOO_FEW_LINES,
        Locale::Arabic,
        Template::Plural {
            zero: Some("يحتاج القيد إلى سطرين على الأقل، ولا يوجد أي سطر."),
            one: Some("يحتاج القيد إلى سطرين على الأقل، ولا يوجد سوى سطر واحد."),
            two: Some("يحتاج القيد إلى سطرين على الأقل."),
            few: Some("يحتاج القيد إلى سطرين على الأقل، والموجود {n} أسطر."),
            many: Some("يحتاج القيد إلى سطرين على الأقل، والموجود {n} سطرًا."),
            other: "يحتاج القيد إلى سطرين على الأقل، والموجود {n} سطر.",
        },
    ),
    (
        MIXED_CURRENCIES,
        Locale::English,
        Template::Simple("This entry is in {expected}, but a line is in {found}."),
    ),
    (
        MIXED_CURRENCIES,
        Locale::Arabic,
        Template::Simple("هذا القيد بعملة {expected}، لكن أحد السطور بعملة {found}."),
    ),
    (
        DOES_NOT_BALANCE,
        Locale::English,
        Template::Simple("Debits and credits differ by {difference}."),
    ),
    (
        DOES_NOT_BALANCE,
        Locale::Arabic,
        Template::Simple("يوجد فرق بين المدين والدائن مقداره {difference}."),
    ),
    (
        ZERO_LINE,
        Locale::English,
        Template::Simple("A line cannot be for zero."),
    ),
    (
        ZERO_LINE,
        Locale::Arabic,
        Template::Simple("لا يمكن أن يكون السطر بقيمة صفر."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::English,
        Template::Simple("That amount is too large to record."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::Arabic,
        Template::Simple("هذا المبلغ أكبر من أن يُسجَّل."),
    ),
    (
        NO_SUCH_ENTRY,
        Locale::English,
        Template::Simple("There is no entry {entry}."),
    ),
    (
        NO_SUCH_ENTRY,
        Locale::Arabic,
        Template::Simple("لا يوجد قيد {entry}."),
    ),
    (
        NO_SUCH_BRANCH,
        Locale::English,
        Template::Simple(
            "There is no open branch {branch}. A document can only be dated to a branch that exists and is still trading.",
        ),
    ),
    (
        NO_SUCH_BRANCH,
        Locale::Arabic,
        Template::Simple("لا يوجد فرع مفتوح {branch}. المستند لا يُنسب إلا لفرع قائم وما زال يعمل."),
    ),
    (
        ALREADY_REVERSED,
        Locale::English,
        Template::Simple("That entry was already reversed by {by}."),
    ),
    (
        ALREADY_REVERSED,
        Locale::Arabic,
        Template::Simple("تم عكس هذا القيد بالفعل بواسطة {by}."),
    ),
    (
        CLOSING_ENTRY,
        Locale::English,
        Template::Simple("Entry {entry} is a year's closing entry. Reopen the year to undo it."),
    ),
    (
        CLOSING_ENTRY,
        Locale::Arabic,
        Template::Simple("القيد {entry} قيد إقفال سنة. أعد فتح السنة لإلغائه."),
    ),
    (
        PERIOD_OUT_OF_ORDER,
        Locale::English,
        Template::Simple("Periods close in order. Close {next} before {period}."),
    ),
    (
        PERIOD_OUT_OF_ORDER,
        Locale::Arabic,
        Template::Simple("تُقفل الفترات بالترتيب. أقفل {next} قبل {period}."),
    ),
    (
        PERIOD_NOT_LATEST,
        Locale::English,
        Template::Simple(
            "Only the latest closed period can be reopened, and that is {latest}, not {period}.",
        ),
    ),
    (
        PERIOD_NOT_LATEST,
        Locale::Arabic,
        Template::Simple("لا يُعاد فتح إلا آخر فترة مقفلة، وهي {latest} لا {period}."),
    ),
    (
        YEAR_BOOKED,
        Locale::English,
        Template::Simple(
            "{period} belongs to {year}, which is booked. Reopen the year before its periods.",
        ),
    ),
    (
        YEAR_BOOKED,
        Locale::Arabic,
        Template::Simple("{period} من السنة {year} المُقفلة دفتريًا. أعد فتح السنة قبل فتراتها."),
    ),
    (
        YEAR_OPEN,
        Locale::English,
        Template::Simple("{year} cannot be booked while {period} is still open. Close it first."),
    ),
    (
        YEAR_OPEN,
        Locale::Arabic,
        Template::Simple("لا يمكن إقفال السنة {year} دفتريًا و{period} ما زالت مفتوحة. أقفلها أولًا."),
    ),
    (
        LATER_YEAR_BOOKED,
        Locale::English,
        Template::Simple(
            "{year} cannot be reopened while {later} is booked. Reopen {later} first.",
        ),
    ),
    (
        LATER_YEAR_BOOKED,
        Locale::Arabic,
        Template::Simple(
            "لا يمكن إعادة فتح السنة {year} والسنة {later} مقفلة دفتريًا. أعد فتح {later} أولًا.",
        ),
    ),
    (
        CLOSING_NEEDS_ACCOUNT,
        Locale::English,
        Template::Simple(
            "No retained-earnings account holds {currency}, so {year} cannot be booked. \
             Name one under the closing accounts.",
        ),
    ),
    (
        CLOSING_NEEDS_ACCOUNT,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد حساب أرباح مبقاة بعملة {currency}، فلا يمكن إقفال السنة {year} دفتريًا. \
             حدّد حسابًا في حسابات الإقفال.",
        ),
    ),
    (
        NOT_A_YEAR_START,
        Locale::English,
        Template::Simple(
            "A new calendar starts on a fiscal-year boundary of the current one. {starts_on} is \
             not; the next is {next}.",
        ),
    ),
    (
        NOT_A_YEAR_START,
        Locale::Arabic,
        Template::Simple(
            "يبدأ التقويم الجديد عند بداية سنة مالية في التقويم الحالي. {starts_on} ليس كذلك؛ \
             البداية التالية {next}.",
        ),
    ),
    (
        NOT_A_YEAR,
        Locale::English,
        Template::Simple("{year} is not a fiscal year. Years read like 2026."),
    ),
    (
        NOT_A_YEAR,
        Locale::Arabic,
        Template::Simple("{year} ليست سنة مالية. تُكتب السنوات هكذا: 2026."),
    ),
    (
        CLOSING_MEMO,
        Locale::English,
        Template::Simple("Closing entry for fiscal year {year}"),
    ),
    (
        CLOSING_MEMO,
        Locale::Arabic,
        Template::Simple("قيد إقفال السنة المالية {year}"),
    ),
    (
        REOPENING_MEMO,
        Locale::English,
        Template::Simple("Reopening fiscal year {year}"),
    ),
    (
        REOPENING_MEMO,
        Locale::Arabic,
        Template::Simple("إعادة فتح السنة المالية {year}"),
    ),
    (
        LEDGER_BEHIND,
        Locale::English,
        Template::Simple(
            "The ledger's figures are {behind} events behind the log, so the year cannot be \
             booked yet. Try again in a moment.",
        ),
    ),
    (
        LEDGER_BEHIND,
        Locale::Arabic,
        Template::Simple(
            "أرقام دفتر الأستاذ متأخرة عن السجل بـ {behind} حدثًا، فلا يمكن إقفال السنة دفتريًا \
             بعد. حاول بعد قليل.",
        ),
    ),
];
