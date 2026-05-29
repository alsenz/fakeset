"""
Session-scoped fixtures: build the fakeset binary once, run each example once,
load output Parquet files into DataFrames for the duration of the test session.
"""

import os
import subprocess

import polars as pl
import pytest

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BINARY = os.path.join(REPO_ROOT, "target", "release", "fakeset")


def _read(directory: str, name: str) -> pl.DataFrame:
    """Read a Parquet file, returning an empty DataFrame when the file is absent.

    Parquet files with 0 expected rows (e.g. medium_enterprises at 0.67% of a small
    population) may not be written at all; tests should call _require_rows() to skip.
    """
    path = os.path.join(directory, f"{name}.parquet")
    if not os.path.exists(path):
        return pl.DataFrame()
    return pl.read_parquet(path)


@pytest.fixture(scope="session")
def fakeset_binary():
    result = subprocess.run(
        ["cargo", "build", "--release", "--bin", "fakeset"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        pytest.fail(f"cargo build --release failed:\n{result.stderr}")
    if not os.path.exists(BINARY):
        pytest.fail(f"Binary not found after build: {BINARY}")
    return BINARY


@pytest.fixture(scope="session")
def insurance_dir(fakeset_binary, tmp_path_factory):
    out = str(tmp_path_factory.mktemp("insurance_stat"))
    result = subprocess.run(
        [fakeset_binary, "examples/insurance", "--output", out],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        pytest.fail(f"fakeset examples/insurance failed:\n{result.stderr}")
    return out


@pytest.fixture(scope="session")
def corporate_dir(fakeset_binary, tmp_path_factory):
    out = str(tmp_path_factory.mktemp("corporate_stat"))
    result = subprocess.run(
        [fakeset_binary, "examples/corporate-registry", "--output", out],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        pytest.fail(f"fakeset examples/corporate-registry failed:\n{result.stderr}")
    return out


@pytest.fixture(scope="session")
def insurance(insurance_dir):
    return {
        name: _read(insurance_dir, name)
        for name in ["customers", "policies", "contracts", "premiums", "claims"]
    }


@pytest.fixture(scope="session")
def corporate(corporate_dir):
    return {
        name: _read(corporate_dir, name)
        for name in [
            "individuals",
            "directors",
            "organisations",
            "smes",
            "micro_enterprises",
            "small_enterprises",
            "medium_enterprises",
            "grants",
        ]
    }
