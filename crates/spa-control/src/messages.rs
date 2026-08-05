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

use spa_i18n::{Locale, MessageCode, Template};

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

/// Every code this crate can produce. The completeness test walks this list.
pub static CODES: &[MessageCode] = &[
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
];
