# tax_sa

**Saudi Arabia**: what the rate is, what the return says, what was filed, and
ZATCA. The largest module in the build, and the first that stands on two others.

**Depends on:** `sales`, `purchases`, `ledger`, plus the core.
**Depended on by:** nothing.

## Why a country is a module

Every country's is different. Saudi Arabia has ZATCA and 15%; the UAE has Peppol
PINT AE and 5%. The rate, the return's shape, the clearance protocol and the
fields an invoice must print all change at the border.

None of that belongs in `ledger`, which is the accounting kernel every country
uses, and none of it belongs in `sales`, which knows what an invoice is and not
where it was issued. So `ledger` owns the *shape*, that a line has a treatment
and a rate, and this module owns the *values*: it seeds `ledger::Rates` when a
tenant enables it, and it holds ZATCA.

## Why the return moved here from erp-api

It was composed in the API, on the reasoning that cross-module composition
belongs in the composition root. Under the core-and-module model that is wrong:
netting output tax against input tax is **domain**, and core holds none.

The test the model gives is *can a tenant disable it?* A business with neither
sales nor purchases had a VAT return endpoint, which is the answer.

Composing it in a module that **declares both** keeps the dependency arrows
straight: `tax_sa → {sales, purchases} → ledger`. Nothing reaches sideways.

## One side is enough, no side is not

```rust
pub fn setup() -> ModuleSetup {
    ModuleSetup::new(module_id(), include_str!("../schema/install.sql"), GROUPS, upcasters)
        .seeding(include_str!("../schema/seed.sql"))
        .requiring(&["ledger"])
        .requiring_any(&["sales", "purchases"])
}
```

The crate depends on both and calls their read functions. The **entitlement**
needs at least one and does not care which: a business that only sells still
files a return, and demanding purchases would force a shop with no supplier bills
to enable a module they do not use in order to declare tax they do owe. Each side
is reported if the tenant has it and zero if not, which is not a fallback but the
truth.

## The files

| File | What is in it |
|---|---|
| [`taxpayer.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/taxpayer.rs) | `Registration`, `Taxpayer`, `IdScheme`, the Saudi address |
| [`report.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/report.rs) | `vat_return`, `Sides`, `Band` |
| [`filing.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/filing.rs) | `Filing`, `FilingEvent` |
| [`clearance.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/clearance.rs) | `Clearance`, `ClearanceEvent` |
| [`onboarded.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/onboarded.rs) | `Onboarding`, `OnboardingEvent` |
| [`commands.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/commands.rs) | `file_return`, `register_taxpayer`, `record_outcome` |
| [`documents.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/documents.rs) | The projections that build ZATCA documents from `sales` events |
| [`projections.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/projections.rs) | The `TaxSa` group, filed returns, onboarding status |
| [`submit.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/submit.rs) | `sign_pending`, `submit_pending` |
| [`zatca/mod.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/mod.rs) | `Kind`, `Document`, `TypeCode`, the shared vocabulary |
| [`zatca/ubl.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/ubl.rs) | UBL 2.1, rendered already canonical |
| [`zatca/chain.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/chain.rs) | The hash chain: PIH and ICV |
| [`zatca/qr.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/qr.rs) | The TLV QR block |
| [`zatca/signing.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/signing.rs) | The XAdES signature |
| [`zatca/csr.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/csr.rs) | Key pair, CSR, `Environment`, `Unit` |
| [`zatca/onboarding.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/onboarding.rs) | The four-step onboarding flow |
| [`zatca/samples.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/samples.rs) | The compliance-check documents |
| [`zatca/wire.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/wire.rs) | Request and response bodies, `Verdict` |
| [`zatca/http.rs`](https://github.com/suhailhijry/modular-erp/blob/main/modules/tax_sa/src/zatca/http.rs) | `Fatoora`, the client that reaches ZATCA |

## The taxpayer

```rust
pub fn taxpayer_id() -> AggregateId;

pub struct Registration { … }
impl Registration { pub fn check(&self) -> Result<(), InvalidRegistration>; }

pub enum IdScheme { … }             // ZATCA's schemeID list, 6 values
pub struct Address { … }            // a Saudi national address, a fixed shape
pub enum TaxpayerEvent { Registered { … } }
pub struct Taxpayer { … }
```

### Why this is an event and not configuration

Every other tenant setting in this system is configuration: the VAT rate, the
posting accounts, the chart. They are read **inside the command's transaction**
and stamped onto the event, so a setting that changes later cannot restate a
document that was already issued.

That mechanism is unavailable here, and the reason is the dependency direction.
The command that issues an invoice lives in `sales`, and `sales` must not know
that Saudi Arabia exists, so nothing in the issuing transaction can read a ZATCA
registration. The ZATCA document is built afterwards, by a projection over the
log.

**A projection that read `configuration` would break L2 outright.** Rebuilding it
after the business moved offices would render every historic invoice with the new
address, produce a different hash for each, and break the chain, silently,
because each document on its own would look fine.

So the registration is a fact in the log with a position. An invoice issued in
March is rendered with the registration that was current in March, whatever
happened in April. It also answers "who changed the VAT number, and when", which
for a tax registration is a question somebody eventually asks.

### Arabic is the invoice's language, not a translation of it

ZATCA requires the invoice in Arabic. UBL carries one registration name, so that
name is the Arabic one, and `Registration::name` is **validated to contain Arabic
script**, never trusted to. The Latin name is ours, for our own screens, and
never reaches ZATCA.

### Why check() instead of letting ZATCA say no

By then the invoice exists. A standard invoice is cleared *before* it goes to the
buyer, so a bad VAT number stalls the sale; a simplified one has already been
handed over when the reporting call fails. Both are worse than refusing the
registration.

One EGS unit per tenant, so `taxpayer_id()` is a single stream. Every invoice
flows through one solution, so one certificate, one counter, one chain. A
business running two tills that each need their own device certificate is a real
ZATCA shape and not this one.

## The return

```rust
pub struct Band { … }
pub struct Side { … }
pub struct Return { … }
pub struct Sides { … }              // which of the two modules the tenant has

pub async fn vat_return(conn: &mut PgConnection, sides: Sides,
    currency: CurrencyCode, from: Timestamp, until: Timestamp)
    -> Result<Return, sqlx::Error>;
```

**It is not a cross-group read.** `proj_sales` and `proj_purchases` are separate
projection groups and neither may read the other. Nothing here does: it calls
each module's own read function and nets the answers in Rust.

What L3 protects is that a group is the unit of consistency, and that protection
is unchanged. Two groups at different checkpoints would give a return that mixes
a caught-up quarter with one that is not, which is what `?consistent_after=`
waits out.

**Every document is reported on its own tax point.** An invoice on its issue
date, a credit note on its credit date, a bill on the date the supplier stated.
Re-running a filed period gives the number that was filed.

A tenant with only one of the two modules gets zeroes for the other side rather
than an error, because a business that has not enabled purchases genuinely
reclaimed nothing.

## Filing

```rust
pub fn period_id(currency: CurrencyCode, from: Timestamp, until: Timestamp)
    -> Result<AggregateId, TaxError>;      // "SAR.2026-01-01.2026-04-01"

pub struct Filed { … }

pub async fn file_return(db: &TenantDb, sides: Sides, currency: CurrencyCode,
    from: Timestamp, until: Timestamp, filed_on: Timestamp, metadata: &Metadata)
    -> Result<Filed, CommandError<TaxError>>;
```

**The period is the identity**, which is what makes filing one twice a conflict.
A second filing is not a second return. A currency is part of it because a business
invoicing in two files two.

Filing the same period twice is a **conflict, not a no-op**: a second filing is
an amendment, and an amendment is a different document with its own rules. The
error carries the date of the one that exists so a caller can say so.

**Why filing is recorded and not inferred.** Every other part of the system
already makes re-running a period give the number that was filed: documents are
reported on their own tax point, and a closed period refuses back-dated writes.
Those are properties of the *arithmetic*, and they hold as long as nobody makes a
mistake.

Recording the filing is stronger and cheaper. The numbers that went to ZATCA are
in the log with the date they went and who sent them, so "does the system still
agree with what we filed?" is a comparison and not an argument, and the
answer survives a rebuild.

## ZATCA: two documents, two obligations

ZATCA splits every invoice by who the buyer is, and the split decides *when* the
authority has to see it.

| | Buyer | Before or after | Call |
|---|---|---|---|
| **Standard** | A VAT-registered business | Cleared **before** it is given to the buyer | `/invoices/clearance/single` |
| **Simplified** | A consumer | Reported within 24 hours | `/invoices/reporting/single` |

One field decides it: whether the buyer gave a VAT number.

```rust
pub enum Kind { Standard, Simplified }
impl Kind {
    pub const ALL: [Self; 2];
    pub const fn of(buyer_vat_number: Option<&String>) -> Self;   // the decision
    pub const fn transaction_code(self) -> &'static str;
    pub const fn reporting_window(self) -> Option<TimeDelta>;
}
```

`Kind::of` is the only place the decision is taken. `reporting_window` is `None`
for a standard invoice, and that is not "no deadline": it has to be cleared
before issue, so there is no window to be late in.

The difference is not paperwork. A standard invoice is not a valid invoice until
ZATCA has stamped it, so the seller cannot hand it over yet.

## The document is a projection

Nothing in the issuing transaction can build it. `sales` issues the invoice and
must not know that Saudi Arabia exists, and inverting the dependency would put
ZATCA in every tenant's sales module.

```rust
pub struct Taxpayers;         // keeps the registration in force
pub struct ZatcaDocuments;    // builds a document per invoice and credit note
```

Three things in the kernel make it work, and none of them is new machinery.

1. **A projection reads the whole log**, and never only its own module's events,
   so subscribing costs nothing.
2. **`Upcasters::also`** folds `sales`' event history into this module's, so a
   `sales` event version added next year is readable here without a second copy
   of its chain.
3. **A projection group is the unit of consistency**, and this builds into
   `proj_tax_sa`, never into `proj_sales`.

`Taxpayers` is its own projection and not part of `ZatcaDocuments`. It is a
different fact with a different lifetime, and the ordering between them is the
log's.

### Why the chain is safe to rebuild

Every input is the log: the invoice, the registration, and the order they arrived
in. The counter is the position in that order and the previous hash is the
previous document's. Replay the log and every document comes out byte-identical,
which it has to, because the hashes went to a tax authority.

## Reading documents

```rust
pub struct Stored { … }
pub enum Status { Unregistered, Pending, Cleared, Reported, Refused }
impl Status {
    pub const ALL: [Self; 5];
    pub const fn is_settled(self) -> bool;
}

pub struct Pending { … }      // one waiting to be submitted
pub struct Unsigned { … }     // one waiting to be signed
pub struct Standing { … }     // where the business stands, in one answer

pub async fn registered(conn: &mut PgConnection) -> Result<Option<Registration>, sqlx::Error>;
pub async fn pending(conn: &mut PgConnection, limit: i64) -> Result<Vec<Pending>, sqlx::Error>;
pub async fn unsigned(conn: &mut PgConnection, limit: i64) -> Result<Vec<Unsigned>, sqlx::Error>;
pub async fn document(conn: &mut PgConnection, number: &str) -> Result<Option<Stored>, sqlx::Error>;
pub async fn standing(conn: &mut PgConnection, now: Timestamp) -> Result<Standing, sqlx::Error>;
pub async fn documents(conn: &mut PgConnection, limit: i64, after: Option<&Cursor>)
    -> Result<Page<Stored>, sqlx::Error>;
```

`Unregistered` is an invoice issued before the business registered. It has no
place in the chain and cannot be cleared retrospectively, because the chain
starts at onboarding.

`Refused` means ZATCA said no and the document is what is wrong. A corrected
document is a new document, never an edit to this one.

`pending` returns **signed documents only**, oldest first. Oldest because the
24-hour clock runs from issue. Signed only because ZATCA refuses an unsigned one,
so submitting it would spend a tenant's rate limit to be told so, and the
rejection would be recorded against a document that is not what is wrong.

`standing` takes `now` as a parameter for the reason everything here does: a
report that cannot be asked "and how did this look on the last day of the
quarter?" is a report somebody screenshots.

## Recording what ZATCA said

```rust
pub enum ClearanceEvent { … }   // signed, cleared/reported, refused
pub struct Clearance { … }

pub async fn record_outcome(db: &TenantDb, document: &str, kind: Kind,
    verdict: &Verdict, at: Timestamp, metadata: &Metadata) -> …;
```

**The verdict is an event because it is the only part that did not come from us.**
Everything else about a ZATCA document, the XML and the hash and the chain and
the QR, is derived from the log and can be rebuilt. What ZATCA said cannot: it
happened once, over a network, and if it is not written down it is gone.

Which is also why it is not a column somebody updates. A cleared invoice is a
legal fact with a date, and "the row says cleared" is a much weaker statement
than "here is the event that says so, at this position, with the stamped document
ZATCA returned".

**What is not recorded** is a failure to *reach* ZATCA. A timeout, a 503, an
expired certificate: none of those are decisions about the document, so none are
appended. The document stays pending and the next sweep tries again.

Recording the same verdict twice writes nothing, which is what makes a submitter
that crashed between the call and the append safe to re-run.

## The sweeps

```rust
pub struct SignedOff { … }
pub struct Swept { … }
impl Swept { pub const fn did_something(&self) -> bool; }

pub async fn sign_pending(db: &TenantDb, sealing: &SealingKey, at: Timestamp,
    batch: i64, metadata: &Metadata) -> Result<SignedOff, SweepError>;

pub async fn submit_pending(db: &TenantDb, zatca: &dyn Submitter, at: Timestamp,
    batch: i64, metadata: &Metadata) -> Result<Swept, SweepError>;
```

Two sweeps, separate because they fail for different reasons and a document needs
the first even when the second cannot happen: a simplified invoice's QR carries
the stamp, and the receipt goes to the customer at the till whether or not ZATCA
is reachable.

**The rule the sweep exists to get right: a refusal is about the document; a
failure to ask is about us.** When ZATCA cannot be reached the sweep stops and
records nothing. Every document in the batch would fail the same way, and marking
them refused would be a permanent verdict on documents that are fine, written by
an outage.

Both are registered in `bin/worker.rs` as jobs (`SignZatcaDocuments`,
`SubmitToZatca`). They are functions here because a module cannot depend on
`erp-worker` without a cycle, and the composition root is what has both.

## The signature

**Why signing is not done in the projection.** ECDSA is randomised: every
signature over the same bytes with the same key is different, because a fresh `k`
goes into each one. A projection that signed would produce different tables on
every rebuild, which is the one thing a projection may not do, and the difference
would be in the column a tax authority holds a copy of.

It also needs the private key, and a projection that could read `module_secret`
would be a projection that could leak it.

So signing happens once, outside, and the result is recorded as an event. The
projection applies that event, which makes the stored signature a replay of
something that happened, never something recomputed.

```rust
pub struct Signer { … }
impl Signer {
    pub fn new(private_key_pem: &[u8], certificate: &X509) -> Result<Self, SigningError>;
    pub fn sign(&self, canonical: &str, invoice_hash: &str,
                signed_at: DateTime<Utc>) -> Result<Signature, SigningError>;
}

pub struct Signature { … }
impl Signature {
    pub fn qr(&self, seller: &str, vat_number: &str, issued_at: &str,
              total: &str, tax: &str, invoice_hash: &str) -> Result<String, SigningError>;
}

pub fn digest(text: &str) -> String;
pub fn certificate_digest(certificate_base64: &str) -> String;
pub fn issuer_name(certificate: &X509) -> String;
pub fn serial_number(certificate: &X509) -> String;
pub fn signed_properties(…) -> String;
pub fn signed_info(invoice: &str, properties: &str) -> String;
pub fn ubl_extensions(…) -> String;
```

`Signer` is built once per sweep and used for every document in it. Parsing a PEM
per invoice would be the most expensive thing in the loop.

`signed_properties` returns one string and not a builder, because **the
whitespace is inside the digest**. An editor reflowing that function changes
every signature it produces.

`certificate_digest` deviates from XAdES on purpose. The standard says this is
the digest of the certificate's DER; ZATCA hashes the certificate's **base64
text**, the characters and not the bytes they encode. A signature ZATCA cannot
verify is worth less than one that deviates in the same direction ZATCA does, and
a document signed this way is accepted.

## UBL, rendered already canonical

**Why it is written by hand.** ZATCA hashes the canonicalised document (C14N 1.1)
and the seller signs that hash, so a serialiser that reorders an attribute or
collapses an empty element changes the hash and invalidates the signature. The
usual pipeline, build a DOM and serialise and XSL-transform and canonicalise and
hash, has four places to go wrong and needs three libraries.

This writes canonical form directly, so canonicalising the output is the identity
function and `hash(bytes) == hash(c14n(bytes))`. The rules, all enforced by
`the_output_is_already_canonical`:

- UTF-8, `\n` endings, no XML declaration in the hashed bytes
- No empty-element tags: `<cbc:Note></cbc:Note>`, never `<cbc:Note/>`
- No comments, no processing instructions
- Namespaces declared once on the root, in prefix order
- Attributes in order, values with `&`, `<` and `"` escaped
- Text with `&`, `<` and `>` escaped, and control characters refused

```rust
pub struct NotRenderable { … }
pub struct Enveloped<'a> { … }

pub fn render(document: &Document) -> Result<String, NotRenderable>;
pub fn signed(document: &Document, enveloped: &Enveloped<'_>) -> Result<String, NotRenderable>;
pub fn with_declaration(canonical: &str) -> String;
```

`render` deliberately omits `ext:UBLExtensions`, `cac:Signature` and the QR's
`AdditionalDocumentReference`, which are the three things ZATCA *removes* before
hashing. Not generating them is the same document as generating and stripping
them, minus the stripping.

`signed` is rendered, never spliced into `render`'s output. A string
insertion at a marker is a second parser that has to agree with the first about
where the document's parts are, and the two disagreeing would produce a document
whose hash is right and whose shape is wrong.

`NotRenderable` is a refusal and not a strip. A document whose customer name
silently lost a character is one whose hash nobody can reproduce.

## The chain

```rust
pub fn invoice_hash(canonical_xml: &str) -> String;
pub fn genesis() -> String;

pub struct Link { … }
impl Link {
    pub fn first() -> Self;
    pub fn after(previous_icv: i64, previous_hash: &str) -> Self;
}
```

Each document carries the hash of the previous document (the **PIH**) and a
counter that never resets (the **ICV**), so removing an invoice from the middle
of a year breaks every hash after it.

**`genesis()` is the odd one out.** It is `base64(hex(sha256("0")))`, the base64
of the sixty-four *characters* `5feceb66…` and not of the thirty-two bytes they
spell. Every subsequent PIH is `base64(sha256(bytes))`, forty-four characters.
The two are encoded differently and that is not a mistake here: it is what
ZATCA's own documentation specifies, and a chain that "fixes" it is rejected at
the first invoice. It is spelled out and not pasted as a constant, so the
derivation is checkable.

## The QR

Tag-length-value, concatenated, base64. Not a URL: the data *is* the payload, so
a phone with no signal still verifies the seller, the tax, and the stamp.

```text
  01 10 "روابي للاستشارات"     seller name
  02 0F "310122393500003"      VAT number
  03 14 "2026-03-01T10:00:00Z" timestamp
  04 06 "115.00"               total, including VAT
  05 05 "15.00"                the VAT
```

**Length is a byte count, not a character count.** An Arabic name is two bytes a
letter, which is the mistake this module exists to not make.

```rust
pub struct TooLong { … }
pub struct Qr<'a> { … }
impl Qr<'_> { pub fn encode(&self) -> Result<String, TooLong>; }
pub fn decode(encoded: &str) -> Result<Vec<(u8, Vec<u8>)>, String>;
```

A field too long is refused, never truncated. A QR with half a seller's name in
it scans fine and is wrong, which is the worst of the three outcomes.

`decode` is not in a test module, because a QR nobody can read back is a QR nobody
can check. The decoder is what proves the encoder puts the bytes where it says it
does, and it is twenty lines.

There is one more quirk, and it was found against the live sandbox:

```rust
pub const QR_TIME: &str = "%Y-%m-%dT%H:%M:%S";     // no Z
```

ZATCA's own QR specification shows a `Z`. Its validator compares the value against
`cbc:IssueDate` + `T` + `cbc:IssueTime`, which carries no zone, so a `Z` produces
`invoiceTimeStamp_QRCODE_INVALID`.

## Certificates and onboarding

```text
  Fatoora portal ──► the taxpayer logs in and generates a 6-digit OTP
           │
           ▼
  1  generate a key pair and a CSR                     zatca::csr
  2  POST /compliance          OTP header + the CSR ──► compliance CSID
  3  POST /compliance/invoices one of each document ──► checks passed
  4  POST /production/csids    the request id       ──► production CSID
           │
           ▼
  from here on, clearance and reporting authenticate with the production CSID
```

```rust
pub enum Environment { Sandbox, Simulation, Production }
impl Environment {
    pub const ALL: [Self; 3];
    pub const fn template(self) -> &'static str;
    pub const fn base_url(self) -> &'static str;
}

pub struct Issues { … }
impl Issues {
    pub const fn both() -> Self;
    pub fn title(self) -> String;                 // "1100", "1000", "0100"
    pub const fn compliance_documents(self) -> usize;
}

pub struct Unit { … }
impl Unit { pub fn egs_serial(&self) -> String; } // "1-<solution>|2-<version>|3-<serial>"

pub struct Generated { … }                        // Debug redacted
impl Generated { pub fn csr_for_zatca(&self) -> String; }

pub fn generate(unit: &Unit, environment: Environment) -> Result<Generated, CsrError>;
pub fn renew(unit: &Unit, environment: Environment, private_key_pem: &[u8])
    -> Result<Generated, CsrError>;
```

**secp256k1, which is not the usual curve.** ZATCA specifies the Koblitz curve,
the one Bitcoin uses, not the secp256r1/P-256 that almost every other X.509 stack
defaults to. A CSR on the wrong curve is refused at onboarding, and the two are
one character apart in the name.
`the_curve_is_the_koblitz_one_and_not_the_usual_one` reads it back out of the
encoded key.

**Two X.509 extensions, written as exact DER bytes**, because both are shapes
OpenSSL's config-string builder cannot express through its Rust binding:

| OID | What |
|---|---|
| `1.3.6.1.4.1.311.20.2` | The certificate template name, which is **the environment** |
| `2.5.29.17` | `subjectAltName`, a `directoryName` holding the EGS identity |

`Environment` is a required argument everywhere it appears and never a default,
because the only visible difference is a string in one extension and a base URL,
and a mistake between them does not fail. It succeeds against the wrong
authority.

**Three template values, one per environment.** An earlier version had sandbox
and simulation sharing one, which a working simulation CSR from another
implementation disproved: it carries `PREZATCA-Code-Signing`. The sandbox does
not check, so the mistake would have surfaced at the first simulation onboarding
and nowhere before it.

`Issues` is a commitment and not a description. It decides which compliance
checks the unit has to pass before it gets a production certificate.

### Where the secrets go

Three, sealed in `module_secret`. Never the event log, which is immutable and
replayed, and never `proj_tax_sa`, which is rebuilt.

| Key | What |
|---|---|
| `tax_sa.zatca.key` | The private key. **Never leaves this process** |
| `tax_sa.zatca.compliance` | The compliance CSID, and the request id step 4 needs |
| `tax_sa.zatca.production` | The production CSID, what signs and submits real invoices |

What goes in the **log** is the fact that a certificate was issued, with its
subject, serial and validity. That answers "which certificate signed this
invoice?" years later, and none of it is secret.

The OTP appears in neither. It is the taxpayer's proof of who they are for about
an hour, and recording it would be recording a credential. `Otp`'s `Debug` is
redacted and its value is readable only through `header()`.

### The onboarder

```rust
pub struct Onboarder<'a> { … }
impl<'a> Onboarder<'a> {
    pub fn new(db: &'a TenantDb, sealing: &'a SealingKey,
               registrar: &'a dyn Registrar) -> Self;

    pub async fn onboard(&self, unit: &Unit, environment: Environment, otp: &Otp,
        at: Timestamp, metadata: &Metadata) -> Result<Issued, OnboardError>;      // 1 and 2

    pub async fn pass_compliance_checks(&self, registration: &Registration,
        unit: &Unit, environment: Environment, at: Timestamp)
        -> Result<ComplianceChecks, OnboardError>;                                // 3

    pub async fn go_live(&self, environment: Environment, at: Timestamp,
        metadata: &Metadata) -> Result<Issued, OnboardError>;                     // 4

    pub async fn renew(&self, unit: &Unit, environment: Environment, otp: &Otp,
        at: Timestamp, metadata: &Metadata) -> Result<Issued, OnboardError>;
}
```

**Steps 2 and 4 are separate calls on purpose.** Step 3 sits between them, needs
every sample document signed with the compliance certificate, and hiding both
certificate requests inside one function would hide that.

`onboard` seals the key **before** the call, so a certificate is never issued
against a key this tenant no longer has.

`renew` keeps the key. A new key would invalidate nothing already signed, because
the old certificate stays valid for what it covered, but it would mean two keys
to keep.

### The manual path

```rust
pub async fn begin(db: &TenantDb, sealing: &SealingKey, unit: &Unit,
    environment: Environment) -> Result<String, OnboardError>;

pub async fn accept_certificate(db: &TenantDb, sealing: &SealingKey, stage: Stage,
    environment: Environment, csid: &Csid, at: Timestamp, metadata: &Metadata)
    -> Result<Issued, OnboardError>;
```

Step 1 on its own, callable without a `Registrar`. Onboarding is a once-per-tenant
act with a human in the middle, so an operator can take this CSR, submit it with
`curl`, and paste what comes back into `accept_certificate`. That is the path a
deployment falls back to when the automated one breaks.

`accept_certificate` checks the certificate is **for the key this tenant holds**
before anything is stored. A certificate over somebody else's key is not a
smaller problem than no certificate: every signature made with it is rejected at
clearance, on an invoice a customer is waiting for, with an error that says
nothing about why.

### Reading the state back

```rust
pub async fn production(db: &TenantDb, sealing: &SealingKey)
    -> Result<Option<Csid>, OnboardError>;
pub async fn credentials(db: &TenantDb, sealing: &SealingKey, stage: Stage)
    -> Result<Option<Csid>, OnboardError>;
pub async fn private_key(db: &TenantDb, sealing: &SealingKey)
    -> Result<Option<Vec<u8>>, OnboardError>;
pub async fn reached(db: &TenantDb) -> Result<Vec<Stage>, OnboardError>;
```

`reached` answers "is this tenant onboarded?" **without unsealing anything**. A
status endpoint must not be able to read the key to answer it.

The status endpoint reads `projections::onboarding()`, a single row. It does not
load the `Onboarding` aggregate. It used to load the aggregate, which made the
log a query engine for one screen. A renewal appends another `CsidIssued` and does not
replace the last, so the aggregate grows without bound while the answer stays one
row. `stage` keeps the furthest reached and not the most recent, because a
production certificate does not un-issue the compliance one.

### The compliance samples

```rust
pub fn compliance_documents(registration: &Registration, unit: &Unit,
                            at: Timestamp) -> Vec<Document>;
pub const fn expected(issues: Issues) -> usize;
```

Between the two certificates, ZATCA makes the solution prove it can produce valid
documents: **one of every type the CSR declared**, signed with the compliance
certificate. Six for a unit that issues both kinds, three for one.

They are invented because there are none yet. A business onboards before it
issues, and the checks are about the *solution*.

**The compliance chain starts at one and is thrown away.** It shares nothing with
the tenant's real counter, which has not started yet and must start at one when
it does. Deriving these from `proj_tax_sa.zatca_document` would either consume six
real positions or produce a chain with a gap where the samples were, and a gap is
the one thing the chain exists to make impossible.

## The wire

```rust
pub enum Endpoint { … }
impl Endpoint {
    pub const fn of(kind: Kind) -> Self;
    pub const fn path(self) -> &'static str;
    pub const fn clearance_status(self) -> &'static str;
}

pub struct Submission { … }
pub struct Remark { … }
pub struct ValidationResults { … }
pub struct Answer { … }

pub enum Verdict { … }
impl Verdict { pub fn of(status: u16, body: &str) -> Result<Self, Unanswered>; }

pub enum Unanswered { … }

pub trait Submitter: Send + Sync + Debug { … }
pub trait Registrar: Send + Sync + Debug { … }
```

**The distinction that matters.** ZATCA saying no and ZATCA not answering are
different facts.

- **A verdict**, cleared or cleared-with-warnings or refused, is about the
  document. It is recorded in the log and it is final.
- **A failure to ask**, a timeout or an expired certificate or a 503, is about
  us. Nothing is recorded, because nothing was decided.

Collapsing the two is how a perfectly good invoice ends up permanently marked
rejected because a token expired, which is `Verdict::of`'s whole job.

| Status | Meaning |
|---|---|
| 200 | Accepted |
| 202 | Accepted, with warnings to look at |
| 400 | The document is wrong. Final |
| 401, 403 | **Not** the document: the solution is not onboarded |
| Anything else | ZATCA is unwell; try again later |

`Registrar` and `Submitter` are separate traits because they are a separate
authority: the first authenticates with an OTP or the compliance CSID and the
second with the production CSID, and a deployment may well have the second
working while the first is still being arranged.

## The client

```rust
pub struct Fatoora { … }
impl Fatoora {
    pub fn new(environment: Environment) -> Result<Self, Unanswered>;
    pub fn with_credentials(self, credentials: Csid) -> Self;
    pub fn at(self, base_url: impl Into<String>) -> Self;
}
impl Registrar for Fatoora { … }
impl Submitter for Fatoora { … }
```

Six endpoints on one host, and the host and the headers and the shapes are all
Saudi facts, so they belong beside the rest of them. What keeps D9 true is *when*
this runs: never inside a command, never inside a projection, only from a sweep.

Every call carries:

```text
  accept-version: V2            ZATCA's API version, not ours
  Accept-Language: en           the language of the validation messages
  Content-Type: application/json
```

and one of `OTP:` (onboarding), `Authorization: Basic` (everything after), or
`Clearance-Status: 0 | 1`.

It is cheap to clone and meant to be. The connection pool is inside, so one per
environment per process, and not one per call.

## Routes

| Method | Path |
|---|---|
| `GET` | `/v1/tax_sa/vat-return` |
| `GET` `POST` | `/v1/tax_sa/returns` |
| `GET` `PUT` | `/v1/tax_sa/registration` |
| `GET` | `/v1/tax_sa/zatca` |
| `GET` | `/v1/tax_sa/zatca/documents` |
| `GET` | `/v1/tax_sa/zatca/documents/{number}` |
| `GET` `POST` | `/v1/tax_sa/zatca/onboarding` |
| `PUT` | `/v1/tax_sa/zatca/onboarding/certificate` |
| `POST` | `/v1/tax_sa/zatca/onboarding/activate` |

## What is proven and what is not

Nine documents accepted with zero warnings, against the **sandbox**. Simulation
and production are untested, and both need a real taxpayer's OTP from the Fatoora
portal, so that is blocked on access and not on code.

Simulation is the one that matters: it is the environment ZATCA requires a
solution to pass before production, and its certificate template differs from
sandbox's, which is how that difference was found.

The sandbox tests are `#[ignore]`d and need credentials. Run them with:

```bash
cargo test -p tax_sa --test sandbox -- --ignored
```

Renewal is a five-year deadline with a sixty-day warning and no automation
possible, because it needs a human with an OTP. `CertificateExpiry` is the
invariant that raises the warning.
