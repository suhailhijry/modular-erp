//! The branches module's messages, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_BRANCH: MessageCode = MessageCode::new("branches.no_such_branch");
pub const CLOSED: MessageCode = MessageCode::new("branches.closed");
pub const NO_NAME: MessageCode = MessageCode::new("branches.no_name");
pub const NO_ADDRESS: MessageCode = MessageCode::new("branches.no_address");
pub const NOT_A_COUNTRY: MessageCode = MessageCode::new("branches.not_a_country");

pub static CODES: &[MessageCode] = &[NO_SUCH_BRANCH, CLOSED, NO_NAME, NO_ADDRESS, NOT_A_COUNTRY];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NO_SUCH_BRANCH,
        Locale::English,
        Template::Simple("There is no branch {id}."),
    ),
    (
        NO_SUCH_BRANCH,
        Locale::Arabic,
        Template::Simple("لا يوجد فرع {id}."),
    ),
    (
        CLOSED,
        Locale::English,
        Template::Simple(
            "Branch {id} is closed and takes no new documents. Its old ones are unaffected.",
        ),
    ),
    (
        CLOSED,
        Locale::Arabic,
        Template::Simple("الفرع {id} مغلق ولا يستقبل مستندات جديدة. مستنداته السابقة كما هي."),
    ),
    (
        NO_NAME,
        Locale::English,
        Template::Simple("A branch needs a name."),
    ),
    (
        NO_NAME,
        Locale::Arabic,
        Template::Simple("الفرع يحتاج إلى اسم."),
    ),
    (
        NO_ADDRESS,
        Locale::English,
        Template::Simple("A branch needs a street and a city."),
    ),
    (
        NO_ADDRESS,
        Locale::Arabic,
        Template::Simple("الفرع يحتاج إلى شارع ومدينة."),
    ),
    (
        NOT_A_COUNTRY,
        Locale::English,
        Template::Simple("{country} is not a two-letter ISO 3166-1 country code."),
    ),
    (
        NOT_A_COUNTRY,
        Locale::Arabic,
        Template::Simple("{country} ليس رمز دولة من حرفين وفق ISO 3166-1."),
    ),
];
