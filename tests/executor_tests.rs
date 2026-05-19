use fakeset::{
    executor::execute, expand_variants::expand_field_variants,
    expressions::pull_down_expression_deps, graph::build_dag,
    load_all_datasets, plan::build_plan, rewrite::resolve_refs, validate::validate,
};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn run(fixture: &str) -> PathBuf {
    let fixture_path = PathBuf::from(fixture);
    let out = std::env::temp_dir().join(format!(
        "fakeset_test_{}",
        fixture.replace(['/', '\\', '.'], "_")
    ));
    let _ = std::fs::remove_dir_all(&out);

    let datasets = load_all_datasets(&[fixture_path]).expect("load datasets");
    let dag = build_dag(&datasets).expect("build dag");
    let datasets = pull_down_expression_deps(&datasets).expect("pull down");
    for w in validate(&datasets).expect("validate") {
        eprintln!("warn: {w}");
    }
    let datasets = expand_field_variants(datasets).expect("expand field variants");
    let resolved = resolve_refs(&datasets).expect("resolve refs");
    let plan = build_plan(&dag, &resolved, 16).expect("build plan");
    execute(&plan, &out).await.expect("execute");
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
    assert_eq!(csv_rows(&out, "person"), 7, "person.csv should have 7 data rows");
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
    assert_eq!(csv_rows(&out, "subset"), 10, "subset should have 20 × 0.5 = 10 rows");
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
    assert_eq!(csv_rows(&out, "cats"), 3, "cats gets its declared 50% of 6 rows");
    assert_eq!(csv_rows(&out, "dogs"), 3, "dogs gets its declared 50% of 6 rows");

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
// 4. Prefill wiring — ref field values flow from includer to includee
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

    assert_eq!(csv_rows(&out, "combined"), 7, "combined.csv should have 3 + 4 = 7 rows");

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
    assert_eq!(csv_rows(&out, "subset"), 5, "subset should have 5 rows (dist 0.5)");

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
            assert!(
                (0.0..=10.0).contains(&n),
                "score {n} should be in [0, 10]"
            );
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
async fn test_bernoulli_nested_include_parent_assembles_correctly() {
    let out = run("tests/fixtures/execute/bernoulli_rich_list").await;

    assert_eq!(jsonl_rows(&out, "items").len(), 20, "items should have 20 rows");

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
            let label = pick["item_label"].as_str().expect("item_label should be a string");
            assert!(!label.is_empty(), "item_label should be non-empty");
            let value = pick["item_value"].as_f64().expect("item_value should be a number");
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
async fn test_plain_fields_in_nested_include_content() {
    let out = run("tests/fixtures/execute/rich_list_plain").await;

    let rows = jsonl_rows(&out, "records");
    assert_eq!(rows.len(), 5, "records should have 5 rows");

    for row in &rows {
        let entries = row["entries"].as_array().expect("entries should be an array");
        assert!(
            (1..=4).contains(&entries.len()),
            "entries count should be 1–4; got {}",
            entries.len()
        );
        for entry in entries {
            let tag = entry["tag"].as_str().expect("tag should be a string");
            assert!(!tag.is_empty(), "include-scoped tag should be non-empty");

            let badge = entry["badge"].as_str().expect("badge should be a string");
            assert!(!badge.is_empty(), "plain-generated badge should be non-empty");

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

    let total_items: usize = rows.iter().map(|r| {
        r["samples"].as_array().expect("samples should be an array").len()
    }).sum();

    // With mean=3 over 10 rows the expected total is ~30 items.
    // Assert it's in a very wide band to avoid flakiness.
    assert!(total_items > 0, "expected at least some list items across all rows");
    assert!(
        total_items < 100,
        "expected total list items < 100 (mean 3 × 10 rows); got {total_items}"
    );

    for row in &rows {
        let samples = row["samples"].as_array().expect("samples should be an array");
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
async fn test_nested_include_refs() {
    let out = run("tests/fixtures/execute/rich_list").await;
    let event_rows = jsonl_rows(&out, "events");

    assert_eq!(event_rows.len(), 5, "events should have 5 rows");

    for row in &event_rows {
        let outer_title = row["title"].as_str().expect("title should be a string");

        let attendees = row["attendees"].as_array().expect("attendees should be a list");
        assert!(
            (1..=4).contains(&attendees.len()),
            "attendee count should be 1–4; got {}",
            attendees.len()
        );

        for attendee in attendees {
            // Include-scoped: name comes from people.full_name (should be a non-empty string)
            let name = attendee["name"].as_str().expect("attendee name should be a string");
            assert!(!name.is_empty(), "attendee name should be non-empty");

            // Outer-scoped: event_title should match this event's title
            let at = attendee["event_title"].as_str().expect("event_title should be a string");
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
    assert_eq!(rows, 100, "all variant rows should be combined into orders.csv");
    let statuses = csv_column(&out, "orders", "status");
    assert!(statuses.contains(&"pending".to_string()), "expected 'pending' status in output");
    assert!(statuses.contains(&"shipped".to_string()), "expected 'shipped' status in output");
    assert!(statuses.contains(&"cancelled".to_string()), "expected 'cancelled' status in output");
}

#[tokio::test]
async fn test_variant_sibling_total_rows() {
    // source: 100 rows split 70/30 across variants.
    // subset: Bernoulli sibling at dist 0.4 → ~40 rows across both variant groups.
    // source output: 100 rows combined; subset output: ~40 rows combined.
    let out = run("tests/fixtures/execute/variant_sibling").await;
    let source_rows = csv_rows(&out, "source");
    assert_eq!(source_rows, 100, "source variants should total 100 rows");
    let subset_rows = csv_rows(&out, "subset");
    assert!(subset_rows > 0, "subset should have rows from both variant groups");
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
    assert_eq!(rows, 120, "all 6 variant combinations should total 120 rows in orders.csv");

    let statuses = csv_column(&out, "orders", "status");
    assert!(statuses.contains(&"pending".to_string()), "expected 'pending' status");
    assert!(statuses.contains(&"shipped".to_string()), "expected 'shipped' status");

    let tiers = csv_column(&out, "orders", "tier");
    assert!(tiers.contains(&"gold".to_string()), "expected 'gold' tier");
    assert!(tiers.contains(&"silver".to_string()), "expected 'silver' tier");
    assert!(tiers.contains(&"bronze".to_string()), "expected 'bronze' tier");
}
