//! fakeset — declarative synthetic dataset generator.
//!
//! Users write YAML schemas describing a concept semi-lattice of datasets; fakeset
//! generates referentially consistent Parquet/CSV/JSON/JSONL output by executing a
//! topologically-sorted plan in which atoms (most-constrained nodes) are generated first
//! and parent values are accumulated upward.
pub mod constraints;
pub mod dq;
pub mod executor;
pub mod expand_variants;
pub mod expressions;
pub mod generator;
pub mod graph;
pub mod import;
pub mod models;
pub mod output;
pub mod plan;
pub mod rewrite;
pub mod schema;
pub mod segment;
pub mod validate;

use anyhow::{Context, Result};
use models::SyntheticDataset;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub fn collect_yaml_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() && is_yaml(path) {
            files.push(path.clone());
        } else if path.is_dir() {
            for entry in WalkDir::new(path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && is_yaml(e.path()))
            {
                files.push(entry.into_path());
            }
        }
    }
    files
}

fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}

fn load_dataset(path: &Path) -> Result<SyntheticDataset> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
}

pub fn load_all_datasets(paths: &[PathBuf]) -> Result<HashMap<PathBuf, SyntheticDataset>> {
    let mut map = HashMap::new();
    for path in collect_yaml_files(paths) {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", path.display()))?;
        map.insert(canonical, load_dataset(&path)?);
    }
    Ok(map)
}
