use fakeset::{
    expand_variants::expand_field_variants, expressions::pull_down_expression_deps,
    graph::build_dag, load_all_datasets,
    plan::{build_plan, ExecutionStep},
    rewrite::{expand_include_fields, resolve_refs},
    validate::validate,
};
// Shorthand used in several assertions below.
macro_rules! find_step {
    ($steps:expr, $pat:pat if $guard:expr) => {
        $steps.iter().find(|s| matches!(s, $pat if $guard))
    };
    ($steps:expr, $pat:pat) => {
        $steps.iter().find(|s| matches!(s, $pat))
    };
}
use std::path::PathBuf;

fn plan_for(fixture: &str) -> Vec<ExecutionStep> {
    let datasets = load_all_datasets(&[PathBuf::from(fixture)]).expect("load");
    let dag = build_dag(&datasets).expect("dag");
    let datasets = pull_down_expression_deps(&datasets).expect("pull_down");
    validate(&datasets).expect("validate");
    let datasets = expand_field_variants(datasets).expect("expand field variants");
    let datasets = expand_include_fields(&datasets).expect("expand include fields");
    let resolved = resolve_refs(&datasets).expect("resolve");
    build_plan(&dag, &resolved, 16).expect("plan").steps
}

fn plan_err_for(fixture: &str) -> anyhow::Error {
    let datasets = load_all_datasets(&[PathBuf::from(fixture)]).expect("load");
    let dag = build_dag(&datasets).expect("dag");
    let datasets = pull_down_expression_deps(&datasets).expect("pull_down");
    validate(&datasets).expect("validate");
    let datasets = expand_field_variants(datasets).expect("expand field variants");
    let datasets = expand_include_fields(&datasets).expect("expand include fields");
    let resolved = resolve_refs(&datasets).expect("resolve");
    match build_plan(&dag, &resolved, 16) {
        Ok(_) => panic!("expected plan error but build_plan succeeded"),
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// Step structure
// ---------------------------------------------------------------------------

#[test]
fn flat_dataset_produces_generate_then_write_steps() {
    let steps = plan_for("tests/fixtures/execute/flat");
    assert_eq!(steps.len(), 2, "expected GenerateDataset + WriteSharedOutput");
    assert!(
        matches!(&steps[0], ExecutionStep::GenerateDataset { dataset, .. } if dataset.name == "person"),
        "expected GenerateDataset for person"
    );
    assert!(
        matches!(&steps[1], ExecutionStep::WriteSharedOutput { output_file, .. } if output_file == "person"),
        "expected WriteSharedOutput for person"
    );
}

#[test]
fn bernoulli_lower_cover_member_absorbed_not_standalone() {
    // single_sibling: source (parent, rows:20) and subset (lower cover member, ratio:0.5).
    // Only source should appear as a step; subset is absorbed into the group.
    let steps = plan_for("tests/fixtures/execute/single_sibling");
    let has_standalone_subset = steps.iter().any(|s| {
        matches!(s, ExecutionStep::GenerateDataset { dataset, .. } if dataset.name == "subset")
    });
    assert!(
        !has_standalone_subset,
        "subset is a Bernoulli lower cover member and must not appear as a standalone GenerateDataset"
    );
    assert!(
        steps.iter().any(|s| matches!(s, ExecutionStep::GenerateLowerCoverGroup { parent, .. } if parent.name == "source")),
        "expected GenerateLowerCoverGroup for source"
    );
}

#[test]
fn write_shared_output_step_appended_at_end() {
    // shared_output: alpha and beta both write to output_file: combined.
    // The plan should end with exactly one WriteSharedOutput step.
    let steps = plan_for("tests/fixtures/execute/shared_output");
    let last = steps.last().expect("plan should not be empty");
    assert!(
        matches!(last, ExecutionStep::WriteSharedOutput { output_file, .. } if output_file == "combined"),
        "last step should be WriteSharedOutput for 'combined', got: {last:?}"
    );
    let write_count = steps
        .iter()
        .filter(|s| matches!(s, ExecutionStep::WriteSharedOutput { .. }))
        .count();
    assert_eq!(write_count, 1, "exactly one WriteSharedOutput step expected");
}

// ---------------------------------------------------------------------------
// Row count propagation (tested through plan segments)
// ---------------------------------------------------------------------------

#[test]
fn distribution_drives_parent_segment_row_counts() {
    // source has rows:20, subset includes source at dist:0.5.
    // Bernoulli plan_segments for source should total 20 rows, with 10 rows
    // going to the segment that includes subset.
    let steps = plan_for("tests/fixtures/execute/single_sibling");
    let segments = steps.iter().find_map(|s| match s {
        ExecutionStep::GenerateLowerCoverGroup { parent, segments, .. }
            if parent.name == "source" =>
        {
            Some(segments)
        }
        _ => None,
    });
    let segments = segments.expect("GenerateLowerCoverGroup for source not found");

    let total: usize = segments.iter().map(|s| s.rows).sum();
    assert_eq!(total, 20, "total segment rows should equal source's row count");

    let member_rows: usize = segments
        .iter()
        .filter(|s| !s.members.is_empty())
        .map(|s| s.rows)
        .sum();
    assert_eq!(member_rows, 10, "subset's segment should cover 20 × 0.5 = 10 rows");
}

// ---------------------------------------------------------------------------
// Prefill wiring
// ---------------------------------------------------------------------------

#[test]
fn ref_field_wired_as_prefill_on_includee() {
    // ref_wiring: derived includes source with no explicit ratio (defaults to 1.0).
    // Since Stage 5, all children are registered as lower cover members unconditionally, so
    // source gets a GenerateLowerCoverGroup step with derived as a lower cover member.
    // The actual ref-field wiring (derived.id → source.id) is verified by the
    // executor test test_ref_wiring_propagates_column_values.
    let steps = plan_for("tests/fixtures/execute/ref_wiring");
    let lower_cover_group = steps.iter().find_map(|s| match s {
        ExecutionStep::GenerateLowerCoverGroup { parent, members, .. } if parent.name == "source" => {
            Some(members)
        }
        _ => None,
    });
    let members = lower_cover_group.expect("GenerateLowerCoverGroup step for 'source' not found");
    assert!(
        members.iter().any(|m| m.dataset.name == "derived"),
        "derived should appear as a lower cover member of source; got: {members:?}"
    );
}

#[test]
fn hidden_ref_field_wired_as_prefill() {
    // expression_pulldown: derived has expression "age * 2" referencing source.age,
    // pulled down as a hidden ref field. Since Stage 5, derived is registered as a
    // lower cover member of source unconditionally, so source gets GenerateLowerCoverGroup.
    // The actual hidden-field wiring is verified by the executor tests.
    let steps = plan_for("tests/fixtures/execute/expression_pulldown");
    let lower_cover_group = steps.iter().find_map(|s| match s {
        ExecutionStep::GenerateLowerCoverGroup { parent, members, .. } if parent.name == "source" => {
            Some(members)
        }
        _ => None,
    });
    let members = lower_cover_group.expect("GenerateLowerCoverGroup step for 'source' not found");
    assert!(
        members.iter().any(|m| m.dataset.name == "derived"),
        "derived should appear as a lower cover member of source; got: {members:?}"
    );
}

// ---------------------------------------------------------------------------
// List-link plan decomposition
// ---------------------------------------------------------------------------

#[test]
fn list_link_dataset_decomposes_into_witness_and_assemble() {
    // events has a list-link field (attendees), people does not.
    // Because events includes people with a ratio (witness source), people gets
    // a GenerateLowerCoverGroup step (witness-source-rows-first ordering for GenerateWitness).
    // Expected steps: GenerateLowerCoverGroup(people, skip_parent_emit=false),
    //                 GenerateDataset(events, skip_emit=true),
    //                 GenerateWitness(attendees),
    //                 AssembleFromWitness(events)
    let steps = plan_for("tests/fixtures/execute/link_content");

    // people: no nested include → must not be skip-emitted (GenerateDataset or GenerateLowerCoverGroup)
    let people_not_skipped = steps.iter().any(|s| match s {
        ExecutionStep::GenerateDataset { dataset, skip_emit: false, .. } => dataset.name == "people",
        ExecutionStep::GenerateLowerCoverGroup { parent, skip_parent_emit: false, .. } => parent.name == "people",
        _ => false,
    });
    assert!(people_not_skipped, "people has no nested include, must have a non-skipped generation step");

    // events: has nested include → skip_emit must be true
    let events_step = find_step!(steps, ExecutionStep::GenerateDataset { dataset, .. } if dataset.name == "events");
    assert!(
        matches!(events_step, Some(ExecutionStep::GenerateDataset { skip_emit: true, .. })),
        "events has a nested include field, skip_emit must be true"
    );

    // GenerateWitness for attendees must be present
    assert!(
        find_step!(steps, ExecutionStep::GenerateWitness { list_field_name, .. } if list_field_name == "attendees").is_some(),
        "expected GenerateWitness step for 'attendees'"
    );

    // AssembleFromWitness for events must be present
    assert!(
        find_step!(steps, ExecutionStep::AssembleFromWitness { dataset, .. } if dataset.name == "events").is_some(),
        "expected AssembleFromWitness step for 'events'"
    );

    // GenerateWitness must come before AssembleFromWitness
    let flat_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::GenerateWitness { list_field_name, .. } if list_field_name == "attendees")
    }).expect("GenerateWitness not found");
    let assemble_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::AssembleFromWitness { dataset, .. } if dataset.name == "events")
    }).expect("AssembleFromWitness not found");
    assert!(
        flat_pos < assemble_pos,
        "GenerateWitness must precede AssembleFromWitness (flat={flat_pos}, assemble={assemble_pos})"
    );
}

#[test]
fn bernoulli_list_link_parent_has_skip_parent_emit() {
    // events is both a Bernoulli parent (vip is its lower cover member at ratio:0.5) and has
    // a list-link field (picks). It must produce:
    //   GenerateLowerCoverGroup(events, skip_parent_emit=true)
    //   GenerateWitness(picks)
    //   AssembleFromWitness(events)
    let steps = plan_for("tests/fixtures/execute/bernoulli_link_content");

    let group_step = find_step!(
        steps, ExecutionStep::GenerateLowerCoverGroup { parent, .. } if parent.name == "events"
    );
    assert!(
        matches!(group_step, Some(ExecutionStep::GenerateLowerCoverGroup { skip_parent_emit: true, .. })),
        "events has a nested include field, skip_parent_emit must be true"
    );

    assert!(
        find_step!(steps, ExecutionStep::GenerateWitness { list_field_name, .. } if list_field_name == "picks").is_some(),
        "expected GenerateWitness for 'picks'"
    );
    assert!(
        find_step!(steps, ExecutionStep::AssembleFromWitness { dataset, .. } if dataset.name == "events").is_some(),
        "expected AssembleFromWitness for 'events'"
    );

    // GenerateLowerCoverGroup must come before GenerateWitness
    let group_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::GenerateLowerCoverGroup { parent, .. } if parent.name == "events")
    }).unwrap();
    let flat_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::GenerateWitness { list_field_name, .. } if list_field_name == "picks")
    }).unwrap();
    assert!(group_pos < flat_pos, "GenerateLowerCoverGroup must precede GenerateWitness");
}

#[test]
fn non_ref_dataset_has_no_prefills() {
    // flat/person has no includes and no includers with ref fields.
    let steps = plan_for("tests/fixtures/execute/flat");
    let prefills = steps.iter().find_map(|s| match s {
        ExecutionStep::GenerateDataset { dataset, prefills, .. } if dataset.name == "person" => {
            Some(prefills)
        }
        _ => None,
    });
    assert!(
        prefills.map_or(false, |p| p.is_empty()),
        "person has no ref relationships and should have empty prefills"
    );
}

// ---------------------------------------------------------------------------
// Variant expansion
// ---------------------------------------------------------------------------

#[test]
fn variant_dataset_produces_n_generate_steps() {
    // orders has 3 variants (60/30/10%) — should produce 3 GenerateDataset steps,
    // no standalone non-variant GenerateDataset for orders, and one WriteSharedOutput.
    let steps = plan_for("tests/fixtures/execute/variants");

    let variant_steps: Vec<_> = steps.iter().filter(|s| {
        matches!(s, ExecutionStep::GenerateDataset { dataset, .. } if dataset.name.starts_with("orders__v"))
    }).collect();
    assert_eq!(variant_steps.len(), 3, "expected 3 variant GenerateDataset steps, got {}", variant_steps.len());

    let write_steps: Vec<_> = steps.iter().filter(|s| {
        matches!(s, ExecutionStep::WriteSharedOutput { output_file, .. } if output_file == "orders")
    }).collect();
    assert_eq!(write_steps.len(), 1, "expected exactly one WriteSharedOutput for 'orders'");
}

#[test]
fn variant_rows_sum_to_parent_and_respect_distribution() {
    let steps = plan_for("tests/fixtures/execute/variants");

    let total: usize = steps.iter().filter_map(|s| match s {
        ExecutionStep::GenerateDataset { dataset, rows, .. } if dataset.name.starts_with("orders__v") => Some(rows),
        _ => None,
    }).sum();
    assert_eq!(total, 100, "variant row counts must sum to parent rows (100)");

    // v0 = 60%, v1 = 30%, v2 = 10% of 100
    let v0 = steps.iter().find_map(|s| match s {
        ExecutionStep::GenerateDataset { dataset, rows, .. } if dataset.name == "orders__v0" => Some(*rows),
        _ => None,
    }).expect("orders__v0 step not found");
    let v1 = steps.iter().find_map(|s| match s {
        ExecutionStep::GenerateDataset { dataset, rows, .. } if dataset.name == "orders__v1" => Some(*rows),
        _ => None,
    }).expect("orders__v1 step not found");
    let v2 = steps.iter().find_map(|s| match s {
        ExecutionStep::GenerateDataset { dataset, rows, .. } if dataset.name == "orders__v2" => Some(*rows),
        _ => None,
    }).expect("orders__v2 step not found");
    assert_eq!(v0, 60);
    assert_eq!(v1, 30);
    assert_eq!(v2, 10);
}

#[test]
fn variant_lower_cover_member_produces_lower_cover_groups_and_shared_outputs() {
    // source has 2 variants (70/30), subset is a Bernoulli lower cover member at ratio:0.4.
    // Expected: 2 GenerateLowerCoverGroup steps (one per variant), 2 WriteSharedOutput steps
    // (one for source variants, one for subset accumulation).
    let steps = plan_for("tests/fixtures/execute/variant_sibling");

    let lower_cover_groups: Vec<_> = steps.iter().filter(|s| {
        matches!(s, ExecutionStep::GenerateLowerCoverGroup { parent, .. } if parent.name.starts_with("source__v"))
    }).collect();
    assert_eq!(lower_cover_groups.len(), 2, "expected 2 GenerateLowerCoverGroup steps for variant parents");

    let write_count = steps.iter().filter(|s| matches!(s, ExecutionStep::WriteSharedOutput { .. })).count();
    assert_eq!(write_count, 2, "expected WriteSharedOutput for source and for subset");
}

#[test]
fn field_variant_expands_to_correct_generate_steps() {
    // orders.yaml: 120 rows, status(50/50) × tier(25/25/50) = 6 combinations.
    // Expect 6 GenerateDataset steps + 1 WriteSharedOutput.
    let steps = plan_for("tests/fixtures/execute/field_variants");

    let variant_steps: Vec<_> = steps.iter().filter(|s| {
        matches!(s, ExecutionStep::GenerateDataset { dataset, .. } if dataset.name.starts_with("orders__v"))
    }).collect();
    assert_eq!(variant_steps.len(), 6, "expected 6 variant GenerateDataset steps, got {}", variant_steps.len());

    let write_steps: Vec<_> = steps.iter().filter(|s| {
        matches!(s, ExecutionStep::WriteSharedOutput { output_file, .. } if output_file == "orders")
    }).collect();
    assert_eq!(write_steps.len(), 1, "expected exactly one WriteSharedOutput for 'orders'");

    let total_rows: usize = steps.iter().filter_map(|s| match s {
        ExecutionStep::GenerateDataset { dataset, rows, .. } if dataset.name.starts_with("orders__v") => Some(rows),
        _ => None,
    }).sum();
    assert_eq!(total_rows, 120, "variant row counts must sum to 120");
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 3: collect target pre-scan
// ---------------------------------------------------------------------------

#[test]
fn list_link_collect_produces_correct_step_sequence() {
    // pool: plain dataset (rows: 5) with a collect-target list field.
    // outer: nested-include dataset; content field has refs: [pool.item_name, {bind: pool.collected_labels, reducer: collect}].
    //
    // Expected step order:
    //   GenerateDataset[pool, skip_emit=true]   ← collect target, file write deferred
    //   GenerateDataset[outer, skip_emit=true]  ← has nested include fields
    //   GenerateWitness[items]
    //   AccumulateToLinked[items.item → pool.collected_labels]
    //   EmitDataset[pool]
    //   AssembleFromWitness[outer]
    let steps = plan_for("tests/fixtures/plan/nested_collect");

    // pool must be generated with skip_emit/skip_parent_emit=true (it is a collect target).
    // It may appear as GenerateDataset or GenerateLowerCoverGroup (lower cover members are registered
    // against it), so we accept either — the key invariant is that file write is deferred.
    let pool_skips_emit = steps.iter().any(|s| match s {
        ExecutionStep::GenerateDataset { dataset, skip_emit: true, .. } => dataset.name == "pool",
        ExecutionStep::GenerateLowerCoverGroup { parent, skip_parent_emit: true, .. } => parent.name == "pool",
        _ => false,
    });
    assert!(pool_skips_emit, "pool is a collect target — file write must be deferred (skip_emit/skip_parent_emit=true)");

    // GenerateWitness for 'items' must be present
    assert!(
        find_step!(steps, ExecutionStep::GenerateWitness { list_field_name, .. } if list_field_name == "items").is_some(),
        "expected GenerateWitness step for 'items'"
    );

    // AccumulateToLinked must be present targeting pool.collected_labels
    let collect_step = find_step!(
        steps,
        ExecutionStep::AccumulateToLinked { source_field, linked_field, .. }
        if source_field == "item" && linked_field == "collected_labels"
    );
    assert!(collect_step.is_some(), "expected AccumulateToLinked for item → pool.collected_labels");

    // EmitDataset for pool must be present
    assert!(
        find_step!(steps, ExecutionStep::EmitDataset { dataset, .. } if dataset.name == "pool").is_some(),
        "expected EmitDataset step for 'pool'"
    );

    // AssembleFromWitness for outer must be present
    assert!(
        find_step!(steps, ExecutionStep::AssembleFromWitness { dataset, .. } if dataset.name == "outer").is_some(),
        "expected AssembleFromWitness for 'outer'"
    );

    // Ordering: GenerateWitness → AccumulateToLinked → EmitDataset[pool] → AssembleFromWitness
    let flat_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::GenerateWitness { list_field_name, .. } if list_field_name == "items")
    }).expect("GenerateWitness not found");
    let collect_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::AccumulateToLinked { linked_field, .. } if linked_field == "collected_labels")
    }).expect("AccumulateToLinked not found");
    let emit_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::EmitDataset { dataset, .. } if dataset.name == "pool")
    }).expect("EmitDataset[pool] not found");
    let assemble_pos = steps.iter().position(|s| {
        matches!(s, ExecutionStep::AssembleFromWitness { dataset, .. } if dataset.name == "outer")
    }).expect("AssembleFromWitness not found");

    assert!(flat_pos < collect_pos, "GenerateWitness must precede AccumulateToLinked");
    assert!(collect_pos < emit_pos,  "AccumulateToLinked must precede EmitDataset[pool]");
    assert!(emit_pos < assemble_pos, "EmitDataset[pool] must precede AssembleFromWitness");
}

// ---------------------------------------------------------------------------
// MULT-2 Stage 9: plan-level error checks
// ---------------------------------------------------------------------------

#[test]
fn case2_collect_with_jointly_segmented_pool_errors() {
    // case2_collect_joint_segment: outer links pool (collect binding) and flat_sibling
    // includes pool with ratio:0.5. pool is therefore jointly segmented with flat_sibling
    // (a non-witness-source lower cover member), which violates the v1 Case 2 restriction.
    let err = plan_err_for("tests/fixtures/plan/case2_collect_joint_segment");
    let msg = err.to_string();
    assert!(
        msg.contains("jointly segmented") || msg.contains("not supported"),
        "error should mention joint segmentation restriction; got: {msg}"
    );
}

#[test]
fn reinforcement_zero_exceeding_pool_errors() {
    // reinforcement_zero_infeasible: outer links pool (3 rows) with reinforcement:0 and
    // cardinality:5. max_cardinality (5) > eligible pool size (3) → planning error.
    let err = plan_err_for("tests/fixtures/plan/reinforcement_zero_infeasible");
    let msg = err.to_string();
    assert!(
        msg.contains("reinforcement") && (msg.contains("eligible") || msg.contains("cardinality")),
        "error should mention reinforcement and eligible pool size; got: {msg}"
    );
}
