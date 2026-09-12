//! Control-plane message codes and their translations.
//!
//! Two things live here that must stay in step, and `tests/localization.rs`
//! enforces it: the codes, and a translation of each in every supported locale.
//!
//! # On wording
//!
//! What a user is told is not what an operator is told. `NoSuchTenant` and
//! `NotAMember` are distinct errors internally — one is a retry, the other is
//! not — but they render identically, because telling an attacker which tenant
//! slugs exist is a free enumeration oracle. The distinction survives in logs
//! and in the `Display` impl; it does not survive to the response.

use erp_i18n::{Locale, MessageCode, Template};

/// Moved to `erp-tenant` with the types that render as them; re-exported so
/// existing callers are unchanged.
pub use erp_tenant::messages::{INTERNAL, OVERLOADED};

// ---------------------------------------------------------------------------
// Codes
// ---------------------------------------------------------------------------

pub const NO_SUCH_IDENTITY: MessageCode = MessageCode::new("access.no_such_identity");
pub const IDENTITY_SUSPENDED: MessageCode = MessageCode::new("access.identity_suspended");
pub const TENANT_UNAVAILABLE: MessageCode = MessageCode::new("access.tenant_unavailable");
pub const TENANT_PROVISIONING: MessageCode = MessageCode::new("access.tenant_provisioning");
pub const ACCESS_DENIED: MessageCode = MessageCode::new("access.denied");
pub const NO_CAPACITY: MessageCode = MessageCode::new("provisioning.no_capacity");
/// Operator-facing. Shown on an ops surface, never to a tenant — a signup form
/// has no business mentioning how our clusters are doing.
pub const CLUSTERS_AT_LIMIT: MessageCode = MessageCode::new("ops.clusters_at_limit");
pub const SLUG_TAKEN: MessageCode = MessageCode::new("provisioning.slug_taken");
pub const DOMAIN_NOT_CLAIMED: MessageCode = MessageCode::new("domains.not_claimed");
pub const DOMAIN_NOT_PROVED: MessageCode = MessageCode::new("domains.not_proved");
pub const DOMAIN_PROOF_UNAVAILABLE: MessageCode = MessageCode::new("domains.proof_unavailable");
pub const NOT_AN_ORIGIN: MessageCode = MessageCode::new("origins.not_an_origin");
pub const ORIGIN_OUTSIDE_DOMAIN: MessageCode = MessageCode::new("origins.outside_domain");
/// Wrong handle, wrong password, unknown handle, suspended identity — one
/// message for all four, for the same reason `NoSuchTenant` and `NotAMember`
/// share one.
pub const INVALID_CREDENTIALS: MessageCode = MessageCode::new("auth.invalid_credentials");
pub const SECOND_FACTOR_REQUIRED: MessageCode = MessageCode::new("auth.second_factor_required");
pub const TENANT_REQUIRES_SECOND_FACTOR: MessageCode =
    MessageCode::new("auth.tenant_requires_second_factor");
pub const HANDLE_TAKEN: MessageCode = MessageCode::new("auth.handle_taken");
pub const SESSION_EXPIRED: MessageCode = MessageCode::new("auth.session_expired");
/// 403, naming the capability. "Ask someone with permission" is only actionable
/// when you know which permission.
pub const NOT_PERMITTED: MessageCode = MessageCode::new("access.not_permitted");
pub const ALREADY_A_MEMBER: MessageCode = MessageCode::new("members.already_a_member");
pub const NOT_A_MEMBER: MessageCode = MessageCode::new("members.not_a_member");
pub const INVITATION_NOT_VALID: MessageCode = MessageCode::new("invitations.not_valid");

/// No live confirmation link for that token. Wrong, expired, cancelled and
/// already-used all render as this one, for the same reason
/// [`INVITATION_NOT_VALID`] does.
pub const SIGNUP_NOT_VALID: MessageCode = MessageCode::new("signups.not_valid");
/// A confirmation went to this address moments ago, so the next one waits.
/// Answered as a 429.
///
/// Carries the seconds. "Too soon" with no number is a page people reload.
pub const SIGNUP_TOO_SOON: MessageCode = MessageCode::new("signups.too_soon");
/// The subject line of a signup confirmation.
pub const SIGNUP_SUBJECT: MessageCode = MessageCode::new("mail.signup_subject");
/// The body of a signup confirmation.
pub const SIGNUP_BODY: MessageCode = MessageCode::new("mail.signup_body");
pub const RESET_SUBJECT: MessageCode = MessageCode::new("mail.reset_subject");
pub const RESET_BODY: MessageCode = MessageCode::new("mail.reset_body");
pub const RESET_NOT_VALID: MessageCode = MessageCode::new("auth.reset_not_valid");
pub const RESET_TOO_SOON: MessageCode = MessageCode::new("auth.reset_too_soon");
pub const LAST_OWNER: MessageCode = MessageCode::new("members.last_owner");
/// Staff whose role may, with no second factor. A 403, and not `not_permitted`:
/// the one thing they can do about it is enrol.
pub const STAFF_SECOND_FACTOR_REQUIRED: MessageCode =
    MessageCode::new("access.staff_second_factor_required");
/// Staff turning their second factor off. A 403: the answer is "come off the
/// staff first", not "try again".
pub const STAFF_KEEPS_SECOND_FACTOR: MessageCode =
    MessageCode::new("auth.staff_keeps_second_factor");
/// A member of a tenant that requires a second factor turning theirs off. A
/// 403, for the same reason: the answer is the owner's — removing them, or no
/// longer requiring it. No route lets a member leave on their own.
pub const TENANT_KEEPS_SECOND_FACTOR: MessageCode =
    MessageCode::new("auth.tenant_keeps_second_factor");
/// Enrolling a second factor on an account whose factor somebody else reset,
/// without the link that was mailed. A 403: the password is not the missing
/// thing, and it holds after the link expires.
pub const ENROLMENT_LINK_REQUIRED: MessageCode = MessageCode::new("auth.enrolment_link_required");
/// Resetting your own second factor, at either reset route. Removing your own
/// is `DELETE /v1/sessions/second-factor`, which costs a code and is refused
/// outright where a factor is required — this must not become a way round it.
pub const RESET_YOURSELF: MessageCode = MessageCode::new("second_factor.reset_yourself");
/// A member resetting the owner's second factor.
pub const RESET_THE_OWNER: MessageCode = MessageCode::new("second_factor.reset_the_owner");
/// A member resetting a platform staff member's second factor.
pub const RESET_PLATFORM_STAFF: MessageCode =
    MessageCode::new("second_factor.reset_platform_staff");
/// The target belongs to another company too, so no one company may weaken
/// their sign-in. **It says to contact support**, who can.
pub const RESET_ANOTHER_COMPANY: MessageCode =
    MessageCode::new("second_factor.reset_another_company");
/// Nothing to mail the link to: the account has no password login, so no
/// address. Refused rather than resetting into a lockout (L6).
pub const RESET_NO_LOGIN: MessageCode = MessageCode::new("second_factor.reset_no_login");
/// A platform reset with no reason, or one over 500 characters.
pub const RESET_REASON: MessageCode = MessageCode::new("second_factor.reset_reason");
/// The subject line of the enrolment link mailed after a reset.
pub const ENROLMENT_SUBJECT: MessageCode = MessageCode::new("mail.enrolment_subject");
/// Its body.
pub const ENROLMENT_BODY: MessageCode = MessageCode::new("mail.enrolment_body");
pub const STAFF_NO_SUCH_ACCOUNT: MessageCode = MessageCode::new("staff.no_such_account");
pub const ALREADY_STAFF: MessageCode = MessageCode::new("staff.already_staff");
pub const STAFF_NO_SECOND_FACTOR: MessageCode = MessageCode::new("staff.no_second_factor");
pub const NOT_STAFF: MessageCode = MessageCode::new("staff.not_staff");
pub const LAST_SUPERADMIN: MessageCode = MessageCode::new("staff.last_superadmin");
/// Suspending a tenant that is not active, reinstating one that is not
/// suspended. Staff-facing, and it names both statuses.
pub const WRONG_TENANT_STATUS: MessageCode = MessageCode::new("tenants.wrong_status");
pub const SUSPENSION_REASON: MessageCode = MessageCode::new("tenants.suspension_reason");
/// Not a phone number this system can send to.
pub const NOT_A_PHONE_NUMBER: MessageCode = MessageCode::new("codes.not_a_phone_number");
/// A code went to this number moments ago, so the next one waits. Answered as a
/// 429, carrying the seconds — "too soon" with no number is a button people
/// press again.
pub const CODE_TOO_SOON: MessageCode = MessageCode::new("codes.too_soon");
/// **One answer for every way a verification fails.** Wrong, expired, spent, out
/// of attempts and never issued all render as this, because distinguishing them
/// says whether the number is known and whether a code is outstanding.
pub const CODE_NOT_VALID: MessageCode = MessageCode::new("codes.not_valid");
/// The text a one-time code arrives in. **Outgoing, not a refusal** — the third
/// message in this system that is, after the two email bodies.
pub const CODE_TEXT: MessageCode = MessageCode::new("mail.code_text");
/// A scope that is not one. `module:capability`, or `*:capability`.
pub const NOT_A_SCOPE: MessageCode = MessageCode::new("keys.not_a_scope");
/// The key is real and does not carry the scope this route needs. A 403, and it
/// names the scope — "ask for a key that can do this" is only actionable when
/// you know which scope to ask for.
pub const OUT_OF_SCOPE: MessageCode = MessageCode::new("keys.out_of_scope");
/// A route about a person, asked by a key. No scope reaches it, so this names
/// none: the answer is to sign in.
pub const NOT_A_PERSON: MessageCode = MessageCode::new("keys.not_a_person");
/// No key by that id in this tenant.
pub const NO_SUCH_KEY: MessageCode = MessageCode::new("keys.no_such_key");

/// Every code this crate can produce. The completeness test walks this list.
/// The subject line of an invitation email. Not an error — the first message
/// code in this system that is *outgoing* rather than a refusal.
pub const INVITATION_SUBJECT: MessageCode = MessageCode::new("mail.invitation_subject");
/// The body of an invitation email.
pub const INVITATION_BODY: MessageCode = MessageCode::new("mail.invitation_body");

pub static CODES: &[MessageCode] = &[
    DOMAIN_NOT_CLAIMED,
    DOMAIN_NOT_PROVED,
    DOMAIN_PROOF_UNAVAILABLE,
    NOT_AN_ORIGIN,
    ORIGIN_OUTSIDE_DOMAIN,
    INVITATION_SUBJECT,
    INVITATION_BODY,
    NO_SUCH_IDENTITY,
    IDENTITY_SUSPENDED,
    TENANT_UNAVAILABLE,
    TENANT_PROVISIONING,
    ACCESS_DENIED,
    NO_CAPACITY,
    CLUSTERS_AT_LIMIT,
    SLUG_TAKEN,
    INVALID_CREDENTIALS,
    SECOND_FACTOR_REQUIRED,
    TENANT_REQUIRES_SECOND_FACTOR,
    HANDLE_TAKEN,
    SESSION_EXPIRED,
    NOT_PERMITTED,
    ALREADY_A_MEMBER,
    NOT_A_MEMBER,
    INVITATION_NOT_VALID,
    LAST_OWNER,
    STAFF_SECOND_FACTOR_REQUIRED,
    STAFF_KEEPS_SECOND_FACTOR,
    TENANT_KEEPS_SECOND_FACTOR,
    ENROLMENT_LINK_REQUIRED,
    RESET_YOURSELF,
    RESET_THE_OWNER,
    RESET_PLATFORM_STAFF,
    RESET_ANOTHER_COMPANY,
    RESET_NO_LOGIN,
    RESET_REASON,
    ENROLMENT_SUBJECT,
    ENROLMENT_BODY,
    STAFF_NO_SUCH_ACCOUNT,
    ALREADY_STAFF,
    STAFF_NO_SECOND_FACTOR,
    NOT_STAFF,
    LAST_SUPERADMIN,
    WRONG_TENANT_STATUS,
    SUSPENSION_REASON,
    SIGNUP_NOT_VALID,
    SIGNUP_TOO_SOON,
    SIGNUP_SUBJECT,
    SIGNUP_BODY,
    RESET_SUBJECT,
    RESET_BODY,
    RESET_NOT_VALID,
    RESET_TOO_SOON,
    NOT_A_SCOPE,
    OUT_OF_SCOPE,
    NOT_A_PERSON,
    NO_SUCH_KEY,
    NOT_A_PHONE_NUMBER,
    CODE_TOO_SOON,
    CODE_NOT_VALID,
    CODE_TEXT,
];

// ---------------------------------------------------------------------------
// Translations
// ---------------------------------------------------------------------------

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (
        DOMAIN_NOT_CLAIMED,
        Locale::English,
        Template::Simple(
            "{domain} has not been claimed by this business. Claim it first, then publish the record you are given.",
        ),
    ),
    (
        DOMAIN_NOT_CLAIMED,
        Locale::Arabic,
        Template::Simple(
            "لم يطالب هذا النشاط بالنطاق {domain}. طالب به أولًا، ثم انشر السجل الذي ستحصل عليه.",
        ),
    ),
    (
        DOMAIN_NOT_PROVED,
        Locale::English,
        Template::Simple(
            "{domain} is not proved yet. Publish a DNS TXT record at {record} with the value {expected}, then try again.",
        ),
    ),
    (
        DOMAIN_NOT_PROVED,
        Locale::Arabic,
        Template::Simple(
            "لم يُثبَت النطاق {domain} بعد. انشر سجل DNS من نوع TXT على {record} بالقيمة {expected}، ثم أعد المحاولة.",
        ),
    ),
    (
        DOMAIN_PROOF_UNAVAILABLE,
        Locale::English,
        Template::Simple("The domain could not be looked up right now. Try again shortly."),
    ),
    (
        DOMAIN_PROOF_UNAVAILABLE,
        Locale::Arabic,
        Template::Simple("تعذّر الاستعلام عن النطاق الآن. أعد المحاولة بعد قليل."),
    ),
    (
        NOT_AN_ORIGIN,
        Locale::English,
        Template::Simple(
            "{origin} is not an origin: it must be https:// followed by a host and, optionally, a port — nothing else.",
        ),
    ),
    (
        NOT_AN_ORIGIN,
        Locale::Arabic,
        Template::Simple(
            "{origin} ليس مصدرًا صالحًا: يجب أن يكون https:// متبوعًا بالمضيف، وبالمنفذ اختياريًا، ولا شيء غير ذلك.",
        ),
    ),
    (
        ORIGIN_OUTSIDE_DOMAIN,
        Locale::English,
        Template::Simple(
            "{origin} is not under {domain}. An origin is licensed by the proved domain it belongs to.",
        ),
    ),
    (
        ORIGIN_OUTSIDE_DOMAIN,
        Locale::Arabic,
        Template::Simple("{origin} ليس ضمن {domain}. يُرخَّص المصدر بالنطاق المُثبَت الذي ينتمي إليه."),
    ),
    // -- one-time codes ----------------------------------------------------
    // **Short on purpose.** An SMS is billed per 160 characters, or per 70 in
    // Arabic — see `messaging::channel` — and a code text that runs to two
    // segments costs twice for the length of a sentence nobody reads.
    (
        CODE_TEXT,
        Locale::English,
        Template::Simple("{code} is your sign-in code."),
    ),
    (
        CODE_TEXT,
        Locale::Arabic,
        Template::Simple("{code} رمز الدخول."),
    ),
    (
        NOT_A_PHONE_NUMBER,
        Locale::English,
        Template::Simple(
            "{number} is not a phone number. Include the country code, like +966500000000.",
        ),
    ),
    (
        NOT_A_PHONE_NUMBER,
        Locale::Arabic,
        Template::Simple("{number} ليس رقم هاتف. أدخِل رمز الدولة، مثل ‎+966500000000."),
    ),
    (
        CODE_TOO_SOON,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("A code was just sent. Try again in a second."),
            two: None,
            few: None,
            many: None,
            other: "A code was just sent. Try again in {n} seconds.",
        },
    ),
    (
        CODE_TOO_SOON,
        Locale::Arabic,
        Template::Plural {
            zero: Some("أُرسل رمز للتو. أعد المحاولة."),
            one: Some("أُرسل رمز للتو. أعد المحاولة بعد ثانية."),
            two: Some("أُرسل رمز للتو. أعد المحاولة بعد ثانيتين."),
            few: Some("أُرسل رمز للتو. أعد المحاولة بعد {n} ثوانٍ."),
            many: Some("أُرسل رمز للتو. أعد المحاولة بعد {n} ثانية."),
            other: "أُرسل رمز للتو. أعد المحاولة بعد {n} ثانية.",
        },
    ),
    (
        CODE_NOT_VALID,
        Locale::English,
        Template::Simple("That code is not valid. Ask for a new one."),
    ),
    (
        CODE_NOT_VALID,
        Locale::Arabic,
        Template::Simple("هذا الرمز غير صالح. اطلب رمزًا جديدًا."),
    ),
    // -- api keys ----------------------------------------------------------
    (
        NOT_A_SCOPE,
        Locale::English,
        Template::Simple(
            "{scope} is not a scope. Write it as `booking:read`, or `*:read` for every module.",
        ),
    ),
    (
        NOT_A_SCOPE,
        Locale::Arabic,
        Template::Simple("{scope} ليس نطاقًا. اكتبه هكذا: booking:read، أو ‎*:read لكل الوحدات."),
    ),
    (
        OUT_OF_SCOPE,
        Locale::English,
        Template::Simple("This key does not carry {scope}."),
    ),
    (
        OUT_OF_SCOPE,
        Locale::Arabic,
        Template::Simple("هذا المفتاح لا يحمل {scope}."),
    ),
    (
        NOT_A_PERSON,
        Locale::English,
        Template::Simple("This is about a person, and a key is not one. Sign in to read it."),
    ),
    (
        NOT_A_PERSON,
        Locale::Arabic,
        Template::Simple("هذا يخص شخصًا، والمفتاح ليس شخصًا. سجّل الدخول لتقرأه."),
    ),
    (
        NO_SUCH_KEY,
        Locale::English,
        Template::Simple("There is no key {id} here."),
    ),
    (
        NO_SUCH_KEY,
        Locale::Arabic,
        Template::Simple("لا يوجد مفتاح {id} هنا."),
    ),
    // -- identity ----------------------------------------------------------
    (
        NO_SUCH_IDENTITY,
        Locale::English,
        Template::Simple("We could not sign you in. Please sign in again."),
    ),
    (
        NO_SUCH_IDENTITY,
        Locale::Arabic,
        Template::Simple("تعذّر تسجيل دخولك. يُرجى تسجيل الدخول مرة أخرى."),
    ),
    (
        IDENTITY_SUSPENDED,
        Locale::English,
        Template::Simple("This account has been suspended. Contact your administrator."),
    ),
    (
        IDENTITY_SUSPENDED,
        Locale::Arabic,
        Template::Simple("تم تعليق هذا الحساب. يُرجى التواصل مع المسؤول."),
    ),
    // -- tenant availability ----------------------------------------------
    (
        TENANT_PROVISIONING,
        Locale::English,
        Template::Simple("Your workspace is still being set up. This usually takes a few seconds."),
    ),
    (
        TENANT_PROVISIONING,
        Locale::Arabic,
        Template::Simple("لا يزال إعداد مساحة العمل جارياً. عادةً ما يستغرق ذلك بضع ثوانٍ."),
    ),
    (
        TENANT_UNAVAILABLE,
        Locale::English,
        Template::Simple("This workspace is unavailable. Contact your administrator."),
    ),
    (
        TENANT_UNAVAILABLE,
        Locale::Arabic,
        Template::Simple("مساحة العمل هذه غير متاحة. يُرجى التواصل مع المسؤول."),
    ),
    // -- access ------------------------------------------------------------
    //
    // Deliberately identical for "no such tenant" and "not a member": a
    // different message for each would let an attacker enumerate tenants.
    (
        ACCESS_DENIED,
        Locale::English,
        Template::Simple("You do not have access to this workspace."),
    ),
    (
        ACCESS_DENIED,
        Locale::Arabic,
        Template::Simple("ليس لديك صلاحية الوصول إلى مساحة العمل هذه."),
    ),
    // -- system ------------------------------------------------------------
    // -- provisioning ------------------------------------------------------
    //
    // The plural forms here are the reason `Plural` exists. English needs two;
    // Arabic needs all six, and `n % 100` decides which.
    // User-facing: a retry, with no mention of our topology.
    (
        NO_CAPACITY,
        Locale::English,
        Template::Simple(
            "We could not create your workspace right now. Please try again in a few minutes.",
        ),
    ),
    (
        NO_CAPACITY,
        Locale::Arabic,
        Template::Simple("تعذّر إنشاء مساحة العمل الآن. يُرجى المحاولة بعد بضع دقائق."),
    ),
    // Operator-facing, and genuinely plural — the case that makes CLDR's six
    // Arabic categories load-bearing rather than theoretical.
    (
        CLUSTERS_AT_LIMIT,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("{n} cluster is at its limit."),
            two: None,
            few: None,
            many: None,
            other: "{n} clusters are at their limit.",
        },
    ),
    (
        CLUSTERS_AT_LIMIT,
        Locale::Arabic,
        Template::Plural {
            zero: Some("لا توجد مجموعات بلغت حدّها الأقصى."),
            one: Some("مجموعة واحدة بلغت حدّها الأقصى."),
            two: Some("مجموعتان بلغتا حدّهما الأقصى."),
            few: Some("{n} مجموعات بلغت حدّها الأقصى."),
            many: Some("{n} مجموعةً بلغت حدّها الأقصى."),
            other: "{n} مجموعة بلغت حدّها الأقصى.",
        },
    ),
    (
        SLUG_TAKEN,
        Locale::English,
        Template::Simple("The name {slug} is already taken. Please choose another."),
    ),
    (
        SLUG_TAKEN,
        Locale::Arabic,
        // `{slug}` is Latin script inside Arabic text; the renderer isolates it
        // so the sentence does not reorder around it.
        Template::Simple("الاسم {slug} مستخدم بالفعل. يُرجى اختيار اسم آخر."),
    ),
    // -- authentication ----------------------------------------------------
    (
        TENANT_REQUIRES_SECOND_FACTOR,
        Locale::English,
        Template::Simple(
            "This organisation requires two-step sign-in. Set up an authenticator app on your account, then try again.",
        ),
    ),
    (
        TENANT_REQUIRES_SECOND_FACTOR,
        Locale::Arabic,
        Template::Simple(
            "تشترط هذه المنشأة تسجيل الدخول بخطوتين. فعّل تطبيق المصادقة في حسابك ثم أعد المحاولة.",
        ),
    ),
    (
        SECOND_FACTOR_REQUIRED,
        Locale::English,
        // Safe to be specific: only somebody who has already given the right
        // password ever sees this.
        Template::Simple("Enter the code from your authenticator app to finish signing in."),
    ),
    (
        SECOND_FACTOR_REQUIRED,
        Locale::Arabic,
        Template::Simple("أدخل الرمز من تطبيق المصادقة لإكمال تسجيل الدخول."),
    ),
    (
        INVALID_CREDENTIALS,
        Locale::English,
        // Says nothing about which half was wrong, or whether the account
        // exists.
        Template::Simple("Those sign-in details are not correct. Please try again."),
    ),
    (
        INVALID_CREDENTIALS,
        Locale::Arabic,
        Template::Simple("بيانات تسجيل الدخول غير صحيحة. يُرجى المحاولة مرة أخرى."),
    ),
    (
        SESSION_EXPIRED,
        Locale::English,
        Template::Simple("Your session has ended. Please sign in again."),
    ),
    (
        SESSION_EXPIRED,
        Locale::Arabic,
        Template::Simple("انتهت جلستك. يُرجى تسجيل الدخول مرة أخرى."),
    ),
    (
        NOT_PERMITTED,
        Locale::English,
        Template::Simple(
            "Your role does not allow this ({capability}). Ask someone with permission.",
        ),
    ),
    (
        NOT_PERMITTED,
        Locale::Arabic,
        Template::Simple("دورك لا يسمح بهذا الإجراء ({capability}). يُرجى طلبه ممن لديه الصلاحية."),
    ),
    // -- members -----------------------------------------------------------
    (
        ALREADY_A_MEMBER,
        Locale::English,
        Template::Simple("{handle} already has access. Change their role instead."),
    ),
    (
        ALREADY_A_MEMBER,
        Locale::Arabic,
        Template::Simple("{handle} لديه صلاحية الوصول بالفعل. يمكنك تغيير دوره بدلاً من ذلك."),
    ),
    (
        LAST_OWNER,
        Locale::English,
        Template::Simple(
            "A workspace must keep at least one owner. Make someone else an owner first.",
        ),
    ),
    (
        LAST_OWNER,
        Locale::Arabic,
        Template::Simple("يجب أن يبقى للمساحة مالك واحد على الأقل. عيّن مالكًا آخر أولاً."),
    ),
    // -- platform staff ----------------------------------------------------
    (
        STAFF_SECOND_FACTOR_REQUIRED,
        Locale::English,
        Template::Simple(
            "Platform staff must use two-step sign-in. Set up an authenticator app on your account, then try again.",
        ),
    ),
    (
        STAFF_SECOND_FACTOR_REQUIRED,
        Locale::Arabic,
        Template::Simple(
            "يجب على موظفي المنصة استخدام تسجيل الدخول بخطوتين. فعّل تطبيق المصادقة في حسابك ثم أعد المحاولة.",
        ),
    ),
    (
        STAFF_KEEPS_SECOND_FACTOR,
        Locale::English,
        Template::Simple(
            "Platform staff cannot turn two-step sign-in off. You can replace your authenticator app, or ask a superadmin to take you off the staff first.",
        ),
    ),
    (
        STAFF_KEEPS_SECOND_FACTOR,
        Locale::Arabic,
        Template::Simple(
            "لا يمكن لموظفي المنصة إيقاف تسجيل الدخول بخطوتين. يمكنك استبدال تطبيق المصادقة، أو اطلب من مشرف عام إخراجك من الموظفين أولًا.",
        ),
    ),
    (
        TENANT_KEEPS_SECOND_FACTOR,
        Locale::English,
        Template::Simple(
            "An organisation you belong to requires two-step sign-in, so you cannot turn it off. You can replace your authenticator app. To turn it off, ask that organisation's owner to remove you from it or to stop requiring two-step sign-in.",
        ),
    ),
    (
        TENANT_KEEPS_SECOND_FACTOR,
        Locale::Arabic,
        Template::Simple(
            "تشترط منشأة أنت عضو فيها تسجيل الدخول بخطوتين، فلا يمكنك إيقافه. يمكنك استبدال تطبيق المصادقة. ولإيقافه، اطلب من مالك تلك المنشأة إزالتك منها أو إلغاء اشتراط تسجيل الدخول بخطوتين.",
        ),
    ),
    (
        ENROLMENT_LINK_REQUIRED,
        Locale::English,
        Template::Simple(
            "Two-step sign-in for this account was reset by somebody else, so a new authenticator app can only be set up from the link that was emailed to you. Open that link and try again, or ask whoever reset it to send a fresh one.",
        ),
    ),
    (
        ENROLMENT_LINK_REQUIRED,
        Locale::Arabic,
        Template::Simple(
            "أعاد شخص آخر تعيين تسجيل الدخول بخطوتين لهذا الحساب، فلا يمكن إعداد تطبيق مصادقة جديد إلا من الرابط المُرسل إلى بريدك. افتح ذلك الرابط ثم أعد المحاولة، أو اطلب ممن أعاد التعيين إرسال رابط جديد.",
        ),
    ),
    (
        RESET_YOURSELF,
        Locale::English,
        Template::Simple(
            "You cannot reset your own two-step sign-in. Replace your authenticator app instead, or ask somebody else to reset it for you.",
        ),
    ),
    (
        RESET_YOURSELF,
        Locale::Arabic,
        Template::Simple(
            "لا يمكنك إعادة تعيين تسجيل الدخول بخطوتين لحسابك. استبدل تطبيق المصادقة بدلًا من ذلك، أو اطلب من شخص آخر إعادة التعيين نيابة عنك.",
        ),
    ),
    (
        RESET_THE_OWNER,
        Locale::English,
        Template::Simple(
            "The owner's two-step sign-in cannot be reset from here. Ask support to do it.",
        ),
    ),
    (
        RESET_THE_OWNER,
        Locale::Arabic,
        Template::Simple(
            "لا يمكن إعادة تعيين تسجيل الدخول بخطوتين للمالك من هنا. اطلب ذلك من الدعم.",
        ),
    ),
    (
        RESET_PLATFORM_STAFF,
        Locale::English,
        Template::Simple(
            "That account belongs to platform staff, and only the platform can reset its two-step sign-in.",
        ),
    ),
    (
        RESET_PLATFORM_STAFF,
        Locale::Arabic,
        Template::Simple(
            "هذا الحساب لأحد موظفي المنصة، ولا يمكن إعادة تعيين تسجيل الدخول بخطوتين له إلا من المنصة.",
        ),
    ),
    (
        RESET_ANOTHER_COMPANY,
        Locale::English,
        Template::Simple(
            "That person also belongs to another organisation, and two-step sign-in is for their account everywhere, not just here. Contact support and they will reset it.",
        ),
    ),
    (
        RESET_ANOTHER_COMPANY,
        Locale::Arabic,
        Template::Simple(
            "هذا الشخص عضو في منشأة أخرى أيضًا، وتسجيل الدخول بخطوتين يخص حسابه في كل مكان لا هنا فقط. تواصل مع الدعم وسيتولّون إعادة التعيين.",
        ),
    ),
    (
        RESET_NO_LOGIN,
        Locale::English,
        Template::Simple(
            "That account has no email login, so there is nowhere to send an enrolment link. Nothing was changed.",
        ),
    ),
    (
        RESET_NO_LOGIN,
        Locale::Arabic,
        Template::Simple(
            "لا يملك هذا الحساب تسجيل دخول ببريد إلكتروني، فلا يوجد عنوان يُرسل إليه رابط التفعيل. لم يتغير شيء.",
        ),
    ),
    (
        RESET_REASON,
        Locale::English,
        Template::Simple(
            "Say why in 1 to 500 characters. It is recorded in the platform audit trail under your name.",
        ),
    ),
    (
        RESET_REASON,
        Locale::Arabic,
        Template::Simple("اذكر السبب في حدود 1 إلى 500 حرف. يُسجَّل في سجل تدقيق المنصة باسمك."),
    ),
    (
        ENROLMENT_SUBJECT,
        Locale::English,
        Template::Simple("Set up two-step sign-in again"),
    ),
    (
        ENROLMENT_SUBJECT,
        Locale::Arabic,
        Template::Simple("أعد إعداد تسجيل الدخول بخطوتين"),
    ),
    (
        ENROLMENT_BODY,
        Locale::English,
        Template::Simple(
            "Two-step sign-in for this account was reset, and your old \
             authenticator app and recovery codes no longer work.\n\n\
             Open this link, sign in with your password, and set up an \
             authenticator app again:\n{link}\n\n\
             The link works once and expires within an hour. Until you use one, \
             your password alone cannot set up a new app — so if the link has \
             expired, ask whoever reset it for a fresh one. If you did not \
             expect this, contact whoever runs your organisation: somebody \
             asked for it, and it is recorded under their name.",
        ),
    ),
    (
        ENROLMENT_BODY,
        Locale::Arabic,
        Template::Simple(
            "أُعيد تعيين تسجيل الدخول بخطوتين لهذا الحساب، ولم يعد تطبيق \
             المصادقة القديم ولا رموز الاسترداد يعملان.\n\n\
             افتح هذا الرابط، وسجّل الدخول بكلمة المرور، ثم أعد إعداد تطبيق \
             المصادقة:\n{link}\n\n\
             يعمل الرابط مرة واحدة وتنتهي صلاحيته خلال ساعة. وإلى أن تستخدم \
             رابطًا، لا تكفي كلمة المرور وحدها لإعداد تطبيق جديد — فإن انتهت \
             صلاحية الرابط فاطلب ممن أعاد التعيين رابطًا جديدًا. وإن لم تكن \
             تتوقع ذلك فتواصل مع المسؤول عن منشأتك: طلب أحدهم هذا الإجراء، \
             وهو مسجَّل باسمه.",
        ),
    ),
    (
        STAFF_NO_SUCH_ACCOUNT,
        Locale::English,
        Template::Simple(
            "No account signs in as {handle}. They need an account before they can join the staff.",
        ),
    ),
    (
        STAFF_NO_SUCH_ACCOUNT,
        Locale::Arabic,
        Template::Simple(
            "لا يوجد حساب يسجّل الدخول باسم {handle}. يلزمه حساب قبل الانضمام إلى الموظفين.",
        ),
    ),
    (
        ALREADY_STAFF,
        Locale::English,
        Template::Simple("{handle} is already platform staff. Change their role instead."),
    ),
    (
        ALREADY_STAFF,
        Locale::Arabic,
        Template::Simple("{handle} من موظفي المنصة بالفعل. يمكنك تغيير دوره بدلاً من ذلك."),
    ),
    (
        STAFF_NO_SECOND_FACTOR,
        Locale::English,
        Template::Simple(
            "{handle} has not set up two-step sign-in, and platform staff must have it. Ask them to set up an authenticator app first.",
        ),
    ),
    (
        STAFF_NO_SECOND_FACTOR,
        Locale::Arabic,
        Template::Simple(
            "لم يفعّل {handle} تسجيل الدخول بخطوتين، وهو شرط لموظفي المنصة. اطلب منه تفعيل تطبيق المصادقة أولًا.",
        ),
    ),
    (
        NOT_STAFF,
        Locale::English,
        Template::Simple("That person is not platform staff."),
    ),
    (
        NOT_STAFF,
        Locale::Arabic,
        Template::Simple("هذا الشخص ليس من موظفي المنصة."),
    ),
    (
        LAST_SUPERADMIN,
        Locale::English,
        Template::Simple(
            "The platform must keep at least one superadmin. Make someone else a superadmin first.",
        ),
    ),
    (
        LAST_SUPERADMIN,
        Locale::Arabic,
        Template::Simple("يجب أن يبقى للمنصة مشرف عام واحد على الأقل. عيّن مشرفًا عامًا آخر أولًا."),
    ),
    (
        WRONG_TENANT_STATUS,
        Locale::English,
        Template::Simple(
            "This workspace is {status}, and this can only be done to one that is {expected}.",
        ),
    ),
    (
        WRONG_TENANT_STATUS,
        Locale::Arabic,
        Template::Simple(
            "حالة مساحة العمل هذه {status}، ولا يمكن تنفيذ هذا الإجراء إلا عندما تكون حالتها {expected}.",
        ),
    ),
    (
        SUSPENSION_REASON,
        Locale::English,
        Template::Simple("Say why in 1 to 500 characters, written for the workspace's owner."),
    ),
    (
        SUSPENSION_REASON,
        Locale::Arabic,
        Template::Simple("اذكر السبب في حدود 1 إلى 500 حرف، بصياغة موجّهة إلى مالك مساحة العمل."),
    ),
    (
        INVITATION_NOT_VALID,
        Locale::English,
        Template::Simple(
            "That invitation is no longer valid. Ask whoever invited you for a new link.",
        ),
    ),
    (
        INVITATION_NOT_VALID,
        Locale::Arabic,
        Template::Simple("لم تعد هذه الدعوة صالحة. اطلب رابطًا جديدًا ممن دعاك."),
    ),
    (
        HANDLE_TAKEN,
        Locale::English,
        Template::Simple("{handle} already has an account. Sign in with it instead."),
    ),
    (
        HANDLE_TAKEN,
        Locale::Arabic,
        Template::Simple("لدى {handle} حساب بالفعل. سجّل الدخول به بدلًا من ذلك."),
    ),
    (
        NOT_A_MEMBER,
        Locale::English,
        Template::Simple("That person is not a member of this tenant."),
    ),
    (
        NOT_A_MEMBER,
        Locale::Arabic,
        Template::Simple("هذا الشخص ليس عضوًا لدى هذا المستأجر."),
    ),
    // -----------------------------------------------------------------------
    // Outgoing mail
    // -----------------------------------------------------------------------
    //
    // Written as a person would write it, not as an error is written. `{link}`
    // is a URL and therefore Latin script inside an Arabic sentence, which the
    // catalog bidi-isolates — without that it renders with the path segments in
    // the wrong order, which is a broken link that *looks* fine.
    (
        INVITATION_SUBJECT,
        Locale::English,
        Template::Simple("You have been invited to {company}"),
    ),
    (
        INVITATION_SUBJECT,
        Locale::Arabic,
        Template::Simple("تمت دعوتك إلى {company}"),
    ),
    (
        INVITATION_BODY,
        Locale::English,
        Template::Simple(
            "You have been invited to join {company}.\n\n\
             Open this link to accept and choose a password:\n{link}\n\n\
             The link works once and expires. If you were not expecting this, \
             ignore this message — nothing happens until you open it.",
        ),
    ),
    (
        INVITATION_BODY,
        Locale::Arabic,
        Template::Simple(
            "تمت دعوتك للانضمام إلى {company}.\n\n\
             افتح هذا الرابط لقبول الدعوة واختيار كلمة مرور:\n{link}\n\n\
             يعمل الرابط مرة واحدة ثم ينتهي. إن لم تكن تتوقع هذه الرسالة \
             فتجاهلها — لا يحدث شيء حتى تفتحها.",
        ),
    ),
    (
        SIGNUP_NOT_VALID,
        Locale::English,
        Template::Simple(
            "That confirmation link is not valid. It may have expired, or been used already.",
        ),
    ),
    (
        SIGNUP_NOT_VALID,
        Locale::Arabic,
        Template::Simple("رابط التأكيد غير صالح. ربما انتهت صلاحيته أو استُخدم من قبل."),
    ),
    // Plural on the seconds, and Arabic uses every category for it: a wait of
    // 2 seconds, 3 seconds and 11 seconds take three different forms, which is
    // the whole reason `Plural` exists.
    (
        SIGNUP_TOO_SOON,
        Locale::English,
        Template::Plural {
            zero: None,
            one: Some("A confirmation is already on its way. Try again in a second."),
            two: None,
            few: None,
            many: None,
            other: "A confirmation is already on its way. Try again in {seconds} seconds.",
        },
    ),
    (
        SIGNUP_TOO_SOON,
        Locale::Arabic,
        Template::Plural {
            zero: Some("رسالة التأكيد في طريقها إليك. أعد المحاولة الآن."),
            one: Some("رسالة التأكيد في طريقها إليك. أعد المحاولة بعد ثانية."),
            two: Some("رسالة التأكيد في طريقها إليك. أعد المحاولة بعد ثانيتين."),
            few: Some("رسالة التأكيد في طريقها إليك. أعد المحاولة بعد {seconds} ثوانٍ."),
            many: Some("رسالة التأكيد في طريقها إليك. أعد المحاولة بعد {seconds} ثانية."),
            other: "رسالة التأكيد في طريقها إليك. أعد المحاولة بعد {seconds} ثانية.",
        },
    ),
    (
        RESET_SUBJECT,
        Locale::English,
        Template::Simple("Choose a new password"),
    ),
    (
        RESET_SUBJECT,
        Locale::Arabic,
        Template::Simple("اختر كلمة مرور جديدة"),
    ),
    (
        RESET_BODY,
        Locale::English,
        Template::Simple(
            "Somebody asked to reset the password for this address.\n\n\
             Open this link to choose a new one:\n{link}\n\n\
             The link works once and expires within an hour. If your account \
             uses an authenticator app, you will be asked for a code as well — \
             resetting a password does not switch that off. Nothing has \
             changed yet, so if this was not you, ignore this message and \
             nothing will.",
        ),
    ),
    (
        RESET_BODY,
        Locale::Arabic,
        Template::Simple(
            "طلب أحدهم إعادة تعيين كلمة المرور لهذا البريد.\n\n\
             افتح هذا الرابط لاختيار كلمة مرور جديدة:\n{link}\n\n\
             يعمل الرابط مرة واحدة وتنتهي صلاحيته خلال ساعة. إذا كان حسابك \
             يستخدم تطبيق المصادقة فسيُطلب منك رمز أيضًا — إعادة تعيين كلمة \
             المرور لا توقف ذلك. لم يتغير شيء بعد، فإن لم تكن أنت من طلب ذلك \
             فتجاهل هذه الرسالة ولن يتغير شيء.",
        ),
    ),
    (
        RESET_NOT_VALID,
        Locale::English,
        Template::Simple("That reset link is not valid. Ask for a new one."),
    ),
    (
        RESET_NOT_VALID,
        Locale::Arabic,
        Template::Simple("رابط إعادة التعيين غير صالح. اطلب رابطًا جديدًا."),
    ),
    (
        RESET_TOO_SOON,
        Locale::English,
        Template::Simple(
            "A link was sent to this address {sent} seconds ago. Try again in {retry_in}.",
        ),
    ),
    (
        RESET_TOO_SOON,
        Locale::Arabic,
        Template::Simple("أُرسل رابط إلى هذا البريد قبل {sent} ثانية. حاول بعد {retry_in}."),
    ),
    (
        SIGNUP_SUBJECT,
        Locale::English,
        Template::Simple("Confirm your address to create {company}"),
    ),
    (
        SIGNUP_SUBJECT,
        Locale::Arabic,
        Template::Simple("أكِّد بريدك لإنشاء {company}"),
    ),
    (
        SIGNUP_BODY,
        Locale::English,
        Template::Simple(
            "Somebody asked to create {company} with this address.\n\n\
             Open this link to confirm it and finish setting up:\n{link}\n\n\
             The link works once and expires within a day. Nothing has been \
             created yet, so if this was not you, ignore this message and \
             nothing will be.",
        ),
    ),
    (
        SIGNUP_BODY,
        Locale::Arabic,
        Template::Simple(
            "طلب أحدهم إنشاء {company} بهذا البريد.\n\n\
             افتح هذا الرابط لتأكيده وإكمال الإعداد:\n{link}\n\n\
             يعمل الرابط مرة واحدة وتنتهي صلاحيته خلال يوم. لم يُنشأ شيء بعد، \
             فإن لم تكن أنت من طلب ذلك فتجاهل الرسالة ولن يُنشأ شيء.",
        ),
    ),
];
