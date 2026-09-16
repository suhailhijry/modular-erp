//! The document as PDF/A-3, the XML embedded — what a standard invoice is
//! *shared* as under ZATCA's rules, and what a receipt can be too.
//!
//! # Why typst
//!
//! Decided 2026-09-16. A PDF of an Arabic invoice needs shaping, bidi
//! ordering, a laid-out table, an embedded subset of a font, and PDF/A-3b's
//! metadata, output intent and attachment — each of which a validator refuses
//! for one missing key. typst as a library does all of it from one template,
//! `print/invoice.typ`, and its PDF/A output is validated upstream. The cost is
//! a larger binary, which is worth not writing a layout engine by hand.
//!
//! # How a document reaches the template
//!
//! Not by building typst markup from strings — a description with a `#` or a
//! `]` in it would be code. The template reads `document.json`, and this module
//! is the [`World`] that serves it: that file, the QR as `qr.svg`, the XML under
//! the name it is attached as, the two faces of IBM Plex Sans Arabic (OFL, in
//! `fonts/`), and nothing else — there is no filesystem behind it.

use std::collections::HashMap;

use chrono::Datelike as _;
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Smart};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt as _, World};
use typst_layout::PagedDocument;
use typst_pdf::{PdfOptions, PdfStandard, PdfStandards, Timestamp};

use crate::print;
use crate::zatca::Document;

static REGULAR: &[u8] = include_bytes!("../fonts/IBMPlexSansArabic-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../fonts/IBMPlexSansArabic-Bold.ttf");
const TEMPLATE: &str = include_str!("../print/invoice.typ");

/// Why no PDF came out. All of these are ours — the template, the fonts, the
/// standard — and none is something a caller can correct.
#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("the invoice template did not compile: {0}")]
    Compile(String),
    #[error("the PDF could not be written: {0}")]
    Export(String),
    #[error("the document could not be described: {0}")]
    Describe(#[from] serde_json::Error),
}

/// The document as PDF/A-3b, `xml` attached as `{number}.xml`.
///
/// # Errors
/// [`PdfError`] — a build problem, never the document's.
pub fn pdf(document: &Document, qr: &str, xml: &str) -> Result<Vec<u8>, PdfError> {
    let attachment = format!("{}.xml", document.number);
    let described = serde_json::to_vec(&View::of(document, qr, &attachment))?;
    let world = Printer::new(document, described, qr, &attachment, xml)?;

    let compiled = typst::compile::<PagedDocument>(&world);
    let paged = compiled.output.map_err(|errors| {
        PdfError::Compile(
            errors
                .iter()
                .map(|e| e.message.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;

    let standards = PdfStandards::new(&[PdfStandard::A_3b])
        .map_err(|e| PdfError::Export(e.message().to_string()))?;
    let options = PdfOptions {
        ident: Smart::Custom(document.number.clone()),
        creator: Smart::Custom(Some("erp".to_owned())),
        timestamp: Some(Timestamp::new_utc(world.today)),
        standards,
        ..PdfOptions::default()
    };
    typst_pdf::pdf(&paged, &options).map_err(|errors| {
        PdfError::Export(
            errors
                .iter()
                .map(|e| e.message.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        )
    })
}

/// Everything the template needs, already formatted — the same facts and the
/// same formatting as the HTML print, so the two never disagree.
#[derive(Debug, serde::Serialize)]
struct View {
    receipt: bool,
    title_ar: &'static str,
    title_en: &'static str,
    number: String,
    issued: String,
    seller: Party,
    buyer: Option<Party>,
    reference: Option<ReferenceView>,
    lines: Vec<LineView>,
    totals: TotalsView,
    note: String,
    attachment: String,
}

#[derive(Debug, serde::Serialize)]
struct Party {
    name: String,
    name_latin: Option<String>,
    vat: Option<String>,
    address: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct ReferenceView {
    number: String,
    day: String,
}

#[derive(Debug, serde::Serialize)]
struct LineView {
    description: String,
    qty: String,
    unit: String,
    net: String,
    rate: String,
    tax: String,
    gross: String,
}

#[derive(Debug, serde::Serialize)]
struct TotalsView {
    before_discount: Option<String>,
    discount: Option<String>,
    net: String,
    bands: Vec<BandView>,
    gross: String,
    currency: String,
    prepaid: Option<PrepaidView>,
}

#[derive(Debug, serde::Serialize)]
struct BandView {
    rate: String,
    tax: String,
}

#[derive(Debug, serde::Serialize)]
struct PrepaidView {
    gross: String,
    number: String,
}

impl View {
    fn of(document: &Document, _qr: &str, attachment: &str) -> Self {
        let (title_ar, title_en) = print::title(document);
        let seller = &document.seller;
        Self {
            receipt: document.kind == crate::zatca::Kind::Simplified,
            title_ar,
            title_en,
            number: document.number.clone(),
            issued: document.calendar.clock(document.issued_at),
            seller: Party {
                name: seller.name.clone(),
                name_latin: seller.name_latin.clone(),
                vat: Some(seller.vat_number.clone()),
                address: Some(print::address(
                    &seller.address.street,
                    Some(&seller.address.building),
                    seller.address.district.as_str(),
                    &seller.address.city,
                    Some(&seller.address.postal_code),
                    &seller.address.country,
                )),
            },
            buyer: document.buyer.as_ref().map(|buyer| Party {
                name: buyer.name.clone(),
                name_latin: None,
                vat: buyer.vat_number.clone(),
                address: buyer.address.as_ref().map(|at| {
                    print::address(
                        &at.street,
                        at.building.as_deref(),
                        at.district.as_deref().unwrap_or_default(),
                        &at.city,
                        at.postal_code.as_deref(),
                        &at.country,
                    )
                }),
            }),
            reference: document.reference.as_ref().map(|r| ReferenceView {
                number: r.number.clone(),
                day: document.calendar.day(r.issued_at).to_string(),
            }),
            lines: document
                .lines
                .iter()
                .map(|line| LineView {
                    description: line.description.clone(),
                    qty: line.units().to_string(),
                    unit: line.price().map(print::amount).unwrap_or_default(),
                    net: print::amount(line.net),
                    rate: print::percent(line.rate_bp),
                    tax: print::amount(line.tax),
                    gross: line.gross().map(print::amount).unwrap_or_default(),
                })
                .collect(),
            totals: TotalsView {
                before_discount: document.totals.before_discount.map(print::amount),
                discount: document
                    .totals
                    .before_discount
                    .map(|_| print::amount(document.totals.discount())),
                net: print::amount(document.totals.net),
                bands: document
                    .totals
                    .bands
                    .iter()
                    .map(|b| BandView {
                        rate: print::percent(b.rate_bp),
                        tax: print::amount(b.tax),
                    })
                    .collect(),
                gross: print::amount(document.totals.gross),
                currency: document.currency.to_string(),
                prepaid: document.prepaid.as_ref().map(|p| PrepaidView {
                    gross: print::amount(p.gross(document.currency)),
                    number: p.number.clone(),
                }),
            },
            note: document.note.clone(),
            attachment: attachment.to_owned(),
        }
    }
}

/// The template's whole world: its source, four files, two faces.
struct Printer {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    main: Source,
    files: HashMap<FileId, Bytes>,
    /// The document's day on the business's clock — the PDF's date, and
    /// `today` for the template. Never the wall clock.
    today: Datetime,
}

impl Printer {
    fn new(
        document: &Document,
        described: Vec<u8>,
        qr: &str,
        attachment: &str,
        xml: &str,
    ) -> Result<Self, PdfError> {
        let fonts: Vec<Font> = [REGULAR, BOLD]
            .into_iter()
            .filter_map(|face| Font::new(Bytes::new(face), 0))
            .collect();
        let book = FontBook::from_infos(fonts.iter().map(|f| f.info().clone()));

        let id = |name: &str| -> Result<FileId, PdfError> {
            let vpath = VirtualPath::new(name).map_err(|e| PdfError::Compile(e.to_string()))?;
            Ok(FileId::new(RootedPath::new(VirtualRoot::Project, vpath)))
        };
        let svg = print::qr_svg(qr).unwrap_or_else(|| {
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"/>".to_owned()
        });
        let mut files = HashMap::new();
        files.insert(id("/document.json")?, Bytes::new(described));
        files.insert(id("/qr.svg")?, Bytes::from_string(svg));
        files.insert(
            id(&format!("/{attachment}"))?,
            Bytes::from_string(xml.to_owned()),
        );

        let day = document.calendar.day(document.issued_at);
        let today = Datetime::from_ymd(
            day.year(),
            u8::try_from(day.month()).unwrap_or(1),
            u8::try_from(day.day()).unwrap_or(1),
        )
        .ok_or_else(|| PdfError::Compile("the document's day is not a date".to_owned()))?;

        Ok(Self {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(book),
            fonts,
            main: Source::new(id("/invoice.typ")?, TEMPLATE.to_owned()),
            files,
            today,
        })
    }
}

impl World for Printer {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else {
            Err(FileError::Other(Some(
                "the template is the only source".into(),
            )))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.files
            .get(&id)
            .cloned()
            .ok_or_else(|| FileError::Other(Some("no such file in the print".into())))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<typst::foundations::Duration>) -> Option<Datetime> {
        Some(self.today)
    }
}
