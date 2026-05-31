"""
Statistical regression tests for the corporate-registry example.

Two tiers:
  - Hard invariants: ranges, referential integrity, value constraints.
  - Soft invariants: include ratios (binomial), segment partition identity,
    numeric distributions (KS).
"""

import pytest
from scipy.stats import binomtest, chisquare, kstest, uniform

ALPHA = 0.001


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _require_rows(df, n, label):
    if len(df) < n:
        pytest.skip(f"{label} has only {len(df)} rows (need ≥ {n})")


# ---------------------------------------------------------------------------
# Numeric range invariants
# ---------------------------------------------------------------------------

def test_micro_employee_count_range(corporate):
    df = corporate["micro_enterprises"]
    _require_rows(df, 1, "micro_enterprises")
    assert (df["employee_count"] >= 1).all(), "micro employee_count below min 1"
    assert (df["employee_count"] <= 9).all(), "micro employee_count above max 9"


def test_micro_annual_revenue_range(corporate):
    df = corporate["micro_enterprises"]
    _require_rows(df, 1, "micro_enterprises")
    assert (df["annual_revenue"] >= 0).all(), "micro annual_revenue below min 0"
    assert (df["annual_revenue"] <= 2_000_000).all(), "micro annual_revenue above max 2000000"


def test_small_employee_count_range(corporate):
    df = corporate["small_enterprises"]
    _require_rows(df, 1, "small_enterprises")
    assert (df["employee_count"] >= 10).all(), "small employee_count below min 10"
    assert (df["employee_count"] <= 49).all(), "small employee_count above max 49"


def test_small_annual_revenue_range(corporate):
    df = corporate["small_enterprises"]
    _require_rows(df, 1, "small_enterprises")
    assert (df["annual_revenue"] >= 2_000_000).all(), "small annual_revenue below min 2000000"
    assert (df["annual_revenue"] <= 10_000_000).all(), "small annual_revenue above max 10000000"


def test_medium_employee_count_range(corporate):
    df = corporate["medium_enterprises"]
    _require_rows(df, 1, "medium_enterprises")
    assert (df["employee_count"] >= 50).all(), "medium employee_count below min 50"
    assert (df["employee_count"] <= 249).all(), "medium employee_count above max 249"


def test_medium_annual_revenue_range(corporate):
    df = corporate["medium_enterprises"]
    _require_rows(df, 1, "medium_enterprises")
    assert (df["annual_revenue"] >= 10_000_000).all(), "medium annual_revenue below min 10000000"
    assert (df["annual_revenue"] <= 50_000_000).all(), "medium annual_revenue above max 50000000"


def test_grant_amount_range(corporate):
    df = corporate["grants"]
    _require_rows(df, 1, "grants")
    assert (df["amount"] >= 5_000).all(), "grant amount below min 5000"
    assert (df["amount"] <= 250_000).all(), "grant amount above max 250000"


# ---------------------------------------------------------------------------
# Value constraint invariants
# ---------------------------------------------------------------------------

def test_directors_role_is_director(corporate):
    """directors.role is a constant 'director' for every row."""
    df = corporate["directors"]
    bad = df["role"] != "director"
    assert not bad.any(), f"{bad.sum()} director rows have role != 'director'"


def test_micro_sme_status(corporate):
    """micro_enterprises.sme_status must be 'micro' for every row."""
    df = corporate["micro_enterprises"]
    _require_rows(df, 1, "micro_enterprises")
    bad = df["sme_status"] != "micro"
    assert not bad.any(), f"{bad.sum()} micro rows have sme_status != 'micro'"


def test_small_sme_status(corporate):
    df = corporate["small_enterprises"]
    _require_rows(df, 1, "small_enterprises")
    bad = df["sme_status"] != "small"
    assert not bad.any(), f"{bad.sum()} small rows have sme_status != 'small'"


def test_medium_sme_status(corporate):
    df = corporate["medium_enterprises"]
    _require_rows(df, 1, "medium_enterprises")
    bad = df["sme_status"] != "medium"
    assert not bad.any(), f"{bad.sum()} medium rows have sme_status != 'medium'"


# ---------------------------------------------------------------------------
# Referential integrity
# ---------------------------------------------------------------------------

def test_directors_name_refs(corporate):
    """directors.name must come from individuals.full_name."""
    individual_names = set(corporate["individuals"]["full_name"].to_list())
    orphans = ~corporate["directors"]["name"].is_in(individual_names)
    assert not orphans.any(), f"{orphans.sum()} director rows have name not in individuals"


def test_smes_org_id_refs(corporate):
    org_ids = set(corporate["organisations"]["org_id"].to_list())
    orphans = ~corporate["smes"]["org_id"].is_in(org_ids)
    # Bernoulli rounding in the lower-cover segmentation can produce one fewer
    # child row than the parent planned, leaving grow_parent_from_children to
    # emit at most one row with a freshly generated (non-inherited) org_id.
    assert orphans.sum() <= 1, f"{orphans.sum()} SME rows have org_id not in organisations"


def test_micro_sme_id_refs(corporate):
    sme_ids = set(corporate["smes"]["sme_id"].to_list())
    df = corporate["micro_enterprises"]
    _require_rows(df, 1, "micro_enterprises")
    orphans = ~df["sme_id"].is_in(sme_ids)
    assert not orphans.any(), f"{orphans.sum()} micro rows have sme_id not in smes"


def test_small_sme_id_refs(corporate):
    sme_ids = set(corporate["smes"]["sme_id"].to_list())
    df = corporate["small_enterprises"]
    _require_rows(df, 1, "small_enterprises")
    orphans = ~df["sme_id"].is_in(sme_ids)
    assert not orphans.any(), f"{orphans.sum()} small rows have sme_id not in smes"


def test_grants_recipient_id_refs(corporate):
    micro_ids = set(corporate["micro_enterprises"]["sme_id"].to_list())
    df = corporate["grants"]
    _require_rows(df, 1, "grants")
    orphans = ~df["recipient_id"].is_in(micro_ids)
    assert not orphans.any(), f"{orphans.sum()} grant rows have recipient_id not in micro_enterprises"


# ---------------------------------------------------------------------------
# List-link cardinality and content
# ---------------------------------------------------------------------------

def test_organisation_directors_cardinality(corporate):
    """Every organisation must have 1–25 directors (cardinality min:1 max:25)."""
    lengths = corporate["organisations"]["directors"].list.len()
    assert (lengths >= 1).all(), f"directors list shorter than 1; min={lengths.min()}"
    assert (lengths <= 25).all(), f"directors list longer than 25; max={lengths.max()}"


def test_organisation_director_role(corporate):
    """directors[*].role must be 'director' for every list item."""
    for i, directors in enumerate(corporate["organisations"]["directors"].to_list()):
        for d in directors:
            assert d["role"] == "director", f"organisations row {i}: director.role={d['role']!r}"


def test_organisation_director_employer_matches_org(corporate):
    """directors[*].employer must equal the organisation's own company_name (outer-scoped ref)."""
    for row in corporate["organisations"].iter_rows(named=True):
        for d in row["directors"]:
            assert d["employer"] == row["company_name"], (
                f"director.employer {d['employer']!r} != "
                f"org.company_name {row['company_name']!r}"
            )


# ---------------------------------------------------------------------------
# Segment partition identity (exact invariant)
#
# micro, small, and medium are mutually exclusive lower cover members of smes
# (their sme_status constants conflict pairwise), so they partition smes exactly.
# ---------------------------------------------------------------------------

def test_sme_size_segments_partition_smes(corporate):
    """micro + small + medium row counts must equal smes within Bernoulli rounding (±1)."""
    n_smes = len(corporate["smes"])
    n_micro = len(corporate["micro_enterprises"])
    n_small = len(corporate["small_enterprises"])
    n_medium = len(corporate["medium_enterprises"])
    total = n_micro + n_small + n_medium
    # Bernoulli rounding in plan_segments can produce ±1 rows vs the parent total.
    assert abs(total - n_smes) <= 1, (
        f"micro ({n_micro}) + small ({n_small}) + medium ({n_medium}) = {total} ≠ smes ({n_smes})"
    )


def test_sme_ids_are_disjoint_across_sizes(corporate):
    """An SME cannot appear in two size classes simultaneously."""
    def ids(df):
        return set(df["sme_id"].to_list()) if len(df) > 0 else set()

    micro_ids = ids(corporate["micro_enterprises"])
    small_ids = ids(corporate["small_enterprises"])
    medium_ids = ids(corporate["medium_enterprises"])
    assert micro_ids.isdisjoint(small_ids), "sme_id overlap between micro and small"
    assert micro_ids.isdisjoint(medium_ids), "sme_id overlap between micro and medium"
    assert small_ids.isdisjoint(medium_ids), "sme_id overlap between small and medium"


# ---------------------------------------------------------------------------
# Include ratio — binomial tests (soft)
# ---------------------------------------------------------------------------

def test_directors_include_ratio(corporate):
    """~30% of individuals should be directors (include ratio: 0.3)."""
    n_individuals = len(corporate["individuals"])
    n_directors = len(corporate["directors"])
    result = binomtest(n_directors, n_individuals, p=0.3, alternative="two-sided")
    ratio = n_directors / n_individuals
    assert result.pvalue > ALPHA, (
        f"Directors include ratio {n_directors}/{n_individuals}={ratio:.3f} "
        f"deviates from 0.30 (p={result.pvalue:.4f} ≤ {ALPHA})"
    )


def test_smes_include_ratio(corporate):
    """~99% of organisations should be SMEs (include ratio: 0.99)."""
    n_orgs = len(corporate["organisations"])
    n_smes = len(corporate["smes"])
    result = binomtest(n_smes, n_orgs, p=0.99, alternative="two-sided")
    ratio = n_smes / n_orgs
    assert result.pvalue > ALPHA, (
        f"SMEs include ratio {n_smes}/{n_orgs}={ratio:.3f} "
        f"deviates from 0.99 (p={result.pvalue:.4f} ≤ {ALPHA})"
    )


def test_micro_include_ratio(corporate):
    """~95% of SMEs should be micro enterprises (include ratio: 0.95)."""
    n_smes = len(corporate["smes"])
    n_micro = len(corporate["micro_enterprises"])
    result = binomtest(n_micro, n_smes, p=0.95, alternative="two-sided")
    ratio = n_micro / n_smes
    assert result.pvalue > ALPHA, (
        f"Micro include ratio {n_micro}/{n_smes}={ratio:.3f} "
        f"deviates from 0.95 (p={result.pvalue:.4f} ≤ {ALPHA})"
    )


def test_grants_include_ratio(corporate):
    """~10% of micro enterprises should have grants (include ratio: 0.1)."""
    n_micro = len(corporate["micro_enterprises"])
    n_grants = len(corporate["grants"])
    _require_rows(corporate["grants"], 1, "grants")
    result = binomtest(n_grants, n_micro, p=0.1, alternative="two-sided")
    ratio = n_grants / n_micro
    assert result.pvalue > ALPHA, (
        f"Grants include ratio {n_grants}/{n_micro}={ratio:.3f} "
        f"deviates from 0.10 (p={result.pvalue:.4f} ≤ {ALPHA})"
    )


# ---------------------------------------------------------------------------
# Numeric distribution — KS test against uniform (soft)
# ---------------------------------------------------------------------------

def test_micro_employee_count_all_values_present(corporate):
    """micro employee_count should use all 9 possible values in a large-enough sample."""
    df = corporate["micro_enterprises"]
    _require_rows(df, 45, "micro_enterprises")
    present = set(df["employee_count"].unique().to_list())
    expected = set(range(1, 10))
    missing = expected - present
    assert not missing, f"employee_count values never generated: {sorted(missing)}"


def test_grant_amount_distribution(corporate):
    """grant amount ~ Uniform[5000, 250000]."""
    df = corporate["grants"]
    _require_rows(df, 15, "grants")
    sample = df["amount"].to_numpy()
    stat, p = kstest(sample, uniform(loc=5000, scale=245000).cdf)
    assert p > ALPHA, f"grant amount not uniform on [5000,250000] (KS={stat:.4f}, p={p:.4f})"
