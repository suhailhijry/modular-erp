// The invoice as a page — the same content as the HTML print, laid out by
// typst so Arabic is shaped and the file is PDF/A-3 with the XML attached.
// Everything comes from `document.json`, which `tax_sa::pdf` writes from the
// stored document; nothing here is typed in by hand.
#let d = json("document.json")

#set document(title: d.number, author: d.seller.name)
#set text(font: "IBM Plex Sans Arabic", lang: "ar", dir: rtl, size: 9pt) if d.receipt
#set text(font: "IBM Plex Sans Arabic", lang: "ar", dir: rtl, size: 10pt) if not d.receipt
#set page(width: 80mm, height: auto, margin: 4mm) if d.receipt
#set page(paper: "a4", margin: 15mm) if not d.receipt
#set par(justify: false)

// The XML is the document; the page is what a person reads of it.
#pdf.attach(
  d.attachment,
  relationship: "data",
  mime-type: "application/xml",
  description: "ZATCA e-invoice XML (" + d.number + ")",
)

// A label in both languages, Arabic first.
#let both(ar, en) = [#ar #h(0.3em) #text(size: 0.8em, fill: luma(90), dir: ltr)[#en]]
#let num(s) = text(dir: ltr)[#s]

// The seller, and the title.
#align(if d.receipt { center } else { start })[
  #text(weight: "bold", size: 1.3em)[#d.seller.name]
  #if d.seller.name_latin != none [ #text(dir: ltr)[#d.seller.name_latin] ] \
  #both("الرقم الضريبي", "VAT no.") #num(d.seller.vat) \
  #d.seller.address \
  #v(0.4em)
  #text(weight: "bold", size: 1.15em)[#both(d.title_ar, d.title_en)]
]
#v(0.6em)

// The document's own facts, and the buyer's.
#let facts = (
  (both("رقم الفاتورة", "Invoice no."), num(d.number)),
  (both("التاريخ والوقت", "Date and time"), num(d.issued)),
)
#let facts = if d.reference != none {
  facts + ((both("مرجع الفاتورة الأصلية", "Original invoice"), num(d.reference.number + " (" + d.reference.day + ")")),)
} else { facts }
#let facts = if d.buyer != none {
  let more = ((both("العميل", "Customer"), d.buyer.name),)
  if d.buyer.vat != none { more.push((both("الرقم الضريبي للعميل", "Customer VAT no."), num(d.buyer.vat))) }
  if d.buyer.address != none { more.push((both("عنوان العميل", "Customer address"), d.buyer.address)) }
  facts + more
} else { facts }
#grid(
  columns: (auto, 1fr),
  row-gutter: 0.35em,
  column-gutter: 0.8em,
  ..facts.map(f => (text(weight: "bold")[#f.at(0)], f.at(1))).flatten(),
)
#v(0.6em)

// The lines.
#let head = (
  both("البيان", "Description"),
  both("الكمية", "Qty"),
  both("سعر الوحدة", "Unit price"),
  both("المبلغ", "Net"),
  both("الضريبة %", "VAT %"),
  both("الضريبة", "VAT"),
  both("الإجمالي", "Total"),
)
#let cells = d.lines.map(l => (l.description, num(l.qty), num(l.unit), num(l.net), num(l.rate), num(l.tax), num(l.gross)))
// A receipt has no room for the unit price.
#let keep = if d.receipt { (0, 1, 3, 4, 5, 6) } else { (0, 1, 2, 3, 4, 5, 6) }
#let pick(row) = keep.map(i => row.at(i))
#table(
  columns: (1fr,) + keep.slice(1).map(_ => auto),
  align: (start,) + keep.slice(1).map(_ => end),
  stroke: (x: none, y: 0.4pt + luma(200)),
  inset: 0.35em,
  table.header(..pick(head).map(c => text(weight: "bold")[#c])),
  ..cells.map(pick).flatten(),
)
#v(0.6em)

// The totals.
#let totals = ()
#if d.totals.before_discount != none {
  totals.push((both("الإجمالي قبل الخصم", "Before discount"), num(d.totals.before_discount)))
  totals.push((both("الخصم", "Discount"), num(d.totals.discount)))
}
#(totals.push((both("الإجمالي الخاضع للضريبة", "Total excluding VAT"), num(d.totals.net))))
#for b in d.totals.bands {
  totals.push((both("ضريبة القيمة المضافة", "VAT") + [ #num(b.rate)], num(b.tax)))
}
#(totals.push((text(weight: "bold", size: 1.15em)[#both("الإجمالي شامل الضريبة", "Total including VAT")], text(weight: "bold", size: 1.15em)[#num(d.totals.gross + " " + d.totals.currency)])))
#if d.totals.prepaid != none {
  totals.push((both("مدفوع مقدماً", "Prepaid"), num(d.totals.prepaid.gross + " (" + d.totals.prepaid.number + ")")))
}
#align(if d.receipt { center } else { end })[
  #grid(
    columns: (auto, auto),
    row-gutter: 0.35em,
    column-gutter: 1.2em,
    align: (start, end),
    ..totals.flatten(),
  )
]

#if d.note != "" [
  #v(0.6em)
  #d.note
]

// The QR, which is the whole reason a phone can check this.
#v(0.8em)
#align(center)[#image("qr.svg", width: 3.2cm)]
#v(0.6em)
#align(center)[#both("شكراً لتعاملكم معنا", "Thank you")]
