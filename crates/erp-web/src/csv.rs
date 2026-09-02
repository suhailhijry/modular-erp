//! Spreadsheets, both directions.
//!
//! # Export: the same query, a different encoder
//!
//! Every list in this API is a paged `GET` that answers JSON. An export is that
//! same query with a different encoder on the end — so a list added tomorrow is
//! exportable the day it exists, and nobody has to remember to make it so.
//!
//! That is why this is a **response layer** and not a per-handler concern.
//! `Accept: text/csv` on any list turns the page into a spreadsheet; nothing in
//! any module knows it happened.
//!
//! # What a cell may hold
//!
//! Scalars, and objects flattened one dot at a time — `stored.checksum`. **Not
//! arrays.** A cell holding `["a","b"]` is JSON wearing a spreadsheet's clothes,
//! and every consumer of it is a parser somebody wrote by hand. A list whose
//! rows contain arrays exports the columns that are not arrays, and the caller
//! who needs them asks for JSON.
//!
//! # Import: partial failure is the outcome, not an exception
//!
//! A thousand-row file with three bad rows **imports 997 and returns the
//! three**, with the row number and what was wrong. The alternative — refuse the
//! file — is what every import in this category does, and it means a person
//! fixing a spreadsheet by bisection.
//!
//! Each row is its own command under its own derived key, so re-uploading a
//! corrected file does not duplicate the 997. See [`row_key`].

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// The largest response this layer will re-encode.
///
/// A page, not a database. Every list in this API is capped well below it, and
/// the cap is here so a route that one day is not cannot make this layer buffer
/// something unbounded.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Turns a list into a spreadsheet when the caller asked for one.
///
/// Applied once, in `erp_api::router`, so it covers every list including the
/// ones that do not exist yet.
///
/// **Only `GET`, only `2xx`, only JSON.** A `POST` that answers a created record
/// is not a list; an error is `application/problem+json` and a client that
/// asked for CSV still needs to be able to read the refusal.
pub async fn layer(request: Request, next: Next) -> Response {
    let wanted = request.method() == axum::http::Method::GET
        && request
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|accept| accept.contains("text/csv"));

    let response = next.run(request).await;
    if !wanted || !response.status().is_success() {
        return response;
    }
    if !response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|kind| kind.starts_with("application/json"))
    {
        return response;
    }

    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the body could not be read",
        )
            .into_response();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    let Some(rows) = rows_of(&value) else {
        // Not a list. Answering `406` says what happened — the alternative is
        // handing back JSON to a client that said it wanted CSV, which it will
        // fail to parse somewhere less obvious.
        return (
            StatusCode::NOT_ACCEPTABLE,
            "this is not a list, so it has no spreadsheet form",
        )
            .into_response();
    };

    let sheet = encode(rows);
    (
        parts.status,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"export.csv\"".to_owned(),
            ),
        ],
        sheet,
    )
        .into_response()
}

/// The rows in a response body, whatever shape the list came in.
///
/// A paged list is `{ "items": [...] }` and an unpaged one is a bare array.
/// Both are lists; a single record is not.
fn rows_of(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    value
        .as_array()
        .or_else(|| value.get("items").and_then(serde_json::Value::as_array))
}

/// Rows to CSV, with the union of every row's columns as the header.
///
/// **The union, not the first row's keys.** A field that is `null` on the first
/// invoice and set on the second is a column somebody needs, and taking the
/// first row's shape as the schema is how it goes missing.
fn encode(rows: &[serde_json::Value]) -> String {
    let mut columns: Vec<String> = Vec::new();
    let mut flattened: Vec<std::collections::BTreeMap<String, String>> =
        Vec::with_capacity(rows.len());

    for row in rows {
        let mut cells = std::collections::BTreeMap::new();
        flatten("", row, &mut cells);
        for column in cells.keys() {
            if !columns.contains(column) {
                columns.push(column.clone());
            }
        }
        flattened.push(cells);
    }
    columns.sort();

    let mut writer = csv::Writer::from_writer(Vec::new());
    let _ = writer.write_record(&columns);
    for row in &flattened {
        let _ = writer.write_record(
            columns
                .iter()
                .map(|column| row.get(column).map_or("", String::as_str)),
        );
    }

    writer
        .into_inner()
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

/// One JSON value into flat cells.
///
/// Objects nest with a dot. **Arrays are skipped** — see the module docs.
fn flatten(
    prefix: &str,
    value: &serde_json::Value,
    into: &mut std::collections::BTreeMap<String, String>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, child) in fields {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten(&path, child, into);
            }
        }
        serde_json::Value::Array(_) => {}
        serde_json::Value::Null => {
            into.insert(prefix.to_owned(), String::new());
        }
        serde_json::Value::String(text) => {
            into.insert(prefix.to_owned(), text.clone());
        }
        other => {
            into.insert(prefix.to_owned(), other.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// A row read out of an uploaded spreadsheet.
///
/// Every value is a string, because that is what a spreadsheet holds. Turning
/// `"1200.50"` into money is the importing module's, which is the only thing
/// that knows what the column means.
pub type Row = std::collections::BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CsvError {
    #[error("that is not a spreadsheet this system can read: {0}")]
    Unreadable(String),
    #[error("a spreadsheet needs a header row")]
    NoHeader,
    #[error("a spreadsheet may not have more than {0} rows")]
    TooManyRows(usize),
}

/// The most rows one upload may carry.
///
/// Ten thousand, which is a year of invoices for the businesses this is for and
/// small enough that a request holding one is not a way to take the process
/// down. Beyond it, an import is a file (11c) and an effect — and that is the
/// shape to build when somebody has one.
pub const MAX_ROWS: usize = 10_000;

/// Reads a spreadsheet into rows keyed by column name.
///
/// A BOM is stripped, because every spreadsheet that has ever come out of Excel
/// on Windows has one and the first column would otherwise be called
/// `\u{feff}id`.
pub fn parse(bytes: &[u8]) -> Result<Vec<Row>, CsvError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);

    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(bytes);

    let headers: Vec<String> = reader
        .headers()
        .map_err(|e| CsvError::Unreadable(e.to_string()))?
        .iter()
        .map(|h| h.trim().to_owned())
        .collect();
    if headers.is_empty() || headers.iter().all(String::is_empty) {
        return Err(CsvError::NoHeader);
    }

    let mut rows = Vec::new();
    for record in reader.records() {
        if rows.len() >= MAX_ROWS {
            return Err(CsvError::TooManyRows(MAX_ROWS));
        }
        let record = record.map_err(|e| CsvError::Unreadable(e.to_string()))?;
        let mut row = Row::new();
        for (column, value) in headers.iter().zip(record.iter()) {
            if !column.is_empty() {
                row.insert(column.clone(), value.trim().to_owned());
            }
        }
        // A blank line in the middle of a spreadsheet is a blank line, not a
        // row that failed. Excel leaves them everywhere.
        if row.values().any(|v| !v.is_empty()) {
            rows.push(row);
        }
    }

    Ok(rows)
}

/// One row that did not go in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Rejected {
    /// **The spreadsheet's own row number**, counting the header as row 1, so
    /// it is the number the person's editor is showing them.
    pub row: usize,
    /// The message code, so a client can branch on it.
    pub code: String,
    /// What went wrong, in the language the request asked for.
    pub detail: String,
}

/// What an import did.
///
/// **Partial failure is the outcome and not an exception.** A thousand-row file
/// with three bad rows imports 997 and returns the three; refusing the file
/// means a person fixing a spreadsheet by bisection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Imported {
    /// Rows that went in, including ones that were already there — a re-upload
    /// of a corrected file is meant to be safe, so a row that was imported last
    /// time counts as imported this time.
    pub imported: usize,
    /// Rows that did not, with the row number and the reason.
    pub rejected: Vec<Rejected>,
}

impl Imported {
    #[must_use]
    pub const fn clean(&self) -> bool {
        self.rejected.is_empty()
    }
}

/// The idempotency key for one row of one import.
///
/// # Why a row needs its own
///
/// An import is a command per row, and the kernel's create refuses a taken id
/// unless the fingerprint matches. Giving every row the file's key would make
/// row two look like a retry of row one; giving each row a fresh one would make
/// a re-upload of a corrected file duplicate the 997 that already went in.
///
/// So it is derived from the caller's key **and** the row's own identity, which
/// makes it stable across uploads of the same file and distinct within one.
#[must_use]
pub fn row_key(import: &str, row: &str) -> String {
    format!("{import}.{row}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_and_a_bare_list_are_both_lists() {
        let paged = serde_json::json!({"items": [{"a": 1}], "next": "x"});
        assert!(rows_of(&paged).is_some());

        let bare = serde_json::json!([{"a": 1}]);
        assert!(rows_of(&bare).is_some());

        let single = serde_json::json!({"a": 1});
        assert!(rows_of(&single).is_none(), "a record is not a list");
    }

    /// **The union of every row's columns**, not the first row's.
    #[test]
    fn a_column_that_is_only_on_the_second_row_is_still_a_column() {
        let rows = vec![
            serde_json::json!({"id": "A", "note": null}),
            serde_json::json!({"id": "B", "note": "late", "paid": 100}),
        ];
        let sheet = encode(&rows);
        let header = sheet.lines().next().unwrap_or_default();
        assert_eq!(header, "id,note,paid");
        assert_eq!(sheet.lines().nth(1), Some("A,,"));
        assert_eq!(sheet.lines().nth(2), Some("B,late,100"));
    }

    #[test]
    fn nested_objects_flatten_and_arrays_are_left_out() {
        let rows = vec![serde_json::json!({
            "id": "A",
            "stored": {"engine": "local", "size": 12},
            "lines": [1, 2, 3]
        })];
        let sheet = encode(&rows);
        assert_eq!(
            sheet.lines().next(),
            Some("id,stored.engine,stored.size"),
            "{sheet}"
        );
    }

    /// A cell with a comma, a quote or a newline in it. This is the reason the
    /// encoder is a library and not a `join(",")`.
    #[test]
    fn a_cell_that_would_break_a_naive_encoder_survives() {
        let rows = vec![serde_json::json!({
            "name": "Najd, Ltd \"the\" one\nsecond line"
        })];
        let sheet = encode(&rows);
        let back = parse(sheet.as_bytes()).expect("reads back");
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].get("name").map(String::as_str),
            Some("Najd, Ltd \"the\" one\nsecond line")
        );
    }

    #[test]
    fn a_spreadsheet_out_of_excel_reads() {
        // A BOM, CRLF line endings, a blank line, and trailing spaces.
        let sheet =
            b"\xEF\xBB\xBFid,name\r\nC-1, \xD9\x86\xD9\x88\xD8\xB1\xD8\xA9 \r\n\r\nC-2,Ahmed\r\n";
        let rows = parse(sheet).expect("reads");
        assert_eq!(rows.len(), 2, "the blank line is not a row: {rows:?}");
        assert_eq!(rows[0].get("id").map(String::as_str), Some("C-1"));
        assert_eq!(rows[0].get("name").map(String::as_str), Some("نورة"));
        assert_eq!(rows[1].get("name").map(String::as_str), Some("Ahmed"));
    }

    #[test]
    fn a_row_key_is_stable_across_uploads_and_distinct_within_one() {
        assert_eq!(row_key("imp-1", "C-1"), row_key("imp-1", "C-1"));
        assert_ne!(row_key("imp-1", "C-1"), row_key("imp-1", "C-2"));
        assert_ne!(row_key("imp-1", "C-1"), row_key("imp-2", "C-1"));
    }

    #[test]
    fn a_file_with_no_header_is_refused() {
        assert_eq!(parse(b""), Err(CsvError::NoHeader));
    }
}
