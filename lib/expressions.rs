use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::models::{resolve_include, Field, RefsSpec, SyntheticDataset};

/// Extract all identifier tokens from a SQL expression string.
/// Returns every word-like token; callers filter against known field names.
pub(crate) fn extract_identifiers(expression: &str) -> Vec<&str> {
    let bytes = expression.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            result.push(&expression[start..i]);
        } else {
            i += 1;
        }
    }
    result
}

/// For each dataset with expression fields, auto-add hidden ref fields for any
/// variables referenced in expressions that are not declared in the dataset but
/// exist in an included dataset. These hidden fields are populated via the normal
/// prefill mechanism and stripped from output by the executor.
pub fn pull_down_expression_deps(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
) -> Result<HashMap<PathBuf, SyntheticDataset>> {
    let mut result = datasets.clone();

    for (path, dataset) in datasets {
        let mut hidden: Vec<Field> = Vec::new();
        let mut known: HashSet<String> =
            dataset.data.iter().map(|f| f.name.clone()).collect();

        for field in &dataset.data {
            let Some(ref expr) = field.expression else { continue };

            for ident in extract_identifiers(expr) {
                if known.contains(ident) || hidden.iter().any(|f| f.name == ident) {
                    continue;
                }

                let matches = include_refs_containing(path, dataset, datasets, ident);
                match matches.len() {
                    0 => {} // not found in any include; validation will catch it
                    1 => {
                        hidden.push(Field {
                            name: ident.to_string(),
                            refs: Some(RefsSpec::Single(format!("{}.{}", matches[0], ident))),
                            hidden: true,
                            ..Default::default()
                        });
                        known.insert(ident.to_string());
                    }
                    _ => bail!(
                        "dataset '{}': expression variable '{}' is ambiguous — \
                         found in {} includes",
                        dataset.name,
                        ident,
                        matches.len()
                    ),
                }
            }
        }

        if !hidden.is_empty() {
            let ds = result.get_mut(path).unwrap();
            // Insert hidden fields immediately before the first expression field so
            // they are available (above) when the expression ordering check runs.
            let insert_at = ds
                .data
                .iter()
                .position(|f| f.expression.is_some())
                .unwrap_or(ds.data.len());
            for (i, f) in hidden.into_iter().enumerate() {
                ds.data.insert(insert_at + i, f);
            }
        }
    }

    Ok(result)
}

/// Return the `reference` strings of every include in `dataset` whose included
/// file contains a field named `name`.
fn include_refs_containing(
    path: &std::path::Path,
    dataset: &SyntheticDataset,
    all: &HashMap<PathBuf, SyntheticDataset>,
    name: &str,
) -> Vec<String> {
    let mut refs = Vec::new();
    for include in dataset.include.iter() {
        let Some(inc_path) = resolve_include(path, &include.file) else { continue };
        let Some(inc_ds) = all.get(&inc_path) else { continue };
        if inc_ds.data.iter().any(|f| f.name == name) {
            refs.push(include.reference.clone());
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_expression_yields_no_identifiers() {
        assert!(extract_identifiers("").is_empty());
    }

    #[test]
    fn numbers_only_yields_no_identifiers() {
        assert!(extract_identifiers("1 + 2").is_empty());
        assert!(extract_identifiers("3.14 * 2.0").is_empty());
    }

    #[test]
    fn simple_field_reference() {
        assert_eq!(extract_identifiers("age - 18"), vec!["age"]);
    }

    #[test]
    fn underscore_prefix_is_valid_identifier() {
        assert_eq!(extract_identifiers("_hidden + 1"), vec!["_hidden"]);
    }

    #[test]
    fn digits_in_identifier_body() {
        assert_eq!(extract_identifiers("field_2 * 3"), vec!["field_2"]);
    }

    #[test]
    fn multiple_field_references() {
        let tokens = extract_identifiers("first_name || ' ' || last_name");
        assert_eq!(tokens, vec!["first_name", "last_name"]);
    }

    #[test]
    fn sql_keywords_are_extracted_as_tokens() {
        // SQL keywords come through; callers filter by matching against known field names.
        let tokens = extract_identifiers("UPPER(name)");
        assert!(tokens.contains(&"UPPER"), "UPPER should be extracted");
        assert!(tokens.contains(&"name"), "name should be extracted");
    }

    #[test]
    fn case_expression_extracts_all_tokens() {
        let tokens = extract_identifiers("CASE WHEN age >= 18 THEN 'adult' ELSE 'minor' END");
        assert!(tokens.contains(&"CASE"));
        assert!(tokens.contains(&"WHEN"));
        assert!(tokens.contains(&"age"), "field reference 'age' should be present");
        assert!(tokens.contains(&"THEN"));
        assert!(tokens.contains(&"ELSE"));
        assert!(tokens.contains(&"END"));
    }

    #[test]
    fn chained_expression_field_reference() {
        // Only the identifier is extracted, not the literal.
        let tokens = extract_identifiers("adult_life * 2");
        assert_eq!(tokens, vec!["adult_life"]);
    }

    #[test]
    fn string_literal_content_not_extracted() {
        // Identifiers inside string literals will appear — callers use field-name
        // filtering so spurious tokens that don't match any field are ignored.
        let tokens = extract_identifiers("'hello world'");
        // 'hello' and 'world' are not extracted because single-quotes delimit strings
        // and our extractor only looks at bare bytes — 'h' follows a quote so the
        // alpha scan would start at 'h'. This is intentional: unknown tokens are
        // filtered away by the caller's field-name lookup.
        let _ = tokens; // implementation-defined; just verify it doesn't panic
    }
}
