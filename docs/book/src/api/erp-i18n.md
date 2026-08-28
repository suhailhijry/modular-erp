# erp-i18n

Localization, and the rule that drives the whole crate:

**An error is a code and some arguments. It is never a sentence.**

`thiserror`'s `#[error("no membership for this identity")]` bakes English into
the type, and that is the pattern that turns localization into a rewrite later
instead of a translation. So error types here carry a stable `MessageCode` and
typed `MessageArg`s, and prose is chosen at the API boundary from the caller's
`Accept-Language`.

The machine-readable code is not only a localization artifact. It is the `type`
field of the RFC 9457 problem response that integrators branch on, so the two
requirements turn out to be one requirement.

**Depends on:** `erp-types`.
**Used by:** everything above it, and every module owns a catalog.

## Arabic is not English with different words

Saudi Arabia is the first market, so Arabic is a first-class target. Three things
follow from that, and each one is a piece of this crate.

**Six plural categories, not two.** `if n == 1` is wrong in a way no reviewer
will catch.

**Bidirectional text.** Interpolating a Latin identifier, an account code or a
tenant slug, into an Arabic sentence scrambles the surrounding text unless the
run is isolated. `Locale::is_rtl` drives that automatically.

**Completeness is enforced.** `tests/completeness.rs` fails the build if any code
lacks a translation in any locale, so a missing Arabic string cannot ship as
English.

## The files

| File | What is in it |
|---|---|
| [`lib.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-i18n/src/lib.rs) | `Locale`, `MessageCode`, `MessageArg`, `Message`, `Localize` |
| [`catalog.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-i18n/src/catalog.rs) | `Template`, the `Catalog` trait, `StaticCatalog`, `Composite` |
| [`plural.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-i18n/src/plural.rs) | CLDR plural categories and the English and Arabic rules |
| [`testing.rs`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-i18n/src/testing.rs) | `audit` and `assert_complete`, run by every crate that owns a catalog |

## Locale

```rust
pub enum Locale { English, Arabic }

impl Locale {
    pub const ALL: [Self; 2];
    pub const DEFAULT: Self;                         // English
    pub const fn code(self) -> &'static str;         // "en" / "ar"
    pub const fn is_rtl(self) -> bool;
    pub const fn plural_categories(self) -> &'static [Plural];
    pub fn from_accept_language(header: &str) -> Self;
}
```

A closed enum on purpose. Adding a language means adding translations, and the
completeness test should refuse to build until they exist, which only works if
the set is known at compile time.

English is the default because it is the language every integrator can read and
these messages reach machines as often as people. A human who states Arabic gets
Arabic.

`from_accept_language` honours quality values, so `en;q=0.5, ar;q=0.9` yields
Arabic. Regional subtags match on their primary tag, so `ar-SA`, `ar-EG` and bare
`ar` all resolve to Arabic. A Gulf user and an Egyptian user are both better
served by Arabic than by falling through to English.

You rarely call this yourself. `erp_web::Language` is the extractor that does it
on every request, and it is infallible: an absent or unparseable header is
English, not a 400, so an error response can still be localized when the *next*
extractor is what failed.

## MessageCode

```rust
pub struct MessageCode(Cow<'static, str>);

impl MessageCode {
    pub const fn new(code: &'static str) -> Self;
    pub fn as_str(&self) -> &str;
}
```

Codes are `domain.thing_that_happened`, lowercase with underscores. They are
globally unique by their prefix, and `no_two_crates_claim_the_same_code` fails
the build if two crates claim one.

**Changing a code is a breaking API change**, exactly like renaming a field.
Integrators branch on it.

It is backed by a `Cow` because it is genuinely two things: a compile-time
constant in the catalogs, and a parsed value when a client deserializes a problem
response. Interning at deserialize time would mean leaking memory on untrusted
input, which is a worse trade than losing `Copy`.

## MessageArg and Message

```rust
pub enum MessageArg {
    Text(String),   // Latin-script; bidi-isolated when rendered into an RTL locale
    Int(i64),       // a plain number, not a count. Selects no plural form
    Count(i64),     // a quantity. Selects the plural form
}

impl MessageArg {
    pub fn text(value: impl Into<String>) -> Self;
}

pub struct Message { /* code, args */ }

impl Message {
    pub fn new(code: MessageCode) -> Self;
    pub fn with(self, name: &str, arg: MessageArg) -> Self;
}
```

Arguments are typed and not pre-formatted strings, because formatting is
locale-dependent. A count selects a plural form, a date renders differently under
a Hijri calendar, and money carries a currency whose symbol placement varies.
Handing the renderer a `String` throws away everything it needs.

`Int` and `Count` are separate for that reason. Only `Count` selects a plural.

## Localize

```rust
pub trait Localize {
    fn message(&self) -> Message;
}
```

Every error type that reaches an API boundary implements this. The `Display` impl
stays English and is for logs and developers; `message` is what a user sees.

That split does real work. From
[`crates/erp-control/src/lib.rs:1520`](https://github.com/suhailhijry/modular-erp/blob/main/crates/erp-control/src/lib.rs):

```rust
impl Localize for AccessError {
    fn message(&self) -> Message {
        match self {
            Self::NoSuchIdentity => Message::new(messages::NO_SUCH_IDENTITY),
            Self::NoSuchTenant | Self::NotAMember => Message::new(messages::ACCESS_DENIED),
            Self::SlugTaken(slug) => Message::new(messages::SLUG_TAKEN)
                .with("slug", MessageArg::text(slug)),
            Self::Database(_) | Self::Corrupt(_) => Message::new(messages::INTERNAL),
            // …
        }
    }
}
```

`NoSuchTenant` and `NotAMember` collapse to one user-facing message on purpose. A
distinct "no such tenant" would let an attacker enumerate tenant slugs by
watching which error comes back. `Display` keeps them apart for logs, where the
distinction is useful and the audience is trusted.

A database failure is never described to a user. They get "something went wrong"
and the detail goes to the log.

## Template

```rust
pub enum Template {
    Simple(&'static str),
    Plural {
        zero:  Option<&'static str>,
        one:   Option<&'static str>,
        two:   Option<&'static str>,
        few:   Option<&'static str>,
        many:  Option<&'static str>,
        other: &'static str,          // CLDR's universal fallback
    },
}

impl Template {
    pub const fn variant(self, plural: Plural) -> Option<&'static str>;
}
```

Plural variants are separate fields and not an inline `{n, plural, …}` syntax.
There is no parser to get wrong, and the shape of the data says which forms a
language needs.

Only `other` is mandatory. The rest are optional because a language does not use
every category: English's rules never select `zero`, so an English `zero` string
would be unreachable text a translator wrote and nobody ever sees. Asking for it
invites exactly that mistake. The enforcement lives in `tests/completeness.rs`
instead, which checks each locale against the categories it actually selects.

## Plural

```rust
pub enum Plural { Zero, One, Two, Few, Many, Other }

impl Plural {
    pub const ALL: [Self; 6];
}
```

Arabic is why this module exists. English has two forms and tempts everyone into
`if n == 1`. Arabic has six, and the rule is not "big numbers are different": it
depends on `n % 100`, so 3 and 103 take one form while 11 and 111 take another.
No amount of care with ad-hoc conditionals gets this right.

```text
Arabic, per CLDR cardinal:
  zero   n = 0
  one    n = 1
  two    n = 2
  few    n % 100 = 3..=10
  many   n % 100 = 11..=99
  other  everything else (100, 101, 102, 200, …)
```

Negative counts are classified by magnitude. A count is a quantity, and "-3
items" should read like "3 items" grammatically.

## Catalog

```rust
pub trait Catalog: Send + Sync {
    fn template(&self, locale: Locale, code: &MessageCode) -> Option<Template>;
    fn codes(&self) -> &'static [MessageCode];

    fn render(&self, locale: Locale, message: &Message) -> Option<String> { … }
    fn render_or_code(&self, locale: Locale, message: &Message) -> String { … }
}
```

`Send + Sync` because every catalog in this system is a `static` shared by every
request, and because `Composite` holds its parts as trait objects.

`codes()` drives the completeness test. A catalog that cannot enumerate itself
cannot be checked.

Prefer `render_or_code`. It tries the requested locale, falls back to English
with a `tracing::warn!`, and finally returns the code itself. Something
diagnosable beats nothing.

### StaticCatalog

```rust
pub struct StaticCatalog { … }

impl StaticCatalog {
    pub const fn new(
        entries: &'static [(MessageCode, Locale, Template)],
        codes: &'static [MessageCode],
    ) -> Self;
}
```

A catalog compiled into the binary. Every module declares one, and the shape is
always the same. From
[`modules/sales/src/messages.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/messages.rs):

```rust
pub const NOT_ISSUED: MessageCode = MessageCode::new("sales.not_issued");

pub static CODES: &[MessageCode] = &[NOT_ISSUED, /* … */];

pub static ENTRIES: &[(MessageCode, Locale, Template)] = &[
    (NOT_ISSUED, Locale::English, Template::Simple("Invoice {invoice} has not been issued.")),
    (NOT_ISSUED, Locale::Arabic,  Template::Simple("لم تُصدَر الفاتورة {invoice}.")),
    // …
];
```

and in `lib.rs`:

```rust
pub static CATALOG: StaticCatalog = StaticCatalog::new(messages::ENTRIES, messages::CODES);
```

Per-tenant terminology overrides, "client" against "patient" against "guest",
arrive with the configuration system and layer on top of this. These are the
defaults, and they always exist.

### Composite

```rust
pub struct Composite { … }

impl Composite {
    pub const fn new(parts: &'static [&'static dyn Catalog]) -> Self;
}
```

Several catalogs behind one lookup. A crate renders messages from itself and from
everything it is built on: a module's route answers with its own failures, the
control plane's, and the request-level ones, and no single catalog holds all
three.

It is `const`, so it can be a `static`. From
[`modules/sales/src/http.rs:55`](https://github.com/suhailhijry/modular-erp/blob/main/modules/sales/src/http.rs):

```rust
static CATALOG: erp_i18n::Composite =
    erp_i18n::Composite::new(&[&crate::CATALOG, &ledger::CATALOG, &erp_web::CATALOG]);
```

Lookup is "first part that has the code", which is unambiguous because codes are
globally unique by their `domain.` prefix. A duplicate would make the answer
depend on the order of the parts, which is what
`no_two_crates_claim_the_same_code` exists to catch.

`erp_api::CATALOG` is the union of every catalog in the build, and
`docs/ERRORS.md` is generated from it.

## Testing helpers

```rust
pub fn audit(catalog: &impl Catalog) -> Vec<String>;
pub fn assert_complete(catalog: &impl Catalog);
```

Every crate that owns a catalog runs these, so the guarantee is uniform instead of
reimplemented per module. `audit` returns every problem it finds and not the
first, so one test run fixes one translation pass. `assert_complete` is the
one-liner a crate's test calls.

```rust
// crates/erp-eventlog/tests/localization.rs
#[test]
fn the_catalog_is_complete() {
    erp_i18n::testing::assert_complete(&CATALOG);
}
```

## Adding a message

1. Add a `MessageCode` const in the module's `messages.rs`, prefixed with the
   module name.
2. Add it to `CODES`.
3. Add one `ENTRIES` row per locale. Both, or the build fails.
4. Return it from the error type's `Localize::message`.
5. Run `just errors` to regenerate `docs/ERRORS.md`, and commit the diff. CI
   fails if it drifts.
