//! **What a second repository is allowed to rely on.**
//!
//! # Why this test exists now and did not before
//!
//! With one repository, renaming a response field was a compile error. With two,
//! it is a deployment that silently stops working: the React booking site reads
//! `name_latin`, this build renames it, both deploy green, and the page shows
//! blanks to customers with no error anywhere.
//!
//! `the_document_matches_the_router` already stops the *document* drifting from
//! the server. Nothing stopped the document itself changing in a way that breaks
//! a client, and `/v1` in every path is a promise nobody was keeping.
//!
//! # How it works
//!
//! `docs/openapi.baseline.json` is what clients may rely on. Every build
//! compares the generated document against it and fails on a change that would
//! break a caller. Compatible changes — a new endpoint, a new optional field, a
//! new response status — pass untouched and the baseline is updated when
//! convenient.
//!
//! Accepting a break is deliberate and takes a person:
//!
//! ```bash
//! just baseline
//! ```
//!
//! which is the point. A break that somebody typed a command to accept is a
//! break somebody knows about; the alternative is one nobody sees until a
//! customer does.
//!
//! # What it checks, and what it deliberately does not
//!
//! It checks the four shapes that actually break callers in this API:
//!
//! | change | why it breaks |
//! |---|---|
//! | an operation disappears or is renamed | the call 404s |
//! | a required request field appears | every existing call becomes a 400 |
//! | a response field disappears | the client reads `undefined` |
//! | a path gains a parameter | the URL the client builds is wrong |
//!
//! It does **not** check type narrowing, enum members, or `format` changes.
//! Those break callers too, and a full structural diff is a different and much
//! larger piece of work — one worth having when the API has outside consumers,
//! and worth being honest about not having until then. What is here catches the
//! ones a normal refactor causes by accident, which is the failure this is
//! actually guarding against.

#![allow(clippy::expect_used)]

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const BASELINE: &str = "../../docs/openapi.baseline.json";

fn generated() -> Value {
    serde_json::to_value(erp_api::openapi()).expect("the document serializes")
}

fn baseline() -> Option<Value> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(BASELINE);
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// **No change here breaks a client that was written against the last one.**
#[test]
fn the_api_stays_compatible_with_what_clients_were_promised() {
    let current = generated();

    if std::env::var("REGENERATE_DOCS").is_ok() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(BASELINE);
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&current).expect("pretty-prints")
        );
        std::fs::write(&path, text).expect("writes the baseline");
        return;
    }

    let Some(baseline) = baseline() else {
        // The first run, before a baseline exists. Not a failure: there is
        // nothing anybody could have been promised yet.
        return;
    };

    let mut breaks = Vec::new();
    let was = operations(&baseline);
    let now = operations(&current);

    for (id, before) in &was {
        let Some(after) = now.get(id) else {
            breaks.push(format!(
                "{id} is gone. A client calling it gets a 404 — if this is a rename, \
                 it is a removal and an addition"
            ));
            continue;
        };

        if before.path != after.path {
            breaks.push(format!(
                "{id} moved from {} to {}. Every client's URL is now wrong",
                before.path, after.path
            ));
        }

        for name in after.required_body.difference(&before.required_body) {
            breaks.push(format!(
                "{id} now requires `{name}` in its body. Every existing call becomes a 400"
            ));
        }

        for name in before.response_fields.difference(&after.response_fields) {
            breaks.push(format!(
                "{id} no longer returns `{name}`. A client reading it now reads nothing"
            ));
        }

        for name in after.path_params.difference(&before.path_params) {
            breaks.push(format!(
                "{id} gained the path parameter `{name}`. The URL a client builds no longer resolves"
            ));
        }
    }

    assert!(
        breaks.is_empty(),
        "this changes the API in ways that break existing clients:\n  {}\n\n\
         `/v1` is a promise. If the break is intended, accept it deliberately with \
         `just baseline` — and think about whether the callers of this API have been told.",
        breaks.join("\n  ")
    );
}

/// What one operation promises.
#[derive(Debug, Default)]
struct Promise {
    path: String,
    /// Properties a request body requires. Adding one breaks every caller.
    required_body: BTreeSet<String>,
    /// Properties a success response carries. Removing one breaks every reader.
    response_fields: BTreeSet<String>,
    path_params: BTreeSet<String>,
}

fn operations(doc: &Value) -> BTreeMap<String, Promise> {
    let mut out = BTreeMap::new();
    let Some(paths) = doc["paths"].as_object() else {
        return out;
    };

    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, operation) in item {
            if !["get", "put", "post", "delete", "patch"].contains(&method.as_str()) {
                continue;
            }
            let Some(id) = operation["operationId"].as_str() else {
                continue;
            };

            out.insert(
                id.to_owned(),
                Promise {
                    path: path.clone(),
                    required_body: required_of(doc, &operation["requestBody"]),
                    response_fields: success_fields(doc, &operation["responses"]),
                    path_params: operation["parameters"]
                        .as_array()
                        .map(|params| {
                            params
                                .iter()
                                .filter(|p| p["in"] == "path")
                                .filter_map(|p| p["name"].as_str())
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            );
        }
    }
    out
}

/// The properties a request body marks required.
fn required_of(doc: &Value, body: &Value) -> BTreeSet<String> {
    let schema = resolve(doc, &body["content"]["application/json"]["schema"]);
    schema["required"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|n| n.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The properties a 2xx response carries, as dotted paths.
///
/// **Nested, and it has to be.** Almost every list in this API answers
/// `Paged<T>`, whose top level is `items` and `next` — so a check that looked
/// only at the top level would pass a rename of every field a client actually
/// reads. That is not a hypothetical: it is what the first version of this test
/// did, and renaming `ServiceView::name` sailed straight through it.
///
/// Bounded at [`MAX_DEPTH`] and by the set of refs already followed, because a
/// schema that references itself is legal and this must not be the thing that
/// hangs the build.
fn success_fields(doc: &Value, responses: &Value) -> BTreeSet<String> {
    let Some(responses) = responses.as_object() else {
        return BTreeSet::new();
    };

    let mut fields = BTreeSet::new();
    for (_, response) in responses.iter().filter(|(s, _)| s.starts_with('2')) {
        walk(
            doc,
            &response["content"]["application/json"]["schema"],
            "",
            0,
            &mut BTreeSet::new(),
            &mut fields,
        );
    }
    fields
}

/// How far into a response shape a client is taken to be reading.
///
/// Three levels covers `Paged<T>` → the item → one struct inside it, which is
/// every shape in this document. Deeper is possible and is where the cost of a
/// full structural diff starts.
const MAX_DEPTH: usize = 3;

fn walk(
    doc: &Value,
    schema: &Value,
    prefix: &str,
    depth: usize,
    seen: &mut BTreeSet<String>,
    into: &mut BTreeSet<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    // A `$ref` already on this branch is a cycle. Following it again would add
    // nothing and would not terminate.
    if let Some(reference) = schema["$ref"].as_str()
        && !seen.insert(reference.to_owned())
    {
        return;
    }
    let schema = resolve(doc, schema);

    // An array's fields are its items' fields, at the same name: a client
    // reading `items[0].name` is reading `items.name` as far as this is
    // concerned.
    if !schema["items"].is_null() {
        walk(doc, &schema["items"], prefix, depth, seen, into);
    }

    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    for (name, property) in properties {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        into.insert(path.clone());
        walk(doc, property, &path, depth + 1, seen, into);
    }
}

/// Follows one `$ref` into `components/schemas`.
///
/// One hop, not a full walk: every schema in this document is either inline or
/// a direct component reference, and `every_reference_resolves` is what keeps
/// that true.
fn resolve(doc: &Value, schema: &Value) -> Value {
    let Some(reference) = schema["$ref"].as_str() else {
        return schema.clone();
    };
    reference
        .rsplit('/')
        .next()
        .and_then(|name| doc["components"]["schemas"].get(name))
        .cloned()
        .unwrap_or_else(|| schema.clone())
}
