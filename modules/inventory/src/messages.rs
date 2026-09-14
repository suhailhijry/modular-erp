//! This module's refusals, in every supported language.

use erp_i18n::{Locale, MessageCode, Template};

pub const NO_SUCH_PRODUCT: MessageCode = MessageCode::new("inventory.no_such_product");
pub const NOT_A_QUANTITY: MessageCode = MessageCode::new("inventory.not_a_quantity");
pub const NOT_A_VALUE: MessageCode = MessageCode::new("inventory.not_a_value");
pub const WRONG_CURRENCY: MessageCode = MessageCode::new("inventory.wrong_currency");
pub const NEEDS_A_NAME_AND_A_UNIT: MessageCode =
    MessageCode::new("inventory.needs_a_name_and_a_unit");
pub const NOT_A_PRODUCT_ID: MessageCode = MessageCode::new("inventory.not_a_product_id");
pub const NOT_A_TRACKING_MODE: MessageCode = MessageCode::new("inventory.not_a_tracking_mode");
pub const NEEDS_A_LOT_CODE: MessageCode = MessageCode::new("inventory.needs_a_lot_code");
pub const NOT_A_LOT_PRODUCT: MessageCode = MessageCode::new("inventory.not_a_lot_product");
pub const NEEDS_SERIALS: MessageCode = MessageCode::new("inventory.needs_serials");
pub const NOT_A_SERIAL_PRODUCT: MessageCode = MessageCode::new("inventory.not_a_serial_product");
pub const SERIAL_ALREADY_HELD: MessageCode = MessageCode::new("inventory.serial_already_held");
pub const NO_SUCH_SERIAL: MessageCode = MessageCode::new("inventory.no_such_serial");
pub const NO_SUCH_LOT: MessageCode = MessageCode::new("inventory.no_such_lot");
pub const LOT_IS_SHORT: MessageCode = MessageCode::new("inventory.lot_is_short");
pub const NOT_ENOUGH_STOCK: MessageCode = MessageCode::new("inventory.not_enough_stock");
pub const NOT_CONSUMED: MessageCode = MessageCode::new("inventory.not_consumed");
pub const MORE_THAN_WAS_TAKEN: MessageCode = MessageCode::new("inventory.more_than_was_taken");
pub const NAMED_UNITS_COME_BACK_WHOLE: MessageCode =
    MessageCode::new("inventory.named_units_come_back_whole");
pub const NO_LOT_TO_JOIN: MessageCode = MessageCode::new("inventory.no_lot_to_join");
pub const NOT_OUT: MessageCode = MessageCode::new("inventory.not_out");
pub const NOT_A_REASON: MessageCode = MessageCode::new("inventory.not_a_reason");
pub const NOT_A_WINDOW: MessageCode = MessageCode::new("inventory.not_a_window");
pub const NOT_A_REFERENCE: MessageCode = MessageCode::new("inventory.not_a_reference");
pub const AMOUNT_OUT_OF_RANGE: MessageCode = MessageCode::new("inventory.amount_out_of_range");
pub const DATABASE: MessageCode = MessageCode::new("inventory.database");

pub const CODES: &[MessageCode] = &[
    NO_SUCH_PRODUCT,
    NOT_A_QUANTITY,
    NOT_A_VALUE,
    WRONG_CURRENCY,
    NEEDS_A_NAME_AND_A_UNIT,
    NOT_A_PRODUCT_ID,
    NOT_A_TRACKING_MODE,
    NEEDS_A_LOT_CODE,
    NOT_A_LOT_PRODUCT,
    NEEDS_SERIALS,
    NOT_A_SERIAL_PRODUCT,
    SERIAL_ALREADY_HELD,
    NO_SUCH_SERIAL,
    NO_SUCH_LOT,
    LOT_IS_SHORT,
    NOT_ENOUGH_STOCK,
    NOT_CONSUMED,
    MORE_THAN_WAS_TAKEN,
    NAMED_UNITS_COME_BACK_WHOLE,
    NO_LOT_TO_JOIN,
    NOT_OUT,
    NOT_A_REASON,
    NOT_A_WINDOW,
    NOT_A_REFERENCE,
    AMOUNT_OUT_OF_RANGE,
    DATABASE,
];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        NOT_CONSUMED,
        Locale::English,
        Template::Simple(
            "This shelf has no record of {reference} going out, so there is nothing to put back \
             and no cost to put it back at.",
        ),
    ),
    (
        NOT_CONSUMED,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد في هذا الرف سجل لخروج {reference}، فلا شيء يُعاد ولا تكلفة يُعاد بها.",
        ),
    ),
    (
        MORE_THAN_WAS_TAKEN,
        Locale::English,
        Template::Simple(
            "{taken} of that movement is still out and {wanted} are coming back. A second \
             credit note can only return what the first one left.",
        ),
    ),
    (
        MORE_THAN_WAS_TAKEN,
        Locale::Arabic,
        Template::Simple(
            "ما زال خارجًا من تلك الحركة {taken}، والمرتجع {wanted}. لا يعيد إشعار دائن ثانٍ إلا ما تركه الأول.",
        ),
    ),
    (
        NAMED_UNITS_COME_BACK_WHOLE,
        Locale::English,
        Template::Simple(
            "{taken} named units went out together; name the ones that came back, or return \
             them all. Which of them came back is not something this system may guess.",
        ),
    ),
    (
        NAMED_UNITS_COME_BACK_WHOLE,
        Locale::Arabic,
        Template::Simple(
            "خرجت {taken} وحدات مُسمّاة معًا؛ سمِّ ما عاد منها أو أرجعها كلها، فالنظام لا يخمّن أيّها عاد.",
        ),
    ),
    (
        NO_SUCH_PRODUCT,
        Locale::English,
        Template::Simple("There is no product {id}. Declare it before moving stock of it."),
    ),
    (
        NO_SUCH_PRODUCT,
        Locale::Arabic,
        Template::Simple("لا يوجد صنف {id}. عرّف الصنف قبل تسجيل حركة عليه."),
    ),
    (
        NOT_A_QUANTITY,
        Locale::English,
        Template::Simple(
            "A quantity here is a whole number of the product's own unit, and more than nothing.",
        ),
    ),
    (
        NOT_A_QUANTITY,
        Locale::Arabic,
        Template::Simple("الكمية هنا عدد صحيح بوحدة الصنف نفسها، وأكبر من الصفر."),
    ),
    (
        NOT_A_VALUE,
        Locale::English,
        Template::Simple("What a delivery cost is an amount of money, and more than nothing."),
    ),
    (
        NOT_A_VALUE,
        Locale::Arabic,
        Template::Simple("قيمة التوريد مبلغ من المال، وأكبر من الصفر."),
    ),
    (
        WRONG_CURRENCY,
        Locale::English,
        Template::Simple(
            "Stock is kept in one currency, and {id} is kept in {kept}. \
             Record this delivery in {kept}.",
        ),
    ),
    (
        WRONG_CURRENCY,
        Locale::Arabic,
        Template::Simple(
            "المخزون يُقيَّد بعملة واحدة، و{id} مُقيَّد بـ{kept}. \
             سجّل هذا التوريد بـ{kept}.",
        ),
    ),
    (
        NEEDS_A_NAME_AND_A_UNIT,
        Locale::English,
        Template::Simple(
            "A product needs a name and the unit it is counted in — grams, pieces. \
             The unit is frozen once it is declared.",
        ),
    ),
    (
        NEEDS_A_NAME_AND_A_UNIT,
        Locale::Arabic,
        Template::Simple(
            "الصنف يحتاج إلى اسم وإلى الوحدة التي يُعدّ بها — جرام، قطعة. \
             ولا يمكن تغيير الوحدة بعد التعريف.",
        ),
    ),
    (
        NOT_A_PRODUCT_ID,
        Locale::English,
        Template::Simple(
            "{id} cannot be a product: a product is named by the key it was declared under.",
        ),
    ),
    (
        NOT_A_PRODUCT_ID,
        Locale::Arabic,
        Template::Simple("{id} لا يصلح معرّفًا لصنف: الصنف يُسمّى بالمفتاح الذي عُرّف به."),
    ),
    (
        NOT_A_TRACKING_MODE,
        Locale::English,
        Template::Simple(
            "{tracking} is not a way of tracking a product: none, lot or serial. \
             It is frozen at declaration, so a wrong one is a new product.",
        ),
    ),
    (
        NOT_A_TRACKING_MODE,
        Locale::Arabic,
        Template::Simple(
            "{tracking} ليست طريقة تتبّع للصنف: none أو lot أو serial. \
             وطريقة التتبّع تُثبّت عند التعريف، فالخطأ فيها يعني صنفًا جديدًا.",
        ),
    ),
    (
        NEEDS_A_LOT_CODE,
        Locale::English,
        Template::Simple(
            "A lot-tracked product's delivery names the batch it came in. Send a code.",
        ),
    ),
    (
        NEEDS_A_LOT_CODE,
        Locale::Arabic,
        Template::Simple("توريد صنف متتبَّع بالتشغيلة يذكر رقم التشغيلة. أرسل الرمز."),
    ),
    (
        NOT_A_LOT_PRODUCT,
        Locale::English,
        Template::Simple(
            "{id} is not lot-tracked, so a delivery of it carries no batch code and \
             no expiry date.",
        ),
    ),
    (
        NOT_A_LOT_PRODUCT,
        Locale::Arabic,
        Template::Simple("{id} غير متتبَّع بالتشغيلة، فلا يحمل توريده رمز تشغيلة ولا تاريخ انتهاء."),
    ),
    (
        NEEDS_SERIALS,
        Locale::English,
        Template::Simple(
            "A serial-tracked product names one serial per unit: {units} units, \
             {named} named.",
        ),
    ),
    (
        NEEDS_SERIALS,
        Locale::Arabic,
        Template::Simple(
            "الصنف المتتبَّع بالأرقام التسلسلية يذكر رقمًا لكل وحدة: {units} وحدة، \
             و{named} رقمًا.",
        ),
    ),
    (
        NOT_A_SERIAL_PRODUCT,
        Locale::English,
        Template::Simple("{id} is not serial-tracked, so its units have no serials to name."),
    ),
    (
        NOT_A_SERIAL_PRODUCT,
        Locale::Arabic,
        Template::Simple("{id} غير متتبَّع بالأرقام التسلسلية، فليس لوحداته أرقام تُذكر."),
    ),
    (
        SERIAL_ALREADY_HELD,
        Locale::English,
        Template::Simple(
            "{serial} is already on the shelf. A serial is an identity, and two units \
             cannot share one.",
        ),
    ),
    (
        SERIAL_ALREADY_HELD,
        Locale::Arabic,
        Template::Simple(
            "{serial} موجود في المخزون بالفعل. الرقم التسلسلي هوية، ولا تتشارك وحدتان هوية واحدة.",
        ),
    ),
    (
        NO_SUCH_SERIAL,
        Locale::English,
        Template::Simple(
            "{serial} is not on the shelf: it was never received, it has already gone, \
             or this movement named it twice.",
        ),
    ),
    (
        NO_SUCH_SERIAL,
        Locale::Arabic,
        Template::Simple(
            "{serial} ليس في المخزون: إمّا لم يُستلم، أو خرج بالفعل، \
             أو ذُكر مرّتين في هذه الحركة.",
        ),
    ),
    (
        NO_SUCH_LOT,
        Locale::English,
        Template::Simple("There is no open lot {lot} on this shelf."),
    ),
    (
        NO_SUCH_LOT,
        Locale::Arabic,
        Template::Simple("لا توجد تشغيلة مفتوحة {lot} في هذا المخزون."),
    ),
    (
        LOT_IS_SHORT,
        Locale::English,
        Template::Simple(
            "Lot {lot} holds {held} and {wanted} were asked for. Naming a lot is a \
             claim about that lot, so it is not topped up from another.",
        ),
    ),
    (
        LOT_IS_SHORT,
        Locale::Arabic,
        Template::Simple(
            "التشغيلة {lot} تحوي {held} والمطلوب {wanted}. \
             تحديد التشغيلة إقرار بما فيها، فلا تُكمَّل من تشغيلة أخرى.",
        ),
    ),
    (
        NOT_ENOUGH_STOCK,
        Locale::English,
        Template::Simple(
            "There are {held} on the shelf and {wanted} were asked for. \
             Count the shelf if that is wrong.",
        ),
    ),
    (
        NOT_ENOUGH_STOCK,
        Locale::Arabic,
        Template::Simple("في المخزون {held} والمطلوب {wanted}. إن كان هذا خطأً فاجرد المخزون."),
    ),
    (
        NO_LOT_TO_JOIN,
        Locale::English,
        Template::Simple(
            "{found} more were counted than the lots on this shelf hold, and no lot is open \
             to add them to. Receive them as a delivery, at what they cost.",
        ),
    ),
    (
        NO_LOT_TO_JOIN,
        Locale::Arabic,
        Template::Simple(
            "عُدّ {found} أكثر مما تحويه تشغيلات هذا المخزون، ولا توجد تشغيلة مفتوحة تُضاف إليها. \
             استلمها توريدًا بتكلفتها.",
        ),
    ),
    (
        NOT_OUT,
        Locale::English,
        Template::Simple(
            "{serial} is not out on that sale: it did not go out on it, or it has already come \
             back.",
        ),
    ),
    (
        NOT_OUT,
        Locale::Arabic,
        Template::Simple("{serial} ليس خارجًا على تلك البيعة: لم يخرج عليها، أو عاد من قبل."),
    ),
    (
        NOT_A_REASON,
        Locale::English,
        Template::Simple("{reason} is not a reason to write stock off: expired or damaged."),
    ),
    (
        NOT_A_REASON,
        Locale::Arabic,
        Template::Simple("{reason} ليس سببًا لإعدام المخزون: expired أو damaged."),
    ),
    (
        NOT_A_WINDOW,
        Locale::English,
        Template::Simple(
            "An expiry warning window is a whole number of days, from none to ten years.",
        ),
    ),
    (
        NOT_A_WINDOW,
        Locale::Arabic,
        Template::Simple("مهلة التنبيه قبل الانتهاء عدد صحيح من الأيام، من صفر إلى عشر سنوات."),
    ),
    (
        NOT_A_REFERENCE,
        Locale::English,
        Template::Simple(
            "{shelf} and {reference} are too long together to name the accounting entry \
             this movement posts. A shorter branch id, or a shorter key, would fit.",
        ),
    ),
    (
        NOT_A_REFERENCE,
        Locale::Arabic,
        Template::Simple(
            "{shelf} و{reference} أطول معًا من أن يُسمّى بهما القيد المحاسبي لهذه الحركة. \
             يكفي معرّف فرع أقصر أو مفتاح أقصر.",
        ),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::English,
        Template::Simple("That amount is too large to record."),
    ),
    (
        AMOUNT_OUT_OF_RANGE,
        Locale::Arabic,
        Template::Simple("المبلغ أكبر من أن يُسجَّل."),
    ),
    (
        DATABASE,
        Locale::English,
        Template::Simple("The stock could not be read. Try again."),
    ),
    (
        DATABASE,
        Locale::Arabic,
        Template::Simple("تعذّرت قراءة المخزون. حاول مرة أخرى."),
    ),
];
