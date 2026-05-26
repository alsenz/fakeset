//! DAG construction and topological sort. `build_dag` encodes the concept semi-lattice
//! as a petgraph `DiGraph` and topo-sorts it so atoms (most-constrained nodes) are
//! always visited before their parents.
use anyhow::{anyhow, Result};
use petgraph::algo::toposort;
use petgraph::graph::DiGraph;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::models::{resolve_include, SyntheticDataset};

#[derive(Debug)]
pub struct DatasetGraph {
    pub graph: DiGraph<PathBuf, ()>,
}


/// Build the DAG that drives execution order.
///
/// Two kinds of edge are added:
///
/// **Constraint edges** (from top-level `include:`): child → parent. A child is a
/// more-constrained subset of its parent and must execute first (preceding). The child
/// is the edge source so topo visits it before the parent.
///
/// **Data-dependency edges** (from `links:` inside list-link fields):
/// linked dataset → enclosing dataset. The linked dataset supplies row data for witness
/// generation and must be fully computed before the enclosing dataset runs. The linked
/// dataset is the edge source so topo visits it first.
///
/// **Outer-ref ordering (staging → witness)** is *not* expressed as a DAG edge; it is
/// satisfied by construction in `build_plan`, which always emits a `GenerateStagingNode`
/// or `GenerateStagingLowerCoverGroup` step before the paired `GenerateWitness` steps in
/// the linear step list. A future DAG-aware scheduler should make this dependency explicit.
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
        for include in dataset.include.iter() {
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

        // Links (list links and junction links): data dependency — the linked dataset must be
        // computed BEFORE this dataset. Add a reversed edge so topo visits the linked dataset
        // (data provider) before this dataset (data consumer).
        for link in &dataset.links {
            let canonical = resolve_include(path, &link.file).ok_or_else(|| {
                anyhow!(
                    "{}: linked file not found: {}",
                    path.display(),
                    link.file
                )
            })?;
            let to = node_indices.get(&canonical).ok_or_else(|| {
                anyhow!(
                    "{}: link '{}' was not part of the traversal",
                    path.display(),
                    canonical.display()
                )
            })?;
            // Edge direction reversed: linked dataset → this dataset (linked is the predecessor).
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
