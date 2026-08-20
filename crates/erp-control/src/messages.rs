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

// ---------------------------------------------------------------------------
// Codes
// ---------------------------------------------------------------------------

pub const NO_SUCH_IDENTITY: MessageCode = MessageCode::new("access.no_such_identity");
pub const IDENTITY_SUSPENDED: MessageCode = MessageCode::new("access.identity_suspended");
pub const TENANT_UNAVAILABLE: MessageCode = MessageCode::new("access.tenant_unavailable");
pub const TENANT_PROVISIONING: MessageCode = MessageCode::new("access.tenant_provisioning");
pub const ACCESS_DENIED: MessageCode = MessageCode::new("access.denied");
pub const OVERLOADED: MessageCode = MessageCode::new("system.overloaded");
pub const INTERNAL: MessageCode = MessageCode::new("system.internal_error");
pub const NO_CAPACITY: MessageCode = MessageCode::new("provisioning.no_capacity");
/// Operator-facing. Shown on an ops surface, never to a tenant — a signup form
/// has no business mentioning how our clusters are doing.
pub const CLUSTERS_AT_LIMIT: MessageCode = MessageCode::new("ops.clusters_at_limit");
pub const SLUG_TAKEN: MessageCode = MessageCode::new("provisioning.slug_taken");
/// Wrong handle, wrong password, unknown handle, suspended identity — one
/// message for all four, for the same reason `NoSuchTenant` and `NotAMember`
/// share one.
pub const INVALID_CREDENTIALS: MessageCode = MessageCode::new("auth.invalid_credentials");
pub const HANDLE_TAKEN: MessageCode = MessageCode::new("auth.handle_taken");
pub const SESSION_EXPIRED: MessageCode = MessageCode::new("auth.session_expired");
/// 403, naming the capability. "Ask someone with permission" is only actionable
/// when you know which permission.
pub const NOT_PERMITTED: MessageCode = MessageCode::new("access.not_permitted");
pub const ALREADY_A_MEMBER: MessageCode = MessageCode::new("members.already_a_member");
pub const NOT_A_MEMBER: MessageCode = MessageCode::new("members.not_a_member");
pub const INVITATION_NOT_VALID: MessageCode = MessageCode::new("invitations.not_valid");
pub const LAST_OWNER: MessageCode = MessageCode::new("members.last_owner");

/// Every code this crate can produce. The completeness test walks this list.
/// The subject line of an invitation email. Not an error — the first message
/// code in this system that is *outgoing* rather than a refusal.
pub const INVITATION_SUBJECT: MessageCode = MessageCode::new("mail.invitation_subject");
/// The body of an invitation email.
pub const INVITATION_BODY: MessageCode = MessageCode::new("mail.invitation_body");

pub static CODES: &[MessageCode] = &[
    INVITATION_SUBJECT,
    INVITATION_BODY,
    NO_SUCH_IDENTITY,
    IDENTITY_SUSPENDED,
    TENANT_UNAVAILABLE,
    TENANT_PROVISIONING,
    ACCESS_DENIED,
    OVERLOADED,
    INTERNAL,
    NO_CAPACITY,
    CLUSTERS_AT_LIMIT,
    SLUG_TAKEN,
    INVALID_CREDENTIALS,
    HANDLE_TAKEN,
    SESSION_EXPIRED,
    NOT_PERMITTED,
    ALREADY_A_MEMBER,
    NOT_A_MEMBER,
    INVITATION_NOT_VALID,
    LAST_OWNER,
];

// ---------------------------------------------------------------------------
// Translations
// ---------------------------------------------------------------------------

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
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
    (
        OVERLOADED,
        Locale::English,
        Template::Simple("The system is busy right now. Please try again in a moment."),
    ),
    (
        OVERLOADED,
        Locale::Arabic,
        Template::Simple("النظام مشغول حالياً. يُرجى المحاولة مرة أخرى بعد قليل."),
    ),
    (
        INTERNAL,
        Locale::English,
        Template::Simple("Something went wrong on our side. The problem has been recorded."),
    ),
    (
        INTERNAL,
        Locale::Arabic,
        Template::Simple("حدث خطأ لدينا. تم تسجيل المشكلة."),
    ),
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
];
