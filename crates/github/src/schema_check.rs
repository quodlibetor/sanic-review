//! Checks every GraphQL document sent against GitHub's published schema,
//! offline, so a field or argument GitHub doesn't have fails a test rather
//! than every poll. The schema is vendored in `schema/`; its README says
//! how to refresh it.

use std::path::Path;

use apollo_compiler::{ExecutableDocument, Schema, validation::Valid};

use crate::graphql::QUERIES;

fn schema() -> Valid<Schema> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/github.graphql");
    let sdl = std::fs::read_to_string(&path).unwrap();
    // GitHub's SDL has quirks a strict schema validation objects to; the
    // queries are what's under test.
    let schema = Schema::parse(sdl, &path).unwrap_or_else(|with_errors| with_errors.partial);
    Valid::assume_valid(schema)
}

#[test]
fn every_query_matches_githubs_schema() {
    let schema = schema();
    for (name, query) in QUERIES {
        if let Err(errors) = ExecutableDocument::parse_and_validate(&schema, *query, *name) {
            panic!("{name} doesn't match GitHub's schema:\n{}", errors.errors);
        }
    }
}

/// Queries are string constants named `*_QUERY` or `*_MUTATION`, and each
/// is listed in `QUERIES`; no document is written inline at a call site.
#[test]
fn every_query_is_listed() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let line = line.trim();
            let line = line.strip_prefix("pub(crate) ").unwrap_or(line);
            let line = line.strip_prefix("pub ").unwrap_or(line);
            if let Some(rest) = line.strip_prefix("const ")
                && let Some((name, _)) = rest.split_once(':')
                && (name.ends_with("_QUERY") || name.ends_with("_MUTATION"))
            {
                assert!(
                    QUERIES.iter().any(|(listed, _)| *listed == name),
                    "{name} in {} isn't in `QUERIES`",
                    path.display()
                );
            }
        }
        // This file names the calls in string literals itself.
        if path.ends_with("schema_check.rs") {
            continue;
        }
        // rustfmt puts the query argument on its own line once a call
        // wraps, so look past the newline.
        for call in [".graphql(", ".graphql_raw("] {
            for (at, _) in text.match_indices(call) {
                let arg = text[at + call.len()..].trim_start();
                assert!(
                    !arg.starts_with('"') && !arg.starts_with("r\"") && !arg.starts_with("r#"),
                    "a GraphQL document written inline in {}: make it a `*_QUERY` const",
                    path.display()
                );
            }
        }
    }
}
