//! **The `OpenAPI` document, generated and checked.**
//!
//! Generated from the router that serves the requests, so the document cannot
//! describe a route the server does not have. Regenerate with:
//!
//! ```text
//! just openapi
//! ```
//!
//! # What is structural and what is checked here
//!
//! The paths and the methods come from `utoipa-axum`, which registers the axum
//! route *from* the `#[utoipa::path]` attribute — one string, not two that agree
//! today. The schemas come from the wire types by derive. Neither can drift.
//!
//! What is hand-written is the response declarations, the examples, and the
//! parameter descriptions. This file checks the parts of those that are checkable
//! without a running server; `http.rs` checks the rest by validating every real
//! response it receives against the schema this document publishes.

#![allow(clippy::expect_used)]

use std::collections::BTreeSet;

use serde_json::Value;

const DOCUMENT: &str = "../../docs/openapi.json";

fn document() -> Value {
    serde_json::to_value(erp_api::openapi()).expect("the document serializes")
}

/// **The drift check.** A published document that no longer matches the server
/// is worse than none: it is believed.
#[test]
fn the_document_matches_the_router() {
    let generated = format!(
        "{}\n",
        serde_json::to_string_pretty(&document()).expect("pretty-prints")
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(DOCUMENT);

    if std::env::var("REGENERATE_DOCS").is_ok() {
        std::fs::write(&path, &generated).expect("writes the document");
        return;
    }

    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        current, generated,
        "docs/openapi.json is out of date. Run `just openapi`."
    );
}

/// Every `$ref` names a schema that is there.
///
/// The failure this catches is a type reachable only through
/// `#[schema(value_type = …)]` or a `responses(body = …)`, which utoipa refs but
/// does not always register — a document that looks complete and is unusable in
/// any generator.
#[test]
fn every_reference_resolves() {
    let doc = document();
    let known: BTreeSet<String> = doc["components"]["schemas"]
        .as_object()
        .expect("there are schemas")
        .keys()
        .cloned()
        .collect();

    let mut refs = BTreeSet::new();
    collect_refs(&doc, &mut refs);
    assert!(!refs.is_empty(), "nothing referenced anything");

    for reference in &refs {
        let name = reference
            .strip_prefix("#/components/schemas/")
            .unwrap_or_else(|| panic!("{reference} is not a local component reference"));
        assert!(
            known.contains(name),
            "{reference} is referenced and not defined; defined: {known:?}"
        );
    }
}

fn collect_refs(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "$ref"
                    && let Some(reference) = child.as_str()
                {
                    into.insert(reference.to_owned());
                } else {
                    collect_refs(child, into);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_refs(item, into)),
        _ => {}
    }
}

/// Every operation says what it answers with, in both directions.
///
/// An operation with no success response is one nobody can write a client
/// against; one whose failures are undocumented sends an integrator to read
/// Rust, which is what this document exists to prevent.
#[test]
fn every_operation_declares_its_answers() {
    for (path, method, operation) in operations(&document()) {
        let responses = operation["responses"]
            .as_object()
            .unwrap_or_else(|| panic!("{method} {path} declares no responses"));

        let where_it_is = format!("{method} {path}");
        assert!(
            responses.keys().any(|s| s.starts_with('2')),
            "{where_it_is} has no success response"
        );
        assert!(
            operation["operationId"].is_string(),
            "{where_it_is} has no operationId"
        );
        assert!(
            operation["description"].is_string() || operation["summary"].is_string(),
            "{where_it_is} is undescribed — a client reading only this document \
             has to guess what it does"
        );

        for (status, response) in responses {
            assert!(
                response["description"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty()),
                "{where_it_is} → {status} has no description"
            );
            // Every failure carries a problem document, and a client that can
            // branch on `code` needs to know that from here rather than by
            // trying it.
            if status.starts_with('4') || status.starts_with('5') {
                assert_eq!(
                    response["content"]["application/json"]["schema"]["$ref"],
                    Value::String("#/components/schemas/Problem".to_owned()),
                    "{where_it_is} → {status} does not declare a Problem body"
                );
            }
        }
    }
}

/// The conventions in [`erp_api::openapi`]'s `Conventions` reached everything.
///
/// Applied by a `Modify` rather than per-handler precisely so none can be
/// missed; this is the assertion that the mechanism ran.
#[test]
fn every_path_takes_accept_language() {
    let doc = document();
    let paths = doc["paths"].as_object().expect("there are paths");
    assert!(
        paths.len() > 20,
        "only {} paths — did they register?",
        paths.len()
    );

    for (path, item) in paths {
        let names: Vec<&str> = item["parameters"]
            .as_array()
            .map(|params| {
                params
                    .iter()
                    .filter_map(|p| p["name"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            names.contains(&"Accept-Language"),
            "{path} does not take Accept-Language; it has {names:?}"
        );
    }

    assert_eq!(
        doc["components"]["securitySchemes"]["session"]["scheme"],
        Value::String("bearer".to_owned()),
        "the bearer scheme did not reach the document"
    );
}

/// The open routes are the ones that are meant to be open.
///
/// Authentication is the one thing a document can be wrong about in a way that
/// costs more than a wasted afternoon, so the exceptions are listed rather than
/// counted. Anything else that opts out fails here.
#[test]
fn only_the_deliberately_public_routes_are_public() {
    const PUBLIC: &[(&str, &str)] = &[
        ("get", "/v1/health"),
        ("get", "/v1/openapi.json"),
        ("post", "/v1/sessions"),
        ("post", "/v1/signups"),
        // The token in the path is the credential, and the person holding it
        // has no account yet — that is what confirming creates.
        ("post", "/v1/signups/{token}"),
        ("get", "/v1/catalogue"),
        ("get", "/v1/ledger/charts"),
        // Same argument as the charts above: a signup form has to show a salon
        // what a salon gets before anybody has an account. Product information,
        // not tenant data — nothing here reads a database.
        ("get", "/v1/booking/trades"),
        // The token in the path is the credential.
        ("get", "/v1/join/{token}"),
        ("post", "/v1/join/{token}"),
        // **Phase 17: a tenant's own customers, who have no account here.**
        //
        // These do read a tenant's data, which is what makes them different in
        // kind from everything above — and the reason they are safe is not that
        // the data is harmless but that `erp_web::Public` carries **no access
        // at all**: every capability check refuses it, so neither of these can
        // reach a guarded command by omission.
        //
        // They are also deliberately narrower than their authenticated
        // counterparts. `services` never shows a withdrawn resource and never
        // its capacity; `availability` answers one number.
        ("get", "/v1/booking/public/services"),
        ("get", "/v1/booking/public/availability"),
        // The one public **write**, and the only one in the build. It is off
        // unless the business turned it on, it never confirms what it takes,
        // and it never names a customer record on the caller's word.
        ("post", "/v1/booking/public/reservations"),
    ];

    for (path, method, operation) in operations(&document()) {
        // `security: []` on an operation overrides the document-wide default.
        let open = operation["security"]
            .as_array()
            .is_some_and(std::vec::Vec::is_empty);
        let expected = PUBLIC.contains(&(method.as_str(), path.as_str()));
        assert_eq!(
            open, expected,
            "{method} {path}: open={open}, and the list says {expected}"
        );
    }
}

fn operations(doc: &Value) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    for (path, item) in doc["paths"].as_object().expect("there are paths") {
        for (method, operation) in item.as_object().expect("a path item") {
            if [
                "get", "put", "post", "delete", "patch", "head", "options", "trace",
            ]
            .contains(&method.as_str())
            {
                out.push((path.clone(), method.clone(), operation.clone()));
            }
        }
    }
    assert!(!out.is_empty(), "no operations at all");
    out
}

/// Every error code the document names is one the API can actually answer with.
///
/// The prose in a `responses(…)` block is hand-written, so a code in it is a
/// claim rather than a fact. Two were wrong on the first pass — `auth.no_session`
/// and `control.internal`, neither of which exists — and a client that branched
/// on either would have waited forever for a code nothing sends.
#[test]
fn every_code_the_document_cites_exists() {
    let known: BTreeSet<&str> = erp_i18n::Catalog::codes(&erp_api::CATALOG)
        .iter()
        .map(erp_i18n::MessageCode::as_str)
        .collect();
    let namespaces: BTreeSet<&str> = known.iter().filter_map(|c| c.split('.').next()).collect();

    let blob = serde_json::to_string(&document()).expect("serializes");
    let mut cited = BTreeSet::new();
    // Codes are written in backticks, which is also how JSON paths like
    // `args.reason` are written — so a token only counts when its namespace is
    // one the catalog uses. A typo inside a real namespace is the case worth
    // catching, and it is the likely one.
    for token in blob.split('`') {
        let Some((namespace, rest)) = token.split_once('.') else {
            continue;
        };
        if namespaces.contains(namespace)
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        {
            cited.insert(token.to_owned());
        }
    }

    assert!(!cited.is_empty(), "the document cites no codes at all");
    let invented: Vec<&String> = cited
        .iter()
        .filter(|c| !known.contains(c.as_str()))
        .collect();
    assert!(
        invented.is_empty(),
        "the document names codes that do not exist: {invented:?}. See docs/ERRORS.md."
    );
}

/// Every hand-written example is the shape its own schema describes.
///
/// An example is what a client copies. One with a field the server does not read
/// — or missing one it requires — is a wrong answer handed over with confidence.
#[test]
fn every_example_matches_its_schema() {
    let doc = document();
    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("there are schemas");

    let mut checked = 0;
    for (name, schema) in schemas {
        let Some(example) = schema["example"].as_object() else {
            continue;
        };
        checked += 1;

        let declared: BTreeSet<&str> = schema["properties"]
            .as_object()
            .map(|p| p.keys().map(String::as_str).collect())
            .unwrap_or_default();

        for field in example.keys() {
            assert!(
                declared.contains(field.as_str()),
                "{name}'s example sends `{field}`, which is not a field of it"
            );
        }
        for required in schema["required"].as_array().unwrap_or(&Vec::new()) {
            let field = required.as_str().unwrap_or_default();
            assert!(
                example.contains_key(field),
                "{name}'s example leaves out `{field}`, which is required"
            );
        }
    }

    assert!(checked >= 8, "only {checked} examples — did they register?");
}

/// `operationId` is unique, which is what a client generator turns into a
/// function name.
///
/// utoipa derives it from the handler's name, and handler names are only unique
/// per module — three routes were called `list`. A generator given that produces
/// three `list()` functions and drops two, or refuses the document outright.
#[test]
fn no_two_operations_share_an_id() {
    let mut seen: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (path, method, operation) in operations(&document()) {
        let id = operation["operationId"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} {path} has no operationId"))
            .to_owned();
        seen.entry(id).or_default().push(format!("{method} {path}"));
    }

    let collisions: Vec<_> = seen.iter().filter(|(_, at)| at.len() > 1).collect();
    assert!(
        collisions.is_empty(),
        "operationIds are reused: {collisions:?}"
    );
}

/// Every `{placeholder}` in a path is a declared, required parameter.
///
/// There is no `{slug}` any more — the tenant is the subdomain — so what is left
/// is genuinely part of the path: an invoice number, an identity, a token.
///
/// `utoipa-axum` takes the axum route *from* the path string, so the string is
/// always right — but the `params(…)` block beside it is hand-written, and a
/// document with an undeclared placeholder is invalid `OpenAPI`. Generators
/// respond to it by emitting a function that cannot build its own URL.
#[test]
fn every_placeholder_in_a_path_is_declared() {
    let doc = document();
    let mut checked = 0;

    for (path, item) in doc["paths"].as_object().expect("there are paths") {
        let shared = named_path_params(&item["parameters"]);

        for (method, operation) in item.as_object().expect("a path item") {
            if operation["responses"].is_null() {
                continue;
            }
            let mut declared = shared.clone();
            declared.extend(named_path_params(&operation["parameters"]));

            for placeholder in path.split('{').skip(1) {
                let name = placeholder
                    .split_once('}')
                    .unwrap_or_else(|| panic!("{path} has an unclosed placeholder"))
                    .0;
                checked += 1;
                assert!(
                    declared.contains(name),
                    "{method} {path}: {{{name}}} is in the path and not in `params(…)`; \
                     declared: {declared:?}"
                );
            }

            for parameter in operation["parameters"].as_array().unwrap_or(&Vec::new()) {
                if parameter["in"] == "path" {
                    assert_eq!(
                        parameter["required"],
                        Value::Bool(true),
                        "{method} {path}: path parameter {} is not required",
                        parameter["name"]
                    );
                }
                assert!(
                    parameter["schema"].is_object(),
                    "{method} {path}: parameter {} has no schema",
                    parameter["name"]
                );
            }
        }
    }

    assert!(
        checked >= 12,
        "only {checked} placeholders — did they register?"
    );
}

fn named_path_params(parameters: &Value) -> BTreeSet<String> {
    parameters
        .as_array()
        .map(|params| {
            params
                .iter()
                .filter(|p| p["in"] == "path")
                .filter_map(|p| p["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Every role the document names is a role that exists, and it names them all.
///
/// `role` is a `String` on the wire so that an unknown one gets a localized
/// `request.unknown_role` rather than a serde rejection — which means the list a
/// client reads is prose, and prose drifts. It did: the first version of this
/// document offered `manager`, which has never existed, in three descriptions
/// and an example. A client copying that gets a 400.
#[test]
fn every_role_the_document_names_exists() {
    let real: BTreeSet<&str> = erp_control::Role::ALL.iter().map(|r| r.as_str()).collect();

    let doc = document();
    let mut described = 0;
    let mut exampled = 0;

    for (name, schema) in doc["components"]["schemas"]
        .as_object()
        .expect("there are schemas")
    {
        // Anything the document offers as a `role` value.
        if let Some(example) = schema["example"]["role"].as_str() {
            exampled += 1;
            assert!(
                real.contains(example),
                "{name}'s example offers the role `{example}`, which does not exist"
            );
        }

        let Some(description) = schema["properties"]["role"]["description"].as_str() else {
            continue;
        };
        described += 1;

        // Backticked tokens in a `role` field's description are the list a
        // client reads, and `Conventions` generates it from `Role::ALL`. This is
        // the assertion that the mechanism reached every one of them.
        let listed: BTreeSet<&str> = description
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(
            listed, real,
            "{name}.role is described as {listed:?}, and the roles are {real:?}"
        );
    }

    assert!(
        described >= 6 && exampled >= 2,
        "only {described} descriptions and {exampled} examples — did they register?"
    );
}

/// Every module this build offers has routes in the document.
///
/// The fifth composition root. A module can be signed up for, installed,
/// entitled, projected and invariant-checked and still have no way in — and
/// "the module is enabled and nothing happens" is a support call nobody can
/// diagnose from the outside.
///
/// Modules mount under their own name by construction (`Allowed<C>` derives the
/// module from the path), so this is also what keeps that mapping honest.
#[test]
fn every_module_has_routes() {
    let doc = document();
    let paths: Vec<&String> = doc["paths"]
        .as_object()
        .expect("there are paths")
        .keys()
        .collect();

    for (name, setup) in erp_api::modules() {
        let prefix = format!("/v1/{}/", setup.module.as_str());
        assert!(
            paths.iter().any(|path| path.starts_with(&prefix)),
            "the {name} module has no routes; a tenant could enable it and find \
             nothing there"
        );
    }
}
