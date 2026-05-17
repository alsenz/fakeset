# corporate-registry

A simple three-dataset example modelling a slice of a corporate registry.

## Datasets

| File | Rows | Description |
|---|---|---|
| `smes.yaml` | 50 | Small and medium enterprises. Includes `organisations`. |
| `organisations.yaml` | 100 | Registered companies. Includes `officers`. |
| `individuals.yaml` | 200 | Individual directors and officers attached to companies. |

## Running

From the repository root:

```bash
cargo run -- examples/corporate-registry --output ./output/corporate-registry
```

This writes three Parquet files to `./output/corporate-registry/`:

```
output/corporate-registry/
  smes.parquet
  organisations.parquet
  officers.parquet
```