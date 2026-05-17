use anyhow::{anyhow, Result};
use petgraph::algo::toposort;
use petgraph::graph::DiGraph;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::models::{resolve_include, Field, Include, SyntheticDataset};

#[derive(Debug)]
pub struct DatasetGraph {
    pub graph: DiGraph<PathBuf, ()>,
}

/// Collect all `Include` entries declared inside `content: {includes: [...]}` blocks,
/// scanning recursively through nested object and list fields.
fn collect_list_content_includes(fields: &[Field]) -> Vec<&Include> {
    let mut result = Vec::new();
    for field in fields {
        if let Some(content) = &field.content {
            if !content.includes.is_empty() {
                result.extend(&content.includes);
                result.extend(collect_list_content_includes(&content.item.fields));
            }
        }
        result.extend(collect_list_content_includes(&field.fields));
    }
    result
}

/// Build the DAG that drives execution order.
///
/// Two kinds of edge are added:
///
/// **Constraint edges** (from top-level `includes:`): child → parent. A child is a
/// more-constrained subset of its parent and must execute first (preceding). The child
/// is the edge source so topo visits it before the parent.
///
/// **Data-dependency edges** (from `content: {includes: [...]}` inside list fields):
/// pool dataset → enclosing dataset. The pool dataset supplies row data for the rich
/// list and must be fully computed before the enclosing dataset runs. The pool dataset
/// is the edge source so topo visits it first.
pub fn build_dag(datasets: &HashMap<PathBuf, SyntheticDataset>) -> Result<DatasetGraph> {
    let mut graph: DiGraph<PathBuf, ()> = DiGraph::new();
    let mut node_indices = HashMap::new();

    for path in datasets.keys() {
        let idx = graph.add_node(path.clone());
        node_indices.insert(path.clone(), idx);
    }

    for (path, dataset) in datasets {
        let from = node_indices[path];

        // Constraint includes: child → parent edge. Children (more constrained) are
        // preceding — the edge makes the child a predecessor so topo visits it first.
        for include in &dataset.includes {
            let canonical = resolve_include(path, &include.file).ok_or_else(|| {
                anyhow!(
                    "{}: included file not found: {}",
                    path.display(),
                    include.file
                )
            })?;
            let to = node_indices.get(&canonical).ok_or_else(|| {
                anyhow!(
                    "{}: include '{}' was not part of the traversal",
                    path.display(),
                    canonical.display()
                )
            })?;
            if !graph.contains_edge(from, *to) {
                graph.add_edge(from, *to, ());
            }
        }

        // List-content includes: data dependency — the included dataset must be computed
        // BEFORE this dataset (opposite order to constraint includes). Add a reversed edge
        // so topo visits the includee (data provider) before the includer (data consumer).
        for include in collect_list_content_includes(&dataset.data) {
            let canonical = resolve_include(path, &include.file).ok_or_else(|| {
                anyhow!(
                    "{}: list-content included file not found: {}",
                    path.display(),
                    include.file
                )
            })?;
            let to = node_indices.get(&canonical).ok_or_else(|| {
                anyhow!(
                    "{}: list-content include '{}' was not part of the traversal",
                    path.display(),
                    canonical.display()
                )
            })?;
            // Edge direction reversed: includee → includer (includee is the predecessor).
            if !graph.contains_edge(*to, from) {
                graph.add_edge(*to, from, ());
            }
        }
    }

    match toposort(&graph, None) {
        Ok(_) => Ok(DatasetGraph { graph }),
        Err(cycle) => Err(anyhow!(
            "circular include detected involving: {}",
            graph[cycle.node_id()].display()
        )),
    }
}
