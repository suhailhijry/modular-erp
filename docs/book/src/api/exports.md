# Spreadsheets, both directions

Phase 11d. Neither half lives in a module: the export is a layer and the import
is shared machinery plus one line per module that wants it.

## Export: the same query, a different encoder

Every list in this API is a paged `GET` that answers JSON. An export is that same
query with a different encoder on the end:

```bash
curl -H 'Accept: text/csv' -H "Authorization: Bearer $TOKEN" \
     https://bassat.erp.com/v1/crm/customers
```

It is a **response layer**, applied once in `erp_api::router`, so a list added
tomorrow is exportable the day it exists and nobody has to remember to make it
so. No handler knows it happened.

`GET` only, `2xx` only, `application/json` only. An error is
`application/problem+json` and a client that asked for CSV still has to be able
to read the refusal.

### What a cell may hold

Scalars, and objects flattened one dot at a time — `stored.checksum`.

**Not arrays.** A cell holding `["a","b"]` is JSON wearing a spreadsheet's
clothes, and every consumer of it is a parser somebody wrote by hand. A list
whose rows contain arrays exports the columns that are not arrays; a caller who
needs them asks for JSON.

### The header is the union of every row's columns

Not the first row's. A field that is `null` on the first invoice and set on the
second is a column somebody needs, and taking the first row's shape as the
schema is how it goes missing.

### What is deliberately not built

**The asynchronous export.** Every list here is capped at a page, so none takes
a minute and none holds a connection long enough to matter. "Generate, store,
send a link" is no longer a design question — a file is `modules/files`, an
effect is the outbox, a link is `erp-links` — it is a job to write when a list
becomes unbounded.

## Import: partial failure is the outcome

```bash
curl -X POST -H 'Content-Type: text/csv' \
     -H "Idempotency-Key: $(uuidgen)" -H "Authorization: Bearer $TOKEN" \
     --data-binary @customers.csv \
     https://bassat.erp.com/v1/crm/customers/import
```

```json
{ "imported": 997,
  "rejected": [ { "row": 43, "code": "crm.no_name", "detail": "A customer needs a name." } ] }
```

A thousand-row file with three bad rows **imports 997 and returns the three**.
The alternative — refuse the file — is what every import in this category does,
and it means somebody fixing a spreadsheet by bisection.

`row` counts the header as row 1, so it is the number the person's editor is
showing them.

### Re-uploading a corrected file is safe

Each row is its own command under a key derived from the file's key **and** the
row's id (`erp_web::importing`). Giving every row the file's key would make row
two look like a retry of row one; giving each row a fresh one would duplicate the
997 that already went in.

A row that was already there counts as imported, because a re-upload of a
corrected file is meant to be safe.

### Reading a spreadsheet that came out of Excel

The BOM is stripped — otherwise the first column is called `\u{feff}id` — CRLF
is handled, values are trimmed, and a blank line in the middle is a blank line
rather than a row that failed. RFC 4180 quoting is the `csv` crate's, because a
field can hold a comma, a quote or a newline and the number of ways to write
that parser wrong is why it is a dependency rather than twenty lines.

Ten thousand rows is the cap on one upload. Beyond it, an import is a file and an
effect — and that is the shape to build when somebody has one.

### Adding an import to another module

`erp_web::csv::parse` gives rows keyed by column name; `Imported` and `Rejected`
are the outcome shape; `importing(&tenant, &key, row_id)` is the metadata. What a
module writes is the loop that turns one row into one command — which is the only
part that knows what a column means.

`crm::http::import_customers` is the worked example, and
`every_handler_that_creates_passes_the_idempotency_key` knows about `importing`
so a second one cannot forget the key.
