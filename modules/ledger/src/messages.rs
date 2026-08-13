//! The ledger's messages, in every supported language.

use spa_i18n::{Locale, MessageCode, Template};

pub const ACCOUNT_EXISTS: MessageCode = MessageCode::new("ledger.account_exists");
pub const NO_SUCH_ACCOUNT: MessageCode = MessageCode::new("ledger.no_such_account");
pub const ACCOUNT_CLOSED: MessageCode = MessageCode::new("ledger.account_closed");
pub const ALREADY_POSTED: MessageCode = MessageCode::new("ledger.already_posted");
pub const TOO_FEW_LINES: MessageCode = MessageCode::new("ledger.too_few_lines");
pub const MIXED_CURRENCIES: MessageCode = MessageCode::new("ledger.mixed_currencies");
pub const DOES_NOT_BALANCE: MessageCode = MessageCode::new("ledger.does_not_balance");
pub const ZERO_LINE: MessageCode = MessageCode::new("ledger.zero_line");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("ledger.amount_out_of_range");

pub static CODES: &[MessageCode] = &[
    ACCOUNT_EXISTS,
    NO_SUCH_ACCOUNT,
    ACCOUNT_CLOSED,
    ALREADY_POSTED,
    TOO_FEW_LINES,
    MIXED_CURRENCIES,
    DOES_NOT_BALANCE,
    ZERO_LINE,
    AMOUNT_OUT_OF_RANGE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
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
];
