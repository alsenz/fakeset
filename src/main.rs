use anyhow::Result;
use clap::Parser;
use fakeset::{
    executor::execute,
    expand_variants::expand_field_variants,
    expressions::pull_down_expression_deps,
    graph::build_dag,
    import::load_import_headers,
    list_norm::desugar_normalize,
    load_all_datasets,
    models::{Format, SeedConfig, SyntheticDataset},
    plan::{ExecutionPlan, ExecutionStep, apply_scale, build_plan},
    rewrite::{apply_global_locales, expand_include_fields, resolve_refs},
    validate::validate,
};
use petgraph::visit::Topo;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "fakeset",
    about = "Generate fake datasets from YAML definitions"
)]
struct Cli {
    /// Files and directories containing YAML dataset definitions
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Directory to write generated outputs into
    #[arg(short, long, default_value = "output")]
    output: PathBuf,

    /// Print the DAG with field details before the rewrite pass, then exit
    #[arg(long)]
    print_dag: bool,

    /// Print the DAG with field details after the rewrite pass (resolved types
    /// and merged constraints), then exit
    #[arg(long)]
    print_rewritten: bool,

    /// Print the execution plan (row counts, lower cover segments, inherited field wiring), then exit
    #[arg(long)]
    print_plan: bool,

    /// Override the output format for every dataset (parquet, csv, json, jsonl).
    /// Takes precedence over per-dataset `format:` declarations.
    #[arg(long, value_name = "FORMAT")]
    output_format: Option<Format>,

    /// Seed for the import hash ring. When set, import file partitions are
    /// deterministic across runs with the same seed and schema. When omitted,
    /// a random seed is chosen each run.
    #[arg(long = "seed.ring", value_name = "SEED")]
    seed_ring: Option<u64>,

    /// Scale all row counts by this factor. Values < 1.0 produce a proportional
    /// sample (e.g. 0.1 = 10%); values > 1.0 scale up. Import datasets are scaled
    /// by narrowing or widening their ring segment — scale-up is only permitted when
    /// every import already uses a ring that leaves unused rows.
    #[arg(long, value_name = "FACTOR")]
    scale: Option<f64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut datasets = load_all_datasets(&cli.paths)?;
    if let Some(ref fmt) = cli.output_format {
        for ds in datasets.values_mut() {
            ds.format = fmt.clone();
        }
    }
    load_import_headers(&mut datasets)?;
    if let Some(scale) = cli.scale {
        apply_scale(&mut datasets, scale)?;
    }
    let dag = build_dag(&datasets)?;
    let datasets = pull_down_expression_deps(&datasets)?;
    for warning in validate(&datasets)? {
        eprintln!("{warning}");
    }
    let datasets = expand_field_variants(datasets)?;
    let datasets = desugar_normalize(datasets)?;
    let datasets = expand_include_fields(&datasets)?;

    if cli.print_dag {
        println!("=== DAG (before rewrite) ===\n");
        print_datasets(&datasets, &dag.graph);
        return Ok(());
    }

    let mut resolved = resolve_refs(&datasets)?;
    apply_global_locales(&mut resolved);

    if cli.print_rewritten {
        println!("=== DAG (after rewrite) ===\n");
        print_datasets(&resolved, &dag.graph);
        return Ok(());
    }

    let plan = build_plan(&dag, &resolved)?;

    if cli.print_plan {
        println!("=== Execution Plan ({} steps) ===\n", plan.steps.len());
        print_plan(&plan);
        return Ok(());
    }

    println!(
        "Loaded {} dataset(s) into DAG ({} edge(s))",
        dag.graph.node_count(),
        dag.graph.edge_count(),
    );

    let seed_config = SeedConfig {
        ring: cli.seed_ring.unwrap_or_else(rand::random),
    };
    execute(&plan, &cli.output, &seed_config).await?;

    println!("Done. Output written to {}", cli.output.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Plan pretty-printer
// ---------------------------------------------------------------------------

fn print_plan(plan: &ExecutionPlan) {
    for (i, step) in plan.steps.iter().enumerate() {
        match step {
            ExecutionStep::GenerateStagingNode {
                dataset,
                rows,
                inherited,
                ..
            } => {
                println!(
                    "[{}] staging node: {} ({} rows, {})",
                    i + 1,
                    dataset.name,
                    rows,
                    dataset.format
                );
                for field in &dataset.data {
                    print_field(field, 4);
                }
                for p in inherited {
                    let src = p
                        .from_path
                        .file_stem()
                        .and_then(|s: &std::ffi::OsStr| s.to_str())
                        .unwrap_or("?");
                    println!(
                        "    inherits: {}.{} → {}",
                        src, p.from_column, p.into_column
                    );
                }
            }
            ExecutionStep::GenerateDataset {
                dataset,
                rows,
                inherited,
                ..
            } => {
                println!(
                    "[{}] generate: {} ({} rows, {})",
                    i + 1,
                    dataset.name,
                    rows,
                    dataset.format
                );
                for field in &dataset.data {
                    print_field(field, 4);
                }
                for p in inherited {
                    let src = p
                        .from_path
                        .file_stem()
                        .and_then(|s: &std::ffi::OsStr| s.to_str())
                        .unwrap_or("?");
                    println!(
                        "    inherits: {}.{} → {}",
                        src, p.from_column, p.into_column
                    );
                }
            }
            ExecutionStep::GenerateStagingLowerCoverGroup {
                parent,
                segments,
                members,
                ..
            } => {
                let total: usize = segments.iter().map(|s| s.rows).sum();
                println!(
                    "[{}] staging lower cover group: {} ({} rows across {} segments, {})",
                    i + 1,
                    parent.name,
                    total,
                    segments.len(),
                    parent.format
                );
                for seg in segments {
                    let names: Vec<&str> = seg
                        .members
                        .iter()
                        .filter_map(|p: &PathBuf| {
                            p.file_stem().and_then(|s: &std::ffi::OsStr| s.to_str())
                        })
                        .collect();
                    let label = if names.is_empty() {
                        "(parent-only)".to_string()
                    } else {
                        format!("{{{}}}", names.join(", "))
                    };
                    println!("    segment {} → {} rows", label, seg.rows);
                }
                println!("    members:");
                for m in members {
                    println!("      {} ({})", m.dataset.name, m.dataset.format);
                }
            }
            ExecutionStep::GenerateLowerCoverGroup {
                parent,
                segments,
                members,
                ..
            } => {
                let total: usize = segments.iter().map(|s| s.rows).sum();
                println!(
                    "[{}] lower cover group: {} ({} rows across {} segments, {})",
                    i + 1,
                    parent.name,
                    total,
                    segments.len(),
                    parent.format
                );
                for seg in segments {
                    let names: Vec<&str> = seg
                        .members
                        .iter()
                        .filter_map(|p: &PathBuf| {
                            p.file_stem().and_then(|s: &std::ffi::OsStr| s.to_str())
                        })
                        .collect();
                    let label = if names.is_empty() {
                        "(parent-only)".to_string()
                    } else {
                        format!("{{{}}}", names.join(", "))
                    };
                    if seg.field_constraints.is_empty() {
                        println!("    segment {} → {} rows", label, seg.rows);
                    } else {
                        let overrides: Vec<String> = seg
                            .field_constraints
                            .iter()
                            .filter_map(|(k, fc)| {
                                fc.value
                                    .as_ref()
                                    .map(|v| format!("{k}={}", format_yaml_value(v)))
                                    .or_else(|| {
                                        let lo = fc.min.map(|v| format!("min:{v}"));
                                        let hi = fc.max.map(|v| format!("max:{v}"));
                                        let parts: Vec<_> =
                                            [lo, hi].into_iter().flatten().collect();
                                        if parts.is_empty() {
                                            None
                                        } else {
                                            Some(format!("{k}({})", parts.join(" ")))
                                        }
                                    })
                            })
                            .collect();
                        println!(
                            "    segment {} → {} rows [{}]",
                            label,
                            seg.rows,
                            overrides.join(", ")
                        );
                    }
                }
                println!("    members:");
                for m in members {
                    println!("      {} ({})", m.dataset.name, m.dataset.format);
                }
            }
            ExecutionStep::GenerateWitness {
                list_field_name,
                staging_path,
                witness_key,
                linked_path,
                include,
                cardinality,
                ..
            } => {
                let staging = staging_path
                    .file_stem()
                    .and_then(|s: &std::ffi::OsStr| s.to_str())
                    .unwrap_or("?");
                let linked = linked_path
                    .file_stem()
                    .and_then(|s: &std::ffi::OsStr| s.to_str())
                    .unwrap_or("?");
                let wkey = witness_key
                    .file_stem()
                    .and_then(|s: &std::ffi::OsStr| s.to_str())
                    .unwrap_or("?");
                let dist = include
                    .ratio
                    .map(|r| format!(" ratio:{:.0}%", r * 100.0))
                    .unwrap_or_default();
                let count_label = match cardinality {
                    fakeset::models::CountSpec::Fixed(n) => format!("{n}"),
                    fakeset::models::CountSpec::Uniform { min, max } => format!("{min}–{max}"),
                    fakeset::models::CountSpec::Normal { mean, .. } => format!("~{mean}"),
                };
                println!(
                    "[{}] witness: {}.{} from {} (count:{}, linked:{}{}) → {}",
                    i + 1,
                    staging,
                    list_field_name,
                    linked,
                    count_label,
                    linked,
                    dist,
                    wkey
                );
            }
            ExecutionStep::AssembleFromWitness {
                staging_path,
                dataset,
                witness_specs,
            } => {
                let staging = staging_path
                    .file_stem()
                    .and_then(|s: &std::ffi::OsStr| s.to_str())
                    .unwrap_or("?");
                let fields: Vec<&str> = witness_specs
                    .iter()
                    .map(|(n, _, _): &(String, Vec<PathBuf>, Option<String>)| n.as_str())
                    .collect();
                println!(
                    "[{}] assemble from witness: {} ← [{}] ({})",
                    i + 1,
                    staging,
                    fields.join(", "),
                    dataset.format
                );
            }
            ExecutionStep::AccumulateToLinked {
                source_field,
                linked_field,
                linked_path,
                ..
            } => {
                let linked = linked_path
                    .file_stem()
                    .and_then(|s: &std::ffi::OsStr| s.to_str())
                    .unwrap_or("?");
                println!(
                    "[{}] accumulate to linked: {} → {}.{}",
                    i + 1,
                    source_field,
                    linked,
                    linked_field
                );
            }
            ExecutionStep::EmitDataset { dataset, .. } => {
                println!(
                    "[{}] emit dataset: {} ({})",
                    i + 1,
                    dataset.name,
                    dataset.format
                );
            }
            ExecutionStep::WriteSharedOutput {
                output_file,
                format,
                ..
            } => {
                println!("[{}] write shared: {} ({})", i + 1, output_file, format);
            }
        }
        println!();
    }
}

// ---------------------------------------------------------------------------
// DAG pretty-printer
// ---------------------------------------------------------------------------

fn print_datasets(
    datasets: &HashMap<PathBuf, SyntheticDataset>,
    graph: &petgraph::graph::DiGraph<PathBuf, ()>,
) {
    let mut topo = Topo::new(graph);
    let mut order: Vec<PathBuf> = Vec::new();
    while let Some(idx) = topo.next(graph) {
        order.push(graph[idx].clone());
    }

    for path in &order {
        let Some(ds) = datasets.get(path) else {
            continue;
        };

        let rows_str = ds
            .rows
            .map(|r| r.to_string())
            .unwrap_or_else(|| "-".to_string());
        let locale_str = ds
            .locale
            .as_ref()
            .map(|l| format!(", locale: {l}"))
            .unwrap_or_default();
        println!(
            "{} [{}] (rows: {}{})",
            ds.name, ds.format, rows_str, locale_str
        );

        for out in ds.resolved_outputs() {
            println!("  output: {}", out.file);
        }

        if let Some(ref inc) = ds.include {
            let ratio = inc
                .ratio
                .map(|r| format!(" ratio: {:.0}%", r * 100.0))
                .unwrap_or_default();
            println!("  include: {} (ref: {}{})", inc.file, inc.reference, ratio);
        }

        if !ds.data.is_empty() {
            println!("  fields:");
            for field in &ds.data {
                print_field(field, 4);
            }
        }

        println!();
    }
}

fn print_field(field: &fakeset::models::Field, indent: usize) {
    let pad = " ".repeat(indent);

    if matches!(field.field_type, Some(fakeset::models::FieldType::Variant)) {
        let parquet_tag = field
            .parquet
            .as_ref()
            .map(|p| format!(" [parquet:{:?}]", p.datatype))
            .unwrap_or_default();
        println!(
            "{pad}{:<24} type:variant ({} choices){}",
            field.name,
            field.variants.len(),
            parquet_tag
        );
        for (i, v) in field.variants.iter().enumerate() {
            let d = v
                .ratio
                .map(|d| format!("{:.0}%", d * 100.0))
                .unwrap_or_else(|| "free".to_string());
            let vtype = v
                .field_type
                .as_ref()
                .map(|t| format!("{t}"))
                .unwrap_or_else(|| "inferred".to_string());
            let vval = v
                .value
                .as_ref()
                .map(|val| format!("={}", format_yaml_value(val)))
                .unwrap_or_default();
            println!("{pad}  [{i}] {d} type:{vtype}{vval}");
        }
        return;
    }

    if let Some(ref expr) = field.expression {
        let hidden_tag = if field.hidden { " [hidden]" } else { "" };
        println!("{pad}{:<24} expr: {}{}", field.name, expr, hidden_tag);
        return;
    }

    let type_str = field
        .field_type
        .as_ref()
        .map(|t| t.to_string())
        .unwrap_or_else(|| "-".to_string());

    let ref_str = field.simple_ref().unwrap_or("-");

    let gen_str = field
        .generator
        .as_ref()
        .map(|g| g.to_string())
        .unwrap_or_else(|| "-".to_string());

    let mut extras: Vec<String> = Vec::new();
    if let Some(r) = &field.range {
        if let Some(lo) = r.min {
            extras.push(format!("min:{lo}"));
        }
        if let Some(hi) = r.max {
            extras.push(format!("max:{hi}"));
        }
    }
    if let Some(ref v) = field.value {
        extras.push(format!("value:{}", format_yaml_value(v)));
    }
    let extras_str = if extras.is_empty() {
        String::new()
    } else {
        format!("  ({})", extras.join(", "))
    };

    println!(
        "{pad}{:<24} type:{:<12} ref:{:<30} gen:{:<20}{}",
        field.name, type_str, ref_str, gen_str, extras_str
    );

    for sub in &field.fields {
        print_field(sub, indent + 2);
    }
    match field.content.as_deref() {
        Some(c) if c.from.is_none() => {
            print!("{pad}  [content] ");
            print_field(&c.item, indent + 2);
        }
        Some(c) => {
            println!("{pad}  [list-link content]");
            if let Some(ref from) = c.from {
                println!("{pad}    from: {from}");
            }
            for f in &c.item.fields {
                print_field(f, indent + 4);
            }
        }
        None => {}
    }
}

fn format_yaml_value(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Null => "null".to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::String(s) => format!("\"{s}\""),
        serde_yaml::Value::Sequence(_) => "[...]".to_string(),
        serde_yaml::Value::Mapping(_) => "{...}".to_string(),
        serde_yaml::Value::Tagged(t) => format!("{:?}", t),
    }
}
