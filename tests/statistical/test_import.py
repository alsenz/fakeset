"""
Statistical tests for the IMPORT feature.

Fixture: tests/fixtures/statistical/tickers.csv — 1000 rows (symbol, name, sector).
Schema:  tests/fixtures/statistical/import_partition/ — stocks (imported parent, parquet),
         tech + finance (children at ratio 0.4 each, conflicting bucket constants).

The ring hash is pinned via --seed.ring 1 for fully reproducible results.

Hard invariants (must always hold):
  - stocks.parquet has exactly 1000 rows (= full file, ring [0, 1))
  - tech.parquet and finance.parquet row counts are both > 0
  - tech.parquet rows + finance.parquet rows ≤ 1000 (no duplication)
  - tech.parquet has only bucket="tech"; finance.parquet has only bucket="finance"
  - All stock_id values are distinct within each output file

Soft invariants (statistical, α = 0.01):
  - tech row count is consistent with ~40% of 1000 rows (binomial test)
  - finance row count is consistent with ~40% of 1000 rows (binomial test)
"""

import subprocess
import os
import math
import pytest
import polars as pl
from scipy import stats

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SCHEMA_DIR = os.path.join(REPO_ROOT, "tests", "fixtures", "statistical", "import_partition")


@pytest.fixture(scope="module")
def import_dir(fakeset_binary, tmp_path_factory):
    out = str(tmp_path_factory.mktemp("import_partition"))
    result = subprocess.run(
        [
            fakeset_binary,
            SCHEMA_DIR,
            "--output",
            out,
            "--seed.ring",
            "1",
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        pytest.fail(f"fakeset import_partition failed:\n{result.stderr}")
    return out


@pytest.fixture(scope="module")
def stocks_df(import_dir):
    path = os.path.join(import_dir, "stocks.parquet")
    assert os.path.exists(path), "stocks.parquet must be written"
    return pl.read_parquet(path)


@pytest.fixture(scope="module")
def tech_df(import_dir):
    path = os.path.join(import_dir, "tech.parquet")
    assert os.path.exists(path), "tech.parquet must be written"
    return pl.read_parquet(path)


@pytest.fixture(scope="module")
def finance_df(import_dir):
    path = os.path.join(import_dir, "finance.parquet")
    assert os.path.exists(path), "finance.parquet must be written"
    return pl.read_parquet(path)


# ---------------------------------------------------------------------------
# Hard invariants
# ---------------------------------------------------------------------------


def test_stocks_row_count(stocks_df):
    assert len(stocks_df) == 1000, (
        f"stocks should have all 1000 rows from tickers.csv, got {len(stocks_df)}"
    )


def test_stocks_has_imported_columns(stocks_df):
    for col in ("symbol", "name", "sector"):
        assert col in stocks_df.columns, f"imported column '{col}' must be present"


def test_stocks_has_synthetic_column(stocks_df):
    assert "stock_id" in stocks_df.columns, "synthetic column stock_id must be present"


def test_children_row_counts_positive(tech_df, finance_df):
    assert len(tech_df) > 0, "tech should have rows"
    assert len(finance_df) > 0, "finance should have rows"


def test_no_row_duplication_across_children(tech_df, finance_df):
    """Ring partition must assign each parent row to at most one child."""
    tech_ids = set(tech_df["stock_id"].to_list())
    finance_ids = set(finance_df["stock_id"].to_list())
    overlap = tech_ids & finance_ids
    assert len(overlap) == 0, (
        f"{len(overlap)} stock_id(s) appear in both tech and finance — "
        "the ring partition must be disjoint"
    )


def test_tech_bucket_constant(tech_df):
    assert (tech_df["bucket"] == "tech").all(), "all tech rows must have bucket='tech'"


def test_finance_bucket_constant(finance_df):
    assert (finance_df["bucket"] == "finance").all(), (
        "all finance rows must have bucket='finance'"
    )


def test_stock_ids_distinct_within_stocks(stocks_df):
    n = len(stocks_df)
    n_distinct = stocks_df["stock_id"].n_unique()
    assert n_distinct == n, f"stock_ids must be unique within stocks ({n_distinct} != {n})"


def test_children_are_subset_of_parent(stocks_df, tech_df, finance_df):
    n_children = len(tech_df) + len(finance_df)
    n_parent = len(stocks_df)
    assert n_children <= n_parent, (
        f"children total ({n_children}) must not exceed parent ({n_parent})"
    )


# ---------------------------------------------------------------------------
# Soft invariants (statistical)
# ---------------------------------------------------------------------------


def _require_rows(df, name, minimum=30):
    if len(df) < minimum:
        pytest.skip(f"{name} has only {len(df)} rows — too few for statistical test")


def test_tech_row_count_consistent_with_ratio(tech_df, stocks_df):
    """tech row count should be consistent with ratio=0.4 of the parent (binomial test)."""
    _require_rows(tech_df, "tech")
    n = len(stocks_df)
    k = len(tech_df)
    p = 0.4
    result = stats.binomtest(k, n=n, p=p, alternative="two-sided")
    assert result.pvalue >= 0.01, (
        f"tech row count {k}/{n} is inconsistent with declared ratio 0.4 "
        f"(p={result.pvalue:.4f} < 0.01)"
    )


def test_finance_row_count_consistent_with_ratio(finance_df, stocks_df):
    """finance row count should be consistent with ratio=0.4 of the parent (binomial test)."""
    _require_rows(finance_df, "finance")
    n = len(stocks_df)
    k = len(finance_df)
    p = 0.4
    result = stats.binomtest(k, n=n, p=p, alternative="two-sided")
    assert result.pvalue >= 0.01, (
        f"finance row count {k}/{n} is inconsistent with declared ratio 0.4 "
        f"(p={result.pvalue:.4f} < 0.01)"
    )


def test_ring_hash_distributes_uniformly(stocks_df, tech_df, finance_df):
    """Children together should cover ≥ 70% of parent rows (ring is near-uniform)."""
    combined = len(tech_df) + len(finance_df)
    total = len(stocks_df)
    fraction = combined / total
    assert fraction >= 0.70, (
        f"combined children cover only {fraction:.1%} of parent rows — "
        "ring hash may not be distributing uniformly"
    )
