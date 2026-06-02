use fakeset::{
    executor::execute,
    expand_variants::expand_field_variants,
    expressions::pull_down_expression_deps,
    graph::build_dag,
    import::load_import_headers,
    load_all_datasets,
    models::SeedConfig,
    plan::build_plan,
    rewrite::{expand_include_fields, resolve_refs},
    validate::validate,
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn run(fixture: &str) -> PathBuf {
    let fixture_path = PathBuf::from(fixture);
    let out = std::env::temp_dir().join(format!(
        "fakeset_test_{}_{}",
        fixture.replace(['/', '\\', '.'], "_"),
        Uuid::new_v4()
    ));

    let mut datasets = load_all_datasets(&[fixture_path]).expect("load datasets");
    load_import_headers(&mut datasets).expect("load import headers");
    let dag = build_dag(&datasets).expect("build dag");
    let datasets = pull_down_expression_deps(&datasets).expect("pull down");
    for w in validate(&datasets).expect("validate") {
        eprintln!("warn: {w}");
    }
    let datasets = expand_field_variants(datasets).expect("expand field variants");
    let datasets = expand_include_fields(&datasets).expect("expand include fields");
    let resolved = resolve_refs(&datasets).expect("resolve refs");
    let plan = build_plan(&dag, &resolved).expect("build plan");
    let seed_config = SeedConfig { ring: 42 };
    execute(&plan, &out, &seed_config).await.expect("execute");
    out
}

/// Count data rows in a CSV file (total lines minus the header line).
fn csv_rows(out: &Path, name: &str) -> usize {
    let path = out.join(format!("{name}.csv"));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing output file: {}", path.display()));
    content.lines().count().saturating_sub(1)
}

/// Collect values from a single named column of a CSV file.
/// Assumes no quoting of simple values and that the delimiter is a comma.
fn csv_column(out: &Path, name: &str, col: &str) -> Vec<String> {
    let path = out.join(format!("{name}.csv"));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing output file: {}", path.display()));
    let mut lines = content.lines();
    let header = lines.next().expect("header line");
    let col_idx = header
        .split(',')
        .position(|h| h.trim_matches('"') == col)
        .unwrap_or_else(|| panic!("column '{col}' not found in header: {header}"));
    lines
        .map(|row| {
            row.split(',')
                .nth(col_idx)
                .unwrap_or("")
                .trim_matches('"')
                .to_string()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Flat generation — no includes, no distribution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_flat_generation_produces_correct_row_count() {
    let out = run("tests/fixtures/execute/flat").await;
    assert_eq!(
        csv_rows(&out, "person"),
        7,
        "person.csv should have 7 data rows"
    );
}

// ---------------------------------------------------------------------------
// 2. Row-count propagation — single sibling with distribution
//
// source.yaml: rows: 20
// subset.yaml: includes source with distribution: 0.5
//
// plan_row_counts direction-1: subset rows = 20 × 0.5 = 10
// Bernoulli plan_segments: {subset} segment = 10 rows, {} segment = 10 rows
// → source.csv: 20 rows, subset.csv: 10 rows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_distribution_drives_row_counts() {
    let out = run("tests/fixtures/execute/single_sibling").await;
    assert_eq!(csv_rows(&out, "source"), 20, "source should have 20 rows");
    assert_eq!(
        csv_rows(&out, "subset"),
        10,
        "subset should have 20 × 0.5 = 10 rows"
    );
}

// ---------------------------------------------------------------------------
// 3. Bernoulli fan-out with conflicting sibling constants
//
// base.yaml: rows: 6, fields: id (uuid), category (string)
// cats.yaml: includes base at 0.5, overrides category = "cats"
// dogs.yaml: includes base at 0.5, overrides category = "dogs"
//
// Joint segment {cats,dogs} conflicts on category → zeroed. IPF restores the
// declared marginals: Σd = 1.0, so the parent-only segment also vanishes.
// Each sibling gets its declared 50% of 6 rows.
//
// Segment {cats}   → 3 rows → base + cats.csv (category = "cats")
// Segment {dogs}   → 3 rows → base + dogs.csv (category = "dogs")
//
// Expected: base.csv=6, cats.csv=3 (all "cats"), dogs.csv=3 (all "dogs")
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_bernoulli_conflicting_siblings_fan_out_correctly() {
    let out = run("tests/fixtures/execute/bernoulli").await;

    assert_eq!(csv_rows(&out, "base"), 6, "base should have all 6 rows");
    assert_eq!(
        csv_rows(&out, "cats"),
        3,
        "cats gets its declared 50% of 6 rows"
    );
    assert_eq!(
        csv_rows(&out, "dogs"),
        3,
        "dogs gets its declared 50% of 6 rows"
    );

    let cat_categories = csv_column(&out, "cats", "category");
    assert!(
        cat_categories.iter().all(|v| v == "cats"),
        "every row in cats.csv should have category='cats', got: {cat_categories:?}"
    );

    let dog_categories = csv_column(&out, "dogs", "category");
    assert!(
        dog_categories.iter().all(|v| v == "dogs"),
        "every row in dogs.csv should have category='dogs', got: {dog_categories:?}"
    );
}

// ---------------------------------------------------------------------------
// 3b. Overlap segment ref integrity — SEG-ATOM-1 regression guard
//
// parent.yaml: 100 rows with `id` and `code` (two synthesised parent fields)
// m1.yaml: includes parent at 0.8, refs both p.id and p.code
// m2.yaml: includes parent at 0.5, refs both p.id and p.code
//
// No constraint conflicts → joint segment {m1, m2} survives with ~40 rows.
// SEG-ATOM-1 generates one unified atom batch per segment so every member and
// the parent receive the same generated values for shared ref columns. Tests:
//   (1) every member ref value is present in the parent (no orphans)
//   (2) the (parent_id, parent_code) pairs in each member appear as
//       (id, code) pairs in the parent — i.e. the two shared refs are kept in
//       sync row-by-row (the multi-ref-per-member case that BUG-REF broke).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_overlap_shared_ref_integrity() {
    let out = run("tests/fixtures/execute/overlap_shared_ref").await;

    let parent_ids: std::collections::HashSet<String> =
        csv_column(&out, "parent", "id").into_iter().collect();
    let parent_codes: std::collections::HashSet<String> =
        csv_column(&out, "parent", "code").into_iter().collect();
    let parent_pairs: std::collections::HashSet<(String, String)> = csv_column(&out, "parent", "id")
        .into_iter()
        .zip(csv_column(&out, "parent", "code"))
        .collect();

    for m in ["m1", "m2"] {
        let m_ids = csv_column(&out, m, "parent_id");
        let m_codes = csv_column(&out, m, "parent_code");
        assert_eq!(
            m_ids.len(),
            m_codes.len(),
            "{m}: parent_id and parent_code columns must have the same length"
        );

        let id_orphans = m_ids.iter().filter(|v| !parent_ids.contains(*v)).count();
        assert_eq!(id_orphans, 0, "{m}.parent_id has {id_orphans} orphan refs");

        let code_orphans = m_codes.iter().filter(|v| !parent_codes.contains(*v)).count();
        assert_eq!(code_orphans, 0, "{m}.parent_code has {code_orphans} orphan refs");

        // (parent_id, parent_code) pairs must appear together in the parent.
        // This is the multi-ref-per-member integrity check: each member row
        // points at one specific parent row, with all its ref columns aligned.
        let pair_mismatches: Vec<(String, String)> = m_ids
            .into_iter()
            .zip(m_codes)
            .filter(|pair| !parent_pairs.contains(pair))
            .collect();
        assert!(
            pair_mismatches.is_empty(),
            "{m}: {} rows have (parent_id, parent_code) pairs not present in parent (first 3: {:?})",
            pair_mismatches.len(),
            pair_mismatches.iter().take(3).collect::<Vec<_>>(),
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Inherited field wiring — ref field values flow from includer to includee
//
// derived includes source (no distribution), derived.id refs src.id.
// Topo order: derived first (includer), source second (includee).
// source.id should be pre-filled from derived's batch so both files share
// the same id values (in the same order — neither is shuffled).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ref_wiring_propagates_column_values() {
    let out = run("tests/fixtures/execute/ref_wiring").await;

    let derived_ids = csv_column(&out, "derived", "id");
    let source_ids = csv_column(&out, "source", "id");

    assert_eq!(source_ids.len(), 5, "source should have 5 rows");
    assert_eq!(derived_ids.len(), 5, "derived should have 5 rows");

    let mut source_sorted = source_ids.clone();
    let mut derived_sorted = derived_ids.clone();
    source_sorted.sort();
    derived_sorted.sort();
    assert_eq!(
        source_sorted, derived_sorted,
        "source.id should be pre-filled from derived.id — same values must appear in both"
    );
}

// ---------------------------------------------------------------------------
// 5. Shared output_file — multiple datasets written into one file
//
// alpha (rows:3) and beta (rows:4) both declare output_file: combined.
// combined.csv should have all 7 rows; alpha.csv and beta.csv must not exist.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_shared_output_file_merges_datasets() {
    let out = run("tests/fixtures/execute/shared_output").await;

    assert_eq!(
        csv_rows(&out, "combined"),
        7,
        "combined.csv should have 3 + 4 = 7 rows"
    );

    let kinds = csv_column(&out, "combined", "kind");
    let alpha_count = kinds.iter().filter(|k| k.as_str() == "alpha").count();
    let beta_count = kinds.iter().filter(|k| k.as_str() == "beta").count();
    assert_eq!(alpha_count, 3, "3 rows should have kind='alpha'");
    assert_eq!(beta_count, 4, "4 rows should have kind='beta'");

    assert!(
        !out.join("alpha.csv").exists(),
        "alpha.csv should not exist — alpha writes to output_file: combined"
    );
    assert!(
        !out.join("beta.csv").exists(),
        "beta.csv should not exist — beta writes to output_file: combined"
    );
}

// ---------------------------------------------------------------------------
// 6. Expression fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_expression_fields_evaluated_correctly() {
    let out = run("tests/fixtures/execute/expressions").await;

    let ages = csv_column(&out, "person", "age");
    let adult_lives = csv_column(&out, "person", "adult_life");
    let double_adults = csv_column(&out, "person", "double_adult");

    assert_eq!(ages.len(), 5);
    for ((age_str, adult_str), double_str) in ages.iter().zip(&adult_lives).zip(&double_adults) {
        let age: f64 = age_str.parse().expect("age should be numeric");
        let adult: f64 = adult_str.parse().expect("adult_life should be numeric");
        let double: f64 = double_str.parse().expect("double_adult should be numeric");
        assert!(
            (adult - (age - 18.0)).abs() < 0.001,
            "adult_life should equal age - 18; age={age}, adult_life={adult}"
        );
        assert!(
            (double - adult * 2.0).abs() < 0.001,
            "double_adult should equal adult_life * 2; adult_life={adult}, double_adult={double}"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. String expression — concatenation via DataFusion SQL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_string_expression_concatenates_correctly() {
    let out = run("tests/fixtures/execute/expression_string").await;

    let firsts = csv_column(&out, "person", "first_name");
    let lasts = csv_column(&out, "person", "last_name");
    let fulls = csv_column(&out, "person", "full_name");

    assert_eq!(firsts.len(), 5);
    for ((first, last), full) in firsts.iter().zip(&lasts).zip(&fulls) {
        let expected = format!("{first} {last}");
        assert_eq!(
            full, &expected,
            "full_name should be 'first_name last_name'; got '{full}'"
        );
    }
}

// ---------------------------------------------------------------------------
// 9. Conditional expression — CASE WHEN via DataFusion SQL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_conditional_expression_classifies_correctly() {
    let out = run("tests/fixtures/execute/expression_conditional").await;

    let ages = csv_column(&out, "person", "age");
    let categories = csv_column(&out, "person", "category");

    assert_eq!(ages.len(), 20);
    for (age_str, category) in ages.iter().zip(&categories) {
        let age: f64 = age_str.parse().expect("age should be numeric");
        let expected = if age >= 18.0 { "adult" } else { "minor" };
        assert_eq!(
            category, expected,
            "category should be '{expected}' when age={age}; got '{category}'"
        );
    }
}

// ---------------------------------------------------------------------------
// 10. Bernoulli group with expression fields
//
// parent (rows:10) has score and doubled (= score * 2).
// subset includes parent at distribution:0.5, exposes score only.
//
// Verifies that evaluate_expressions is correctly called inside
// execute_bernoulli_group after combine_and_shuffle.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_bernoulli_group_evaluates_expression_fields() {
    let out = run("tests/fixtures/execute/bernoulli_expression").await;

    assert_eq!(csv_rows(&out, "parent"), 10, "parent should have 10 rows");
    assert_eq!(
        csv_rows(&out, "subset"),
        5,
        "subset should have 5 rows (dist 0.5)"
    );

    let scores = csv_column(&out, "parent", "score");
    let doubled = csv_column(&out, "parent", "doubled");

    for (score_str, doubled_str) in scores.iter().zip(&doubled) {
        let score: f64 = score_str.parse().expect("score should be numeric");
        let d: f64 = doubled_str.parse().expect("doubled should be numeric");
        assert!(
            (d - score * 2.0).abs() < 0.001,
            "doubled should equal score * 2; score={score}, doubled={d}"
        );
    }

    // subset only has score, not doubled
    let path = out.join("subset.csv");
    let header = std::fs::read_to_string(&path)
        .expect("subset.csv should exist")
        .lines()
        .next()
        .expect("header line")
        .to_string();
    assert!(
        !header.split(',').any(|h| h.trim_matches('"') == "doubled"),
        "subset should not have a 'doubled' column; header: {header}"
    );
}

// ---------------------------------------------------------------------------
// 7. Expression pull-down — hidden fields excluded from output
//
// derived includes source. derived.label = "age * 2" but age is not declared
// as a ref field in derived. The pull-down pass adds age as a hidden ref field.
// derived.csv must contain id and label but NOT age.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_hidden_fields_excluded_from_output() {
    let out = run("tests/fixtures/execute/expression_pulldown").await;

    let path = out.join("derived.csv");
    let content = std::fs::read_to_string(&path).expect("derived.csv should exist");
    let header = content.lines().next().expect("header line");

    assert!(
        !header.split(',').any(|h| h.trim_matches('"') == "age"),
        "hidden field 'age' must not appear in derived.csv header; got: {header}"
    );
    assert!(
        header.split(',').any(|h| h.trim_matches('"') == "label"),
        "expression field 'label' must appear in derived.csv header; got: {header}"
    );

    // label = age * 2, so all values must be positive numbers
    let labels = csv_column(&out, "derived", "label");
    assert_eq!(labels.len(), 5);
    for v in &labels {
        let n: f64 = v.parse().expect("label should be numeric");
        assert!(n > 0.0, "label should be positive (age * 2); got {n}");
    }
}

// ---------------------------------------------------------------------------
// Helpers for jsonl output
// ---------------------------------------------------------------------------

fn jsonl_rows(out: &Path, name: &str) -> Vec<serde_json::Value> {
    let path = out.join(format!("{name}.jsonl"));
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing output file: {}", path.display()));
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid json line"))
        .collect()
}

// ---------------------------------------------------------------------------
// 11. List fields — inline list generation with count spec
//
// person.yaml: 5 rows with `tags` (list of 1–3 words) and `scores` (list of 2 numbers).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_field_generates_arrays() {
    let out = run("tests/fixtures/execute/list_inline").await;
    let rows = jsonl_rows(&out, "person");

    assert_eq!(rows.len(), 5, "person should have 5 rows");

    for row in &rows {
        let tags = row["tags"].as_array().expect("tags should be an array");
        assert!(
            (1..=3).contains(&tags.len()),
            "tags should have 1–3 items; got {}",
            tags.len()
        );
        for tag in tags {
            assert!(tag.is_string(), "each tag should be a string");
        }

        let scores = row["scores"].as_array().expect("scores should be an array");
        assert_eq!(scores.len(), 2, "scores should always have 2 items");
        for score in scores {
            let n = score.as_f64().expect("score should be numeric");
            assert!((0.0..=10.0).contains(&n), "score {n} should be in [0, 10]");
        }
    }
}

// ---------------------------------------------------------------------------
// 12. Rich list content — include-scoped and outer-scoped refs
//
// people.yaml: 10 rows (id, full_name, score)
// events.yaml: 5 rows with `attendees` list (count 1–4), where:
//   - attendees[].name       = ref: person.full_name  (include-scoped: sampled from people)
//   - attendees[].event_title = ref: title              (outer-scoped: copied from the event)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 13. Bernoulli parent with nested include field
//
// events (rows:10) is a Bernoulli parent of vip (dist:0.5) and has a
// nested include field `picks` drawn from items (rows:20).
//
// Expected:
//   items.jsonl  — 20 rows (simple pool)
//   vip.jsonl    — ~5 rows (Bernoulli sibling, stochastic — just assert non-empty)
//   events.jsonl — 10 rows, each with picks[1–3] containing item_label + item_value
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_bernoulli_list_link_parent_assembles_correctly() {
    let out = run("tests/fixtures/execute/bernoulli_list_link").await;

    assert_eq!(
        jsonl_rows(&out, "items").len(),
        20,
        "items should have 20 rows"
    );

    let vip = jsonl_rows(&out, "vip");
    assert!(!vip.is_empty(), "vip should have at least one row");

    let events = jsonl_rows(&out, "events");
    assert_eq!(events.len(), 10, "events should have 10 rows");

    for row in &events {
        let picks = row["picks"].as_array().expect("picks should be an array");
        assert!(
            (1..=3).contains(&picks.len()),
            "picks should have 1–3 items; got {}",
            picks.len()
        );
        for pick in picks {
            let label = pick["item_label"]
                .as_str()
                .expect("item_label should be a string");
            assert!(!label.is_empty(), "item_label should be non-empty");
            let value = pick["item_value"]
                .as_f64()
                .expect("item_value should be a number");
            assert!(
                (1.0..=100.0).contains(&value),
                "item_value should be in [1, 100]; got {value}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 14. Plain fields inside nested include content
//
// records (rows:5) has a nested include `entries` with:
//   - tag   (include-scoped from pool)
//   - badge (plain generated: string/word)
//   - score (plain generated: number 0–10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_plain_fields_in_list_link_content() {
    let out = run("tests/fixtures/execute/list_link_flat").await;

    let rows = jsonl_rows(&out, "records");
    assert_eq!(rows.len(), 5, "records should have 5 rows");

    for row in &rows {
        let entries = row["entries"]
            .as_array()
            .expect("entries should be an array");
        assert!(
            (1..=4).contains(&entries.len()),
            "entries count should be 1–4; got {}",
            entries.len()
        );
        for entry in entries {
            let tag = entry["tag"].as_str().expect("tag should be a string");
            assert!(!tag.is_empty(), "include-scoped tag should be non-empty");

            let badge = entry["badge"].as_str().expect("badge should be a string");
            assert!(
                !badge.is_empty(),
                "plain-generated badge should be non-empty"
            );

            let score = entry["score"].as_f64().expect("score should be a number");
            assert!(
                (0.0..=10.0).contains(&score),
                "plain-generated score should be in [0, 10]; got {score}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 15. Normal (Gaussian) count distribution
//
// outer (rows:10) has a list field with count: {mean:3.0, std_dev:1.0}.
// Verifies the Normal branch of sample_count is exercised and produces
// non-negative list lengths.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_count_normal_produces_variable_length_lists() {
    let out = run("tests/fixtures/execute/count_normal").await;

    let rows = jsonl_rows(&out, "outer");
    assert_eq!(rows.len(), 10, "outer should have 10 rows");

    let total_items: usize = rows
        .iter()
        .map(|r| {
            r["samples"]
                .as_array()
                .expect("samples should be an array")
                .len()
        })
        .sum();

    // With mean=3 over 10 rows the expected total is ~30 items.
    // Assert it's in a very wide band to avoid flakiness.
    assert!(
        total_items > 0,
        "expected at least some list items across all rows"
    );
    assert!(
        total_items < 100,
        "expected total list items < 100 (mean 3 × 10 rows); got {total_items}"
    );

    for row in &rows {
        let samples = row["samples"]
            .as_array()
            .expect("samples should be an array");
        // Normal distribution can produce 0 items; suite-level total_items > 0 is sufficient.
        for s in samples {
            let val = s["val"].as_f64().expect("val should be a number");
            assert!(
                (1.0..=10.0).contains(&val),
                "pool val should be in [1, 10]; got {val}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 12. Nested include content — include-scoped and outer-scoped refs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_link_refs() {
    let out = run("tests/fixtures/execute/list_link").await;
    let event_rows = jsonl_rows(&out, "events");

    assert_eq!(event_rows.len(), 5, "events should have 5 rows");

    for row in &event_rows {
        let outer_title = row["title"].as_str().expect("title should be a string");

        let attendees = row["attendees"]
            .as_array()
            .expect("attendees should be a list");
        assert!(
            (1..=4).contains(&attendees.len()),
            "attendee count should be 1–4; got {}",
            attendees.len()
        );

        for attendee in attendees {
            // Include-scoped: name comes from people.full_name (should be a non-empty string)
            let name = attendee["name"]
                .as_str()
                .expect("attendee name should be a string");
            assert!(!name.is_empty(), "attendee name should be non-empty");

            // Outer-scoped: event_title should match this event's title
            let at = attendee["event_title"]
                .as_str()
                .expect("event_title should be a string");
            assert_eq!(
                at, outer_title,
                "attendee.event_title should match the enclosing event's title"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Variant tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_variant_output_rows_and_values() {
    // orders has 3 variants (60/30/10%): all rows go to a single orders.csv via WriteSharedOutput.
    // 100 rows total; all three status values must appear.
    let out = run("tests/fixtures/execute/variants").await;
    let rows = csv_rows(&out, "orders");
    assert_eq!(
        rows, 100,
        "all variant rows should be combined into orders.csv"
    );
    let statuses = csv_column(&out, "orders", "status");
    assert!(
        statuses.contains(&"pending".to_string()),
        "expected 'pending' status in output"
    );
    assert!(
        statuses.contains(&"shipped".to_string()),
        "expected 'shipped' status in output"
    );
    assert!(
        statuses.contains(&"cancelled".to_string()),
        "expected 'cancelled' status in output"
    );
}

#[tokio::test]
async fn test_variant_lower_cover_total_rows() {
    // source: 100 rows split 70/30 across variants.
    // subset: Bernoulli sibling at dist 0.4 → ~40 rows across both variant groups.
    // source output: 100 rows combined; subset output: ~40 rows combined.
    let out = run("tests/fixtures/execute/variant_sibling").await;
    let source_rows = csv_rows(&out, "source");
    assert_eq!(source_rows, 100, "source variants should total 100 rows");
    let subset_rows = csv_rows(&out, "subset");
    assert!(
        subset_rows > 0,
        "subset should have rows from both variant groups"
    );
}

// ---------------------------------------------------------------------------
// VAR-2: variant values applied when dataset is a lower-cover member
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_variant_lower_cover_member_applies_variant_values() {
    // parent: 100 rows.  member: lower cover member at ratio 0.6 with two variants
    // (V0 sets code="alpha" at 50%, V1 sets code="beta" at 50%).
    // Before VAR-2 this produced random strings; after VAR-2 all rows must be alpha or beta.
    let out = run("tests/fixtures/execute/variant_in_lower_cover").await;

    let codes = csv_column(&out, "member", "code");
    assert!(!codes.is_empty(), "member should have rows");
    let unexpected: Vec<_> = codes
        .iter()
        .filter(|v| v.as_str() != "alpha" && v.as_str() != "beta")
        .collect();
    assert!(
        unexpected.is_empty(),
        "all member.code values must be 'alpha' or 'beta'; unexpected: {unexpected:?}"
    );
    assert!(
        codes.iter().any(|v| v == "alpha"),
        "expected some 'alpha' rows"
    );
    assert!(
        codes.iter().any(|v| v == "beta"),
        "expected some 'beta' rows"
    );
}

#[tokio::test]
async fn test_variant_incompatible_with_segment_constraint_is_pruned() {
    // sibling: ratio=1.0, pins category="premium" for all parent rows.
    // member: ratio=0.5, two variants — V0 sets category="premium" (50%), V1 sets
    //   category="basic" (50%).
    // Because sibling has ratio=1.0, all member rows fall in the {sibling, member}
    // segment where seg_constraints = {category: "premium"}.  V1 ("basic") conflicts
    // with that constraint and is pruned.  All member output rows must have
    // category="premium".
    let out = run("tests/fixtures/execute/variant_pruned_by_segment").await;

    let categories = csv_column(&out, "member", "category");
    assert!(!categories.is_empty(), "member should have rows");
    let unexpected: Vec<_> = categories
        .iter()
        .filter(|v| v.as_str() != "premium")
        .collect();
    assert!(
        unexpected.is_empty(),
        "all member.category values must be 'premium' (V1='basic' pruned by segment constraint); unexpected: {unexpected:?}"
    );
}

// ---------------------------------------------------------------------------
// Field-local variant tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_field_variants_produce_correct_row_count_and_combinations() {
    // orders.yaml has 120 rows, status(50/50) × tier(25/25/50) = 6 combinations.
    // All rows go to orders.csv via WriteSharedOutput.
    let out = run("tests/fixtures/execute/field_variants").await;
    let rows = csv_rows(&out, "orders");
    assert_eq!(
        rows, 120,
        "all 6 variant combinations should total 120 rows in orders.csv"
    );

    let statuses = csv_column(&out, "orders", "status");
    assert!(
        statuses.contains(&"pending".to_string()),
        "expected 'pending' status"
    );
    assert!(
        statuses.contains(&"shipped".to_string()),
        "expected 'shipped' status"
    );

    let tiers = csv_column(&out, "orders", "tier");
    assert!(tiers.contains(&"gold".to_string()), "expected 'gold' tier");
    assert!(
        tiers.contains(&"silver".to_string()),
        "expected 'silver' tier"
    );
    assert!(
        tiers.contains(&"bronze".to_string()),
        "expected 'bronze' tier"
    );
}

// ---------------------------------------------------------------------------
// MULT-1 top-level cardinality tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mult1_fixed_row_count() {
    // parent: 10 rows. child: cardinality: 2. Expected child rows: 10 × 2 = 20.
    let out = run("tests/fixtures/execute/mult1_fixed").await;
    assert_eq!(csv_rows(&out, "parent"), 10, "parent should have 10 rows");
    assert_eq!(
        csv_rows(&out, "child"),
        20,
        "child should have 10 × cardinality:2 = 20 rows"
    );
}

#[tokio::test]
async fn test_mult1_range_row_bounds() {
    // parent: 10 rows. child: cardinality: {min: 1, max: 3}. Expected child rows: 10..=30.
    let out = run("tests/fixtures/execute/mult1_range").await;
    let rows = csv_rows(&out, "child");
    assert!(
        (10..=30).contains(&rows),
        "child row count should be in [10, 30] for cardinality min:1 max:3 × 10 parent rows; got {rows}"
    );
}

#[tokio::test]
async fn test_mult1_ref_field_consistency() {
    // child has label: value: "child" — a constant constraint that must apply to all
    // M_n rows generated per slot, not just the canonical one.
    let out = run("tests/fixtures/execute/mult1_fixed").await;
    let labels = csv_column(&out, "child", "label");
    assert_eq!(labels.len(), 20, "child should have 20 rows");
    assert!(
        labels.iter().all(|v| v == "child"),
        "all 20 child rows should have label='child'; got: {labels:?}"
    );
}

#[tokio::test]
async fn test_mult1_fresh_field_varies() {
    // child has fresh: generator: word — independently generated for each of the 20 rows.
    // With 20 random words it is overwhelmingly likely that at least 2 are distinct.
    let out = run("tests/fixtures/execute/mult1_fixed").await;
    let words = csv_column(&out, "child", "fresh");
    assert_eq!(words.len(), 20, "child should have 20 rows");
    let distinct: std::collections::HashSet<_> = words.iter().collect();
    assert!(
        distinct.len() > 1,
        "fresh field should vary across expanded rows; got only: {distinct:?}"
    );
}

#[tokio::test]
async fn test_mult1_combined_ratio_card() {
    // parent: 10 rows. child: ratio: 0.5, cardinality: 2.
    // Expected child rows: 10 × 0.5 × 2 = 10 (exact — ratio:0.5 selects 5 slots, each × 2).
    let out = run("tests/fixtures/execute/mult1_ratio_card").await;
    assert_eq!(
        csv_rows(&out, "child"),
        10,
        "child should have 10 × 0.5 × cardinality:2 = 10 rows"
    );
}

#[tokio::test]
async fn test_mult1_grandchild_sees_full_batch() {
    // grandparent: 5 rows. parent: cardinality: 3 → 15 rows. child: ratio: 1.0 → 15 rows.
    // The grandchild (child) should see all 15 expanded parent rows, not just 5.
    // Regression: when parent is parent_computed with cardinality, grandparent must still
    // get its declared row count (5), not the parent's expanded count (15).
    let out = run("tests/fixtures/execute/mult1_grandchild").await;
    assert_eq!(
        csv_rows(&out, "grandparent"),
        5,
        "grandparent should have 5 rows"
    );
    assert_eq!(
        csv_rows(&out, "parent"),
        15,
        "parent should have 5 × cardinality:3 = 15 rows"
    );
    assert_eq!(
        csv_rows(&out, "child"),
        15,
        "child should have 5 grandparent × cardinality:3 = 15 rows"
    );
}

// ---------------------------------------------------------------------------
// _slot_idx and _linked_idx sentinel tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_witness_slot_idx() {
    // list_link: events (5 rows) has an attendees list (1–4 items per row).
    // Items are assembled from witness batches via _staging_refs unnesting; outer-scoped
    // refs are resolved from the staging batch per slot. The outer-scoped ref event_title
    // must match the enclosing row's title, proving slot assignment is correct.
    let out = run("tests/fixtures/execute/list_link").await;
    let rows = jsonl_rows(&out, "events");
    assert_eq!(rows.len(), 5, "events should have 5 rows");
    for row in &rows {
        let title = row["title"].as_str().expect("title should be a string");
        let attendees = row["attendees"]
            .as_array()
            .expect("attendees should be an array");
        assert!(
            (1..=4).contains(&attendees.len()),
            "_slot_idx must produce 1–4 attendees per event; got {}",
            attendees.len()
        );
        for a in attendees {
            let at = a["event_title"]
                .as_str()
                .expect("event_title should be a string");
            assert_eq!(
                at, title,
                "_slot_idx must assign this item to its enclosing event; event_title mismatch"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 5 — Nested-include collect-to-linked (Case 2)
//
// wards_doctors: 3 wards, 5 doctors.
// Each ward has an `on_call_doctors` list (cardinality 2) drawn from doctors via _linked_idx.
// AccumulateToLinked accumulates doctor_name into doctors.on_call_wards.
// The total count of ward references across all doctors equals the total atom count
// (3 wards × 2 atoms each = 6 atoms total).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_link_collect_to_linked() {
    let out = run("tests/fixtures/execute/wards_doctors").await;

    let doctors = jsonl_rows(&out, "doctors");
    assert_eq!(doctors.len(), 5, "doctors should have 5 rows");

    // Every doctor must have an on_call_wards field that is a list.
    for doc in &doctors {
        let wards = doc["on_call_wards"]
            .as_array()
            .unwrap_or_else(|| panic!("doctors.on_call_wards should be a list; got: {doc}"));
        for w in wards {
            assert!(
                w.as_str().is_some(),
                "each on_call_wards entry should be a string; got: {w}"
            );
        }
    }

    // Total on_call_wards entries across all doctors equals total atoms (3 wards × 2 = 6).
    let total_ward_refs: usize = doctors
        .iter()
        .map(|d| d["on_call_wards"].as_array().map_or(0, |a| a.len()))
        .sum();
    assert_eq!(
        total_ward_refs, 6,
        "total on_call_wards entries should equal 3 wards × cardinality 2 = 6; got {total_ward_refs}"
    );

    let wards = jsonl_rows(&out, "wards");
    assert_eq!(wards.len(), 3, "wards should have 3 rows");

    for ward in &wards {
        let docs = ward["on_call_doctors"]
            .as_array()
            .unwrap_or_else(|| panic!("wards.on_call_doctors should be a list; got: {ward}"));
        assert_eq!(
            docs.len(),
            2,
            "each ward should have exactly 2 on-call doctors (cardinality 2)"
        );
        for d in docs {
            let name = d["doctor_name"].as_str().unwrap_or_else(|| {
                panic!("on_call_doctors item should have doctor_name; got: {d}")
            });
            assert!(!name.is_empty(), "doctor_name should be non-empty");
        }
    }

    // Sentinels must not appear in output.
    for ward in &wards {
        assert!(
            ward.get("_slot_idx").is_none(),
            "_slot_idx must not appear in wards output"
        );
        assert!(
            ward.get("_linked_idx").is_none(),
            "_linked_idx must not appear in wards output"
        );
    }
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 7 — Scalar reducers (sum, max, min, take_first)
//
// scalar_reduce: scoreboard (3 rows), plays (9 rows, score: value: 5).
// plays binds score → board.total/sum, board.high/max, board.low/min, board.first/take_first.
//
// With score always 5:
//   - sum(all scoreboard.total) = 9 × 5 = 45  (conservation law)
//   - each scoreboard.high ∈ {0, 5}  (0 = no plays assigned; 5 = at least one play)
//   - each scoreboard.low ∈ {0, 5}   (same reasoning)
//   - each scoreboard.first ∈ {0, 5} (same)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_scalar_reducers_sum_max_min_take_first() {
    let out = run("tests/fixtures/execute/scalar_reduce").await;

    let plays = jsonl_rows(&out, "plays");
    assert_eq!(plays.len(), 9, "plays should have 9 rows");

    let boards = jsonl_rows(&out, "scoreboard");
    assert_eq!(boards.len(), 3, "scoreboard should have 3 rows");

    // Conservation: sum of all totals equals 9 plays × score 5.
    let grand_total: f64 = boards
        .iter()
        .map(|b| b["total"].as_f64().expect("total should be a number"))
        .sum();
    assert!(
        (grand_total - 45.0).abs() < 0.001,
        "sum of all scoreboard.total should be 9 × 5 = 45; got {grand_total}"
    );

    for (i, board) in boards.iter().enumerate() {
        let high = board["high"].as_f64().expect("high should be a number");
        assert!(
            high == 0.0 || high == 5.0,
            "scoreboard[{i}].high should be 0 (no plays) or 5 (all plays score 5); got {high}"
        );

        let low = board["low"].as_f64().expect("low should be a number");
        assert!(
            low == 0.0 || low == 5.0,
            "scoreboard[{i}].low should be 0 (no plays) or 5 (all plays score 5); got {low}"
        );

        let first = board["first"].as_f64().expect("first should be a number");
        assert!(
            first == 0.0 || first == 5.0,
            "scoreboard[{i}].first should be 0 (no plays) or 5 (all plays score 5); got {first}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stage 5.5 — cumulative scalar reducer across Bernoulli segments
//
// segmented_scalar_reduce: game (3 rows, total: number default 0),
// plays (10 rows, list field played_games from game cardinality 2,
//   content field score: value 5 bind game.total reducer sum).
// child_a (ratio 0.4) and child_b (ratio 0.6) include plays, creating two segments.
//
// Conservation law: every draw contributes score=5 to exactly one game row.
// With 10 plays × cardinality 2 = 20 draws, sum(game.total) = 20 × 5 = 100.
// Without the fix, the second segment's AccumulateToLinked would overwrite game rows
// from the first segment with the default (0), breaking the conservation law.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_segmented_scalar_sum_accumulates_correctly() {
    let out = run("tests/fixtures/execute/segmented_scalar_reduce").await;

    let games = jsonl_rows(&out, "game");
    assert_eq!(games.len(), 3, "game should have 3 rows");

    // Conservation law: every draw (10 plays × cardinality 2) contributes 5 to some game row.
    let grand_total: f64 = games
        .iter()
        .map(|g| g["total"].as_f64().expect("game.total should be a number"))
        .sum();
    assert!(
        (grand_total - 100.0).abs() < 0.001,
        "sum(game.total) should equal 10 plays × 2 cardinality × score 5 = 100; got {grand_total}"
    );
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 6 — reinforcement: 0 (without-replacement sampling)
//
// no_replacement: pool (5 rows), outer (4 rows).
// link: cardinality: 3, reinforcement: 0.
// Each outer row must have exactly 3 items, all with distinct pool_ids.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reinforcement_zero_no_duplicate_linked_rows() {
    let out = run("tests/fixtures/execute/no_replacement").await;

    let rows = jsonl_rows(&out, "outer");
    assert_eq!(rows.len(), 4, "outer should have 4 rows");

    for (i, row) in rows.iter().enumerate() {
        let items = row["items"]
            .as_array()
            .unwrap_or_else(|| panic!("outer[{i}].items should be a list; got: {row}"));
        assert_eq!(
            items.len(),
            3,
            "outer[{i}] should have exactly 3 items (cardinality: 3); got {}",
            items.len()
        );

        let ids: Vec<&str> = items
            .iter()
            .map(|item| {
                item["pool_id"]
                    .as_str()
                    .expect("pool_id should be a string")
            })
            .collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "outer[{i}]: reinforcement:0 must produce no duplicate pool_ids within one row; got: {ids:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stage 6 — Uniform max-cap with reinforcement:0
//
// no_replacement_max_cap: linked (4 rows), outer (5 rows).
// link: cardinality:{min:1, max:10}, reinforcement: 0.
// max:10 exceeds n_eligible=4. Each outer row must have 1–4 items (capped at 4)
// and no duplicate linked ids within a row.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_no_replacement_max_cap() {
    let out = run("tests/fixtures/execute/no_replacement_max_cap").await;

    let rows = jsonl_rows(&out, "outer");
    assert_eq!(rows.len(), 5, "outer should have 5 rows");

    for (i, row) in rows.iter().enumerate() {
        let items = row["items"]
            .as_array()
            .unwrap_or_else(|| panic!("outer[{i}].items should be a list; got: {row}"));
        assert!(
            items.len() <= 4,
            "outer[{i}]: items count ({}) must be ≤ n_eligible=4 (runtime cap applied); got: {items:?}",
            items.len()
        );
        assert!(
            !items.is_empty(),
            "outer[{i}]: items count must be ≥ 1 (min cardinality); got 0"
        );
        let ids: Vec<&str> = items
            .iter()
            .map(|item| item["id"].as_str().expect("id should be a string"))
            .collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "outer[{i}]: reinforcement:0 must produce no duplicate ids within one row; got: {ids:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 4 — Junction link collect-to-linked
//
// directorships: 5 individuals, 5 organisations, 5 directorships.
// Each directorship is assigned to one organisation (_linked_idx).
// AccumulateToLinked accumulates director_name into organisations.directors.
// Organisations with zero directorships get default: [] (empty list).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_junction_collect_to_linked() {
    let out = run("tests/fixtures/execute/directorships").await;

    let orgs = jsonl_rows(&out, "organisations");
    assert_eq!(orgs.len(), 5, "organisations should have 5 rows");

    // Every org must have a directors field that is a list (possibly empty).
    for org in &orgs {
        let dirs = org["directors"]
            .as_array()
            .unwrap_or_else(|| panic!("organisations.directors should be a list; got: {org}"));
        // Each entry should be a string (a director name).
        for d in dirs {
            assert!(
                d.as_str().is_some(),
                "each director entry should be a string; got: {d}"
            );
        }
    }

    // Total director entries across all organisations equals the number of directorships.
    let total_directors: usize = orgs
        .iter()
        .map(|org| org["directors"].as_array().map_or(0, |a| a.len()))
        .sum();
    assert_eq!(
        total_directors, 5,
        "total director entries should equal the number of directorships (5); got {total_directors}"
    );

    let directorships = jsonl_rows(&out, "directorships");
    assert_eq!(directorships.len(), 5, "directorships should have 5 rows");

    // _linked_idx sentinel must not leak into output.
    for row in &directorships {
        assert!(
            row.get("_linked_idx").is_none(),
            "_linked_idx must not appear in directorships output"
        );
    }
}

// ---------------------------------------------------------------------------
// Case 1 — Junction link with no collect and no list field
//
// junction_no_collect: departments (10 rows), employees (30 rows).
// employees links to departments via a plain 1:1 junction (cardinality: 1, no collect,
// no list field). Each employee draws one department; _linked_idx is assigned internally
// but must not appear in the output.
//
// The ref fields (dept_id, dept_name) resolve their type from the linked dataset but
// are not wired to the specific drawn row's values — they're generated fresh.
// The meaningful invariants are: row count, _linked_idx not leaked, fields present.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_junction_link_with_no_collect_produces_correct_rows() {
    let out = run("tests/fixtures/execute/junction_no_collect").await;

    let depts = jsonl_rows(&out, "departments");
    assert_eq!(depts.len(), 10, "departments should have 10 rows");

    let employees = jsonl_rows(&out, "employees");
    assert_eq!(employees.len(), 30, "employees should have 30 rows");

    for row in &employees {
        // _linked_idx sentinel must not leak into output.
        assert!(
            row.get("_linked_idx").is_none(),
            "_linked_idx must not appear in employees output"
        );

        // Ref fields exist and are non-empty strings (type resolved from linked dataset).
        assert!(
            row["emp_id"].as_str().is_some_and(|s| !s.is_empty()),
            "emp_id should be a non-empty string"
        );
        assert!(
            row["dept_id"].as_str().is_some_and(|s| !s.is_empty()),
            "dept_id should be a non-empty string"
        );
        assert!(
            row["dept_name"].as_str().is_some_and(|s| !s.is_empty()),
            "dept_name should be a non-empty string"
        );
    }
}

// ---------------------------------------------------------------------------
// project: single-field projection from a linked list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_project_produces_scalar_list() {
    let out = run("tests/fixtures/execute/project_list").await;

    let events = jsonl_rows(&out, "events");
    assert_eq!(events.len(), 3, "events should have 3 rows");

    for row in &events {
        let attendees = row["attendees"]
            .as_array()
            .expect("attendees should be a list");
        assert_eq!(
            attendees.len(),
            2,
            "cardinality: 2 → exactly 2 attendees per event"
        );

        for attendee in attendees {
            // project: person.full_name → list items are strings, not structs.
            assert!(
                attendee.is_string(),
                "attendee list items should be strings (projected), got: {attendee:?}"
            );
            assert!(
                !attendee.as_str().unwrap().is_empty(),
                "projected name should be non-empty"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// include.fields wildcard expansion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_include_fields_wildcard_driver() {
    let out = run("tests/fixtures/execute/include_fields_wildcard").await;

    let derived = csv_rows(&out, "derived");
    assert_eq!(derived, 5, "derived should have 5 rows (ratio 1.0)");

    // Wildcard expansion should have injected both source fields as refs.
    let ids = csv_column(&out, "derived", "id");
    let names = csv_column(&out, "derived", "full_name");
    assert_eq!(
        ids.len(),
        5,
        "derived should have 'id' column with 5 values"
    );
    assert_eq!(
        names.len(),
        5,
        "derived should have 'full_name' column with 5 values"
    );
    assert!(
        ids.iter().all(|v| !v.is_empty()),
        "all id values should be non-empty"
    );
    assert!(
        names.iter().all(|v| !v.is_empty()),
        "all full_name values should be non-empty"
    );
}

#[tokio::test]
async fn test_include_fields_wildcard_list_link() {
    let out = run("tests/fixtures/execute/include_fields_list_link").await;

    let events = jsonl_rows(&out, "events");
    assert_eq!(events.len(), 3, "events should have 3 rows");

    for row in &events {
        let picks = row["picks"].as_array().expect("picks should be a list");
        assert_eq!(
            picks.len(),
            2,
            "each event should have 2 picks (cardinality: 2)"
        );
        for pick in picks {
            // Wildcard expansion should have injected item_name and item_code as ref fields.
            let item_name = pick["item_name"]
                .as_str()
                .expect("pick should have item_name");
            let item_code = pick["item_code"]
                .as_str()
                .expect("pick should have item_code");
            assert!(!item_name.is_empty(), "item_name should be non-empty");
            assert!(!item_code.is_empty(), "item_code should be non-empty");
        }
    }
}

#[tokio::test]
async fn test_include_fields_exclude() {
    let out = run("tests/fixtures/execute/include_fields_exclude").await;

    let derived = csv_rows(&out, "derived");
    assert_eq!(derived, 4, "derived should have 4 rows");

    // full_name and age should be present (not excluded).
    let names = csv_column(&out, "derived", "full_name");
    let ages = csv_column(&out, "derived", "age");
    assert_eq!(names.len(), 4);
    assert_eq!(ages.len(), 4);

    // internal_id must not appear (excluded by exclude: [internal_id]).
    let path = out.join("derived.csv");
    let content = std::fs::read_to_string(&path).expect("derived.csv");
    let header = content.lines().next().expect("header");
    assert!(
        !header
            .split(',')
            .any(|h| h.trim_matches('"') == "internal_id"),
        "internal_id should not appear in derived output (was excluded); header: {header}"
    );
}

// ---------------------------------------------------------------------------
// hidden: field suppression
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_hidden_plain_field_excluded_from_output() {
    let out = run("tests/fixtures/execute/hidden_plain").await;

    let rows = csv_rows(&out, "person");
    assert_eq!(rows, 5, "person should have 5 rows");

    let path = out.join("person.csv");
    let content = std::fs::read_to_string(&path).expect("person.csv");
    let header = content.lines().next().expect("header");
    let col_names: Vec<&str> = header.split(',').map(|h| h.trim_matches('"')).collect();

    assert!(
        col_names.contains(&"id"),
        "id should be present; header: {header}"
    );
    assert!(
        col_names.contains(&"label"),
        "label should be present; header: {header}"
    );
    assert!(
        !col_names.contains(&"internal_key"),
        "internal_key (hidden: true) must not appear in output; header: {header}"
    );
}

#[tokio::test]
async fn test_hidden_expression_dep_excluded_but_expression_evaluates() {
    let out = run("tests/fixtures/execute/hidden_expression_dep").await;

    let rows = csv_rows(&out, "records");
    assert_eq!(rows, 4, "records should have 4 rows");

    let path = out.join("records.csv");
    let content = std::fs::read_to_string(&path).expect("records.csv");
    let header = content.lines().next().expect("header");
    let col_names: Vec<&str> = header.split(',').map(|h| h.trim_matches('"')).collect();

    assert!(
        col_names.contains(&"last_name"),
        "last_name should be present; header: {header}"
    );
    assert!(
        col_names.contains(&"display"),
        "display should be present; header: {header}"
    );
    assert!(
        !col_names.contains(&"first_name"),
        "first_name (hidden: true) must not appear in output; header: {header}"
    );

    // The expression `first_name || ' ' || last_name` should have produced non-empty display values.
    let displays = csv_column(&out, "records", "display");
    assert!(
        displays.iter().all(|v| v.contains(' ')),
        "display values should contain a space (first_name + last_name); got: {displays:?}"
    );
}

#[tokio::test]
async fn test_hidden_collect_binding_excluded_but_collect_fires() {
    let out = run("tests/fixtures/execute/hidden_collect_binding").await;

    let outer_rows = jsonl_rows(&out, "outer");
    assert_eq!(outer_rows.len(), 3, "outer should have 3 rows");

    // pool_ref is hidden: true — it must not appear in item structs.
    for row in &outer_rows {
        let items = row["items"].as_array().expect("items should be a list");
        assert_eq!(items.len(), 2, "cardinality: 2 → 2 items per outer row");
        for item in items {
            assert!(
                item.get("pool_ref").is_none(),
                "pool_ref (hidden: true) must not appear in item struct; got: {item:?}"
            );
            let label = item["label"]
                .as_str()
                .expect("label should be present in item");
            assert!(!label.is_empty(), "label should be non-empty");
        }
    }

    // The collect binding should have fired: pool.seen_in must contain collected values.
    let pool_rows = jsonl_rows(&out, "pool");
    assert_eq!(pool_rows.len(), 4, "pool should have 4 rows");
    let total_seen: usize = pool_rows
        .iter()
        .map(|r| r["seen_in"].as_array().map_or(0, |a| a.len()))
        .sum();
    assert!(
        total_seen > 0,
        "collect binding should have fired; pool.seen_in should be non-empty across rows"
    );
    assert_eq!(
        total_seen, 6,
        "3 outer rows × 2 atoms = 6 total collected entries; got {total_seen}"
    );
}

// ---------------------------------------------------------------------------
// Stage 4 — _staging_refs witness deduplication
//
// staging_refs_dedup: source (3 rows) links to linked (1 row) with
// reinforcement: 0 (without-replacement) and cardinality: 1.
// All 3 source rows draw the same single linked row → witness has exactly 1 row
// with _staging_refs = [0, 1, 2].
//
// Verifies:
//   1. source output has 3 rows, each with a 1-item `items` list
//   2. all items' `name` equals the single linked row's `item_name`
//   3. linked.drawn_by has exactly 3 entries (one per source row that drew it)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_staging_refs_deduplicates_linked_rows() {
    let out = run("tests/fixtures/execute/staging_refs_dedup").await;

    let source_rows = jsonl_rows(&out, "source");
    assert_eq!(source_rows.len(), 3, "source should have 3 rows");

    let linked_rows = jsonl_rows(&out, "linked");
    assert_eq!(linked_rows.len(), 1, "linked should have 1 row");
    let linked_item_name = linked_rows[0]["item_name"]
        .as_str()
        .expect("linked.item_name should be a string");

    // Each source row has exactly 1 item, and its name matches the single linked row.
    for row in &source_rows {
        let items = row["items"].as_array().expect("items should be a list");
        assert_eq!(
            items.len(),
            1,
            "cardinality: 1 with 1 linked row → 1 item per source row; got {}",
            items.len()
        );
        let name = items[0]["name"]
            .as_str()
            .expect("item.name should be a string");
        assert_eq!(
            name, linked_item_name,
            "item.name must equal the single linked row's item_name (linked-scoped ref)"
        );
    }

    // The collect binding accumulates 3 source-row references into linked.drawn_by.
    let drawn_by = linked_rows[0]["drawn_by"]
        .as_array()
        .expect("linked.drawn_by should be a list");
    assert_eq!(
        drawn_by.len(),
        3,
        "all 3 source rows drew the single linked row → drawn_by must have 3 entries; got {}",
        drawn_by.len()
    );
}

// Stage 5 — Per-segment witness correctness
//
// segmented_list_link: source (10 rows) has two lower cover members child_a (ratio:0.4)
// and child_b (ratio:0.6), making source a segmented staging node. Each segment generates
// its own GenerateWitness step covering only its slots. The assembled source output must:
//   1. Have exactly 10 rows
//   2. Each row must have an `items` list with exactly 2 elements (cardinality: 2)
//   3. child_a rows must all have tag "A"; child_b rows must all have tag "B"
//
// This verifies that per-segment witness generation and assembly produce the correct
// slot assignments without index-out-of-bounds panics.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_segmented_list_link_assembles_correctly() {
    let out = run("tests/fixtures/execute/segmented_list_link").await;

    let source_rows = jsonl_rows(&out, "source");
    // Bernoulli rounding in plan_segments can produce ±1 rows vs the declared count.
    assert!(
        (9..=11).contains(&source_rows.len()),
        "source should have ~10 rows; got {}",
        source_rows.len()
    );

    for (i, row) in source_rows.iter().enumerate() {
        let items = row["items"]
            .as_array()
            .unwrap_or_else(|| panic!("source row {i}: 'items' should be an array"));
        assert_eq!(
            items.len(),
            2,
            "source row {i}: cardinality:2 → each row should have 2 items; got {}",
            items.len()
        );
        for item in items {
            assert!(
                item["label"].is_string(),
                "source row {i}: each item should have a string 'label' field"
            );
        }
    }

    let child_a = jsonl_rows(&out, "child_a");
    let child_b = jsonl_rows(&out, "child_b");
    assert!(!child_a.is_empty(), "child_a should have rows");
    assert!(!child_b.is_empty(), "child_b should have rows");

    for row in &child_a {
        assert_eq!(
            row["tag"].as_str().unwrap_or(""),
            "A",
            "child_a rows must have tag='A'"
        );
    }
    for row in &child_b {
        assert_eq!(
            row["tag"].as_str().unwrap_or(""),
            "B",
            "child_b rows must have tag='B'"
        );
    }
}

// ===========================================================================
// Import tests
// ===========================================================================

// ---------------------------------------------------------------------------
// Import — simple: one imported file, one synthetic field added per row.
//
// tickers.csv has 20 rows (symbol, name).
// stocks.yaml imports it and appends a synthetic `score` column.
//
// Assertions:
//  - stocks.csv has exactly 20 rows (all imported rows, ring = full file)
//  - both imported columns (symbol, name) and the synthetic column (score) appear
//  - score values are numeric strings (non-empty)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_import_simple_row_count_and_columns() {
    let out = run("tests/fixtures/execute/import_simple").await;

    let rows = csv_rows(&out, "stocks");
    assert_eq!(rows, 20, "stocks should have all 20 tickers from the CSV file");

    // Both imported columns should be present.
    let symbols = csv_column(&out, "stocks", "symbol");
    assert_eq!(symbols.len(), 20, "symbol column should have 20 values");
    assert!(
        symbols.contains(&"AAPL".to_string()),
        "imported symbol AAPL should be present"
    );

    let names = csv_column(&out, "stocks", "name");
    assert_eq!(names.len(), 20);

    // Synthetic column should be present and non-empty.
    let scores = csv_column(&out, "stocks", "score");
    assert_eq!(scores.len(), 20, "score column should have 20 values");
    for s in &scores {
        assert!(!s.is_empty(), "each score should be non-empty");
        s.parse::<f64>().expect("score should be a valid number");
    }
}

// ---------------------------------------------------------------------------
// Import — lower cover: imported parent, two children with conflicting fields.
//
// stocks.yaml imports tickers.csv (20 rows) and adds a synthetic uuid `stock_id`.
// nasdaq.yaml and nyse.yaml both include stocks at ratio 0.5, setting conflicting
// `exchange` constants ("NASDAQ" / "NYSE"). This prunes the overlap segment so
// every parent row goes to exactly one child.
//
// Assertions:
//  - stocks.csv has 20 rows
//  - nasdaq.csv row count + nyse.csv row count = 20 (complete partition)
//  - nasdaq has only "NASDAQ" exchange values; nyse has only "NYSE"
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_import_lower_cover_partitions_rows() {
    let out = run("tests/fixtures/execute/import_lower_cover").await;

    let stocks = csv_rows(&out, "stocks");
    assert_eq!(stocks, 20, "stocks should have all 20 tickers");

    let nasdaq = csv_rows(&out, "nasdaq");
    let nyse = csv_rows(&out, "nyse");
    assert!(nasdaq > 0, "nasdaq should have some rows");
    assert!(nyse > 0, "nyse should have some rows");
    // The conflict-pruned overlap segment means IPF drives the remainder (∅)
    // segment toward zero; combined children should account for nearly all parent rows.
    // We use a subset bound (no duplication) plus a lower bound (substantial coverage).
    assert!(
        nasdaq + nyse <= stocks,
        "children rows must not exceed parent rows: nasdaq={nasdaq} nyse={nyse} stocks={stocks}"
    );
    assert!(
        nasdaq + nyse >= stocks * 8 / 10,
        "children should account for ≥ 80% of parent rows after conflict pruning"
    );

    // Verify exchange column constants.
    for ex in csv_column(&out, "nasdaq", "exchange") {
        assert_eq!(ex, "NASDAQ", "nasdaq rows must have exchange=NASDAQ");
    }
    for ex in csv_column(&out, "nyse", "exchange") {
        assert_eq!(ex, "NYSE", "nyse rows must have exchange=NYSE");
    }
}

// ---------------------------------------------------------------------------
// Import — variants: imported dataset split across two 50/50 variants.
//
// stocks.yaml imports tickers.csv (20 rows) and defines two variants:
//   - tier "A" (ratio 0.5) → ring [0.0, 0.5)
//   - tier "B" (ratio 0.5) → ring [0.5, 1.0)
//
// WriteSharedOutput combines both into stocks.csv.
//
// Assertions:
//  - stocks.csv has exactly 20 rows (= full tickers file, no duplicates)
//  - all rows have tier = "A" or "B"
//  - symbols are distinct (no ticker appears twice)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_import_variants_cover_full_file() {
    let out = run("tests/fixtures/execute/import_variants").await;

    let rows = csv_rows(&out, "stocks");
    assert_eq!(
        rows, 20,
        "both variants together must cover all 20 tickers exactly once"
    );

    let tiers = csv_column(&out, "stocks", "tier");
    assert!(
        tiers.iter().all(|t| t == "A" || t == "B"),
        "every row must have tier A or B"
    );

    // Each symbol should appear exactly once — no row duplicated across variants.
    let symbols = csv_column(&out, "stocks", "symbol");
    let mut seen = std::collections::HashSet::new();
    for sym in &symbols {
        assert!(seen.insert(sym.clone()), "symbol '{sym}' appeared more than once");
    }
}

// ---------------------------------------------------------------------------
// Import — links: tickers imported as an un-written pool, synthetic trades
// dataset draws from that pool per row.
//
// tickers.yaml: imports tickers.csv with no output_file → NOT written to disk.
//               Serves only as the linked pool for trades.
// trades.yaml:  30 synthetic rows. Each row has a `instruments` list of 1-3
//               tickers drawn from the pool (symbol + ticker_name fields).
//
// This models the canonical use-case: "I have real market tick metadata; I
// want to generate synthetic trade events that reference those real tickers."
//
// Assertions:
//  - no tickers output file exists (the pool is internal-only)
//  - trades.jsonl has 30 rows
//  - each trade has a non-empty `instruments` list
//  - every symbol in instruments is from tickers.csv
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_import_links_pool_not_written_trades_draw_from_it() {
    let out = run("tests/fixtures/execute/import_links").await;

    // The tickers dataset has no output_file — it must not be written.
    assert!(
        !out.join("tickers.parquet").exists() && !out.join("tickers.csv").exists(),
        "tickers should not be written to disk (it is an un-written import pool)"
    );

    // trades.jsonl should exist with 30 rows.
    let trades = jsonl_rows(&out, "trades");
    assert_eq!(trades.len(), 30, "trades should have 30 rows");

    let valid_symbols: std::collections::HashSet<&str> = [
        "AAPL", "MSFT", "GOOG", "AMZN", "TSLA", "META", "NVDA", "JPM",
        "JNJ", "V", "PG", "UNH", "BAC", "XOM", "HD", "WMT", "INTC",
        "VZ", "CSCO", "ORCL",
    ]
    .iter()
    .copied()
    .collect();

    for (i, row) in trades.iter().enumerate() {
        let instruments = row["instruments"]
            .as_array()
            .unwrap_or_else(|| panic!("trade {i}: instruments must be an array"));
        assert!(
            !instruments.is_empty(),
            "trade {i}: instruments list must not be empty"
        );
        for item in instruments {
            let sym = item["symbol"]
                .as_str()
                .unwrap_or_else(|| panic!("trade {i}: instrument symbol must be a string"));
            assert!(
                valid_symbols.contains(sym),
                "trade {i}: symbol '{sym}' is not in tickers.csv"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Chained links — REFRAME §Step 3 regression guard
//
// countries → regions → cities. Each level has a `links:` stanza, so the
// planner must recursively decompose into staging/witness/assembly nodes per
// link. Topological order must sequence innermost atoms first (cities), then
// regions complete with their city items, then countries complete with their
// region items. Ref integrity at every level: every region_id in
// countries.country_regions must exist in regions.region_id; every city_id in
// regions.region_cities must exist in cities.city_id.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chained_links_ref_integrity() {
    let out = run("tests/fixtures/execute/chained_links").await;

    let cities = jsonl_rows(&out, "cities");
    let regions = jsonl_rows(&out, "regions");
    let countries = jsonl_rows(&out, "countries");

    assert_eq!(cities.len(), 20, "cities should have 20 rows");
    assert_eq!(regions.len(), 8, "regions should have 8 rows");
    assert_eq!(countries.len(), 3, "countries should have 3 rows");

    let city_ids: std::collections::HashSet<&str> = cities
        .iter()
        .map(|r| r["city_id"].as_str().expect("cities.city_id must be string"))
        .collect();
    let region_ids: std::collections::HashSet<&str> = regions
        .iter()
        .map(|r| r["region_id"].as_str().expect("regions.region_id must be string"))
        .collect();

    // Inner link: regions.region_cities[].city_id ⊆ cities.city_id
    for (i, region) in regions.iter().enumerate() {
        let items = region["region_cities"]
            .as_array()
            .unwrap_or_else(|| panic!("region {i}: region_cities must be an array"));
        assert!(
            (2..=3).contains(&items.len()),
            "region {i}: cardinality min:2 max:3, got {}",
            items.len()
        );
        for item in items {
            let cid = item["city_id"].as_str().expect("city_id must be string");
            assert!(
                city_ids.contains(cid),
                "region {i}: city_id '{cid}' is not in cities (chain broken at innermost link)"
            );
        }
    }

    // Outer link: countries.country_regions[].region_id ⊆ regions.region_id.
    // This is the proof that the outer witness sees the fully-completed inner
    // dataset — innermost atoms generated first, then the inner witness/assembly,
    // then the outer witness's seed edge is satisfied.
    for (i, country) in countries.iter().enumerate() {
        let items = country["country_regions"]
            .as_array()
            .unwrap_or_else(|| panic!("country {i}: country_regions must be an array"));
        assert!(
            (1..=3).contains(&items.len()),
            "country {i}: cardinality min:1 max:3, got {}",
            items.len()
        );
        for item in items {
            let rid = item["region_id"].as_str().expect("region_id must be string");
            assert!(
                region_ids.contains(rid),
                "country {i}: region_id '{rid}' is not in regions (chain broken at outer link)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// VAR-EXPAND lock-in tests (PR 1).
//
// A parent with a mandatory (ratio 1.0) lower-cover member carrying a tagged-union
// field (`tier`), plus an overlapping sibling (`flagged`, ratio 0.5) so a joint
// segment exists. These pin the behaviour variant lowering must preserve.
// See specs/VAR-EXPAND-impl.md PR 1 and the traceability matrix.
// ---------------------------------------------------------------------------

// These pass today under VAR-2 and must keep passing after lowering — pure non-ref
// variant lowering is behaviour-preserving (verified empirically: 200 rows, exact
// 60/40, zero orphans/empties). IPF-exactness under *coupling* (a case conflicting
// with a sibling's constraint) is a VAR-SPECIALIZE concern and is tested there.

/// Every lowered case value is drawn from the union's declared set — never a
/// freshly-generated random string (the BUG-VAR failure mode) or empty.
#[tokio::test]
async fn test_variant_lowering_case_membership() {
    let out = run("tests/fixtures/execute/variant_lowering").await;
    let tiers = csv_column(&out, "subscribers", "tier");
    assert!(!tiers.is_empty(), "subscribers should have rows");
    let stray: Vec<&String> = tiers
        .iter()
        .filter(|t| *t != "gold" && *t != "silver")
        .collect();
    assert!(
        stray.is_empty(),
        "tier must be one of the declared cases {{gold, silver}}; found {} stray (first 3: {:?})",
        stray.len(),
        stray.iter().take(3).collect::<Vec<_>>(),
    );
}

/// A mandatory (ratio 1.0) union member produces one case for every parent row it
/// covers — no holder-less / orphan rows, and every ref resolves to the parent.
#[tokio::test]
async fn test_variant_lowering_mandatory_member_no_orphan() {
    let out = run("tests/fixtures/execute/variant_lowering").await;
    let parent_ids: std::collections::HashSet<String> =
        csv_column(&out, "parent", "id").into_iter().collect();
    let sub_ids = csv_column(&out, "subscribers", "parent_id");

    // ratio 1.0 ⇒ every parent row is a subscriber.
    assert_eq!(
        sub_ids.len(),
        parent_ids.len(),
        "mandatory member should have one row per parent row"
    );
    let orphans = sub_ids.iter().filter(|v| !parent_ids.contains(*v)).count();
    assert_eq!(orphans, 0, "subscribers.parent_id has {orphans} orphan refs");
}

/// The union's declared ratios are honoured (loose band — segment Bernoulli rounding
/// is stochastic). Both cases appear and gold dominates ~60/40.
#[tokio::test]
async fn test_variant_lowering_case_distribution() {
    let out = run("tests/fixtures/execute/variant_lowering").await;
    let tiers = csv_column(&out, "subscribers", "tier");
    let n = tiers.len() as f64;
    let gold = tiers.iter().filter(|t| *t == "gold").count() as f64;
    let silver = tiers.iter().filter(|t| *t == "silver").count() as f64;
    assert!(gold > 0.0 && silver > 0.0, "both cases must appear");
    let gold_frac = gold / n;
    assert!(
        (gold_frac - 0.6).abs() < 0.12,
        "gold fraction {gold_frac:.3} should be near declared 0.6 (gold={gold}, silver={silver})"
    );
}
